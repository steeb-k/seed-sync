# Windows: build, bundle, package, service (M4)

This is the Windows-validation milestone. The steps below are run/validated on a
real Windows machine — the MSI is built and signed locally (there is no CI).
Primary development happens here per the project plan.

## 0. One-time dev setup

1. Install **rustup** with the MSVC toolchain: `rustup default stable-msvc`
   (Rust ≥ 1.94 — `libsqlite3-sys`'s bundled Windows build uses `cfg_select!`).
2. Install **Visual Studio Build Tools** (MSVC C++), Git, and the `gh` CLI.
3. Install the GTK stack from the **official prebuilt gvsbuild bundle** — do NOT
   build from source. `gvsbuild build gtk4 libadwaita` reliably fails on gettext
   (and needs a full VS C++ toolchain its detector can't always find), so we use
   the prebuilt release zip, which already includes libadwaita:
   ```pwsh
   $ver = "2026.4.1"   # github.com/wingtk/gvsbuild/releases
   curl.exe -L -o gtk.zip "https://github.com/wingtk/gvsbuild/releases/download/$ver/GTK4_Gvsbuild_${ver}_x64.zip"
   mkdir C:\gtk; tar -xf gtk.zip -C C:\gtk   # gives C:\gtk\{bin,lib,include,share}
   ```
   (Pin the gvsbuild version; bump it from the wingtk/gvsbuild releases page.)
4. Point the gtk-rs build at it:
   ```pwsh
   $env:PKG_CONFIG_PATH = "C:\gtk\lib\pkgconfig"
   $env:PATH = "C:\gtk\bin;$env:PATH"
   $env:LIB = "C:\gtk\lib;$env:LIB"
   ```
5. `git clone https://github.com/steeb-k/seed-sync-gtk` and `cargo build --release`.

## 1. Build + bundle the portable tree

```pwsh
cargo build --release
pwsh -File scripts\bundle-gtk-windows.ps1 -GtkRoot C:\gtk
# -> dist\SeedSync\  (bin\*.exe + GTK DLLs + schemas + pixbuf loaders + icons)
```
The bundle script copies the runtime pieces GTK needs at startup and rebuilds the
three caches (`gschemas.compiled`, pixbuf `loaders.cache`, icon-theme cache).
**Trim aggressively** if size matters — only what GTK4/libadwaita needs.

Smoke test: run `dist\SeedSync\bin\seed-daemon.exe run` in one terminal and
`dist\SeedSync\bin\seed-gui.exe` in another; the window should appear and connect.

## 1b. Windows on ARM (ARM64), cross-built from x86_64

```pwsh
pwsh -File scripts\build-msi.ps1 -Arch arm64   # -> seed-sync-<ver>-windows-arm64.msi
```

The ARM64 build is split across **two ABIs**, which looks odd and isn't negotiable:

| | target | why |
|---|---|---|
| daemon, CLI | `aarch64-pc-windows-msvc` | no GTK dependency at all — they just cross-compile |
| GUI | `aarch64-pc-windows-gnullvm` | must match the ABI of the only ARM64 GTK that exists |

**Where ARM64 GTK comes from.** gvsbuild — the source of our x86_64 `C:\gtk` — is x64-only, and
vcpkg's `gtk` port explicitly excludes the platform (`"supports": "… & !(arm64 & windows)"`). The
only prebuilt GTK4 + libadwaita for Windows on ARM is **MSYS2's CLANGARM64** repo, and those are
mingw-ABI, which forces the GUI onto the `gnullvm` target. The mixed ABI costs nothing: the GUI and
the daemon are separate processes that only meet over IPC, so no ABI boundary is ever crossed
inside a process.

`scripts\fetch-gtk-msys2.ps1` resolves the dependency closure straight from the MSYS2 package
database and unpacks the `.pkg.tar.zst` archives — no MSYS2 or pacman install required, which is
what keeps the whole thing cross-buildable on the x86_64 box.

One-time setup:
```pwsh
rustup target add aarch64-pc-windows-msvc aarch64-pc-windows-gnullvm
# VS installer: "MSVC v143 - VS 2022 C++ ARM64/ARM64EC build tools" + the ARM64 Windows SDK
# llvm-mingw (ucrt-x86_64) from https://github.com/mstorsjo/llvm-mingw/releases -> C:\llvm-mingw-*
pwsh -File scripts\fetch-gtk-msys2.ps1                                  # -> C:\gtk-arm64
pwsh -File scripts\fetch-gtk-msys2.ps1 -Env ucrt64 -Root C:\gtk-msys2-x64
```
That second fetch is the **host-tools mirror**, and it is not optional. The GTK helper tools in the
ARM64 tree (`glib-compile-schemas`, `gdk-pixbuf-query-loaders`, `gtk4-update-icon-cache`) are ARM64
binaries and cannot run on the build host — and `gdk-pixbuf-query-loaders` in particular
`dlopen()`s each loader, so it can only ever query loaders of its own architecture. Their output is
architecture-independent, so we run the **x86_64 build of the very same MSYS2 packages** to generate
it; the bundler asserts the two trees' loader sets match before trusting the cache.

Gotchas, all of which cost real time to find:
- **`ring` needs `clang` on `PATH`** for either aarch64 target — the only dependency that does. The
  two passes need *different* clangs: LLVM's (which finds the MSVC/SDK headers) for the msvc pass,
  llvm-mingw's (which brings a mingw sysroot) for the gnullvm pass. `build-arm64.ps1` sets `PATH`
  per pass; one shared ordering breaks one of them with `'assert.h' file not found`.
- **`winresource` emits an x64 resource object when cross-compiling to ARM64** — it passes no
  `--target` to `windres` for aarch64, so an unprefixed `windres` defaults to x86-64 and the link
  dies with `machine type x64 conflicts with arm64`. `crates/seed-gui/build.rs` pins
  `aarch64-w64-mingw32-windres` when the *target* is aarch64.
- **Don't copy `*.dll` wholesale out of the MSYS2 tree.** Unlike gvsbuild's purpose-built GTK
  prefix, MSYS2's `bin\` is a shared prefix for every package in the closure — a blanket copy ships
  `libpython3.14.dll` and friends. The bundler walks the import tables from our binaries (plus the
  pixbuf loaders, which GTK `dlopen`s rather than imports) and copies only that closure.

Since the ARM64 bundle can't be launched on the build host, `scripts\verify-bundle.ps1` reads the PE
header of every binary in it and checks that they are all ARM64 and that every imported DLL is
either bundled or a system DLL. It runs automatically at the end of every bundle, for both
architectures.

## 2. The Windows service

The daemon is one binary; `service` mode is entered by the SCM, and
install/uninstall/start/stop are subcommands (see `crates/seed-daemon/src/service.rs`).

> **Updating cycles the service *and* the tray.** The MSI's `ServiceControl`
> (`Stop="both"` / `Start="install"`) restarts `SeedSyncDaemon` on upgrade, but nothing
> restarted the per-user tray GUI: it kept running the **old** `seed-gui.exe`, and while
> running it holds a **lock on the very file msiexec is replacing** — which can demote a
> clean upgrade into a reboot-pending `3010`. `seed-sync-update.ps1` now stops the tray
> *before* `msiexec`, verifies the service is actually `Running` afterwards
> (`Assert-DaemonRunning`), and relaunches the tray `--hidden`. Because the daily update
> task runs as **SYSTEM**, it cannot `Start-Process` the GUI directly — that would land it
> in session 0 with no visible tray — so `Restart-Tray` hands it to the interactive user
> via a one-shot scheduled task (and just launches it directly when a human runs the
> updater by hand).

```pwsh
# from an elevated prompt:
seed-daemon.exe install      # registers "SeedSyncDaemon" (auto-start, LocalSystem)
seed-daemon.exe start
seed-daemon.exe stop
seed-daemon.exe uninstall
```

### Open questions — resolved (commit `0638ade`)
The service stays **LocalSystem**; the GUI/CLI run as the user. They meet via:
- **Machine-wide socket.** Both sides default to `%PROGRAMDATA%\SeedSync\seed.sock`
  on Windows (`seed_ipc::machine_data_dir`/`machine_socket`, used by the daemon's
  `default_data_dir` and the GUI's `default_socket`) — same path ⇒ same pipe name.
- **Permissive pipe DACL.** `transport::bind` creates the pipe with an SDDL DACL
  (`D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;AU)`): SYSTEM/Admins full, Authenticated
  Users read/write (connect + IO, not create-instance). Lets the user GUI open the
  service's pipe. Verified: daemon started with no args is reachable via
  `seed-cli --socket %PROGRAMDATA%\SeedSync\seed.sock list`.
- **Named-pipe naming.** ✅ Resolved (commit `368382b`): `socket_name(path)` uses
  `GenericFilePath` on Unix, `GenericNamespaced` (hashed path) on Windows.
- **Keystore note.** Seeds the daemon stores live in the **service account's**
  Credential Manager vault (SYSTEM); only the daemon needs them, so that's fine.
  A daemon run in *console* mode (as the user) uses the user vault instead — so
  shares created in console mode aren't visible to the service and vice-versa.

Still to validate live: install/start the service from an **elevated** prompt and
confirm the user-run GUI connects (cross-account, the real test vs. same-user CLI).

## 3. MSI installer (WiX 5)

Built with **WiX v5** (the `dotnet tool`), not cargo-wix: cargo-wix wraps the
EOL WiX v3, while WiX 4+ has the `<Files>` harvesting element (no `heat`) and the
v6/v7 builds gate behind the Open-Source-Maintenance-Fee EULA — v5 has neither
problem. Authoring lives in `wix\seedsync.wxs`.

```pwsh
dotnet tool install --global wix --version "5.*"   # one-time
pwsh -File scripts\build-msi.ps1                    # build + bundle + sign + wix build + sign MSI
#   -> target\wix\seed-sync-<version>-windows-x86_64.msi
```

`build-msi.ps1` needs the **UI + Util** WiX extensions; it adds them automatically
(`wix extension add -g WixToolset.UI.wixext WixToolset.Util.wixext`) on first run.
The **version is single-sourced** from `Cargo.toml`'s `[workspace.package].version`
(override with `-Version`), and the artifact name matches the Linux convention
(`seed-sync-<version>-windows-x86_64.msi`) so both OSes' assets sit on the same
release.

`wix\seedsync.wxs` (per-machine, x64):
- Installs the whole `dist\SeedSync` tree into `C:\Program Files\SeedSync`
  (`bin\*.dll` globbed via `<Files>`; `share\`/`lib\` harvested; the four exes +
  the updater script explicit so the daemon can carry the service).
- Registers **SeedSyncDaemon** as a LocalSystem, auto-start service via
  `<ServiceInstall>` + `<ServiceControl>` (`Arguments="service"`, matching
  `service.rs`); MSI starts it on install and waits for it to stop on
  uninstall/upgrade before removing files.
- **WixUI_Minimal** UI: a single Welcome+License page (shows the GPL-3.0 license,
  generated to `wix\license.rtf` from the repo `LICENSE` at build time), then
  progress + finish. No folder chooser.
- Registers the **SeedSyncUpdate** scheduled task (daily auto-update, §3.2) via a
  deferred `util:QuietExec64` custom action that runs the bundled updater script as
  SYSTEM; the task is removed on a real uninstall but survives version upgrades.
- Start-menu + desktop shortcuts to `seed-gui.exe`; `MajorUpgrade`
  (`AllowSameVersionUpgrades`) for in-place upgrades — also what the silent updater
  relies on.

The daemon's data still lives machine-wide in `%PROGRAMDATA%\SeedSync` regardless
of the Program Files install location (see §2).

### 3.1 Code signing (Azure Trusted Signing)
`build-msi.ps1` signs **our three exes** (before `wix build`, since they're embedded
in the MSI) and then the **MSI** itself, via `scripts\sign-artifacts.ps1` — a
`signtool … /dlib Azure.CodeSigning.Dlib.dll /dmdf <metadata.json>` wrapper
(timestamp `http://timestamp.acs.microsoft.com`). The ~third-party GTK DLLs are left
unsigned (standard practice; SmartScreen keys off the MSI + the launched exe).

**Releases are signed by default.** The signing metadata is kept at repo-root
`artifact-signing-metadata.json` (git-ignored), which is the path `sign-artifacts.ps1`
looks for automatically — so a release `build-msi.ps1` signs the three exes + the MSI
with no extra flags. **Do not publish an unsigned MSI as a release** (SmartScreen will
warn users). Signing requires an authenticated Azure session for the Trusted Signing
account (`az login`, or service-principal env vars) in addition to the metadata.

Signing is technically skippable for **local testing / other contributors**: if the
metadata JSON is absent (and `$env:ARTIFACT_SIGNING_METADATA` unset), the build still
succeeds and produces unsigned artifacts. The metadata is
`{ Endpoint, CodeSigningAccountName, CertificateProfileName }`; tools auto-resolve
from the Windows SDK + Trusted Signing client tools (override with `SIGNTOOL_PATH` /
`ARTIFACT_SIGNING_DLIB`).

### 3.2 Auto-update (the Windows analog of the Linux timer)
Distribution mirrors Linux: one **GitHub Release per `vX.Y.Z` tag** on the public
`steeb-k/seed-sync-binaries` repo carries both OSes' assets. The Windows half is
built + signed **locally** and attached to the Linux-created release:

```pwsh
pwsh -File scripts\build-msi.ps1                    # -> signed MSI
pwsh -File scripts\publish-msi.ps1                  # gh release upload to seed-sync-binaries vX.Y.Z
```

`packaging\windows\seed-sync-update.ps1` (installed to `…\SeedSync\bin`) is the
update engine, run as SYSTEM by the **SeedSyncUpdate** scheduled task (daily +
shortly after boot). It compares `seed-daemon.exe --version` to the latest release
tag and, when newer, downloads the `*windows-x86_64.msi` asset and applies it with
`msiexec /i … /qn` — the MSI's `MajorUpgrade` stops the service, swaps files
(GTK DLLs included), and restarts it. Modes: default = check+apply, `-Check` =
report only, `-RegisterTask` / `-UnregisterTask` = used by the MSI. Logs to
`%PROGRAMDATA%\SeedSync\update.log`.

To cut a release: bump `[workspace.package].version` in `Cargo.toml`, run
`cargo update --workspace`, commit. Then build, sign, and publish the MSI **locally**
(there is no CI):

```pwsh
cargo build --release
az login                                  # the signer-role account; see below
pwsh -File scripts\build-msi.ps1 -SkipBuild   # bundle + sign exes + wix + sign MSI
# -> target\wix\seed-sync-<ver>-windows-x86_64.msi
pwsh -File scripts\publish-msi.ps1            # or `gh release upload …` (see releasing.md)
```

### 3.3 Local signing (Azure Artifact Signing)
`build-msi.ps1` signs via `sign-artifacts.ps1` + the committed
`artifact-signing-metadata.json` — the cert is never stored locally. Signing
authenticates to Azure through your **interactive `az login`** session; the
`Azure.CodeSigning.Dlib` (from the `Microsoft.ArtifactSigning.Client` NuGet) signs
remotely against the account/profile named in the metadata. The
`ExcludeCredentials` list in that metadata pins the dlib to `AzureCliCredential`, so
it uses the `az` session instead of probing IMDS (which would otherwise hang).

**Requirements on the build machine:**
1. An Azure account holding the **"Artifact Signing Certificate Profile Signer"** role
   (formerly "Trusted Signing …") on the signing account / cert profile.
2. The `Microsoft.ArtifactSigning.Client` dlib installed and discoverable (point
   `ARTIFACT_SIGNING_DLIB` at `Azure.CodeSigning.Dlib.dll` if it isn't auto-found).
3. `az login` as that account before running `build-msi.ps1`.

To produce an **unsigned** MSI for a quick local test, point
`ARTIFACT_SIGNING_METADATA` at a nonexistent path.

## 4. Checkpoint #3 (end-to-end)

1. Install the MSI on a Windows box; confirm the service auto-starts.
2. Launch the GUI; create a share.
3. On the Linux dev box (`cargo run -p seed-daemon -- run` + `seed-cli`), add the
   share via the key (no bootstrap — discovery) and confirm it syncs **Windows ↔
   Linux** over real iroh (hole-punch + relay fallback).
