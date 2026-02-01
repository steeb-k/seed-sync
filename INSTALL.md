# S.E.E.D. Installation Guide

This guide covers installing and running S.E.E.D. (Secure Environment Exchange Daemon) on Windows.

## Prerequisites

- **Windows 10** (build 19041 or later) or **Windows 11**
- **.NET 8.0 Runtime** (for Daemon and CLI) — required only if using framework-dependent builds
- **.NET 10.0 Runtime** (for WinUI App) — required only if using framework-dependent builds

If you use the **self-contained** ZIP package from releases, no .NET runtime needs to be installed separately.

## Installation Options

### Option 1: ZIP Package (Recommended for testing)

1. Download the latest `SeedSync-*-win-x64.zip` from [Releases](https://github.com/YOUR_ORG/seed-sync/releases).

2. Extract the ZIP to a folder, e.g. `C:\Program Files\SeedSync` or your user folder.

3. The extracted folder contains:
   - **App/** — WinUI desktop application (run `SeedSync.App.exe`)
   - **Daemon/** — Background sync service (run `SeedSync.Daemon.exe`)
   - **Cli/** — Command-line interface (run `SeedSync.Cli.exe`)
   - **README.md**, **INSTALL.md**, **LICENSE**

### Option 2: Build from source

1. Clone the repository:
   ```powershell
   git clone https://github.com/YOUR_ORG/seed-sync.git
   cd seed-sync
   ```

2. Install prerequisites:
   - [.NET 8.0 SDK](https://dotnet.microsoft.com/download/dotnet/8.0)
   - [.NET 10.0 SDK](https://dotnet.microsoft.com/download/dotnet/10.0) (for the WinUI app)

3. Build:
   ```powershell
   dotnet build
   ```

4. Run:
   - **Daemon:** `dotnet run --project src/SeedSync.Daemon`
   - **CLI:** `dotnet run --project src/SeedSync.Cli -- <command>`
   - **App:** `dotnet run --project src/SeedSync.App`

## First-Run Setup

### 1. Start the daemon

The daemon must be running for sync to work. You can either:

- **Run manually:** Start `SeedSync.Daemon.exe` (or `dotnet run --project src/SeedSync.Daemon`). It listens on `http://127.0.0.1:9876`.

- **Use the GUI:** The WinUI app can start the daemon for you if configured (or run the daemon yourself in the background).

### 2. Create or add a share

**Using the GUI:**

1. Run `SeedSync.App.exe`.
2. Click the **+** button.
3. Choose **Create new share** or **Add existing share**.
4. Follow the prompts (folder path, key if adding).

**Using the CLI:**

```powershell
# Create a new share (you get RW and RO keys)
.\Cli\SeedSync.Cli.exe create "C:\path\to\folder"

# Add an existing share with a key
.\Cli\SeedSync.Cli.exe add "SEEDRO..." "C:\path\to\save"

# List shares
.\Cli\SeedSync.Cli.exe list

# Check status
.\Cli\SeedSync.Cli.exe status
```

### 3. Sync between machines

- On **Machine A:** Create a share and note the **Read-Only (RO)** key.
- On **Machine B:** Add the share using the RO key and a local folder path.
- Ensure both machines can reach each other (same network; firewall may need to allow the app).

## Data Locations

- **Share/torrent data:** Stored under the daemon’s working directory (e.g. `%LOCALAPPDATA%\SeedSync` or the folder from which you run the daemon).
- **GUI window state:** Handled by the WinUI app (window position/size are not persisted in the current version).

## Troubleshooting

### "Cannot connect to S.E.E.D. daemon"

- Ensure the daemon is running (`SeedSync.Daemon.exe` or `dotnet run --project src/SeedSync.Daemon`).
- Check that nothing else is using port **9876**: `netstat -an | findstr 9876`.
- The GUI and CLI use `http://127.0.0.1:9876` by default.

### Peers not connecting / files not syncing

- **Firewall:** Allow the daemon and app through Windows Firewall (private networks).
- **Network:** Both sides must be able to accept incoming connections (no symmetric NAT issues).
- **Status:** Run `SeedSync.Cli.exe status` and check that the share shows **Syncing** or **UpToDate** and that **ConnectedPeers** is greater than 0 when another peer is running.

### GUI does not start

- Install the **.NET 10.0 runtime** (or SDK) if you’re running a framework-dependent build.
- For a self-contained build, ensure you run the exe from the **App** folder inside the extracted ZIP.
- Check [Windows version](https://docs.microsoft.com/en-us/windows/apps/winui/winui3/#windows-10-version-1809-and-later) and that all Visual C++ redistributables are installed if the installer or docs require them.

### Tray icon not visible

- Open **Settings → Personalization → Taskbar → Other system tray icons** and enable **S.E.E.D.** (or the name used by the app).
- Restart the app after changing this.

### Quit from tray does not close the app

- Use **Quit** from the tray context menu. If the process still stays running, close it from Task Manager and consider reporting the issue (include Windows and app version).

## Uninstall

- **ZIP install:** Delete the folder where you extracted SeedSync and, if desired, remove `%LOCALAPPDATA%\SeedSync` (or wherever the daemon stored data).
- **MSIX install:** Use **Settings → Apps → SeedSync.App → Uninstall**.

## Support

- **Bugs and features:** [GitHub Issues](https://github.com/YOUR_ORG/seed-sync/issues)
- **License:** GPLv3 — see [LICENSE](LICENSE)
