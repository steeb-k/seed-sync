**S.E.E.D. (SEED Sync) v0.6.10** — P2P mirrored-folder sync.

A reliability release: ignore lists now reach every device, disk no longer grows
without bound, and wake-from-sleep recovery extends to Windows and macOS.

### What's new

- **Ignore lists now reach every device.** A shared folder's ignore list is written
  by a master, but read-only members weren't receiving it — so a viewer could
  **delete files a master had chosen not to sync** (it treated anything not in the
  shared list as removed). The ignore list now replicates to every peer, so ignored
  files are honored everywhere and left untouched.

- **Automatic disk cleanup.** The content store never reclaimed space, so leftover
  data — most visibly partial downloads stranded by a removed share — accumulated
  indefinitely. A background pass now garbage-collects blobs no share references any
  more, built conservatively from the live shares so it never touches data in use
  (and never anything in your synced folders).

- **Wake-from-sleep recovery on Windows and macOS.** The suspend/resume self-heal
  added for Linux in 0.6.9 now runs on Windows and macOS too: after the machine
  wakes, connectivity is re-established automatically so a large download resumes on
  its own, instead of needing a restart.

- **Warns on possible in-place file corruption.** On a master, if a deep verify finds
  a file's contents changed with no change to its size or timestamp — a hallmark of
  silent corruption rather than a genuine edit — it now logs a warning instead of
  quietly publishing the changed bytes to peers.

### Downloads
- **Linux x86_64** — `seed-sync-0.6.10-linux-x86_64.tar.gz`
- **macOS universal** (Apple Silicon + Intel) — `seed-sync-0.6.10-macos-universal.tar.gz`
- **Windows x86_64** (signed MSI) — `seed-sync-0.6.10-windows-x86_64.msi`
- **Windows ARM64** (signed MSI) — `seed-sync-0.6.10-windows-arm64.msi`
- **Android** (universal APK) — `seed-sync-0.6.10-android-universal.apk`

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
