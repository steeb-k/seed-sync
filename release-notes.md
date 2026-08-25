**S.E.E.D. (SEED Sync) v0.7.2** — P2P mirrored-folder sync.

A patch release with one significant fix: removing or pausing a folder now
actually stops it syncing.

### What's new

- **Fixed: removing a folder didn't stop it syncing.** Removing a shared folder
  took it out of the app immediately — the folder list emptied, and both the app
  and the command line correctly reported it was gone. Underneath, the sync pass
  already in progress kept running: it carried on writing files back into the
  folder you had just detached, kept contacting other devices for content, and
  because folders sync one at a time, it **stalled every one of your other
  folders for as long as it ran**. Nothing in the interface showed this was
  happening. In one case a folder removed at 11:11 was still re-creating a
  deleted directory 15 minutes later, and only stopped when the app was quit.

  Removing a folder now cancels the pass in flight, and a cancelled pass writes
  nothing — so it can't leave behind records for a folder you just removed.
  **This is the reason to update.**

- **Fixed: pausing a folder had the same hole, in smaller form.** Pausing (and
  "pause all", and suspending sync) stopped new downloads but let the pass in
  progress keep writing files and publishing changes through to the end of its
  walk. All four now stop the pass itself.

- **Fixed: a sync pass could run forever.** The watchdog that noticed a slow pass
  only wrote a log line — nothing ever actually stopped one. A pass is now
  abandoned after 30 minutes; nothing is committed and the work is simply redone
  on the next pass, which also caps how long one stuck folder can hold up the
  rest.

- **Fixed: one unreachable device could stall a whole pass.** When repairing
  files, the app retried the same unreachable device once for *every* file, at up
  to 15 seconds each — so a folder of 200 files could spend nearly an hour
  waiting on one device that was never going to answer. It now tries a given
  device once per pass and skips it thereafter.

**Note on scope:** this does not change what happens when a device that has been
away rejoins holding files that were deleted while it was gone — it can still
restore them. That is a separate, already-known limitation and is unchanged here.

### Downloads
- **Linux x86_64** — `seed-sync-0.7.2-linux-x86_64.tar.gz`
- **macOS universal** (Apple Silicon + Intel) — `seed-sync-0.7.2-macos-universal.tar.gz` *(added shortly after release; the 0.7.1 build remains available in the meantime)*
- **Windows x86_64** (signed MSI) — `seed-sync-0.7.2-windows-x86_64.msi`
- **Windows ARM64** (signed MSI) — `seed-sync-0.7.2-windows-arm64.msi`
- **Android** (universal APK) — `seed-sync-0.7.2-android-universal.apk`

### System requirements

**Linux** — GTK is *not* bundled; install the runtime packages first:
- GTK 4.10+, libadwaita 1.4+, libdbus-1
- Debian/Ubuntu: `libgtk-4-1 libadwaita-1-0 libdbus-1-3`
- Fedora: `gtk4 libadwaita dbus-libs`
- Arch: `gtk4 libadwaita dbus`

**macOS** — Apple Silicon or Intel. GTK4 + libadwaita are bundled in the app; no
Homebrew or other runtime needed.

**Windows** — **Windows 10 (64-bit)** or later. GTK4 + libadwaita and all
libraries are bundled in the signed MSI; no separate runtime install required.

**Android** — Android 11 (API 30) or later.
