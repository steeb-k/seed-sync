# Cross-OS testing log (shared: Windows ⇄ Linux)

A shared scratchpad for the two Claude instances testing Seed Sync's Windows↔Linux
sync (M4 Checkpoint #3). Both machines have this repo checked out; the human syncs
git between them.

## How to use this file
- **Read it first** at the start of a session to catch up on what the other side found.
- Append entries to the **Findings log** tagged **[WIN]** or **[LINUX]** with the date.
- Keep the **Status board** and **Open issues** sections current (edit in place).
- Write down what you learned **before** handing back, so the human can sync.
- One fact per entry; link commits by short hash.

## Status board
Legend: ✅ pass · ❌ fail/bug · ⏳ not yet tested

| Check | Linux→Win | Win→Linux |
|---|---|---|
| Mirrors all files, sizes exact | ✅ | ⏳ |
| Cross-OS path (`/`↔`\`) + unicode names | ✅ | ⏳ |
| Self-heal (corrupt mirror → auto-restore) | ✅ ~6s | ⏳ |
| Viewer 1× dedup, **same volume** | ✅ (local) | ⏳ |
| Viewer 1× dedup, **cross volume** | ✅ fixed (#1) | ⏳ |
| Deletion / update propagation | ⏳ | ⏳ |

## Open issues
### #1 — Cross-volume viewer dedup leaves a 2× copy on Windows  *(FIXED — Option A)*
**Resolution (2026-06-18):** vendored + one-line-patched `iroh-blobs` (`vendor/iroh-blobs`,
`[patch.crates-io]` in `Cargo.toml`) so its `TryReference` export also treats Windows
`ERROR_NOT_SAME_DEVICE` (17) as a cross-volume move and falls back to copy + `External`.
The leftover owned copy (Windows holds the handle briefly) is then deleted by a reclaim
retry queue in `engine.rs`/`node.rs` (`reclaim_pending`, retried each reconcile until iroh
releases the handle, ~3 s). Verified: cross-volume viewer blob store **100.91 MB → 0.91 MB**
(1×), same-volume still 0.91 MB, all seed-core tests green. **Linux:** the patch is inert
there (Linux gets EXDEV=18, already handled), so behavior is unchanged — but `vendor/iroh-blobs`
must be present after a pull for the build to resolve the `[patch]`.

Original analysis below for reference.

---

When a viewer's **mirror folder is on a different volume than its data dir**, the
content is stored twice (auto-downloaded blob in the store **and** the mirror file).
Same-volume is fine (1×).

- **Root cause (refined — it's an upstream iroh-blobs Windows bug):**
  `ExportMode::TryReference` on an owned blob does `std::fs::rename(source→target)`
  (`iroh-blobs-0.103.0/src/store/fs.rs:1300-1313`). On a cross-volume move it only
  falls back to copy when the OS error is `EXDEV` (**18**, Linux). Windows returns
  `ERROR_NOT_SAME_DEVICE` (**17**), which iroh doesn't match, so it **returns an
  error instead of falling back** — the export fails outright. Confirmed in the
  daemon log: `reference-export p.bin failed; will self-heal: Error::Io`. The file
  is then materialized by **self-heal** (`get_blob` → mirror), and the owned blob
  that iroh-docs auto-downloaded is never converted to a reference → it lingers.
- Why the obvious fixes don't work: re-importing the mirror by reference can't help
  (`entry_state.rs` union prefers `Owned`, so it stays Owned); there's no public
  single-blob delete; and on Windows the owned `.data` can't be deleted while iroh
  holds its handle anyway (it releases it ~5 s later — Linux can unlink an open file,
  so **Linux almost certainly does NOT have this bug**: its cross-fs rename hits
  `EXDEV`→copy, then iroh unlinks the open owned file fine).
- Impact: **high on Windows** — the service data dir is always `C:\ProgramData`, and
  users pick mirror folders on other drives. Defeats "no disk doubling" there.
- Repro (Windows, local 2-daemon): viewer data on `C:`, mirror on `D:`, 100 MB file
  → viewer `blobs` = **100.91 MB** (vs **0.91 MB** same-volume).
- **[LINUX] please confirm** cross-filesystem is 1× (data dir on `/`, mirror on a
  different mount). If so this is Windows-only and the fix can be `#[cfg(windows)]`.
- Fix options (deciding):
  - **A. Patch iroh-blobs** to also treat Windows err 17 as cross-volume (1-line; fork
    via `[patch]` or vendor). Then iroh copies + sets `External`, and a reclaim pass
    deletes the leftover owned `.data` once iroh releases the handle. Surgical; keeps
    the sync architecture; needs a patched dependency.
  - **B. Never create an owned blob on the viewer:** disable iroh-docs content
    auto-download and materialize via `get_blob`→mirror + import-by-reference (the
    self-heal path, generalized). No dependency patch, but a bigger change to the core
    sync path. *(A reclaim-retry queue in `engine.rs`/`node.rs` is already written for
    option A's second half; uncommitted pending this decision.)*

## Findings log

### [WIN] 2026-06-18 — Checkpoint #3, Linux master → Windows viewer (first run)
Linux master shared ~100 MB (`readme.txt`, `docs/notes.md`, `docs/unicode-name.txt`,
`bigfile-100mb.bin`) to a Windows viewer (MSI service, data `C:\ProgramData\SeedSync`,
mirror `D:\linuxMasterTest`).
- ✅ **Mirrors:** all 4 files present; `bigfile-100mb.bin` == 104,857,600 bytes exact.
- ✅ **Path/unicode:** `docs/` came through with `/`→`\` conversion; filenames intact.
- ✅ **Self-heal:** corrupted the 100 MB mirror file to 54 bytes → auto-restored to the
  exact original (size + SHA256) in **~6 s**, no user action. Cross-OS self-heal works.
- ❌ **Dedup:** can't measure from this run (the store also held an unrelated leftover
  share). Isolated 2-daemon test confirmed **Open issue #1** (cross-volume 2×).

### [WIN] 2026-06-18 — diagnosed issue #1 (cross-volume 2×)
Traced it to iroh-blobs' `TryReference` export only handling Linux `EXDEV` (18), not
Windows `ERROR_NOT_SAME_DEVICE` (17) — so cross-volume export fails outright, self-heal
saves the file, and the auto-downloaded owned blob is never reclaimed. Verified iroh
releases the owned `.data` handle ~5 s after the op (so an external delete works then;
in-process delete must be retried). Wrote a reclaim-retry queue (`reclaim_pending` in
`engine.rs`, `blobs_dir` on `IrohNode`) — correct but insufficient alone, because the
export never succeeds so nothing gets queued. Need decision A vs B (see issue #1).
**Likely Windows-only — Linux to confirm cross-fs is 1×.**

### [WIN] 2026-06-19 — more daemon fixes (publish robustness)
Two master-publish bugs found while sharing from Windows, both fixed in shared
`seed-core` (so they apply on Linux too once pulled):
- **Publish flap on a still-copying file** (`a4e4b4a`): the master republishes
  whenever the folder's `(path,size,mtime)` signature changes, so a large file
  being copied/downloaded in changed every tick → endless re-index (healthy → 0%
  → healthy). Added a **debounce**: only publish once the signature has held steady
  across a reconcile tick (~750 ms).
- **Publish wedged on a size/hash mismatch** (`9ce5672`): a file still being
  written reports a **stale 0 size** in the Windows directory entry, but `add_path`
  hashes the real content, so `set_hash` got `(non-empty hash, len 0)` — which
  iroh-docs rejects (`Attempted to insert an empty entry`) — failing the publish on
  that one file and retrying every tick (share stuck "indexing"). Now the publish
  takes the size from the blob iroh just imported (`blobs().status → Complete{size}`),
  which is hashed from the same bytes and always agrees with the hash. Empty files
  still work; mid-write files no longer wedge. (Surfaced as an "error about a unicode
  filename" — `café.txt` — but the name was incidental; it was just the file in flight.)

**[LINUX] please confirm:** the stale-0-size is a Windows directory-entry quirk, but the
size/hash mismatch can also happen from a genuine write-during-scan race on any OS — so
verify a Win→Linux (or local) sync of a file that's actively being written doesn't wedge,
and that empty + unicode-named files round-trip. The fix is in shared code; no Linux-side
change expected, just confirmation.

### [WIN] notes / environment
- Windows build: GTK4 via gvsbuild at `C:\gtk`; MSI via WiX 5 (`scripts\build-msi.ps1`).
- The MSI is x64, installs to `C:\Program Files\SeedSync`, registers the LocalSystem
  `SeedSyncDaemon` service. Recent fixes: GSettings schema path (file-chooser crash),
  share/lib MSI layout, x64 arch, same-version upgrades.
- GUI single-instance: launching a 2nd instance just re-activates the first.

### [LINUX] — (add your entries here)
- Regression pass results (`cargo test --workspace -- --include-ignored`):
- Win→Linux sync:
- Cross-filesystem dedup check (issue #1):
- Publish of a file being written / empty / unicode-named doesn't wedge (see 2026-06-19):
