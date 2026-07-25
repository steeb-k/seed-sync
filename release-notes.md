**S.E.E.D. (SEED Sync) v0.7.1** — P2P mirrored-folder sync.

A patch release. It fixes the storage-cleanup fault listed as a known issue in
v0.7.0, and three interface annoyances on Windows and macOS.

### What's new

- **Fixed: routine storage cleanup could discard content your devices still
  needed.** The hourly cleanup pass worked from a list of "content still in use"
  that was refreshed every couple of minutes — so anything saved *since* that
  list was built looked unused, and was thrown away. Your files were never
  touched, but the device quietly lost its ability to *hand that file to
  another device*. Because every device cleans up on the same schedule, they
  could all discard the same content at once: a device joining the folder later
  then couldn't download those files, and a device that lost or damaged one
  couldn't repair it from the others. Devices also reported **`Syncing 98%`**
  forever instead of `Healthy 100%`.

  Content saved since the last refresh is now protected from cleanup, and any
  file already affected is **repaired automatically** — the next sync pass puts
  it back from the copy on disk, so upgrading heals a folder this already
  happened to. Cleanup still reclaims genuinely unused data, so storage does not
  grow unbounded. **This is the reason to update.**

  Only folders under sustained, continuous writes were affected. A folder that
  sits idle never hit this.

- **Fixed (Windows): the tray icon didn't come back after an automatic update.**
  Updates install overnight, and the updater stops the tray app so the installer
  can replace it — then failed to work out who to restart it as whenever the
  screen was locked, which it always is at 3am. Syncing carried on normally, but
  the tray icon stayed gone until you opened the app by hand. It now restarts
  reliably, and says so in its log either way.

- **Fixed (macOS): opening the window from the tray put it behind other
  windows.** A tray click doesn't bring an app to the front on macOS, so the
  window appeared underneath whatever you were looking at. It now comes to the
  front.

- **Changed (macOS): with no window open, SEED Sync leaves ⌘-Tab and the Dock.**
  It's a tray app, so it no longer sits in the app switcher with no window to
  switch to. Open it again from the tray icon and it returns to both.

### Downloads
- **Linux x86_64** — `seed-sync-0.7.1-linux-x86_64.tar.gz`
- **macOS universal** (Apple Silicon + Intel) — `seed-sync-0.7.1-macos-universal.tar.gz`
- **Windows x86_64** (signed MSI) — `seed-sync-0.7.1-windows-x86_64.msi`
- **Windows ARM64** (signed MSI) — `seed-sync-0.7.1-windows-arm64.msi`
- **Android** (universal APK) — `seed-sync-0.7.1-android-universal.apk`

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
