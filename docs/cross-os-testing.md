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
| Viewer 1× dedup, **cross volume** | ❌ see #1 | ⏳ |
| Deletion / update propagation | ⏳ | ⏳ |

## Open issues
### #1 — Cross-volume viewer dedup leaves a 2× copy  *(open, needs engine fix)*
When a viewer's **mirror folder is on a different volume than its data dir**, the
downloaded blob is **not** reclaimed → the content is stored twice (blob store +
mirror). Same-volume is fine (1×).

- Root cause (confirmed in `iroh-blobs-0.103.0/src/store/fs.rs:1300-1321`):
  `ExportMode::TryReference` on a previously-owned blob does `rename(source→target)`.
  Cross-volume that fails with `ERR_CROSS`/`EXDEV` (18) and it falls back to a
  **copy**, but the owned `data/<hash>.data` file is left behind. The comment claims
  setting the `External` state deletes it, but with no GC running it persists — and
  a daemon **restart does not reclaim it** either (verified).
- Impact: **high** — the Windows service's data dir is always `C:\ProgramData\SeedSync`,
  and users put mirror folders on other drives, so this is the common case. Defeats
  the "no disk doubling" guarantee.
- Repro (Windows, local 2-daemon): viewer data dir on `C:`, mirror on `D:`, sync a
  100 MB file → viewer `blobs` = **100.91 MB** (vs **0.91 MB** same-volume).
- **Linux: please verify** the same on a cross-filesystem mirror (e.g. data dir on `/`,
  mirror on a different mount / tmpfs). Expect the same `EXDEV` fallback → 2×.
- Fix direction (Windows side, not yet implemented): after a `TryReference` export in
  `engine.rs::apply()`, if the entry is now `External` but the owned `data/<hash>.data`
  still exists in the store, delete it (it's orphaned; outboard `.obao4` must stay so
  the viewer can still serve). Need to confirm this is safe w.r.t. iroh's delete_set.
  Discuss here before implementing so both platforms stay consistent.

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
