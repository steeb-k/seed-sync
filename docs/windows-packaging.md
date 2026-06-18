# Windows: build, bundle, package, service (M4)

This is the Windows-validation milestone. The code is written and CI
compile-checks the Windows build; the steps below are run/validated on a real
Windows machine. Primary development moves here per the project plan.

## 0. One-time dev setup

1. Install **rustup** with the MSVC toolchain: `rustup default stable-msvc`.
2. Install **Visual Studio Build Tools** (MSVC C++), Git, and the `gh` CLI.
3. Install **gvsbuild** and build the GTK stack (this takes a while):
   ```pwsh
   py -m pip install --user pipx; py -m pipx ensurepath
   pipx install gvsbuild
   gvsbuild build gtk4 libadwaita
   # output lands in C:\gtk by default
   ```
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

### Open questions to validate here (flagged in the code)
- **Service account & IPC reachability.** The service installs as **LocalSystem**;
  the GUI runs as the logged-in user. Confirm the user can reach the daemon's IPC
  endpoint. Options: run the service as the user, or set a permissive DACL on the
  pipe. Decide and implement.
- **Named-pipe naming.** The transport currently builds the socket name via
  interprocess `GenericFilePath` (correct for Unix domain sockets). On Windows a
  named pipe wants `\\.\pipe\...`; verify `GenericFilePath` works or switch to
  `GenericNamespaced` on Windows in `crates/seed-ipc/src/transport.rs`.
- **Data dir under LocalSystem.** `directories` resolves the data dir to the
  service account's profile; ensure the GUI (user) and service agree on the socket
  path, or pass an explicit machine-wide `--data-dir` (e.g. `%PROGRAMDATA%\SeedSync`).

## 3. MSI installer (cargo-wix)

Use **cargo-wix** (WiX Toolset) rather than a hand-written `.wxs`:

```pwsh
cargo install cargo-wix
cargo wix init          # generates wix\main.wxs from Cargo.toml metadata
# then customize wix\main.wxs to:
#   - install the whole dist\SeedSync tree (use `heat` to harvest the GTK runtime)
#   - add a Start-menu + desktop shortcut to seed-gui.exe
#   - register the service via a custom action running `seed-daemon.exe install`
#     on install and `uninstall` on remove (or a ServiceInstall element)
cargo wix                # builds target\wix\seed-sync-<ver>.msi
```

### Code signing (avoid SmartScreen)
Sign every exe + DLL + the MSI with an OV/EV certificate:
```pwsh
signtool sign /fd SHA256 /a /tr http://timestamp.digicert.com /td SHA256 <file>
```
cargo-wix can invoke SignTool on the MSI; sign the bundled exes before harvesting.

## 4. Checkpoint #3 (end-to-end)

1. Install the MSI on a Windows box; confirm the service auto-starts.
2. Launch the GUI; create a share.
3. On the Linux dev box (`cargo run -p seed-daemon -- run` + `seed-cli`), add the
   share via the key (no bootstrap — discovery) and confirm it syncs **Windows ↔
   Linux** over real iroh (hole-punch + relay fallback).
