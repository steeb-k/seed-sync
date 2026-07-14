**S.E.E.D. (SEED Sync) v0.6.3** — P2P mirrored-folder sync.

Supersedes 0.6.2 (which was withdrawn). It carries everything from that release, plus two
fixes to how membership and sync status are *reported* — including one regression
introduced by 0.6.2 itself.

The Windows ARM64 build is a **full release**, no longer a pre-release.

### Fixed in 0.6.3

**A new share no longer reports "Out of sync — members disagree" while it is still
syncing.** Divergence is detected by comparing each member's file manifest — but a member
that is still completing its *initial* sync holds a partial manifest by definition, so
every device in a freshly-created share accused the others of disagreeing, within a minute
of joining and long before the first sync had finished. A member that is **behind** is not
a member that has **diverged**: the first resolves itself, the second needs you. Only
fully-synced members are now compared, and a disagreement must be *stable* — one that is
still changing is simply propagation in progress.

**A device no longer counts itself as an extra member.** In 0.6.2, the device that created
a share could list one more member than actually existed (other devices saw the correct
count). Harmless to syncing, but confusing — and it was a regression in 0.6.2's new
bootstrap code.

### Fixed in 0.6.2 (included here)

**A locked login keyring no longer demotes a master to read-only — or reverts your files.**
Master shares keep their write key in the OS keystore. If that key could not be read at
startup — most commonly on Linux, where the daemon starts at boot and *races the login
keyring*, leaving the unlock prompt dismissed — the share quietly came up **read-only**.
That was not a graceful degradation: a read-only share treats the network copy as
authoritative and **reverts local edits**, fetching the old bytes from a peer and writing
them over your file. So edits made in what looked like your own writable share were
silently rolled back, while every screen still read `Healthy`.

Now a share whose write key is unavailable is held **inert** — it syncs in neither
direction and cannot touch your files — reports **"Write key locked — unlock your login
keyring"**, and resumes **on its own** the moment the key becomes available. No restart.

**Any member can now bootstrap a new device — not just the one that created the share.**
A share key carried a single device's address: whoever created the share. If that one
device was offline, a new member had nowhere to connect and simply never synced, no matter
how many other members were online. Every master now advertises itself under the share's
own key, so a joiner finds whichever master is up. Restarts are also more robust: a device
now remembers and re-dials every member it has seen, instead of only the creator.

> **Update your existing devices first.** The new bootstrap only works once the *masters*
> are running 0.6.2+ — they are the ones doing the advertising.

**A device that can reach nobody now says so.** It used to report `Healthy 100%` — because
it agreed with every peer it could hear, and it could hear none. Such a share now reads
**"No members reachable"**, in the app and in the tray.

**Linux: the tray icon now appears reliably.** At login the app could start before the
desktop's tray service was ready, fail once, and never retry — so the icon was missing
until the app was restarted by hand. It now waits for the tray and registers as soon as
it's available.

### Downloads
- **Linux x86_64** — `seed-sync-0.6.3-linux-x86_64.tar.gz`
- **Windows x86_64** (signed MSI) — `seed-sync-0.6.3-windows-x86_64.msi`
- **Windows ARM64** (signed MSI) — `seed-sync-0.6.3-windows-arm64.msi`
- **Android** (universal APK) — `seed-sync-0.6.3-android-universal.apk`

### System requirements

**Linux** — GTK is *not* bundled; install the runtime packages first:
- GTK 4.10+, libadwaita 1.4+, libdbus-1
- Debian/Ubuntu: `libgtk-4-1 libadwaita-1-0 libdbus-1-3`
- Fedora: `gtk4 libadwaita dbus-libs`
- Arch: `gtk4 libadwaita dbus`

**Windows** — **Windows 10 (64-bit)** or later, x86_64 or ARM64. GTK4 + libadwaita and all
libraries are bundled in the signed MSI; no separate runtime install required.

**Android** — Android 8.0 (API 26) or later.
