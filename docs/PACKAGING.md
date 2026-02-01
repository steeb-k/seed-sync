# Packaging and Distribution

This document describes how to build distributable packages for S.E.E.D.

## Version

Version is set in `Directory.Build.props` at the solution root. Update the `<Version>` element there before releasing.

## ZIP Package (recommended for testing)

From the repository root:

```powershell
.\build.ps1
```

Or with options:

```powershell
.\build.ps1 -Version "1.0.1" -Configuration Release -Runtime win-x64
```

Output: `dist\SeedSync-<Version>-<Runtime>.zip`

The ZIP contains:
- **App/** — WinUI application
- **Daemon/** — Sync daemon (single-file executable)
- **Cli/** — CLI (single-file executable)
- **README.md**, **INSTALL.md**, **LICENSE**

## MSIX Package (Windows app package)

MSIX allows installation via the Store or side-loading and supports auto-update.

### Prerequisites

- Windows 10/11 SDK
- Optional: code signing certificate (`.pfx`) for production; omit for self-signed test builds

### Build MSIX (Visual Studio)

1. Open `src/SeedSync.App/SeedSync.App.csproj` in Visual Studio.
2. Right-click the project → **Store** → **Create App Packages** (or **Pack**).
3. Select **Sideloading** (or **Store** if publishing to Microsoft Store).
4. Choose architecture (x64, x86, ARM64).
5. For testing, use the auto-generated self-signed certificate.

### Build MSIX (command line)

From the repository root:

```powershell
dotnet publish src/SeedSync.App/SeedSync.App.csproj `
  -c Release `
  -p:Platform=x64 `
  -p:WindowsPackageType=Msix `
  -p:AppxPackageSigningEnabled=true
```

If you have a signing certificate:

```powershell
dotnet publish src/SeedSync.App/SeedSync.App.csproj `
  -c Release `
  -p:Platform=x64 `
  -p:WindowsPackageType=Msix `
  -p:AppxPackageSigningEnabled=true `
  -p:PackageCertificateKeyFile=path\to\SeedSync.pfx `
  -p:PackageCertificatePassword=YourPassword
```

Output is under `src/SeedSync.App/bin/Release/` (exact path depends on the SDK; look for `.msix` or `.msixbundle`).

### Install MSIX (side-loading)

1. Enable side-loading: **Settings → Update & Security → For developers → Install apps from any source** (or **Sideload apps**).
2. Double-click the `.msix` file or run:
   ```powershell
   Add-AppxPackage -Path .\SeedSync.App_1.0.0.0_x64_*.msix
   ```

### Uninstall MSIX

**Settings → Apps → S.E.E.D. → Uninstall**

Or:

```powershell
Get-AppxPackage *SeedSync* | Remove-AppxPackage
```

## Code signing (production)

For production distribution:

1. **MSIX:** Use a certificate from a trusted CA or the Microsoft Partner Center (Store). Place the `.pfx` in the project directory or set `PackageCertificateKeyFile` in the build.
2. **ZIP/exe:** Sign the executables with `signtool` and a code signing certificate to avoid SmartScreen warnings.

## CI/CD

- **CI:** `.github/workflows/ci.yml` runs on push/PR: build and test.
- **Release:** `.github/workflows/release.yml` runs on tag push (`v*`): runs tests, builds ZIP for win-x64, and creates a GitHub Release with the ZIP attached.

To create a release:

```bash
git tag v1.0.0
git push origin v1.0.0
```

Then open the repository’s **Releases** page to edit the generated release or add more assets.
