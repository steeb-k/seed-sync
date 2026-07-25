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

On a folder being **actively and continuously written to**, the routine hourly
storage-cleanup pass can discard its copy of content that was saved in the couple
of minutes before it ran. Devices then report **`Syncing 98%`** instead of
`Healthy 100%` and stay there.

**Your files are not damaged** — every device keeps a correct copy on disk, and
this was verified byte-for-byte across four 28-device test runs. But because every
device cleans up on the same schedule, they can *all* discard the same content at
once, and then:

- a **device joining the folder later** cannot download those particular files;
- a device that loses or corrupts one of them cannot repair it from the others.

A quiet folder is not affected: with no writes just before the cleanup pass there
is nothing to discard. A folder that has been sitting idle stays `Healthy 100%`
indefinitely.

This is **not new in this release** — v0.6.10 and earlier behave the same way. A
fix is written and in verification for 0.7.1. If you are running a folder under
heavy continuous writes and want to be certain nothing is unreachable, the 0.7.1
update repairs it automatically on the next sync pass.

### Downloads
- **Linux x86_64** — `seed-sync-0.7.0-linux-x86_64.tar.gz`
- **macOS universal** (Apple Silicon + Intel) — `seed-sync-0.7.0-macos-universal.tar.gz`
- **Windows x86_64** (signed MSI) — `seed-sync-0.7.0-windows-x86_64.msi`
- **Windows ARM64** (signed MSI) — `seed-sync-0.7.0-windows-arm64.msi`
- **Android** (universal APK) — `seed-sync-0.7.0-android-universal.apk`

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
