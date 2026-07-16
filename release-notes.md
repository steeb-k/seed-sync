**S.E.E.D. (SEED Sync) v0.6.4** — P2P mirrored-folder sync.

A focused fix for a false **"Out of sync — members disagree"** alarm that fired the
moment you added a new member to a share.

### Fixed in 0.6.4

**Adding a member no longer trips a false "Out of sync — members disagree."** A device
you have just added hasn't received the shared file list yet, so its manifest starts
**empty** — and an empty manifest was being reported as **100% synced** (it holds
everything it knows about, which is nothing). Every established member then read the
newcomer as a fully-synced device that happened to hold a *different* set of files, and
raised the pool's most serious status within seconds of the join — long before the new
device had downloaded anything.

A member that is **still receiving the share** is *behind*, not *diverged*: it fixes
itself, it doesn't need you. A device now withholds its manifest fingerprint until its
copy of the share has actually arrived, so it counts as still-joining rather than as a
settled disagreement. Once its file list syncs it reports real download progress and
converges to Healthy as usual — with no false alarm in between.

> This completes the divergence-reporting work from 0.6.3, which had already stopped a
> member that was *still downloading* (visibly below 100%) from being counted; 0.6.4
> closes the remaining case, where a just-joined member reported 100% because it held
> nothing yet.

### Downloads
- **Linux x86_64** — `seed-sync-0.6.4-linux-x86_64.tar.gz`
- **Windows x86_64** (signed MSI) — `seed-sync-0.6.4-windows-x86_64.msi`
- **Windows ARM64** (signed MSI) — `seed-sync-0.6.4-windows-arm64.msi`
- **Android** (universal APK) — `seed-sync-0.6.4-android-universal.apk`

### System requirements

**Linux** — GTK is *not* bundled; install the runtime packages first:
- GTK 4.10+, libadwaita 1.4+, libdbus-1
- Debian/Ubuntu: `libgtk-4-1 libadwaita-1-0 libdbus-1-3`
- Fedora: `gtk4 libadwaita dbus-libs`
- Arch: `gtk4 libadwaita dbus`

**Windows** — **Windows 10 (64-bit)** or later, x86_64 or ARM64. GTK4 + libadwaita and all
libraries are bundled in the signed MSI; no separate runtime install required.

**Android** — Android 11 (API 30) or later.
