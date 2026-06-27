# SEED Sync

A Resilio Sync–style **P2P mirrored folder sync** app in Rust + GTK4/Libadwaita.

Any *master* key holder can modify a shared folder — multiple masters across
devices stay in sync, changes flowing both ways (last-writer-wins). *Viewer* key
holders are read-only: any local edits they make are discarded so every peer's
copy stays byte-identical. Built on the [iroh](https://iroh.computer) 1.0 stack
(QUIC, content-addressed blobs, multi-writer docs, gossip); the share's
multi-writer doc replica is the trust root — every entry is signed by the master
namespace key, so only master-key holders can write.

> Status: early development. See `project-plan-human.md` for the product brief
> and the implementation plan for the build sequence.

## Architecture

| Crate | Role |
|-------|------|
| `seed-core` | Sync engine: iroh transport, signed-manifest trust root, share keys, folder mirroring. No GUI/IPC. Peer discovery uses n0 DNS + relays for the internet path **and** mDNS for the local network, so members on the same LAN sync with no internet at all. |
| `seed-ipc` | Wire contract (request/response/event types + CBOR framing) between GUI and daemon. Dependency-light. |
| `seed-daemon` | Background sync daemon. Console process in dev; Windows service in production (same binary). |
| `seed-gui` | GTK4 + Libadwaita GUI and system tray. IPC client to the daemon. |
| `seed-cli` | Headless IPC client for scripted/loopback testing. |
| `seed-mobile` | UniFFI facade over `seed-core` for the Android app (`android/`): owns the runtime + engine in-process, no IPC. |

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

One command installs, updates, or removes — it detects what's already there and
prompts (install / update / remove):

```bash
curl -fsSL https://steeb-k.github.io/seed-install.sh | sh
```

It drops the binaries in `~/.local/bin`, runs the daemon as a `systemd --user`
service (auto-starts at login), and enables a daily auto-update timer. Launch
**S.E.E.D.** from your app menu. Non-interactive: append `-s -- install`
(or `update` / `remove`) after `sh`.

After install, the `seed-sync` command manages everything locally:

```bash
seed-sync --update              # check + apply a newer release (the timer does this daily)
seed-sync --status              # installed/latest version + service state
seed-sync --uninstall [--purge] # remove (--purge also deletes synced data)
```

Maintainers: see **`docs/linux-packaging.md`** for how releases, the tarball, and
auto-update work, **`docs/windows-packaging.md`** for the MSI side, and
**`docs/android-packaging.md`** for the Android APK (build + signing).

## Installing (Android)

A native Android app (Kotlin + Jetpack Compose, same engine via `seed-mobile`)
is published as a sideloadable universal APK on the
[`seed-sync-binaries`](https://github.com/steeb-k/seed-sync-binaries) releases.
Requires **Android 11+**; on first run grant **All-Files Access** so the engine
can mirror real folders. Not on Google Play (All-Files Access precludes it).
Sources are in `android/`; see `android-app.md` for the design and
`docs/android-packaging.md` to build/sign locally.

## Permission model

- **Master key** (`seedm1…`) carries an Ed25519 signing seed → write access. It
  doubles as the iroh-docs namespace secret, so anyone holding it can write to the
  share's replica. **Multi-master** is supported: add the master key on any number
  of devices and they all read/write the same share.
- **Viewer key** (`seedv1…`) carries only the public verifying key → read-only.
  A viewer holds a read capability and physically cannot write doc entries, so its
  local edits are reverted to keep every copy byte-identical.
- `share_id = BLAKE3(master_pub)`, which is also the doc namespace id. Every entry
  in the replica is signed by the namespace key, so a peer without the master
  secret cannot forge file content; masters' entries merge **last-writer-wins**.
- Conflict caveat: two masters editing the *same* file before they sync resolve
  last-writer-wins, so one of the two concurrent edits is dropped. Conflict-copy
  preservation (à la Syncthing) is planned future work.

## License

GPL-3.0-or-later.
