# Android Port Plan — SEED Sync

## Context

SEED Sync works across Windows/macOS/Linux today. The architecture is unusually
well-suited to an Android port because the sync engine is already cleanly
isolated:

- **`crates/seed-core`** — pure-Rust, headless engine (iroh endpoint + iroh-blobs
  + iroh-docs + iroh-gossip, SQLite via `db.rs`, Ed25519 trust model in
  `manifest.rs`/`identity.rs`, filesystem scan in `scan.rs`). **Zero GTK
  dependencies.** iroh, quinn (QUIC), tokio, blake3, and rusqlite all compile and
  run on Android (`aarch64-linux-android`).
- **`crates/seed-ipc`** — transport-agnostic CBOR wire types.
- **`crates/seed-gui`** (~2,300 lines GTK4) and **`crates/seed-daemon`** — the
  desktop-only layers that get replaced on Android.

The two decisions that shape this plan (confirmed with the user):

1. **UI:** Native **Kotlin + Jetpack Compose**, with `seed-core` exposed through
   **UniFFI** bindings. Best native feel and first-class access to the Android
   platform APIs that do the hard work (Storage Access, foreground service,
   notifications, battery, keystore).
2. **Storage:** **All-Files Access** (`MANAGE_EXTERNAL_STORAGE`), distributed by
   sideload (GitHub releases / F-Droid / direct APK), matching the existing
   self-distribution model. This preserves real filesystem paths, so
   `seed-core`'s `std::fs`-based engine (`scan.rs`, blob import/export) needs
   **almost no change**.

Intended outcome: an Android APK that runs the same sync engine in a foreground
service, with a Compose UI that mirrors today's feature set (share list, status,
peers, throughput, create/add/reveal-keys flows).

## Architecture: how the pieces map

| Desktop | Android |
|---|---|
| `seed-daemon` process + Unix-socket/named-pipe IPC | **In-process** `seed-core` inside a foreground `Service`; IPC removed |
| `seed-gui` (GTK4) | Compose UI calling Rust via UniFFI |
| `reconcile_loop` / throughput loop in `seed-daemon/src/main.rs` | Lifted into a new mobile facade crate, run on a tokio runtime owned by the service |
| System tray (`tray.rs`) | Persistent foreground-service notification |
| `keyring` crate (`secrets.rs`) | Android Keystore (or DB fallback initially) |
| systemd/registry autostart | `RECEIVE_BOOT_COMPLETED` receiver + WorkManager |
| GTK `FileDialog` | SAF `ACTION_OPEN_DOCUMENT_TREE` to pick the folder; real path resolved for the engine |

The Unix-socket/named-pipe IPC (`seed-ipc/src/transport.rs`) is **not used** on
Android — the UI and engine live in the same process. The `seed-ipc` *types*
(`ShareSummary`, `PeerInfo`, `Settings`, etc. in `seed-ipc/src/lib.rs`) are
reused as the shape for UniFFI records so the data model stays identical.

## Work plan

### 1. New crate: `crates/seed-mobile` (the UniFFI facade)
A thin crate that owns a tokio runtime + an `Engine` (from `seed-core`) and
exposes a flat, UniFFI-friendly API. Reuse, do not rewrite, the engine:

- Wrap the existing `Engine` methods (create/add/pause/resume/remove share, list,
  get keys, get peers, node addr, device name) — these already exist and are
  driven today by `seed-daemon/src/main.rs`'s IPC handler. Port that handler's
  request→engine mapping into UniFFI methods.
- Lift the **reconcile loop and throughput loop** out of
  `seed-daemon/src/main.rs` into this crate (start them on `init`, stop on
  shutdown). This is the only real logic move; it's a copy-and-adapt, not new
  code.
- Event streaming (today's `IpcEvent`: `ShareStatus`, `Throughput`,
  `Membership`, `LastUpdated`) → a UniFFI **callback interface** (observer) the
  Kotlin side registers, replacing the socket subscription.
- Use UniFFI proc-macro bindings (`#[uniffi::export]`) — no `.udl` file needed.
- Add Android targets and `cargo-ndk` for cross-compilation; generate Kotlin
  bindings into the Gradle module.

### 2. Adapt `seed-core` for Android (small, surgical)
- **Data dir:** the `directories`-based default path resolution must accept an
  injected base dir. Pass Android's `Context.getFilesDir()` (internal, for
  `state.db` + `node.key` + `docs/`) and a shared-storage dir for `blobs/`.
  *Keep the blob store on the same volume as the synced folders* so the existing
  zero-copy reference export (rename/hardlink) works; cross-volume already falls
  back to copy in the engine, but co-locating avoids that cost.
- **Secrets:** `secrets.rs` uses the `keyring` crate, which has no Android
  backend. Gate it out under `cfg(target_os = "android")` and use the existing
  `seed_in_keyring = 0` DB fallback path (already supported in `db.rs`). Later,
  optionally back seeds with Android Keystore via a Kotlin callback.
- Everything else in `scan.rs` / engine import-export works unchanged with real
  paths once All-Files Access is granted.

### 3. Android app module (Kotlin + Compose)
- **Foreground `Service`** that loads the `.so`, calls `seed-mobile` init, and
  shows a persistent notification (the tray replacement) with live throughput.
- **Permissions flow:** request `MANAGE_EXTERNAL_STORAGE` (All-Files Access) via
  the system settings intent; `RECEIVE_BOOT_COMPLETED`,
  `FOREGROUND_SERVICE`/`FOREGROUND_SERVICE_DATA_SYNC`, `POST_NOTIFICATIONS`,
  `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`.
- **Folder picking:** `ACTION_OPEN_DOCUMENT_TREE` for UX, then resolve the real
  filesystem path (`/storage/emulated/0/...`) to hand to the engine — the engine
  needs a real path, not a `content://` URI.
- **Compose UI** mirroring `seed-gui/src/main.rs`: share list with status badges
  + peer-health dots, throughput/last-updated footer, empty/paused/needs-perms
  states, and the create / add-existing / reveal-keys / set-device-name /
  show-node-address dialogs.
- **Boot receiver** restarts the service; **WorkManager** periodic job as a
  fallback resync when the service is killed.

### 4. Build & distribution
- `cargo-ndk` + Mozilla `rust-android-gradle` plugin (or a manual cargo-ndk
  Gradle task) to build `aarch64-linux-android` (primary), `armeabi-v7a`, and
  `x86_64` (emulator), bundling the `.so`s into the APK.
- Reuse `seed-core` from the same workspace — **monorepo, one shared engine**;
  no fork. Desktop and Android stay in lockstep.
- New CI job alongside `.github/workflows/release.yml` to build a signed APK and
  publish it to the existing `seed-sync-binaries` repo.

## Critical files

- **New:** `crates/seed-mobile/` (UniFFI facade — wraps `seed-core::Engine`,
  hosts reconcile/throughput loops, exposes event callback).
- **New:** `android/` Gradle project (Compose UI, foreground service, boot
  receiver, permission + SAF flows, Kotlin bindings).
- **Modify (small):** `crates/seed-core/src/secrets.rs` (gate out `keyring` on
  Android), data-dir resolution in `seed-core` (inject base dir), and
  `seed-core/Cargo.toml` (target-gated `keyring`).
- **Reference / port from:** `crates/seed-daemon/src/main.rs` (reconcile loop,
  throughput loop, IPC request→engine handler), `crates/seed-ipc/src/lib.rs`
  (record shapes), `crates/seed-gui/src/main.rs` (UI feature set + dialog flows).

## Limitations to expect on Android

**Should be fine (preserved by the chosen path):**
- The full sync engine, crypto/trust model, blob/doc/gossip networking, and
  real-path folder mirroring all carry over unchanged.
- NAT traversal via iroh relays works on cellular and Wi-Fi.
- Local-network (mDNS) discovery works on Wi-Fi: `EngineService` holds a
  `WifiManager.MulticastLock` (permission `CHANGE_WIFI_MULTICAST_STATE`) so the
  engine receives inbound multicast and can find LAN peers with no internet.
  Without the lock the device can advertise but never sees others' replies.

**Genuine Android constraints (inherent, not fixable in our code):**
- **Background reliability.** Android aggressively limits background work (Doze,
  App Standby, OEM task killers). A foreground service + battery-optimization
  exemption keeps sync running, but it is not as bulletproof as a desktop daemon;
  on some OEM ROMs the service may still be killed and rely on the boot
  receiver / WorkManager to restart.
- **Battery.** Continuous QUIC keepalive + folder polling drains battery. Plan to
  expose settings to sync only on Wi-Fi and/or while charging, and to lengthen
  the poll interval when the screen is off. (Today's engine polls
  `quick_signature` every 750 ms; that cadence is too aggressive for always-on
  mobile.)
- **Distribution.** All-Files Access effectively rules out Google Play for a
  generic sync app; sideload / F-Droid / direct APK is the channel (consistent
  with current distribution).
- **Storage scope.** Sync targets must live on shared storage
  (`/storage/emulated/0/...`). Folders inside *other apps'* private
  `Android/data/<pkg>/` dirs are off-limits even with All-Files Access on
  Android 11+.
- **Network transitions.** Wi-Fi↔cellular handoff requires the iroh endpoint to
  rebind cleanly — verify this explicitly during testing (a known mobile edge
  case rather than a desktop one).
- **Min SDK.** `MANAGE_EXTERNAL_STORAGE` is Android 11 (API 30)+. Recommend
  targeting minSdk 30 to avoid the legacy `WRITE_EXTERNAL_STORAGE` branch.

**Not a limitation (worth stating since you asked):**
- You do **not** lose the core feature set, the security model, or
  cross-device interop. An Android device is a first-class peer with the same
  master/viewer roles and the same wire protocol — it syncs bidirectionally with
  your existing desktop nodes with no protocol changes.

## Verification

1. **Engine on-device first (de-risk before UI):** build `seed-mobile` for
   `aarch64-linux-android` with `cargo-ndk`; in a minimal harness (or via the
   UniFFI Kotlin bindings in an instrumented test), `init` with a temp dir, add a
   viewer share for a folder synced from a desktop master, and confirm files
   materialize on the device and a peer appears. This proves iroh + blobs + docs
   work on Android before any Compose work.
2. **Round-trip interop:** make the Android device a *master* of a folder on
   shared storage; confirm a desktop viewer receives the files and the signed
   manifest verifies (seqno increments, signature valid).
3. **Permissions + service:** verify the All-Files Access grant flow, that the
   foreground-service notification shows live throughput, and that sync survives
   screen-off and app-swipe-away (then is restarted by the boot receiver).
4. **Battery/network:** toggle Wi-Fi↔cellular mid-sync and confirm the endpoint
   recovers; confirm the Wi-Fi-only / charging-only settings gate sync as
   expected.
5. **Existing tests:** `seed-core`'s standalone tests (`scan.rs`, engine) keep
   passing on the host — they are unaffected by the port and guard against
   regressions in the shared engine.
