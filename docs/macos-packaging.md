# macOS packaging, distribution & auto-update — maintainer guide

> **Status: PLANNED (2026-06-20). Nothing here is built yet.** This is the design record
> for bringing S.E.E.D. (Seed Sync) to macOS. It mirrors `docs/linux-packaging.md`; sections
> marked _(planned)_ describe files/flows that still need to be written. As pieces land,
> drop the _(planned)_ marker and fill in the real command output.

## Why the Linux model, not a `.dmg`/`.pkg`

S.E.E.D. is a *per-user background daemon* + GUI + CLI that reads/writes arbitrary user
folders and needs the keychain, full network (iroh), and a tray. The Linux tarball model
(`docs/linux-packaging.md`) maps to macOS **1:1 in shape**, and macOS adds a bonus reason
to prefer it:

- **`curl | sh` dodges Gatekeeper quarantine.** macOS only stamps the `com.apple.quarantine`
  xattr on files written by apps that opt into `LSFileQuarantineEnabled` (browsers, Mail,
  Messages) — **not** `curl`, `tar`, or a shell. So a tarball fetched and unpacked by a
  `curl | sh` bootstrap is **not quarantined**, and Gatekeeper never throws the "unidentified
  developer" block — even with only an **ad-hoc** signature and **no notarization / no Apple
  Developer account**. A browser-downloaded `.dmg`/`.pkg` would be quarantined and blocked. This
  is the macOS analog of how the Linux tarball sidesteps code-signing.
- **No autostart story in a `.dmg`.** A drag-to-Applications `.dmg` still wouldn't register the
  launchd agents; we'd need a wrapper anyway. A `.pkg` could, but reintroduces signing pressure.
- Consistency: one mental model, one `seed-sync` wrapper shape, one `seed-sync-binaries` release
  per tag carrying every OS's asset.

**Rejected:** Homebrew cask (still needs the daemon/launchd story + a public tap), `.app` in a
`.dmg` (quarantine + no autostart), `.pkg` (signing pressure, less consistent with Linux).

## Locked decisions (human, 2026-06-20)

| Decision | Choice | Consequence |
|---|---|---|
| Architecture | **Universal2 (arm64 + x86_64)** | `lipo` both slices of binaries **and** every bundled dylib. Built **phased**: arm64 first, x86_64 slice added after. |
| GTK4 + libadwaita | **Bundle the dylibs** | Self-contained tarball; no user Homebrew. Relocate + re-sign; ship pixbuf loaders, GSettings schemas, Adwaita resources. ~60–80 MB tarball. |
| Signing | **Ad-hoc only** | Re-sign after relocation (mandatory on Apple Silicon). No notarization; rely on the `curl \| sh` quarantine dodge. $0, no Apple account. |

## Distribution / update flow (identical to Linux/Windows)

```
  main repo (private)                seed-sync-binaries (PUBLIC)        user machine (macOS)
  ───────────────────                ──────────────────────────        ────────────────────
  git tag vX.Y.Z  ──►  release.yml ──►  Release "vX.Y.Z"        ◄─── seed-sync --update
   (Cargo version)     macOS job adds   ├─ ...linux-x86_64.tar.gz  poll  (launchd timer, daily)
                       the macOS asset   ├─ ...windows-x86_64.msi    +    compares to
                                         └─ ...macos-universal.tar.gz fetch `seed-daemon --version`
```

Same public artifact repo, same "installed version is the source of truth" rule (the updater
reads `seed-daemon --version` and compares to the latest release tag), same **mandatory Cargo
version bump per release**. Asset name convention: **`seed-sync-<ver>-macos-universal.tar.gz`**.

## Files to build _(planned — `packaging/macos/`)_

| File | Purpose |
|---|---|
| `scripts/package-macos.sh` _(planned)_ | The macOS analog of `scripts/package-linux.sh`: `cargo build --release` for each target arch, **bundle + relocate + re-sign GTK dylibs**, `lipo` to universal, stage the tree, write `dist/seed-sync-<ver>-macos-universal.tar.gz`. `--skip-build` to repackage. |
| `scripts/bundle-gtk-macos.sh` _(planned)_ | The hard part (analog of `bundle-gtk-windows.ps1`): copy the GTK/libadwaita/pixbuf/cairo/pango/… dylib closure into `lib/`, rewrite install names to `@executable_path/../lib` with `install_name_tool`, copy the gdk-pixbuf `loaders.cache` + loaders, compile + bundle GSettings schemas (`glib-compile-schemas`), bundle the Adwaita icon theme, then **ad-hoc re-sign** every relocated binary/dylib (`codesign -s - --force`). |
| `packaging/macos/seed-sync` _(planned)_ | The one wrapper — install/update/uninstall/status, per-user, using `launchctl` instead of `systemctl`. Mirrors `packaging/linux/seed-sync` (shared `apply_tree` logic). |
| `packaging/macos/web-install.sh` _(planned)_ | `curl \| sh` bootstrap (POSIX sh). Same shape as the Linux one; selects the `…macos-universal.tar.gz` asset. **This is what dodges quarantine.** |
| `packaging/macos/io.github.steeb_k.SeedSync.daemon.plist` _(planned)_ | launchd **LaunchAgent** that runs `seed-daemon run`, `KeepAlive`, `RunAtLoad`. The macOS analog of `seed-daemon.service`. |
| `packaging/macos/io.github.steeb_k.SeedSync.update.plist` _(planned)_ | LaunchAgent with `StartCalendarInterval` (daily) running `seed-sync --update`. Analog of the systemd timer. |
| `packaging/macos/io.github.steeb_k.SeedSync.gui.plist` _(planned)_ | LaunchAgent `RunAtLoad` for the tray GUI (`seed-gui --hidden`). Analog of the `.desktop` autostart entry. |

## Tarball layout _(planned)_

```
seed-sync-<ver>-macos-universal/
├── bin/{seed-daemon,seed-gui,seed-cli}     # universal2, ad-hoc signed, @executable_path rpaths
├── lib/                                     # bundled GTK/Adwaita/pixbuf/... dylibs (universal, re-signed)
│   ├── gdk-pixbuf-2.0/.../loaders/ + loaders.cache
│   └── ...
├── share/
│   ├── glib-2.0/schemas/gschemas.compiled   # compiled GSettings schemas (file-chooser etc.)
│   └── icons/...                            # Adwaita/hicolor app + theme icons
├── seed-sync                                # the wrapper; also copied to ~/.local/bin on install
├── INSTALL.txt
└── LaunchAgents/                            # the three .plist templates (paths rewritten on install)
```

## Where things land on the user's machine _(planned, per-user, no root)_

```
~/.local/bin/                     seed-daemon, seed-gui, seed-cli, seed-sync (+ bundled lib/, share/)
~/Library/LaunchAgents/           io.github.steeb_k.SeedSync.{daemon,update,gui}.plist
~/Library/Application Support/    DATA: state.db, blobs/, docs/, node.key, seed.sock
                                   (directories crate: ProjectDirs "io.github"/"steeb_k"/"SeedSync")
```

The keychain (`keyring` `apple-native`) holds the master seed. `launchctl bootstrap gui/$UID
<plist>` loads the agents; `launchctl bootout` on uninstall.

## The bundling process (the hard part) _(planned)_

1. **Build** for `aarch64-apple-darwin` (and later `x86_64-apple-darwin`) against Homebrew GTK.
2. **Walk the dylib closure** of `seed-gui` (`otool -L`, recursively) and copy every non-system
   dylib (everything under the Homebrew prefix; skip `/usr/lib`, `/System/...`) into `lib/`.
3. **Relocate**: for each copied dylib and each binary, `install_name_tool -change <old> \
   @executable_path/../lib/<name>` for every Homebrew reference, and `-id @executable_path/../lib/<name>`
   on the dylib itself. Add an `@executable_path/../lib` rpath.
4. **gdk-pixbuf**: copy the loader modules + regenerate `loaders.cache` with paths relative to the
   bundle (`GDK_PIXBUF_MODULEDIR` at runtime, or a patched cache).
5. **GSettings**: `glib-compile-schemas` the GTK + app schemas into `share/glib-2.0/schemas/`
   (the file-chooser needs this — same class of bug fixed on Windows).
6. **Adwaita resources / icons**: bundle the icon theme so the GUI renders without a system theme.
7. **Ad-hoc re-sign**: `codesign --force --sign - <each dylib and binary>`, inside-out (dylibs
   before the executables that load them). **Mandatory on Apple Silicon** — relocation invalidates
   the linker's ad-hoc signature, and an invalid signature = `Killed: 9`.
8. **Verify**: `otool -L` shows only `@executable_path`/`@rpath` + system frameworks; `codesign
   --verify` passes; the app launches with the Homebrew prefix removed/renamed (the real test that
   nothing leaks to the system GTK).

## Universal2 (phased) _(planned)_

- **Phase 1:** arm64-only (`aarch64-apple-darwin`). Prove build → bundle → relocate → re-sign →
  launch → `curl | sh` install → launchd → update, end to end. Asset temporarily `…macos-arm64.tar.gz`.
- **Phase 2:** add the x86_64 slice. **Homebrew GTK is single-arch**, so universal GTK requires both
  arch slices of every dylib. Options:
  - **Two Homebrew prefixes** — arm64 (`/opt/homebrew`) + x86_64 under Rosetta (`/usr/local`), then
    `lipo -create arm64.dylib x86_64.dylib -output universal.dylib` per dylib (and per binary).
  - **From-source universal GTK** (gvsbuild-grade effort) — last resort.
  - Re-sign after `lipo` (lipo invalidates signatures too). Switch asset to `…macos-universal.tar.gz`.

## CI _(planned)_

Add a `macos` job to `.github/workflows/release.yml` on **`macos-14`** (Apple Silicon runner):
`brew install gtk4 libadwaita`, build, run `scripts/package-macos.sh`, assert the tag matches the
Cargo version (same guard as Linux), publish the universal tarball to `seed-sync-binaries` with
`SEED_BINARIES_TOKEN`, `fail_on_unmatched_files: true`. (Universal build on an arm64 runner needs the
x86_64 Homebrew/Rosetta setup from Phase 2 — until then the CI job ships arm64.)

## Caveats / gotchas

- **Re-sign AFTER every mutation.** `install_name_tool` and `lipo` both invalidate the ad-hoc
  signature; Apple Silicon refuses to run an invalidly-signed Mach-O. Always `codesign --force -s -`
  last, inside-out.
- **GSettings schemas** must be compiled and discoverable or the GTK file-chooser crashes on open
  (same failure mode fixed on Windows). Bundle `gschemas.compiled` and set `GSETTINGS_SCHEMA_DIR`.
- **gdk-pixbuf loaders** must be bundled + the cache regenerated, or icons/images fail to load.
- **launchd is per-user (`gui/$UID` domain).** Needs an active GUI session (Aqua); headless/SSH-only
  Macs won't load Aqua agents — the wrapper should warn and still place files (mirrors the Linux
  systemd-session caveat).
- **Quarantine dodge is install-path-specific.** It holds for `curl | sh`. If a user instead
  downloads the tarball in a browser, it *will* be quarantined; document `xattr -dr
  com.apple.quarantine <dir>` as the escape hatch, or revisit notarization later.
- **Version bump is mandatory per release** (updater is version-driven; CI guard fails otherwise).

## Future work (not built)

- Developer ID signing + **notarization** (clean install from any source, incl. a future `.dmg`) —
  needs an Apple Developer account ($99/yr). Keeps ad-hoc as the default; notarize on top.
- A real `.app` bundle + `.dmg` for users who prefer drag-to-Applications (would need notarization).
- Sparkle-style in-app updates (vs the launchd timer).

See also: `docs/linux-packaging.md` (the model this mirrors), `docs/windows-packaging.md` (the MSI
side), and the macOS bring-up checklist + sync matrix in `docs/cross-os-testing.md`.
