# CLAUDE.md

Guidance for working in this repository. Read this first, then jump to the
specific doc under `docs/` for whatever you're touching.

## What this is

**SEED Sync** — a Resilio Sync–style **P2P mirrored folder sync** app in Rust
with a GTK4 / Libadwaita desktop GUI and a native Android app. Built on the
[iroh](https://iroh.computer) 1.0 stack (QUIC transport, content-addressed
blobs, multi-writer docs, gossip).

Core model: any **master** key holder can modify a shared folder; multiple
masters across devices stay in sync with changes flowing both ways
(last-writer-wins). **Viewer** key holders are read-only — their local edits are
reverted so every peer's copy stays byte-identical. The share's iroh-docs replica
is the trust root: every entry is signed by the master namespace key, so only
master-key holders can write.

See `README.md` for the user-facing overview, permission model, and install paths.

## Workspace layout

Cargo workspace (`Cargo.toml`), members under `crates/`:

| Crate | Role |
|-------|------|
| `seed-core` | The sync engine. iroh endpoint + blob/doc/gossip stores, multi-master reconcile, share identity/keys, filesystem scan + mirror loop, local persistence. **No GUI/IPC.** |
| `seed-ipc` | Wire contract (request/response/event types + CBOR framing) between GUI and daemon. Dependency-light. |
| `seed-daemon` | Background sync daemon. Console process in dev; Windows service in production (same binary). |
| `seed-gui` | GTK4 + Libadwaita GUI and system tray. IPC client to the daemon. |
| `seed-cli` | Headless IPC client for scripted / loopback testing. |
| `seed-mobile` | UniFFI facade over `seed-core` for the Android app (`android/`): owns the runtime + engine in-process, **no IPC**. |

`seed-core` internal modules (`crates/seed-core/src/`): `engine` (reconcile
loop), `node` (iroh endpoint/stores), `identity` (share keys / roles), `manifest`
+ `scan` (file-entry types + fs scan), `presence` (gossip roster / online
status), `secrets` (OS keystore), `db` (sqlite persistence).

The Android app is Kotlin + Jetpack Compose under `android/` (Gradle), wrapping
`seed-core` via `seed-mobile`.

## Build / test / run

```bash
cargo build --workspace          # build everything
cargo test  --workspace          # run tests
cargo clippy --workspace         # lint (toolchain pins rustfmt + clippy)
cargo fmt --all                  # format before committing

# The acceptance gate. `cargo test --workspace` runs ZERO integration tests
# (they are all `#[ignore]`d), so this is what actually proves the app syncs.
# Required before cutting a release — see docs/testing.md.
pwsh scripts/test-acceptance.ps1         # Windows
bash scripts/test-acceptance.sh          # Linux/macOS

bash scripts/run-linux.sh                # build (release) + launch daemon + GUI on Linux/WSL
bash scripts/run-linux.sh --skip-build   # relaunch without rebuilding
```

Linux dev needs the GTK4 / Libadwaita / libdbus dev packages
(`libgtk-4-dev libadwaita-1-dev libdbus-1-dev` on Debian/Ubuntu). The toolchain
is pinned to stable Rust ≥ 1.85 via `rust-toolchain.toml`. If the build/run
environment looks off, `scripts/run-linux.sh` self-checks and points at
`docs/dev-environment.md`.

## Conventions & gotchas

- **The Windows ARM64 build mixes two ABIs in one bundle by necessity.** The GUI builds for
  `aarch64-pc-windows-gnullvm` to match MSYS2's CLANGARM64 GTK (the only prebuilt GTK4 + libadwaita
  for ARM Windows, which is mingw-ABI), while the daemon + CLI — no GTK dependency — stay on
  `aarch64-pc-windows-msvc`. They only meet over IPC, so no ABI boundary is crossed inside a
  process. It all cross-compiles from the x86_64 box; `docs/windows-packaging.md` §1b has the
  rationale and the four cross-compile traps (`ring`/clang paths, the `windres` target, the MSYS2
  DLL closure, the host-tools cache), and `scripts/verify-bundle.ps1` proves each bundle is
  single-arch. The updater picks its MSI by **OS** arch and never falls back across architectures.
- **iroh ecosystem crates are pinned and bumped *together*.** `iroh`,
  `iroh-blobs`, `iroh-gossip`, `iroh-docs`, `iroh-tickets`,
  `iroh-mdns-address-lookup`, `bao-tree` all share `iroh-base`; mixing minors
  fails to compile. Versions live in `[workspace.dependencies]` in `Cargo.toml`.
  After any bump run `cargo tree -d` and confirm a single `iroh-base` resolves.
  The exact API surface we build against is documented in
  `docs/iroh-1.0-api-notes.md`.
- **Vendored `iroh-blobs` patch.** `[patch.crates-io]` points `iroh-blobs` at
  `vendor/iroh-blobs`, a one-line patch for cross-volume export on Windows. See
  the comment in `Cargo.toml` and known-issues #25 before touching it or bumping
  iroh-blobs.
- **No CI.** All releases are built **locally on each platform's own machine**
  and published to the public `steeb-k/seed-sync-binaries` repo. The old GitHub
  Actions workflow was removed. See `docs/releasing.md`.
- **Version** is set once in `[workspace.package]` in `Cargo.toml`.
- **License:** GPL-3.0-or-later.
- Match the surrounding code's style; run `cargo fmt` and `cargo clippy` before
  committing.

## Documentation map (`docs/`)

Architecture / engine internals:
- `known-issues.md` — the engine bug & design-caveat catalog. **Check here first when something's wrong.**
- `testing.md` — what's covered, the tiers, and the release gate. **Read before trusting a green test run:** every integration test is `#[ignore]`d, so `cargo test --workspace` runs none of them.
- `iroh-1.0-api-notes.md` — verified iroh 1.0 stack API reference (versions + exact calls).
- `distributed-downloads.md` — how blob *content* is fetched between peers; swarming large files.
- `divergence-detection.md` — cross-member divergence detection, self-heal, deep-verify.
- `relay-outage-field-note.md` — why a relay outage looked like an app bug, and the follow-ups to make it self-evident next time.
- `member-registry.md` — replicated last-known member names (`\x00m/` doc records + `peer_names` cache) so the member list survives disconnects/restarts.

Packaging / distribution (maintainer guides):
- `releasing.md` — how to cut a release across platforms + the shared distribution model.
- `linux-packaging.md` — tarball + `systemd --user` + auto-update; the release baseline.
- `windows-packaging.md` — MSI build/bundle/sign + Windows service.
- `macos-packaging.md` — `.app` bundle, launchd, universal2, install/update flow.
- `android-packaging.md` — building & signing the release APK from `android/`.
- `dev-environment.md` — single-box (Windows) setup to build/run/package for Win/Linux/Android.

Design:
- `../android-app.md` — Android app design (engine → UniFFI → Compose).
```
