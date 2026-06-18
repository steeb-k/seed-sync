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
pub fn quick_signature(root: &Path, ignore: &IgnoreSet) -> u64 {
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
        if ignore.is_ignored(&rel) {
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
pub fn scan(root: &Path, ignore: &IgnoreSet) -> std::io::Result<Vec<ScannedFile>> {
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
        let (hash, size) = hash_file(dent.path())?;
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
    Ok(out)
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
        let files = scan(root, &ig).unwrap();

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
        let files = scan(dir.path(), &ig).unwrap();
        assert!(files.is_empty());
    }
}
