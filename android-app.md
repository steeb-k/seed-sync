# Android app

SEED Sync's Android app (`android/`, Kotlin + Jetpack Compose) runs the same
`seed-core` engine in-process through the `crates/seed-mobile` UniFFI facade —
no fork, one shared engine with desktop. Build and signing are covered in
`docs/android-packaging.md`; this is the design.

## Why the port is clean

The sync engine was already cleanly isolated:

- **`crates/seed-core`** — pure-Rust, headless (iroh endpoint + iroh-blobs +
  iroh-docs + iroh-gossip, SQLite via `db.rs`, Ed25519 trust model in
  `manifest.rs`/`identity.rs`, filesystem scan in `scan.rs`), with zero GTK
  dependencies. iroh, quinn (QUIC), tokio, blake3, and rusqlite all compile and
  run on `aarch64-linux-android`.
- **`crates/seed-ipc`** — transport-agnostic CBOR wire types.
- **`crates/seed-gui`** (GTK4) and **`crates/seed-daemon`** — the desktop-only
  layers replaced on Android.

Two decisions shape the design:

1. **UI:** native Kotlin + Jetpack Compose, with `seed-core` exposed through
   **UniFFI**. Best native feel and first-class access to the Android platform
   APIs that do the hard work (Storage Access, foreground service, notifications,
   battery, keystore).
2. **Storage:** All-Files Access (`MANAGE_EXTERNAL_STORAGE`), distributed by
   sideload (GitHub releases / F-Droid / direct APK), matching the existing
   self-distribution model. This preserves real filesystem paths, so
   `seed-core`'s `std::fs`-based engine (`scan.rs`, blob import/export) needs
   almost no change.

## How the pieces map

| Desktop | Android |
|---|---|
| `seed-daemon` process + Unix-socket/named-pipe IPC | **In-process** `seed-core` inside a foreground `Service`; IPC removed |
| `seed-gui` (GTK4) | Compose UI calling Rust via UniFFI |
| `reconcile_loop` / throughput loop in `seed-daemon/src/main.rs` | Lifted into `seed-mobile`, run on a tokio runtime owned by the service |
| System tray (`tray.rs`) | Persistent foreground-service notification |
| `keyring` crate (`secrets.rs`) | Android Keystore (or DB fallback) |
| systemd/registry autostart | `RECEIVE_BOOT_COMPLETED` receiver + WorkManager |
| GTK `FileDialog` | SAF `ACTION_OPEN_DOCUMENT_TREE` to pick the folder; real path resolved for the engine |

The Unix-socket/named-pipe IPC is not used on Android — the UI and engine live in
the same process. The `seed-ipc` *types* (`ShareSummary`, `PeerInfo`, `Settings`)
are reused as the shape for UniFFI records so the data model stays identical.

`seed-mobile` owns a tokio runtime and an `Engine`, exposes a flat UniFFI API
(create/add/pause/resume/remove share, list, keys, peers, node addr, device name)
and hosts the reconcile and throughput loops that live in `seed-daemon` on desktop.
Engine events (`ShareStatus`, `Throughput`, `Membership`, `LastUpdated`) go to a
UniFFI callback interface the Kotlin side registers, replacing the socket
subscription.

Two small `seed-core` adaptations make this work. The data-dir resolution accepts
an injected base dir — Android's `Context.getFilesDir()` for `state.db`, `node.key`,
and `docs/`, and a shared-storage dir for `blobs/`, kept on the **same volume** as
the synced folders so the zero-copy reference export (rename/hardlink) works. And
`secrets.rs`'s `keyring` crate, which has no Android backend, is gated out under
`cfg(target_os = "android")` in favour of the `seed_in_keyring = 0` DB-stored key
path already supported in `db.rs`.

## Android constraints

The full sync engine, crypto/trust model, blob/doc/gossip networking, and
real-path folder mirroring carry over unchanged: an Android device is a first-class
peer with the same master/viewer roles and wire protocol, syncing bidirectionally
with desktop nodes. NAT traversal via iroh relays works on cellular and Wi-Fi, and
local-network mDNS discovery works on Wi-Fi — `EngineService` holds a
`WifiManager.MulticastLock` (permission `CHANGE_WIFI_MULTICAST_STATE`) so the engine
receives inbound multicast and can find LAN peers with no internet.

The genuine constraints are inherent to the platform, not fixable in our code:

- **Background reliability.** Android aggressively limits background work (Doze,
  App Standby, OEM task killers). A foreground service + battery-optimization
  exemption keeps sync running, but it is not as bulletproof as a desktop daemon;
  on some OEM ROMs the service may still be killed and rely on the boot receiver /
  WorkManager to restart.
- **Battery.** Continuous QUIC keepalive + folder polling drains battery. Settings
  to sync only on Wi-Fi and/or while charging, and to lengthen the poll interval
  when the screen is off, matter here (today's engine polls `quick_signature` every
  750 ms, too aggressive for always-on mobile).
- **Distribution.** All-Files Access effectively rules out Google Play for a
  generic sync app; sideload / F-Droid / direct APK is the channel.
- **Storage scope.** Sync targets must live on shared storage
  (`/storage/emulated/0/...`). Folders inside *other apps'* private
  `Android/data/<pkg>/` dirs are off-limits even with All-Files Access on
  Android 11+.
- **Network transitions.** Wi-Fi↔cellular handoff requires the iroh endpoint to
  rebind cleanly — a mobile edge case worth verifying explicitly in testing.
- **Min SDK.** `MANAGE_EXTERNAL_STORAGE` is Android 11 (API 30)+, so minSdk 30
  avoids the legacy `WRITE_EXTERNAL_STORAGE` branch.
