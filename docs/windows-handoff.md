# Windows handoff — start here

You're picking this repo up on a Windows machine after the Linux phase. This is
the orientation note; the step-by-step lives in **`docs/windows-packaging.md`**,
and the verified iroh 1.0 API surface is in **`docs/iroh-1.0-api-notes.md`**.

## Where things stand

A complete, working P2P mirrored-folder sync app — **fully built and tested on
Linux** (engine, daemon, CLI, GTK GUI + tray scaffold). CI is green on **both**
Linux and Windows; the Windows job already compile-checks the service code.

Done and tested (Linux): create/add/publish/apply strict mirror, signed-manifest
trust (master/viewer keys), SQLite persistence + restart-safety, master seed in
the OS keystore, pause/resume/remove, live peer counts + throughput, ignore
lists, auto-discovery (no bootstrap needed). 16 unit + 6 integration tests.

Run the integration tests anytime (they open real iroh endpoints):
```
cargo test --workspace -- --ignored
```

## First things to do on Windows (in order)

1. **Toolchain + GTK** — `docs/windows-packaging.md` §0: `rustup default stable-msvc`,
   VS Build Tools, gvsbuild `build gtk4 libadwaita`, set PKG_CONFIG_PATH/PATH/LIB,
   then `cargo build --release`. Confirm `seed-gui.exe` launches.
2. **Smoke test the daemon in console mode** before bothering with the service:
   `seed-daemon.exe run` + `seed-gui.exe` (set `SEED_SOCKET` to match if needed).

## ⚠️ The one thing most likely to break: the IPC named pipe

`crates/seed-ipc/src/transport.rs` builds the socket name with
`path.to_fs_name::<GenericFilePath>()`. That's correct for Unix domain sockets,
but on **Windows a named pipe wants `\\.\pipe\<name>`** — a *namespaced* name, not
a filesystem path. **Expect the daemon's `transport::bind` to fail on Windows as
written.** Fix: on Windows, derive a namespaced name and use
`to_ns_name::<GenericNamespaced>()`; on Unix keep `GenericFilePath`. Make `bind`
and `connect` derive the **same** name from the `--socket` arg so the GUI/CLI and
daemon agree. This is the first real Windows bug to fix — do it before the service.

## Other open questions (flagged in code + windows-packaging.md §2)

- **Service account vs IPC reachability.** The service installs as **LocalSystem**;
  the GUI runs as the logged-in user. Decide: run the service as the user, or set
  a permissive DACL on the pipe. The GUI only needs IPC (not the seed), so the
  keystore-under-LocalSystem split is fine — but confirm pipe access across the
  account boundary.
- **Data dir under LocalSystem.** `directories` resolves to the service account's
  profile; the user-run GUI resolves elsewhere. Make them agree — likely pass an
  explicit machine-wide `--data-dir` (e.g. `%PROGRAMDATA%\SeedSync`) to both, or
  run the service as the user.
- **Keystore.** On Windows keyring uses Credential Manager (`windows-native`
  feature) — no libdbus, no extra setup. Seeds stored by the daemon live in the
  daemon's account vault (fine; only the daemon needs them).

## Then: package + Checkpoint #3

`scripts\bundle-gtk-windows.ps1` → portable tree; `cargo-wix` → MSI (registers the
service); sign everything. Final acceptance is **Checkpoint #3**: MSI installs,
service auto-starts, GUI talks to it, and a share syncs **Windows ↔ the Linux dev
box** over real iroh (use auto-discovery — no bootstrap address needed).

## Repo map (quick)

- `crates/seed-core` — engine: `manifest.rs` (trust root), `identity.rs` (keys),
  `scan.rs`, `node.rs` (iroh), `engine.rs` (create/add/publish/apply), `db.rs`
  (sqlite), `secrets.rs` (keystore). Tests in `crates/seed-core/tests/`.
- `crates/seed-ipc` — wire types + `transport.rs` (the pipe code to fix).
- `crates/seed-daemon` — `main.rs` (serve loop), `service.rs` (Windows service).
- `crates/seed-gui` — GTK4 GUI; `crates/seed-cli` — test/automation client.

## Don't forget

- One commit per milestone; keep CI green (it gates both OSes).
- Master seed must never land in the DB — keep the keystore/fallback split in
  `engine.rs::persist_share`.
- `cargo fmt --all` + `cargo clippy --workspace --all-targets` before pushing.
