# Linux handoff — Checkpoint #3 (Windows ↔ Linux sync)

You're on the Linux box. Linux was the original dev platform (engine, daemon, CLI,
GTK GUI all built/tested here first). The **Windows** phase (M4) is essentially
done — packaging works, and a bunch of engine features were added *on Windows*
that you should now validate cross-platform and use for the real cross-OS test.

`git pull` first — the branch is `main`. Recent, relevant commits:
- `6cfa24a` engine: viewers store synced content 1× with hands-off self-heal
- `f09ab04` engine: import shared files by reference, not copy (no disk doubling)
- `1976217` engine: run publish off the engine lock
- `5923263` engine: single-pass publish + persist folder signature
- `3800911` packaging: scaffold the MSI installer (WiX 5)

## What changed since the Linux phase (validate these)

These are cross-platform engine changes — they run on Linux too and need a
Linux-side sanity pass:

1. **Master imports by reference** (`ImportMode::TryReference`): the blob store
   keeps only the outboard and points at the folder files — no content copy. So
   `<data_dir>/blobs` stays tiny regardless of share size.
2. **Viewers store 1× too**: `apply()` exports with `ExportMode::TryReference`,
   which *moves* a downloaded blob into the mirror folder and references it. On
   Linux same-volume this is a `rename` (instant); cross-volume it's copy+delete.
3. **Hands-off self-heal**: if a viewer's mirror file is edited/corrupted, the
   engine auto-re-fetches the verified bytes from a peer (`get::request::get_blob`)
   and rewrites the file on the next reconcile — no user action. New dep: `bao-tree`.
4. **Off-lock publish + single-pass import + persisted change-signature** (no
   re-index on restart, responsive during big transfers, live indexing %).

What is **Windows-only and does not apply here** (all `#[cfg(windows)]`):
- Named-pipe IPC + the machine-wide `%PROGRAMDATA%\SeedSync` socket + the pipe
  DACL. On Linux the IPC is the usual Unix domain socket at the per-user data dir
  (`directories` crate); `--socket`/`SEED_SOCKET` override as before.
- The Windows service, the MSI, the Win11 GUI CSS (`windows.css`). The frameless
  header (`style.css`) does apply on Linux; controls stay default Adwaita.

## First: regression pass on Linux

```sh
cargo build --workspace
cargo test --workspace -- --include-ignored
```
The integration tests open real iroh endpoints. Of note, `crates/seed-core/tests/loopback.rs`
now includes (added on Windows, must also pass here):
- `viewer_stores_by_reference_not_copy` — viewer blob store stays outboard-sized.
- `viewer_auto_heals_corrupted_file` — corrupt a mirror file, it auto-restores.
- `referenced_viewer_serves_peers` — a referenced viewer re-serves with the master offline.

If anything is red on Linux, that's the first thing to fix (cross-platform regression).

## Then: Checkpoint #3 — real Windows ↔ Linux sync

Goal: a share syncs **both directions** over real iroh using **auto-discovery
(no bootstrap address)** — the share key carries the master's endpoint id and n0
DNS discovery resolves it. Both machines need internet (for relay/discovery).

The Windows box has the MSI installed (LocalSystem `SeedSyncDaemon` service + GUI),
data under `C:\ProgramData\SeedSync`. On Linux, just run a console daemon as your
user: `cargo run -p seed-daemon -- --data-dir /tmp/seed-lin run` (pick any data dir),
and drive it with `seed-cli --socket <data_dir>/seed.sock …` (or the GUI).

Test matrix:
1. **Linux master → Windows viewer.** On Linux: `seed-cli … create --folder <dir>`
   (note the printed `viewer_key`). On Windows: add the share via the GUI (or
   `seed-cli add --key <viewer_key> --folder <dir>` with **no** `--bootstrap`).
   Confirm the folder mirrors, the Windows viewer's `blobs` dir stays tiny (1×),
   and editing a file in the Windows mirror auto-heals.
2. **Windows master → Linux viewer.** Create the share in the Windows GUI; on Linux
   `seed-cli add --key <viewer_key> --folder <dir>` (no bootstrap). Confirm sync;
   corrupt a file in the Linux mirror and confirm it self-heals from Windows.
3. **Large file** (e.g. a few GB) in at least one direction — confirms reference
   import + streaming + viewer dedup hold up cross-OS with no doubling on either end.
4. **Deletion + update propagation** both ways (these are the strict-mirror basics).

Watch for: cross-OS path handling (the manifest stores forward-slash relative
paths; `rel_to_native` converts), and that discovery actually resolves (if it
doesn't, fall back to passing the master's endpoint-ticket via `--bootstrap`
from `seed-cli node-addr` to isolate discovery vs. transport).

## Reporting back
The Windows side is at: MSI built (`target\wix\SeedSync-0.1.0.msi`), service +
GUI working same-machine, viewer dedup/self-heal verified locally (live test:
100 MB file → 0.91 MB store, corrupted file self-healed in ~6 s). What's left for
M4 is exactly this cross-OS confirmation, plus (deferred) MSI code signing + a
WixUI. Note anything that needs a Windows-side change back here.
