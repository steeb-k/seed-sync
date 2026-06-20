# Seed Sync

A Resilio Sync–style **P2P mirrored folder sync** app in Rust + GTK4/Libadwaita.

A *master* key holder can modify a shared folder; *viewer* key holders are
read-only — any local edits they make are discarded so every peer's copy stays
byte-identical. Built on the [iroh](https://iroh.computer) 1.0 stack (QUIC,
content-addressed blobs, multi-writer docs, gossip), with an end-to-end
signed-manifest trust model layered on top.

> Status: early development. See `project-plan-human.md` for the product brief
> and the implementation plan for the build sequence.

## Architecture

| Crate | Role |
|-------|------|
| `seed-core` | Sync engine: iroh transport, signed-manifest trust root, share keys, folder mirroring. No GUI/IPC. |
| `seed-ipc` | Wire contract (request/response/event types + CBOR framing) between GUI and daemon. Dependency-light. |
| `seed-daemon` | Background sync daemon. Console process in dev; Windows service in production (same binary). |
| `seed-gui` | GTK4 + Libadwaita GUI and system tray. IPC client to the daemon. |
| `seed-cli` | Headless IPC client for scripted/loopback testing. |

## Building (Linux dev)

Requires the GTK4 and Libadwaita development packages and a stable Rust toolchain.

```bash
# Debian/Ubuntu: sudo apt install libgtk-4-dev libadwaita-1-dev libdbus-1-dev
#   (libdbus is for the Secret Service keystore backend; on Windows/macOS the
#    native keystore is used and no extra dev lib is needed)
cargo build --workspace
cargo test --workspace
```

## Installing (Linux)

Releases are published as a portable tarball on the public
[`seed-sync-binaries`](https://github.com/steeb-k/seed-sync-binaries) repo. Per-user
install (no root); requires GTK 4.10+, libadwaita 1.4+, and libdbus-1 on the system.

**Install** (also upgrades in place) — fetches the latest release:

```bash
cd "$(mktemp -d)" && curl -fsSL "$(curl -fsSL https://api.github.com/repos/steeb-k/seed-sync-binaries/releases/latest | grep -oE 'https://[^"]+linux-x86_64\.tar\.gz' | head -1)" | tar xz && seed-sync-*/seed-sync --install
```

It drops the binaries in `~/.local/bin`, runs the daemon as a `systemd --user`
service (auto-starts at login), and enables a daily auto-update timer. Launch
**S.E.E.D.** from your app menu. After install, the `seed-sync` command manages
everything:

```bash
seed-sync --update              # check + apply a newer release (the timer does this daily)
seed-sync --status              # installed/latest version + service state
seed-sync --uninstall [--purge] # remove (--purge also deletes synced data)
```

Maintainers: see **`docs/linux-packaging.md`** for how releases, the tarball, and
auto-update work, and **`docs/windows-packaging.md`** for the MSI side.

## Permission model

- **Master key** (`seedm1…`) carries an Ed25519 signing seed → write access.
- **Viewer key** (`seedv1…`) carries only the public verifying key → read-only.
- `share_id = BLAKE3(master_pub)`. The master signs a versioned manifest
  (merkle root + monotonic seqno + expiry + ignore list); viewers verify against
  the pinned master key and reject/overwrite anything not validly signed.
- Multi-master (per-device identities, revocation, attribution) is planned future
  work; the manifest format reserves fields for it.

## License

GPL-3.0-or-later.
