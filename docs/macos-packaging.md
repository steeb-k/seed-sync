# macOS packaging, distribution & auto-update — maintainer guide

> **Status: BUILT (universal2), 2026-06-20.** Bring-up checklist 0–4 are done and verified on Apple
> Silicon (see `docs/cross-os-testing.md` → [MACOS]). The build → bundle → `.app` → install →
> launchd → update flow works end-to-end, including a `sandbox-exec` "no Homebrew" proof. **Universal2
> (arm64 + x86_64) is built and verified** (needs a second x86_64 Homebrew at `/usr/local`). Still
> open: running the CI job on a runner + publishing the macOS asset to `seed-sync-binaries`, the unified
> hosted bootstrap, and the live cross-OS sync runs. One design change from the original plan:
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
| `scripts/package-macos.sh` ✅ | The macOS analog of `scripts/package-linux.sh`: `cargo build --release`, build the **`SEED Sync.app`** (binaries → `Contents/MacOS`), run the bundler over `Contents/`, write `Info.plist` + `Resources/AppIcon.icns` (sips + iconutil from `icon/appIcon.png`), seal the bundle, tar `dist/seed-sync-<ver>-macos-<arch>.tar.gz`. `--skip-build` to repackage. Builds universal2 when an x86_64 brew at `/usr/local` is present, else arm64. |
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

## Universal2 (built)

`package-macos.sh` builds universal when an x86_64 Homebrew (`/usr/local`) + the x86_64 Rust target are
present (else it falls back to arm64-only). It builds both Rust slices, bundles each arch's GTK closure
separately, then `lipo`s every Mach-O (binaries + dylibs + pixbuf loaders) into the arm64 `.app` and
re-signs inside-out (lipo invalidates the ad-hoc signature). Verified: fat (arm64+x86_64) binaries +
all 57 dylibs, both slices run (native + Rosetta), both self-contained under a no-Homebrew sandbox.

- **Two Homebrew prefixes** — arm64 (`/opt/homebrew`) auto, x86_64 under Rosetta at `/usr/local`
  (bootstrap needs sudo; `arch -x86_64 brew install gtk4 libadwaita pkg-config`). Use the **same GTK
  version** in both (here 4.22.4 / libadwaita 1.9.1) so the dylib sets match for `lipo`.
- **x86_64 cross-build pkg-config:** set `PKG_CONFIG_LIBDIR` (replaces the default search → no arm64
  leak) to the x86_64 brew's per-formula `opt/*/lib/pkgconfig` (keg-only) + `lib`/`share/pkgconfig` +
  `Homebrew/Library/Homebrew/os/mac/pkgconfig/<macOS-major>` (system-lib stubs: zlib/libffi/expat/…),
  plus `PKG_CONFIG_ALLOW_CROSS=1`. Without the per-version stubs dir, gobject/cairo/fontconfig fail to
  resolve their system deps.
- **CI:** the `macos-14` job sets up the x86_64 Homebrew/Rosetta prefix itself (below) so it ships
  universal, not arm64-only.

## CI (built — not yet run on a runner)

The `macos` job in `.github/workflows/release.yml` builds **universal** by bootstrapping a second
x86_64 Homebrew on the runner: tag↔version guard, `brew install gtk4 libadwaita` (arm64),
`softwareupdate --install-rosetta`, a NONINTERACTIVE x86_64 Homebrew at `/usr/local` +
`arch -x86_64 brew install gtk4 libadwaita pkg-config`, `dtolnay/rust-toolchain` with
`targets: x86_64-apple-darwin`, then `scripts/package-macos.sh` (auto-detects the x86_64 brew →
universal) and publishes to `seed-sync-binaries` with `SEED_BINARIES_TOKEN`, `fail_on_unmatched_files:
true`. The runner's Xcode CLT provides `install_name_tool`/`codesign`/`otool`/`lipo`/`iconutil`. **The
second-Homebrew + Rosetta setup is the heaviest/most novel part — validate it on the first release tag.**

### Minimum macOS version is set by the build host — pin `macos-14`
The bundle's real floor is the `minos` (LC_BUILD_VERSION) of the **bundled GTK dylibs**, which Homebrew
stamps with the macOS version of the build machine. Our Rust binaries are low (≈11), but GTK dominates:

| Build host | arm64 GTK `minos` | x86_64 GTK `minos` | Effective floor |
|---|---|---|---|
| `macos-14` runner (Sonoma) | **14** | 13–14 | **macOS 14** (recommended) |
| `macos-15` runner (Sequoia) | 15 | 14–15 | macOS 15 |
| a dev box on macOS 26 | 26 | 14 | macOS 26 on Apple Silicon (too high to ship) |

So **always cut the release on `macos-14`** (the *oldest* Apple-Silicon GitHub runner) to reach the
widest install base — Apple Silicon ≥ 14 and Intel ≥ 14. GitHub has no older Apple-Silicon runner, so
macOS 14 (Sonoma) is the practical arm64 floor for hosted CI; a lower floor needs a self-hosted older
Mac. (On Apple Silicon dyld always loads the arm64 slice, so the arm64 `minos` is what gates those
machines — the x86_64 slice's lower floor only helps Intel Macs.)

**Can we go below macOS 14?** Not cheaply. GitHub's *oldest* Apple-Silicon runner is `macos-14`, and
Homebrew's arm64 GTK bottles target recent macOS — so 14 is the practical arm64 floor for hosted CI.
Lower needs either a **self-hosted older Apple-Silicon Mac** (where `brew` builds GTK from source against
that OS) or **building the whole GTK stack from source with `MACOSX_DEPLOYMENT_TARGET` pinned** (a macOS
"gvsbuild" — a real project). The Intel angle is also closing: Apple discontinued x86_64, the `macos-13`
runner image is being retired, and GitHub drops Intel runners after `macos-15` (Fall 2027) — so there's
no longer a cheap sub-14 Intel runner either. In practice macOS 14 (Sonoma) is the oldest macOS still in
Apple's support window, and every Apple-Silicon Mac runs 14+, so a 14 floor already covers the entire
currently-supported install base.

**Caveat for the current release:** the `v1.1.0` macOS asset was published *manually* from a macOS-26
dev box, so its arm64 slice needs macOS 26. Re-cut via the `macos-14` CI job to drop it to macOS 14.

Until a tag runs this job, the CI-built macOS asset isn't on `seed-sync-binaries`.

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

- **Unified hosted bootstrap** — mirror `packaging/web-install.sh` (built, OS-detecting) to
  `steeb-k.github.io/seed-install.sh`, replacing the Linux-only one.
- Developer ID signing + **notarization** (clean install from any source, incl. a future `.dmg`) —
  needs an Apple Developer account ($99/yr). Keeps ad-hoc as the default; notarize on top.
- A `.dmg` for drag-to-Applications (would need notarization; the `.app` already exists).
- Sparkle-style in-app updates (vs the launchd timer).

See also: `docs/linux-packaging.md` (the model this mirrors), `docs/windows-packaging.md` (the MSI
side), and the macOS bring-up checklist + sync matrix in `docs/cross-os-testing.md`.
