# macOS packaging, distribution & auto-update — maintainer guide

> **Status: BUILT (arm64), 2026-06-20.** Bring-up checklist 0–3 are done and verified on Apple
> Silicon (see `docs/cross-os-testing.md` → [MACOS]). The build → bundle → `.app` → install →
> launchd → update flow works end-to-end, including a `sandbox-exec` "no Homebrew" proof. Still
> open: universal2 (x86_64 `lipo`), running the CI job on a runner + publishing the macOS asset to
> `seed-sync-binaries`, and the unified hosted bootstrap. One design change from the original plan:
> we ship a **`SEED Sync.app` bundle _inside_ the curl|sh tarball** (installed to `~/Applications`)
> rather than loose binaries — this keeps the quarantine dodge while giving a real Dock/Applications
> icon. Details below; `_(planned)_` markers remain only on what's genuinely not built.

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

## Files built (`packaging/macos/` + `scripts/`)

| File | Purpose |
|---|---|
| `scripts/package-macos.sh` ✅ | The macOS analog of `scripts/package-linux.sh`: `cargo build --release`, build the **`SEED Sync.app`** (binaries → `Contents/MacOS`), run the bundler over `Contents/`, write `Info.plist` + `Resources/AppIcon.icns` (sips + iconutil from `icon/appIcon.png`), seal the bundle, tar `dist/seed-sync-<ver>-macos-<arch>.tar.gz`. `--skip-build` to repackage. arm64 today; universal `lipo` = phase 2. |
| `scripts/bundle-gtk-macos.sh` ✅ | The hard part: walk the `seed-gui` otool closure + the gdk-pixbuf/librsvg loader modules, copy every non-system dylib into `lib/`, rewrite install names to `@executable_path/../lib` (handles absolute `/opt/homebrew/*` **and** `@rpath/*` — librsvg uses `@rpath`), regenerate `loaders.cache`, compile GSettings schemas, bundle the fontconfig config, **ad-hoc re-sign** inside-out. `BUNDLE_BINDIR=MacOS` targets a `.app`'s `Contents/`. No icon theme needed (GTK4 embeds its icons). |
| `packaging/macos/seed-sync` ✅ | The wrapper — install/update/uninstall/status, per-user, via `launchctl bootstrap/bootout`. Installs the `.app` to `~/Applications`, symlinks the CLI into `~/.local/bin`. |
| `packaging/macos/Info.plist` ✅ | App bundle metadata (`CFBundleExecutable=seed-gui`, `CFBundleIconFile=AppIcon`, identifier, `__VERSION__` rewritten from Cargo). What makes NSBundle resolve → the Dock/Applications icon. |
| `packaging/macos/web-install.sh` ✅ | `curl \| sh` bootstrap; selects the macOS asset, unpacks, runs `SEED Sync.app`'s sibling `seed-sync --install`. **Dodges quarantine.** (Superseded for hosting by the unified cross-OS `packaging/web-install.sh` — _planned_.) |
| `packaging/macos/io.github.steeb_k.SeedSync.daemon.plist` ✅ | LaunchAgent: `__APP__/Contents/MacOS/seed-daemon run`, `KeepAlive`, `RunAtLoad`. Analog of `seed-daemon.service`. |
| `packaging/macos/io.github.steeb_k.SeedSync.update.plist` ✅ | LaunchAgent: `RunAtLoad` + daily `StartCalendarInterval` → `__BIN__/seed-sync --update` (the real-file wrapper, not the in-app copy, so a self-update can't yank it). |
| `packaging/macos/io.github.steeb_k.SeedSync.gui.plist` ✅ | LaunchAgent `RunAtLoad` for the tray GUI (`__APP__/Contents/MacOS/seed-gui --hidden`), Aqua-only. Analog of the `.desktop` autostart. |

## Tarball layout (built)

```
seed-sync-<ver>-macos-arm64/
├── SEED Sync.app/
│   └── Contents/
│       ├── Info.plist                         # CFBundleExecutable=seed-gui, CFBundleIconFile=AppIcon
│       ├── MacOS/{seed-daemon,seed-gui,seed-cli}   # ad-hoc signed; @executable_path/../lib
│       ├── lib/                               # bundled GTK/pixbuf/... dylibs (re-signed)
│       │   └── gdk-pixbuf-2.0/.../loaders/ + loaders.cache
│       ├── share/glib-2.0/schemas/gschemas.compiled   # compiled GSettings schemas (file-chooser)
│       ├── etc/fonts/fonts.conf               # fontconfig (points at system macOS font dirs)
│       └── Resources/AppIcon.icns             # Dock/Applications icon
├── seed-sync                                  # the wrapper (bootstrap; copied to ~/.local/bin on install)
├── INSTALL.txt
├── LICENSE
└── LaunchAgents/                              # the three .plist templates (__APP__/__BIN__/__LOG__ rewritten)
```

GTK4 embeds its own icon resource, so no Adwaita/hicolor icon theme is bundled. ~22 MB tarball.

## Where things land on the user's machine (per-user, no root)

```
~/Applications/SEED Sync.app/     the self-contained app (binaries + bundled lib/share/etc + icns)
~/.local/bin/                     seed-gui, seed-daemon, seed-cli (symlinks into the .app) + seed-sync (real file)
~/Library/LaunchAgents/           io.github.steeb_k.SeedSync.{daemon,update,gui}.plist
~/Library/Logs/SeedSync/          seed-daemon.log, seed-update.log
~/Library/Application Support/    DATA: state.db, blobs/, docs/, node.key, seed.sock
                                   (directories crate: ProjectDirs "io.github"/"steeb_k"/"SeedSync")
```

The Keychain (`keyring` `apple-native`) holds the master seed. `launchctl bootstrap gui/$UID <plist>`
loads the agents; `launchctl bootout` on uninstall. The CLI symlinks are why `@executable_path` must
resolve through a symlink — `setup_runtime_env` calls `fs::canonicalize` so the prefix is the real
`.app`, not the symlink's parent (see the gotcha in `cross-os-testing.md`).

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

## CI (built — not yet run on a runner)

A `macos` job is added to `.github/workflows/release.yml` on **`macos-14`** (Apple Silicon runner):
`brew install gtk4 libadwaita pkg-config`, the tag↔Cargo-version guard (same as Linux), run
`scripts/package-macos.sh`, publish `dist/*.tar.gz` to `seed-sync-binaries` with `SEED_BINARIES_TOKEN`
and `fail_on_unmatched_files: true`. Ships **arm64** (universal needs the x86_64 Homebrew/Rosetta setup
from Phase 2). **Validate on the first macOS release tag** — the runner's Xcode CLT provides
`install_name_tool`/`codesign`/`otool`/`iconutil`. Until a tag runs this job, the macOS asset isn't on
`seed-sync-binaries`, so the live `curl | sh` install can't find it yet.

## Caveats / gotchas

- **Re-sign AFTER every mutation.** `install_name_tool` and `lipo` both invalidate the ad-hoc
  signature; Apple Silicon refuses to run an invalidly-signed Mach-O. Always `codesign --force -s -`
  last, inside-out (dylibs/loaders first, then the binaries, then seal the `.app`).
- **`@executable_path` through symlinks.** The installed binaries are `~/.local/bin` symlinks into the
  `.app`. `std::env::current_exe()` may hand back the *symlink* path → wrong prefix → bundled
  schemas/loaders not found. On a dev box GLib silently falls back to Homebrew's copies, masking it; a
  brew-less Mac then crashes the file-chooser. **Fix:** `fs::canonicalize` the exe in `setup_runtime_env`.
  Catch it with `sandbox-exec -p '(version 1)(allow default)(deny file-read* (subpath "/opt/homebrew"))'`
  — the real "no Homebrew" test on a machine that still has brew.
- **fontconfig compiled-in config path.** libfontconfig has a baked-in `/opt/homebrew/etc/fonts` dir
  (absent on users' Macs). Bundle the Homebrew `fonts.conf` (it references the system macOS font dirs)
  to `etc/fonts/` and set `FONTCONFIG_PATH`; the stale brew cachedir falls through to the xdg cache.
- **`loaders.cache` must be generated against a launchable bundle.** `gdk-pixbuf-query-loaders` dlopens
  each module, so after relocation the modules must be re-signed AND `@executable_path/../lib` must
  resolve — the bundler runs a temporary copy of `query-loaders` from the stage's bindir to satisfy both.
- **Don't put the shell wrapper inside `Contents/MacOS`.** `codesign` of the bundle treats files there as
  code and fails to seal an unsigned script. The `seed-sync` wrapper ships at the tarball root only.
- **GSettings schemas** must be compiled + discoverable or the file-chooser crashes (Windows parallel).
- **launchd is per-user (`gui/$UID` domain).** Needs an active GUI session (Aqua); headless/SSH-only
  Macs won't load Aqua agents — the wrapper warns and still places files.
- **Quarantine dodge is install-path-specific.** It holds for `curl | sh`. A browser download *will* be
  quarantined; the escape hatch is `xattr -dr com.apple.quarantine "<dir>"`.
- **Version bump is mandatory per release** (updater is version-driven; CI guard fails otherwise).

## Future work (not built)

- **Universal2** — `lipo` the x86_64 slice in (Phase 2 above); switch the asset to `…macos-universal`.
- **Unified hosted bootstrap** — `steeb-k.github.io/seed-install.sh` should detect the OS and serve the
  Linux or macOS path from one script (`packaging/web-install.sh`).
- Developer ID signing + **notarization** (clean install from any source, incl. a future `.dmg`) —
  needs an Apple Developer account ($99/yr). Keeps ad-hoc as the default; notarize on top.
- A `.dmg` for drag-to-Applications (would need notarization; the `.app` already exists).
- Sparkle-style in-app updates (vs the launchd timer).

See also: `docs/linux-packaging.md` (the model this mirrors), `docs/windows-packaging.md` (the MSI
side), and the macOS bring-up checklist + sync matrix in `docs/cross-os-testing.md`.
