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
