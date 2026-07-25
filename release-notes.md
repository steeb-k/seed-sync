**S.E.E.D. (SEED Sync) v0.7.0** — P2P mirrored-folder sync.

A minor release, not a patch: it closes a way deleted files could come back on
every device, and it changes when a newly-joined master starts publishing.

### What's new

- **Fixed: a deleted file could come back permanently, on every device.** If a
  master joined a shared folder while already holding its own copy of a file that
  had been deleted — a laptop that was offline for a while, or a device restored
  from a backup — and it ran its first sync pass before receiving the folder's
  current state, it re-published that file. The re-publish looked *newer* than the
  deletion, so the deletion lost and was discarded, and the file reappeared
  everywhere with nothing able to remove it again. Deletions are now protected
  against that race. **This is the reason to update.**

- **A newly-joined master waits before publishing.** Following from the above: a
  master that joins a folder already containing files now waits until it has
  received the folder's current state before publishing them, rather than
  publishing immediately and possibly overriding changes it hasn't seen yet. Its
  files appear to other devices a pass or two later than before. Nothing is
  skipped — only delayed — and a folder you created yourself is unaffected.

- **Networking stack updated (iroh 1.0.3).** Three fixes worth calling out: direct
  peer-to-peer paths no longer get starved by relay traffic; on Windows, an
  unreachable relay no longer disrupts connections that are otherwise working; and
  joining a folder for the first time finds peers faster, without waiting on DNS.

### Known issue

After roughly an hour of running, every device reports **`Syncing 98%`** instead
of `Healthy 100%`, caused by the routine hourly storage-cleanup pass. **Your files
are not affected.** This is a status-display fault only — verified across three
28-device test runs in which every device's files stayed byte-for-byte identical
throughout. It is **not new in this release**; v0.6.10 behaves the same way. A fix
is targeted for 0.7.1.

### Downloads
- **Linux x86_64** — `seed-sync-0.7.0-linux-x86_64.tar.gz`
- **Windows x86_64** (signed MSI) — `seed-sync-0.7.0-windows-x86_64.msi`
- **Windows ARM64** (signed MSI) — `seed-sync-0.7.0-windows-arm64.msi`
- **Android** (universal APK) — `seed-sync-0.7.0-android-universal.apk`
- **macOS universal** (Apple Silicon + Intel) — *building separately; will be
  added to this release shortly.* macOS machines will pick up 0.7.0 automatically
  once it lands.

### System requirements

**Linux** — GTK is *not* bundled; install the runtime packages first:
- GTK 4.10+, libadwaita 1.4+, libdbus-1
- Debian/Ubuntu: `libgtk-4-1 libadwaita-1-0 libdbus-1-3`
- Fedora: `gtk4 libadwaita dbus-libs`
- Arch: `gtk4 libadwaita dbus`

**macOS** — Apple Silicon or Intel. GTK4 + libadwaita are bundled in the app; no
Homebrew or other runtime needed.

**Windows** — **Windows 10 (64-bit)** or later, x86_64 or ARM64. GTK4 + libadwaita and
all libraries are bundled in the signed MSI; no separate runtime install required.

**Android** — Android 11 (API 30) or later.
