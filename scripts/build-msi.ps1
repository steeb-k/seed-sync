# Build the Seed Sync MSI end to end: release build -> GTK bundle -> sign exes ->
# wix build -> sign MSI.
#
# Prereqs:
#   * GTK built/installed via gvsbuild (default C:\gtk) — see docs/windows-packaging.md
#   * WiX 5 dotnet tool:  dotnet tool install --global wix --version "5.*"
#     plus the UI + Util extensions (this script adds them if missing).
#   * For signing (optional): the Trusted Signing client tools + Windows SDK, and
#     artifact-signing-metadata.json at the repo root (or $env:ARTIFACT_SIGNING_METADATA).
#     Without it the build still succeeds but the MSI/exes are UNSIGNED.
#   * The SeedSyncDaemon service must be STOPPED (a running service locks
#     target\release\seed-daemon.exe and the release build will fail).
#
# Usage:  pwsh -File scripts\build-msi.ps1 [-GtkRoot C:\gtk] [-Version <ver>]
#   -> target\wix\seed-sync-<version>-windows-x86_64.msi
#
# Version defaults to the workspace version in Cargo.toml (single source of truth,
# matching scripts\package-linux.sh and the release.yml tag guard).

param(
    [string]$GtkRoot = "C:\gtk",
    [string]$Version
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

if (-not $Version) {
    $line = Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $line) { throw "could not read version from Cargo.toml" }
    $Version = $line.Matches[0].Groups[1].Value
}
Write-Host "Building Seed Sync MSI $Version" -ForegroundColor Cyan

$env:PKG_CONFIG_PATH = "$GtkRoot\lib\pkgconfig"
$env:PATH = "$GtkRoot\bin;$env:USERPROFILE\.dotnet\tools;$env:PATH"
$env:LIB = "$GtkRoot\lib;$env:LIB"

Write-Host "[1/6] cargo build --release" -ForegroundColor Cyan
& cargo build --release
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed. If it couldn't replace seed-daemon.exe, stop the service first: seed-daemon.exe stop"
}

Write-Host "[2/6] bundling the GTK runtime -> dist\SeedSync" -ForegroundColor Cyan
& "$root\scripts\bundle-gtk-windows.ps1" -GtkRoot $GtkRoot -Target release

$dist = Join-Path $root "dist\SeedSync"

Write-Host "[3/6] signing our exes (Azure Trusted Signing, if configured)" -ForegroundColor Cyan
$exes = "seed-gui.exe", "seed-daemon.exe", "seed-cli.exe" | ForEach-Object { Join-Path $dist "bin\$_" }
& "$root\scripts\sign-artifacts.ps1" -Files $exes

Write-Host "[4/6] generating the license RTF from LICENSE" -ForegroundColor Cyan
$licenseRtf = Join-Path $root "wix\license.rtf"
$plain = Get-Content -Raw (Join-Path $root "LICENSE")
# Minimal RTF: escape RTF metacharacters, map newlines to \par. Non-ASCII becomes
# \u escapes so the GPL's curly quotes etc. render correctly.
$escaped = $plain -replace '\\', '\\' -replace '\{', '\{' -replace '\}', '\}'
$sb = [System.Text.StringBuilder]::new()
[void]$sb.Append("{\rtf1\ansi\deff0{\fonttbl{\f0\fnil Segoe UI;}}\fs18`r`n")
foreach ($ch in $escaped.ToCharArray()) {
    $code = [int]$ch
    if ($ch -eq "`n") { [void]$sb.Append("\par`r`n") }
    elseif ($ch -eq "`r") { }
    elseif ($code -gt 127) { [void]$sb.Append("\u$code?") }
    else { [void]$sb.Append($ch) }
}
[void]$sb.Append("`r`n}")
Set-Content -Path $licenseRtf -Value $sb.ToString() -Encoding ASCII

Write-Host "[5/6] wix build" -ForegroundColor Cyan
# Ensure the UI + Util extensions are available, pinned to the installed wix
# version (idempotent; needs network only the first time). `wix extension add`
# takes the version as id/version, not a --version flag.
$wixVer = ((& wix --version) -split '\+')[0].Trim()
$haveExt = (& wix extension list -g) 2>$null
foreach ($ext in "WixToolset.UI.wixext", "WixToolset.Util.wixext") {
    if ($haveExt -notmatch [regex]::Escape($ext)) {
        Write-Host "  adding extension $ext/$wixVer" -ForegroundColor DarkGray
        & wix extension add -g "$ext/$wixVer"
    }
}

$out = Join-Path $root "target\wix\seed-sync-$Version-windows-x86_64.msi"
New-Item -ItemType Directory -Force -Path (Split-Path $out) | Out-Null
& wix build -arch x64 "$root\wix\seedsync.wxs" `
    -ext WixToolset.UI.wixext -ext WixToolset.Util.wixext `
    -d DistDir="$dist" -d Version="$Version" -d LicenseRtf="$licenseRtf" `
    -o $out
if ($LASTEXITCODE -ne 0) { throw "wix build failed" }

Write-Host "[6/6] signing the MSI (Azure Trusted Signing, if configured)" -ForegroundColor Cyan
& "$root\scripts\sign-artifacts.ps1" -Files @($out)

Write-Host ("Done -> {0}  ({1:N1} MB)" -f $out, ((Get-Item $out).Length / 1MB)) -ForegroundColor Green
Write-Host "Install (elevated):   msiexec /i `"$out`"" -ForegroundColor Green
Write-Host "Uninstall (elevated): msiexec /x `"$out`"" -ForegroundColor Green
Write-Host "Publish to release:   pwsh -File scripts\publish-msi.ps1 -Version $Version" -ForegroundColor Green
