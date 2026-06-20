# Windows: build, bundle, package, service (M4)

This is the Windows-validation milestone. The code is written and CI
compile-checks the Windows build; the steps below are run/validated on a real
Windows machine. Primary development moves here per the project plan.

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
   The release CI does exactly this (`.github/workflows/release.yml`).
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

## 2. The Windows service

The daemon is one binary; `service` mode is entered by the SCM, and
install/uninstall/start/stop are subcommands (see `crates/seed-daemon/src/service.rs`).

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

To cut a release: bump `[workspace.package].version` in `Cargo.toml`, commit, then
`git tag vX.Y.Z && git push origin vX.Y.Z`. The `release.yml` **`windows` job** now
builds + signs + publishes the MSI autonomously alongside Linux + macOS (no manual
`build-msi.ps1` / `publish-msi.ps1` run needed) once the one-time Azure setup below
is in place — those two scripts remain for local builds and as a fallback.

### 3.3 Autonomous CI signing (Azure Trusted Signing via OIDC)
The `windows` job in `release.yml` builds GTK with **gvsbuild** (cached at `C:\gtk`),
installs WiX 5, then signs via the **same `sign-artifacts.ps1` + committed
`artifact-signing-metadata.json`** — the cert never touches CI. The runner
authenticates to Azure as a **service principal over OIDC** (`azure/login@v2`); the
`Azure.CodeSigning.Dlib` (installed from the `Microsoft.Trusted.Signing.Client` NuGet)
signs remotely against the account/profile named in the metadata.

**One-time maintainer setup** (without it, `azure/login` fails — but the Linux/macOS
assets still publish, since the jobs are independent):
1. Register an **Azure AD app** (service principal).
2. Grant it the **"Trusted Signing Certificate Profile Signer"** role on the Trusted
   Signing account (or scoped to the certificate profile).
3. Add an **OIDC federated credential** on the app for this repo — subject e.g.
   `repo:steeb-k/seed-sync-gtk:ref:refs/tags/v*`.
4. Add repo secrets: **`AZURE_CLIENT_ID`**, **`AZURE_TENANT_ID`**, **`AZURE_SUBSCRIPTION_ID`**
   (`SEED_BINARIES_TOKEN` already exists for publishing).

Status: **wired but unvalidated on a runner** — the gvsbuild build and the dlib
install want a first-tag shakeout. If the dlib/auth path misbehaves, the alternative
is the purpose-built `azure/artifact-signing-action@v2` (would mean splitting
`build-msi.ps1` so the action signs the exes before `wix build` and the MSI after).

## 4. Checkpoint #3 (end-to-end)

1. Install the MSI on a Windows box; confirm the service auto-starts.
2. Launch the GUI; create a share.
3. On the Linux dev box (`cargo run -p seed-daemon -- run` + `seed-cli`), add the
   share via the key (no bootstrap — discovery) and confirm it syncs **Windows ↔
   Linux** over real iroh (hole-punch + relay fallback).
