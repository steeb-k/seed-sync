# Build the Seed Sync MSI end to end: release build -> GTK bundle -> wix build.
#
# Prereqs:
#   * GTK built/installed via gvsbuild (default C:\gtk) — see docs/windows-packaging.md
#   * WiX 5 dotnet tool:  dotnet tool install --global wix --version "5.*"
#   * The SeedSyncDaemon service must be STOPPED (a running service locks
#     target\release\seed-daemon.exe and the release build will fail).
#
# Usage:  pwsh -File scripts\build-msi.ps1 [-GtkRoot C:\gtk] [-Version 0.1.0]
#   -> target\wix\SeedSync-<version>.msi

param(
    [string]$GtkRoot = "C:\gtk",
    [string]$Version = "0.1.0"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

$env:PKG_CONFIG_PATH = "$GtkRoot\lib\pkgconfig"
$env:PATH = "$GtkRoot\bin;$env:USERPROFILE\.dotnet\tools;$env:PATH"
$env:LIB = "$GtkRoot\lib;$env:LIB"

Write-Host "[1/3] cargo build --release" -ForegroundColor Cyan
& cargo build --release
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed. If it couldn't replace seed-daemon.exe, stop the service first: seed-daemon.exe stop"
}

Write-Host "[2/3] bundling the GTK runtime -> dist\SeedSync" -ForegroundColor Cyan
& "$root\scripts\bundle-gtk-windows.ps1" -GtkRoot $GtkRoot -Target release

Write-Host "[3/3] wix build" -ForegroundColor Cyan
$dist = Join-Path $root "dist\SeedSync"
$out = Join-Path $root "target\wix\SeedSync-$Version.msi"
New-Item -ItemType Directory -Force -Path (Split-Path $out) | Out-Null
& wix build -arch x64 "$root\wix\seedsync.wxs" -d DistDir="$dist" -o $out
if ($LASTEXITCODE -ne 0) { throw "wix build failed" }

Write-Host ("Done -> {0}  ({1:N1} MB)" -f $out, ((Get-Item $out).Length / 1MB)) -ForegroundColor Green
Write-Host "Install (elevated):   msiexec /i `"$out`"" -ForegroundColor Green
Write-Host "Uninstall (elevated): msiexec /x `"$out`"" -ForegroundColor Green
