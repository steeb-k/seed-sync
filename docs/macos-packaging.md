# macOS packaging, distribution & auto-update — maintainer guide

> **Status: BUILT, floor macOS 11, 2026-06-27.** Bring-up checklist 0–4 are done and verified on Apple
> Silicon (see `docs/cross-os-testing.md` → [MACOS]). The build → bundle → `.app` → install →
> launchd → update flow works end-to-end, including a `sandbox-exec` "no build-time libs" proof.
> **GTK is now sourced from conda-forge, not Homebrew** — its dylibs carry a `minos` of macOS 11
> regardless of the build host, so releases are cut **manually on any Mac** (no `macos-14` CI runner,
> no second x86_64 Homebrew/Rosetta). Build with `scripts/setup-conda-macos.sh [--universal]` then
> `scripts/package-macos.sh`. The minimum-version section below has the full rationale. One design
> change from the original plan: we ship a **`SEED Sync.app` bundle _inside_ the curl|sh tarball**
> (installed to `~/Applications`) rather than loose binaries — this keeps the quarantine dodge while
> giving a real Dock/Applications icon. Details below; `_(planned)_` markers remain only on what's
> genuinely not built.

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
  dev machines (local builds)        seed-sync-binaries (PUBLIC)        user machine (macOS)
  ───────────────────────────        ──────────────────────────        ────────────────────
  package-macos.sh  ──► gh release ──►  Release "vX.Y.Z"        ◄─── seed-sync --update
  (on a Mac)            create/upload   ├─ ...linux-x86_64.tar.gz  poll  (launchd timer, daily)
                        the macOS asset  ├─ ...windows-x86_64.msi    +    compares to
                                         └─ ...macos-universal.tar.gz fetch `seed-daemon --version`
```

Same public artifact repo, same "installed version is the source of truth" rule (the updater
reads `seed-daemon --version` and compares to the latest release tag), same **mandatory Cargo
version bump per release**. Asset name convention: **`seed-sync-<ver>-macos-universal.tar.gz`**.

## Files built (`packaging/macos/` + `scripts/`)

| File | Purpose |
|---|---|
| `scripts/setup-conda-macos.sh` ✅ | Creates the conda-forge GTK env(s) the packager bundles from — `osx-arm64` (and `osx-64` with `--universal`) at `.conda-gtk/`. conda-forge's macOS-11-SDK builds are what set the floor at macOS 11 regardless of the build host. Needs conda/mamba/micromamba (miniforge). |
| `scripts/package-macos.sh` ✅ | The macOS analog of `scripts/package-linux.sh`: `cargo build --release` (pkg-config → conda env, `MACOSX_DEPLOYMENT_TARGET=11.0`), build the **`SEED Sync.app`** (binaries → `Contents/MacOS`), run the bundler over `Contents/`, write `Info.plist` + `Resources/AppIcon.icns` (sips + iconutil from `icon/appIcon.png`), seal the bundle, tar `dist/seed-sync-<ver>-macos-<arch>.tar.gz`. `--skip-build` to repackage. Builds universal when the `osx-64` conda env is present, else arm64. |
| `scripts/bundle-gtk-macos.sh` ✅ | The hard part: walk the `seed-gui` otool closure + the gdk-pixbuf/librsvg loader modules, copy every non-system dylib into `lib/`, rewrite install names to `@executable_path/../lib` (handles `@rpath/*`, `@loader_path/*`, and absolute-prefix refs), regenerate `loaders.cache`, compile GSettings schemas, bundle the fontconfig config, **ad-hoc re-sign** inside-out. Source-agnostic via `BUNDLE_PREFIX` (a conda env; or `BUNDLE_BREW`/`brew --prefix` for Homebrew). `BUNDLE_BINDIR=MacOS` targets a `.app`'s `Contents/`; `BUNDLE_SKIP_AUX=1` does the dylib closure only (universal x86_64 pass). No icon theme needed (GTK4 embeds its icons). |
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

1. **Build** for `aarch64-apple-darwin` (and later `x86_64-apple-darwin`) against the conda-forge GTK
   env (pkg-config → `$ENV/lib/pkgconfig`; `MACOSX_DEPLOYMENT_TARGET=11.0`).
2. **Walk the dylib closure** of `seed-gui` (`otool -L`, recursively) and copy every non-system
   dylib (everything under the env prefix / `@rpath` / `@loader_path`; skip `/usr/lib`, `/System/...`)
   into `lib/`.
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

`package-macos.sh` builds universal when the `osx-64` conda env + the x86_64 Rust target are present
(else it falls back to arm64-only). It builds both Rust slices, bundles each arch's GTK closure
separately, then `lipo`s every Mach-O (binaries + dylibs + pixbuf loaders) into the arm64 `.app` and
re-signs inside-out (lipo invalidates the ad-hoc signature).

- **Two conda envs** — `osx-arm64` and `osx-64`, both from conda-forge (`scripts/setup-conda-macos.sh
  --universal`). Use the **same GTK version** in both so the dylib sets match for `lipo` (conda-forge
  resolves the same latest build for each subdir). No Rosetta or sudo needed to *create* the envs.
- **x86_64 cross-build pkg-config:** `PKG_CONFIG_LIBDIR=$X86_ENV/lib/pkgconfig` (replaces the default
  search → no arm64 leak) + `PKG_CONFIG_ALLOW_CROSS=1`. conda keeps every `.pc` in one
  `lib/pkgconfig`, so there's no keg-only fan-out or system-stub dir to assemble (unlike Homebrew).
- **x86_64 bundle pass** runs with `BUNDLE_SKIP_AUX=1`: `loaders.cache`, compiled schemas, and
  fontconfig are arch-independent and already written by the arm64 pass, so the x86_64 pass only walks
  + relocates the dylib closure for the `lipo` — and never needs to run x86_64 tools under Rosetta.

## Building a release (manual, current method)

Releases are cut manually with the conda-forge pipeline. One-time per machine: install miniforge
(or any conda/mamba/micromamba) — https://github.com/conda-forge/miniforge.

```sh
# 1. Create the GTK env(s). arm64-only:
scripts/setup-conda-macos.sh
#    …or universal (arm64 + Intel) — also needs: rustup target add x86_64-apple-darwin
scripts/setup-conda-macos.sh --universal

# 2. Build + bundle + tarball → dist/seed-sync-<ver>-macos-{arm64,universal}.tar.gz
scripts/package-macos.sh

# 3. Confirm the floor really dropped (expect: minos 11.0)
otool -l "dist/seed-sync-"*"-macos-"*/"SEED Sync.app/Contents/lib/libgtk-4."*.dylib \
  | grep -A3 LC_BUILD_VERSION
```

Env locations default to `.conda-gtk/{arm64,x86}` (gitignored); override with `SEED_CONDA_ARM` /
`SEED_CONDA_X86`. The build works on any macOS the dev box happens to run — the floor comes from
conda-forge's SDK, not this machine. **Verified end-to-end** on a macOS 26 box: arm64 bundle, all
GTK/libadwaita/glib + `seed-gui` at `minos 11.0`, leak-free, and launches under a `sandbox-exec` that
denies both `/opt/homebrew` and the conda env.

### conda env composition — why `setup-conda-macos.sh` installs more than gtk4
conda-forge splits "runtime" from "dev" more aggressively than Homebrew, so a bare `gtk4 libadwaita`
env can't *build* against it. The setup script adds what the Rust `*-sys` crates' `pkg-config` step and
the linker need (all discovered the hard way — keep them):
- **`zlib`, `freetype`, `expat`** — gtk4 pulls only the runtime libs (`libzlib`/`libfreetype`/…); the
  `.pc` files (referenced via `Requires.private` of gio/harfbuzz/fontconfig) live in these dev packages.
- **`libintl-devel`** — provides the unversioned `libintl.dylib` symlink the linker needs for the
  `-lintl` glib's `.pc` emits (the env otherwise has only `libintl.8.dylib`).
- **synthesized `libxml-2.0.pc`** — conda-forge's `libxml2` ≥ 2.14 ships *no* `.pc` (and no headers);
  `appstream` (pulled by libadwaita) lists `libxml-2.0` in `Requires.private`, so `pkg-config` errors on
  the libadwaita probe. libxml-2.0 is never linked directly, so the script writes a minimal stub rather
  than pinning the whole stack back to libxml2 2.13 (which drags gtk4 down to 4.14 + icu/zlib conflicts).

And in `package-macos.sh`: **`RUSTFLAGS=-C link-arg=-Wl,-headerpad_max_install_names`** — conda dylibs
use short `@rpath/<name>` install names, so rewriting `seed-gui`'s load commands to the longer
`@executable_path/../lib/<name>` overflows a stock Mach-O header (`install_name_tool: load commands do
not fit`). Homebrew's long absolute paths happened to shrink on rewrite, hiding this.

## No CI — releases are built locally

There is no GitHub Actions release job (the `.github/workflows/*.yml` were removed). Every macOS
release is cut **manually** on a Mac with the conda-forge pipeline (above) and published with `gh`
(see [`releasing.md`](releasing.md)). conda-forge sourcing is what makes this practical: the floor is
macOS 11 regardless of the build host, so any Mac — including the maintainer's current dev box — can
cut a shippable universal build with no hosted runner.

**If CI is ever reintroduced**, the same pipeline runs on any `macos-*` runner with no runner-version
pin and no Rosetta/second-Homebrew dance: install miniforge, `scripts/setup-conda-macos.sh
--universal`, add the `x86_64-apple-darwin` Rust target, then `scripts/package-macos.sh`.

### Minimum macOS version is set by where the GTK dylibs come from — we use conda-forge (floor = macOS 11)
The bundle's real floor is the `minos` (LC_BUILD_VERSION) of the **bundled GTK dylibs**. Our Rust
binaries are low (we pin `MACOSX_DEPLOYMENT_TARGET=11.0`), so GTK dominates — and *who built the GTK
dylibs* decides the floor:

| GTK source | arm64 GTK `minos` | x86_64 GTK `minos` | Effective floor |
|---|---|---|---|
| **conda-forge** (what we ship) | **11** | ~10.13 | **macOS 11** (Big Sur) |
| Homebrew on a `macos-14` runner | 14 | 13–14 | macOS 14 |
| Homebrew on a `macos-15` runner | 15 | 14–15 | macOS 15 |
| Homebrew on a macOS 26 dev box | 26 | 14 | macOS 26 (unshippable) |

**Why this matters:** Homebrew stamps each bottle with the *build host's* OS and won't ship
lower-targeted bottles, so a Homebrew build is hostage to the build machine — cutting on a modern dev
box would force a floor of that machine's OS (the v1.1.0 macOS-26 incident). The old plan worked around
this by always building on `macos-14`, GitHub's *oldest* Apple-Silicon runner.

**conda-forge breaks that coupling.** It builds `osx-arm64` packages against the macOS 11.0 SDK (Big
Sur — the floor for *all* Apple Silicon) and `osx-64` against ~10.13, independent of the build host.
So sourcing the GTK closure from conda-forge envs lets us cut a **macOS 11 floor on any Mac, including
the macOS 26 dev box** — no old hardware, no hosted CI runner. macOS 11 is the absolute floor on Apple
Silicon anyway (no Apple Silicon Mac runs anything older), and covers Intel back to High Sierra. (On
Apple Silicon dyld always loads the arm64 slice, so the arm64 `minos` gates those machines; the x86_64
floor only affects Intel Macs.)

**How:** `scripts/setup-conda-macos.sh` creates the env(s) from conda-forge; `scripts/package-macos.sh`
sources the closure from them (via `BUNDLE_PREFIX`). See "Building the release" below. The bundler
(`scripts/bundle-gtk-macos.sh`) is source-agnostic — it still accepts a Homebrew prefix
(`BUNDLE_BREW`/`brew --prefix`) if you ever want a Homebrew-sourced bundle.

**Verify the floor after building:**
`otool -l "<app>/Contents/lib/libgtk-4."*.dylib | grep -A3 LC_BUILD_VERSION` → expect `minos 11.0`
on the arm64 slice (use `lipo -thin x86_64` first to check the Intel slice).

## Caveats / gotchas

- **`launchctl bootout` is ASYNCHRONOUS — an update must wait for the daemon to
  actually die.** It returns before launchd has reaped the job, and the daemon is
  `KeepAlive`. The original updater booted the daemon out, immediately replaced the
  `.app` underneath the still-live process, and then `bootstrap`ed the replacement —
  which fails with launchd error 37 ("Operation already in progress") while the old
  job lingers. Under the wrapper's `set -e` that aborted `cmd_update` outright,
  leaving new files on disk and a **stale daemon** still serving the old binary. The
  symptom is nasty because it looks fine: the GUI connects and reports healthy while
  the node is silently absent from every peer, and only a manual
  `launchctl kickstart -k` fixes it. `seed-sync` now has `stop_wait` (poll until the
  process is really gone, escalating to SIGKILL) and `restart_runtime` (bring the
  daemon back, **verify** a live pid, fall back to `kickstart -k`). Quitting and
  relaunching the GUI does NOT help — the stale process is the daemon.
- **The tray GUI must be cycled on update too**, on every OS. It keeps running the
  old binary otherwise. `restart_runtime` relaunches it `--hidden` if it was running
  — keyed off the *process* as well as the agent, since a GUI started by hand from
  the Dock has no agent loaded and would otherwise be killed and never come back.
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
- **Register with LaunchServices after copying the `.app`.** A `cp`/`tar` install (unlike a Finder
  drag) never notifies LaunchServices, so the bundle won't appear in Spotlight / Launchpad / "Open
  With" until it's registered. `apply_tree` runs `lsregister -f "<app>"`
  (`…/LaunchServices.framework/…/Support/lsregister`) after the swap. Existing installs pick it up on
  the next `seed-sync --update`; to fix one in place, run that `lsregister -f` on the app by hand.
- **Version bump is mandatory per release** (updater is version-driven; bump the Cargo version before building or installs never see it).

## Future work (not built)

- **Unified hosted bootstrap** — mirror `packaging/web-install.sh` (built, OS-detecting) to
  `steeb-k.github.io/seed-install.sh`, replacing the Linux-only one.
- Developer ID signing + **notarization** (clean install from any source, incl. a future `.dmg`) —
  needs an Apple Developer account ($99/yr). Keeps ad-hoc as the default; notarize on top.
- A `.dmg` for drag-to-Applications (would need notarization; the `.app` already exists).
- Sparkle-style in-app updates (vs the launchd timer).

See also: `docs/linux-packaging.md` (the model this mirrors), `docs/windows-packaging.md` (the MSI
side), and the macOS bring-up checklist + sync matrix in `docs/cross-os-testing.md`.
