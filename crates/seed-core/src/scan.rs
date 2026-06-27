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

use std::collections::HashSet;
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
    use std::time::UNIX_EPOCH;
    let mut entries: Vec<(String, u64, u128)> = Vec::new();
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
        entries.push((rel, size, mtime));
    }
    entries.sort();
    let mut hasher = blake3::Hasher::new();
    for (path, size, mtime) in &entries {
        hasher.update(path.as_bytes());
        hasher.update(&size.to_le_bytes());
        hasher.update(&mtime.to_le_bytes());
    }
    let bytes = hasher.finalize();
    u64::from_le_bytes(bytes.as_bytes()[..8].try_into().unwrap())
}

/// Walk `root`, skipping ignored paths, and produce the live (non-deleted)
/// file set. Symlinks are not followed. Hidden control dirs (e.g. our own
/// `.seed`) should be passed in `ignore`.
/// Returns `(files, skipped)` where `skipped` is the relative paths present on disk
/// but unreadable this pass (locked by another process, permission denied). Skipped
/// files are NOT an error — the caller retries them later — so one bad file never
/// aborts the whole scan (which would block the entire share from publishing).
pub fn scan(root: &Path, ignore: &IgnoreSet) -> std::io::Result<(Vec<ScannedFile>, Vec<String>)> {
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
