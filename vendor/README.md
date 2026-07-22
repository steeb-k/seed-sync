# Vendored, patched dependencies

## iroh-blobs (0.103.0) — Windows cross-volume reference export

Vendored from crates.io and patched with a single change, wired in via
`[patch.crates-io]` in the workspace `Cargo.toml`.

**Why:** `ExportMode::TryReference` (used so a viewer references its mirror file
instead of keeping a second copy) moves the owned blob with `std::fs::rename` and
only falls back to a copy when the OS error is `EXDEV` (unix, 18). On Windows a
cross-volume move returns `ERROR_NOT_SAME_DEVICE` (17), which upstream didn't
match, so the export failed and a viewer whose mirror was on a different drive
than its data dir kept the content twice. See known-issues #25.

**The patch** (`src/store/fs.rs`, in `export_path_impl`): also treat error 17 as a
cross-volume move so it falls back to copy + sets the entry to `External`. The now-
redundant owned `.data` is then reclaimed by `seed-core`'s reclaim-retry queue
(it can't be deleted until iroh releases the file handle, ~3 s later on Windows).

Inert on Linux/macOS (they hit 18, already handled).

**Re-applying on an iroh-blobs bump:** re-vendor the new version, re-apply the
`ERR_NOT_SAME_DEVICE` check at the same spot, and bump the version in `Cargo.toml`.
Ideally upstream this fix so the vendor copy can be dropped.
