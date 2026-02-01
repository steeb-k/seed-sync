<#
.SYNOPSIS
    Build script for S.E.E.D. - creates distributable packages.

.DESCRIPTION
    This script builds the S.E.E.D. application in Release mode and creates
    a ZIP package for distribution.

.PARAMETER Version
    Version number for the build (default: 1.0.0)

.PARAMETER Configuration
    Build configuration (default: Release)

.PARAMETER Runtime
    Target runtime (default: win-x64)

.EXAMPLE
    .\build.ps1
    
.EXAMPLE
    .\build.ps1 -Version "1.0.1" -Runtime "win-arm64"
#>

param(
    [string]$Version = "1.0.0",
    [string]$Configuration = "Release",
    [string]$Runtime = "win-x64"
)

$ErrorActionPreference = "Stop"

# Paths
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$SolutionDir = $ScriptDir
$OutputDir = Join-Path $SolutionDir "dist"
$PublishDir = Join-Path $OutputDir "publish"
$ZipName = "SeedSync-$Version-$Runtime.zip"
$ZipPath = Join-Path $OutputDir $ZipName

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "  S.E.E.D. Build Script" -ForegroundColor Cyan
Write-Host "  Version: $Version" -ForegroundColor Cyan
Write-Host "  Configuration: $Configuration" -ForegroundColor Cyan
Write-Host "  Runtime: $Runtime" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

# Clean previous output
if (Test-Path $OutputDir) {
    Write-Host "Cleaning previous build..." -ForegroundColor Yellow
    Remove-Item -Recurse -Force $OutputDir
}
New-Item -ItemType Directory -Path $OutputDir | Out-Null
New-Item -ItemType Directory -Path $PublishDir | Out-Null

# Run tests first
Write-Host ""
Write-Host "Running tests..." -ForegroundColor Yellow
dotnet test "$SolutionDir\tests\SeedSync.Tests\SeedSync.Tests.csproj" --configuration $Configuration --verbosity minimal
if ($LASTEXITCODE -ne 0) {
    Write-Host "Tests failed! Aborting build." -ForegroundColor Red
    exit 1
}
Write-Host "Tests passed!" -ForegroundColor Green

# Build and publish Daemon
Write-Host ""
Write-Host "Publishing SeedSync.Daemon..." -ForegroundColor Yellow
$DaemonDir = Join-Path $PublishDir "Daemon"
dotnet publish "$SolutionDir\src\SeedSync.Daemon\SeedSync.Daemon.csproj" `
    --configuration $Configuration `
    --runtime $Runtime `
    --self-contained true `
    --output $DaemonDir `
    -p:PublishSingleFile=true `
    -p:IncludeNativeLibrariesForSelfExtract=true `
    -p:Version=$Version
if ($LASTEXITCODE -ne 0) {
    Write-Host "Daemon publish failed!" -ForegroundColor Red
    exit 1
}

# Build and publish CLI
Write-Host ""
Write-Host "Publishing SeedSync.Cli..." -ForegroundColor Yellow
$CliDir = Join-Path $PublishDir "Cli"
dotnet publish "$SolutionDir\src\SeedSync.Cli\SeedSync.Cli.csproj" `
    --configuration $Configuration `
    --runtime $Runtime `
    --self-contained true `
    --output $CliDir `
    -p:PublishSingleFile=true `
    -p:IncludeNativeLibrariesForSelfExtract=true `
    -p:Version=$Version
if ($LASTEXITCODE -ne 0) {
    Write-Host "CLI publish failed!" -ForegroundColor Red
    exit 1
}

# Build WinUI App (different runtime handling)
Write-Host ""
Write-Host "Publishing SeedSync.App..." -ForegroundColor Yellow
$AppDir = Join-Path $PublishDir "App"

# WinUI apps need special handling - use the publish profile
$Platform = switch ($Runtime) {
    "win-x64" { "x64" }
    "win-x86" { "x86" }
    "win-arm64" { "ARM64" }
    default { "x64" }
}

dotnet publish "$SolutionDir\src\SeedSync.App\SeedSync.App.csproj" `
    --configuration $Configuration `
    -p:Platform=$Platform `
    -p:Version=$Version `
    --output $AppDir
if ($LASTEXITCODE -ne 0) {
    Write-Host "App publish failed!" -ForegroundColor Red
    exit 1
}

# Copy documentation
Write-Host ""
Write-Host "Copying documentation..." -ForegroundColor Yellow
Copy-Item "$SolutionDir\README.md" -Destination $PublishDir
Copy-Item "$SolutionDir\LICENSE" -Destination $PublishDir -ErrorAction SilentlyContinue
if (Test-Path "$SolutionDir\INSTALL.md") {
    Copy-Item "$SolutionDir\INSTALL.md" -Destination $PublishDir
}

# Create ZIP package
Write-Host ""
Write-Host "Creating ZIP package: $ZipName..." -ForegroundColor Yellow
Compress-Archive -Path "$PublishDir\*" -DestinationPath $ZipPath -Force

# Summary
Write-Host ""
Write-Host "========================================" -ForegroundColor Green
Write-Host "  Build Complete!" -ForegroundColor Green
Write-Host "========================================" -ForegroundColor Green
Write-Host ""
Write-Host "Output: $ZipPath" -ForegroundColor Cyan
Write-Host "Size: $([math]::Round((Get-Item $ZipPath).Length / 1MB, 2)) MB" -ForegroundColor Cyan
Write-Host ""
Write-Host "Contents:" -ForegroundColor Cyan
Write-Host "  - App/       : WinUI desktop application" -ForegroundColor White
Write-Host "  - Daemon/    : Background sync service" -ForegroundColor White
Write-Host "  - Cli/       : Command-line interface" -ForegroundColor White
Write-Host "  - README.md  : Documentation" -ForegroundColor White
Write-Host ""
