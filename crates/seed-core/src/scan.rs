//! Filesystem scanning: turn a folder on disk into the canonical set of
//! [`FileEntry`]s that the manifest's merkle root is computed over.
//!
//! Paths are stored relative to the share root, using forward slashes, so the
//! same file produces the same key on Windows and Unix. Ignore patterns are
//! `.gitignore`/`.stignore`-style globs evaluated against the relative path.
//!
//! This module is intentionally free of any iroh dependency: it is the bridge
//! between the local filesystem and the [`crate::manifest`] trust model, and is
//! unit-tested on its own.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use walkdir::WalkDir;

use crate::manifest::FileEntry;

/// A compiled set of ignore globs.
#[derive(Clone, Default)]
pub struct IgnoreSet {
    set: Option<GlobSet>,
}

impl IgnoreSet {
    /// Compile ignore patterns. Patterns that fail to parse are skipped (the
    /// returned vec lists them) rather than failing the whole scan.
    pub fn compile(patterns: &[String]) -> (Self, Vec<String>) {
        let mut builder = GlobSetBuilder::new();
        let mut bad = Vec::new();
        for p in patterns {
            // Allow a leading slash to mean "anchored at root"; globset matches
            // against the relative path either way, so just trim it.
            let pat = p.trim_start_matches('/');
            match Glob::new(pat) {
                Ok(g) => {
                    builder.add(g);
                }
                Err(_) => bad.push(p.clone()),
            }
        }
        match builder.build() {
            Ok(set) => (Self { set: Some(set) }, bad),
            Err(_) => (Self { set: None }, patterns.to_vec()),
        }
    }

    /// Whether a relative POSIX path is ignored. Also matches if any ancestor
    /// directory is ignored (so `node_modules` ignores everything beneath it).
    pub fn is_ignored(&self, rel_posix: &str) -> bool {
        let Some(set) = &self.set else {
            return false;
        };
        if set.is_match(rel_posix) {
            return true;
        }
        // Check ancestor directories: "a/b/c" -> test "a", "a/b".
        let mut acc = String::new();
        for comp in rel_posix.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(comp);
            if acc != rel_posix && set.is_match(&acc) {
                return true;
            }
        }
        false
    }
}

/// Fail if the share root itself can't be opened for enumeration, instead of letting
/// `WalkDir(...).filter_map(|e| e.ok())` silently swallow the error and return an empty
/// set. A silent empty result from an *unreadable* root is dangerous: it looks exactly
/// like "the folder is empty", which reports a false 100%-healthy and, on a master,
/// would tombstone every file (propagating the deletion to peers). A genuinely empty
/// but *readable* directory still returns `Ok(())`.
fn ensure_root_readable(root: &Path) -> std::io::Result<()> {
    // Opening the directory for reading is exactly what the walk needs; if that fails
    // (permission denied, not a directory, gone), surface it rather than hide it. An
    // empty readable directory yields an empty iterator and still succeeds here.
    std::fs::read_dir(root).map(|_| ())
}

/// Convert an absolute path under `root` into a relative POSIX path string.
fn rel_posix(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut s = String::new();
    for comp in rel.components() {
        let part = comp.as_os_str().to_string_lossy();
        if !s.is_empty() {
            s.push('/');
        }
        s.push_str(&part);
    }
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Result of hashing a single file.
pub struct ScannedFile {
    pub entry: FileEntry,
    /// Absolute path on disk (for the engine to import into blob storage).
    pub abs_path: PathBuf,
}

/// A file discovered by [`list_files`]: relative POSIX path, absolute path, and
/// size — with no content hash (the publish path obtains the hash from the blob
/// store as it imports the file).
pub struct ListedFile {
    pub rel: String,
    pub abs: PathBuf,
    pub size: u64,
}

/// Walk `root` like [`scan`] but **without hashing** file contents: returns the
/// live (non-ignored, non-symlink) file set with sizes, sorted by relative path
/// (so the manifest's file order is deterministic). Used by publish, which reads
/// each file exactly once by importing it into the blob store.
pub fn list_files(root: &Path, ignore: &IgnoreSet) -> std::io::Result<Vec<ListedFile>> {
    ensure_root_readable(root)?;
    let mut out = Vec::new();
    for dent in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !dent.file_type().is_file() {
            continue;
        }
        let Some(rel) = rel_posix(root, dent.path()) else {
            continue;
        };
        if ignore.is_ignored(&rel) {
            continue;
        }
        let size = dent.metadata().map(|m| m.len()).unwrap_or(0);
        out.push(ListedFile {
            rel,
            abs: dent.path().to_path_buf(),
            size,
        });
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

/// Hash one file with BLAKE3, streaming so large files don't load into memory.
pub fn hash_file(path: &Path) -> std::io::Result<(Vec<u8>, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut f = std::fs::File::open(path)?;
    let n = std::io::copy(&mut f, &mut hasher)?;
    Ok((hasher.finalize().as_bytes().to_vec(), n))
}

/// A cheap change signature over `root`: a hash of the sorted
/// `(relative path, size, mtime)` tuples, without reading file contents. The
/// master's reconcile loop compares this between ticks and only does a full
/// scan + republish when it changes.
///
/// `exclude` holds relative POSIX paths to leave OUT of the signature — the files
/// that couldn't be read/imported this round. Excluding them is what keeps the
/// gate honest: a skipped file is handled by the targeted retry instead, so it
/// must not make the signature look "settled" (which would suppress full scans and
/// hide later adds/deletes — the poisoning bug). Pass an empty set for "everything".
pub fn quick_signature(root: &Path, ignore: &IgnoreSet, exclude: &HashSet<String>) -> u64 {
    signature_map(root, ignore, exclude).0
}

/// Per-path `(size, mtime_nanos)` as observed by one signature walk. Sorted by
/// relative path, which is what makes [`hash_signature`] order-independent of the
/// walk.
pub type SigMap = BTreeMap<String, (u64, u128)>;

/// One signature walk, returning both the [`quick_signature`] value and the
/// per-path metadata it was computed from.
///
/// The map is what lets a reconcile pass tell *its own* disk writes apart from a
/// file the user changed underneath it while the pass was running — see
/// [`settled_signature`].
pub fn signature_map(root: &Path, ignore: &IgnoreSet, exclude: &HashSet<String>) -> (u64, SigMap) {
    use std::time::UNIX_EPOCH;
    let mut map = SigMap::new();
    for dent in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !dent.file_type().is_file() {
            continue;
        }
        let Some(rel) = rel_posix(root, dent.path()) else {
            continue;
        };
        if ignore.is_ignored(&rel) || exclude.contains(&rel) {
            continue;
        }
        let (size, mtime) = dent
            .metadata()
            .ok()
            .map(|m| {
                let mt = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                (m.len(), mt)
            })
            .unwrap_or((0, 0));
        map.insert(rel, (size, mtime));
    }
    (hash_signature(&map), map)
}

/// Hash a [`SigMap`] into the folder's change signature. Iteration order is the
/// map's (sorted by path), so this matches the value a plain walk produces.
pub fn hash_signature(map: &SigMap) -> u64 {
    let mut hasher = blake3::Hasher::new();
    for (path, (size, mtime)) in map {
        hasher.update(path.as_bytes());
        hasher.update(&size.to_le_bytes());
        hasher.update(&mtime.to_le_bytes());
    }
    let bytes = hasher.finalize();
    u64::from_le_bytes(bytes.as_bytes()[..8].try_into().unwrap())
}

/// Domain separator mixed into a signature that is *not* a settled observation of
/// the folder. It can never appear in a value produced by a plain walk, which is
/// exactly the point: a drifted signature is guaranteed to differ from the next
/// pass's [`quick_signature`], forcing the rescan.
const DRIFT_TAG: &[u8] = b"seed-sync/quick-sig/drifted/v1";

/// The signature to record as "the folder as this reconcile pass left it".
///
/// A pass takes real time — on a multi-GB share, over a minute — and the user can
/// write to the folder throughout it. Recording a fresh end-of-pass walk (which is
/// what this replaces) silently absorbed those writes into the "settled" value: the
/// next pass then compared the new signature against a baseline that already
/// included the change it had never scanned, decided nothing had changed, and
/// skipped the full scan. A file overwritten while a pass was running therefore
/// stopped propagating entirely, until some *other* change moved the signature or
/// the 4-hourly deep verify forced a rescan. See known-issues #30.
///
/// So the settled signature covers only paths the pass can actually vouch for:
///   * paths **we** wrote this pass (`wrote`) — absorb their new metadata, since
///     re-scanning our own materialize/delete is the churn the signature exists to
///     avoid;
///   * paths whose `(size, mtime)` is unchanged since the pass's opening walk.
///
/// Anything else *drifted* under us. Drifted paths are left out of the settled set
/// and the result is tagged, so the next pass's signature cannot match it and a
/// full scan is guaranteed. Returns the signature and the drifted paths (for
/// logging). Vanished-under-us paths count as drift too — they are absent from
/// `after`, so without the tag the next walk would agree and the delete would never
/// propagate.
pub fn settled_signature(
    before: &SigMap,
    after: &SigMap,
    wrote: &HashSet<String>,
) -> (u64, Vec<String>) {
    let mut settled = SigMap::new();
    let mut drifted: Vec<String> = Vec::new();
    for (path, sig) in after {
        if wrote.contains(path) || before.get(path) == Some(sig) {
            settled.insert(path.clone(), *sig);
        } else {
            // Changed under us, or appeared mid-pass and was never scanned.
            drifted.push(path.clone());
        }
    }
    // Gone since the opening walk without us removing it: a user delete we haven't
    // scanned. Nothing to exclude (it isn't in `after`), so it only matters via the
    // tag below.
    for path in before.keys() {
        if !after.contains_key(path) && !wrote.contains(path) {
            drifted.push(path.clone());
        }
    }
    if drifted.is_empty() {
        return (hash_signature(&settled), drifted);
    }
    drifted.sort();
    drifted.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(DRIFT_TAG);
    hasher.update(&hash_signature(&settled).to_le_bytes());
    for path in &drifted {
        hasher.update(path.as_bytes());
    }
    let bytes = hasher.finalize();
    (
        u64::from_le_bytes(bytes.as_bytes()[..8].try_into().unwrap()),
        drifted,
    )
}

/// Walk `root`, skipping ignored paths, and produce the live (non-deleted)
/// file set. Symlinks are not followed. Hidden control dirs (e.g. our own
/// `.seed`) should be passed in `ignore`.
/// Returns `(files, skipped)` where `skipped` is the relative paths present on disk
/// but unreadable this pass (locked by another process, permission denied). Skipped
/// files are NOT an error — the caller retries them later — so one bad file never
/// aborts the whole scan (which would block the entire share from publishing).
pub fn scan(root: &Path, ignore: &IgnoreSet) -> std::io::Result<(Vec<ScannedFile>, Vec<String>)> {
    ensure_root_readable(root)?;
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for dent in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !dent.file_type().is_file() {
            continue;
        }
        let Some(rel) = rel_posix(root, dent.path()) else {
            continue;
        };
        if ignore.is_ignored(&rel) {
            continue;
        }
        // A file we can't read (locked by another process, permission denied, a
        // transient/odd entry) must NOT abort the whole scan — that would block the
        // entire share from publishing over one bad file. Skip it (it stays as-is on
        // disk and in the manifest) and retry on a later pass.
        let (hash, size) = match hash_file(dent.path()) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "scan: skipping {} (cannot read; will retry): {e}",
                    dent.path().display()
                );
                skipped.push(rel);
                continue;
            }
        };
        out.push(ScannedFile {
            entry: FileEntry {
                path: rel,
                hash,
                size,
                deleted: false,
            },
            abs_path: dent.path().to_path_buf(),
        });
    }
    out.sort_by(|a, b| a.entry.path.cmp(&b.entry.path));
    Ok((out, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn ignores_patterns_and_subtrees() {
        let (ig, bad) =
            IgnoreSet::compile(&["*.tmp".into(), "node_modules".into(), "/.seed".into()]);
        assert!(bad.is_empty());
        assert!(ig.is_ignored("foo.tmp"));
        assert!(ig.is_ignored("node_modules"));
        assert!(ig.is_ignored("node_modules/pkg/index.js")); // subtree
        assert!(ig.is_ignored(".seed/state.db"));
        assert!(!ig.is_ignored("src/main.rs"));
    }

    #[test]
    fn scan_produces_sorted_relative_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("b.txt"), b"hello").unwrap();
        fs::write(root.join("sub/a.bin"), b"world!!").unwrap();
        fs::write(root.join("skip.tmp"), b"junk").unwrap();

        let (ig, _) = IgnoreSet::compile(&["*.tmp".into()]);
        let (files, skipped) = scan(root, &ig).unwrap();
        assert!(skipped.is_empty());

        let paths: Vec<&str> = files.iter().map(|f| f.entry.path.as_str()).collect();
        assert_eq!(paths, vec!["b.txt", "sub/a.bin"]); // sorted, forward slashes, .tmp skipped

        let b = &files[0];
        assert_eq!(b.entry.size, 5);
        assert_eq!(b.entry.hash, blake3::hash(b"hello").as_bytes().to_vec());
    }

    #[test]
    fn empty_dir_scans_clean() {
        let dir = tempfile::tempdir().unwrap();
        let (ig, _) = IgnoreSet::compile(&[]);
        let (files, skipped) = scan(dir.path(), &ig).unwrap();
        assert!(files.is_empty());
        assert!(skipped.is_empty());
    }

    /// A share root that can't be read must make the scan ERROR, not return an empty
    /// set — a silent "empty folder" from an unreadable root reports a false
    /// 100%-healthy and, on a master, would tombstone every file. Regression for the
    /// macOS "reports Healthy 100% with 0 files" investigation.
    #[test]
    fn scan_errors_on_missing_root() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let (ig, _) = IgnoreSet::compile(&[]);
        assert!(scan(&missing, &ig).is_err(), "missing root must error");
        assert!(
            list_files(&missing, &ig).is_err(),
            "missing root must error"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scan_errors_on_unreadable_root() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("locked-dir");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.txt"), b"hi").unwrap();
        // 0o000: the directory itself can't be opened for reading/enumeration.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();

        let (ig, _) = IgnoreSet::compile(&[]);
        let scanned = scan(&root, &ig);
        let listed = list_files(&root, &ig);

        // Restore perms before asserting so the tempdir cleans up regardless.
        let _ = fs::set_permissions(&root, fs::Permissions::from_mode(0o755));
        assert!(
            scanned.is_err(),
            "unreadable root must error, not scan empty"
        );
        assert!(
            listed.is_err(),
            "unreadable root must error, not list empty"
        );
    }

    /// `quick_signature` must NOT count excluded paths — that's what stops a
    /// skipped/unpublished file from making the folder look "settled" and
    /// suppressing future scans (the gate-poisoning bug). An excluded new file
    /// leaves the signature unchanged; a non-excluded new file changes it.
    #[test]
    fn quick_signature_excludes_listed_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), b"a").unwrap();
        let (ig, _) = IgnoreSet::compile(&[]);
        let empty = HashSet::new();
        let base = quick_signature(root, &ig, &empty);

        fs::write(root.join("b.txt"), b"b").unwrap();
        let with_b = quick_signature(root, &ig, &empty);
        assert_ne!(
            base, with_b,
            "a new (non-excluded) file must change the signature"
        );

        let excl: HashSet<String> = std::iter::once("b.txt".to_string()).collect();
        let with_b_excluded = quick_signature(root, &ig, &excl);
        assert_eq!(
            base, with_b_excluded,
            "an excluded file must not affect the signature (no gate poisoning)"
        );
    }

    fn sig(pairs: &[(&str, u64, u128)]) -> SigMap {
        pairs
            .iter()
            .map(|(p, s, m)| (p.to_string(), (*s, *m)))
            .collect()
    }

    fn set(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    /// The steady state: nothing moved under the pass, so the settled signature is
    /// just the folder's signature and the next pass skips its full scan.
    #[test]
    fn settled_signature_absorbs_a_quiet_pass() {
        let before = sig(&[("a.txt", 1, 10), ("b.txt", 2, 20)]);
        let (s, drifted) = settled_signature(&before, &before, &HashSet::new());
        assert!(drifted.is_empty());
        assert_eq!(
            s,
            hash_signature(&before),
            "an untouched folder settles at its own signature"
        );
    }

    /// Files the pass itself materialized/deleted must be absorbed — re-scanning our
    /// own writes every tick is the churn the quick signature exists to prevent.
    #[test]
    fn settled_signature_absorbs_our_own_writes() {
        let before = sig(&[("a.txt", 1, 10)]);
        let after = sig(&[("a.txt", 1, 10), ("new.bin", 9, 99)]);
        let (s, drifted) = settled_signature(&before, &after, &set(&["new.bin"]));
        assert!(drifted.is_empty(), "our own materialize is not drift");
        assert_eq!(
            s,
            hash_signature(&after),
            "the folder we just wrote is settled as-is"
        );
    }

    /// The regression (known-issues #30): a file overwritten by the *user* while the
    /// pass was running must NOT be absorbed. If it is, the next pass compares the
    /// new signature against a baseline that already contains the change it never
    /// scanned, skips the full scan, and the overwrite never propagates.
    #[test]
    fn settled_signature_rejects_a_midpass_overwrite() {
        let before = sig(&[("iso.bin", 100, 10)]);
        let after = sig(&[("iso.bin", 200, 50)]); // user replaced it mid-pass
        let (s, drifted) = settled_signature(&before, &after, &HashSet::new());
        assert_eq!(drifted, vec!["iso.bin".to_string()]);
        assert_ne!(
            s,
            hash_signature(&after),
            "a mid-pass overwrite must not settle as scanned — the next pass has to rescan"
        );
        // The concrete guarantee: whatever the next pass's walk produces, it differs
        // from what we recorded, so `do_scan` is true.
        assert_ne!(s, hash_signature(&before));
    }

    /// Same trap for a file *created* mid-pass: it was never scanned, so absorbing it
    /// would make it invisible until something else moved the signature.
    #[test]
    fn settled_signature_rejects_a_midpass_create() {
        let before = sig(&[("a.txt", 1, 10)]);
        let after = sig(&[("a.txt", 1, 10), ("dropped-in.bin", 5, 55)]);
        let (s, drifted) = settled_signature(&before, &after, &HashSet::new());
        assert_eq!(drifted, vec!["dropped-in.bin".to_string()]);
        assert_ne!(s, hash_signature(&after));
    }

    /// And for a file the user *deleted* mid-pass. This one can't be handled by
    /// excluding a path (it isn't in `after` to exclude), so it relies on the drift
    /// tag — without it the next walk would agree and the delete would never
    /// propagate to peers.
    #[test]
    fn settled_signature_rejects_a_midpass_delete() {
        let before = sig(&[("a.txt", 1, 10), ("gone.bin", 7, 70)]);
        let after = sig(&[("a.txt", 1, 10)]);
        let (s, drifted) = settled_signature(&before, &after, &HashSet::new());
        assert_eq!(drifted, vec!["gone.bin".to_string()]);
        assert_ne!(
            s,
            hash_signature(&after),
            "a mid-pass delete must force a rescan, not settle silently"
        );
    }

    /// Drift must clear once the folder holds still: otherwise a single mid-pass
    /// write would pin the share into rescanning forever.
    #[test]
    fn settled_signature_converges_once_the_folder_holds_still() {
        let before = sig(&[("iso.bin", 100, 10)]);
        let after = sig(&[("iso.bin", 200, 50)]);
        let (drifted_sig, _) = settled_signature(&before, &after, &HashSet::new());

        // Next pass: opens on the new state, nothing changes under it.
        let (settled, drifted) = settled_signature(&after, &after, &HashSet::new());
        assert!(drifted.is_empty());
        assert_eq!(settled, hash_signature(&after));
        assert_ne!(
            settled, drifted_sig,
            "the rescan pass settles at a different value than the drifted one"
        );
    }

    /// A file that can't be read (locked by another process, permission denied)
    /// must be SKIPPED, not abort the whole scan — otherwise a single bad file
    /// blocks the entire share from publishing. Regression for the reported
    /// "one locked file breaks sync completely" bug.
    #[test]
    fn scan_skips_unreadable_file_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("good.txt"), b"hello").unwrap();
        let bad = root.join("locked.bin");
        fs::write(&bad, b"some bytes").unwrap();

        // Hold `locked.bin` unreadable for the duration of the scan.
        #[cfg(windows)]
        let _guard = {
            use std::os::windows::fs::OpenOptionsExt;
            // share_mode(0) = deny all sharing, so hash_file's File::open hits a
            // sharing violation (os error 32) — exactly a file held by another process.
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&bad)
                .unwrap()
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();
        }

        let (ig, _) = IgnoreSet::compile(&[]);
        let (files, skipped) = scan(root, &ig).expect("scan must not abort on one unreadable file");
        let paths: Vec<&str> = files.iter().map(|f| f.entry.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["good.txt"],
            "the unreadable file is skipped but the rest still scan"
        );
        assert_eq!(
            skipped,
            vec!["locked.bin"],
            "the unreadable file is reported as skipped"
        );

        // Restore perms so the unix tempdir can be cleaned up.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&bad, fs::Permissions::from_mode(0o644));
        }
    }
}
