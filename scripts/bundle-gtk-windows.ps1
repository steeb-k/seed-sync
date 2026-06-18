# Assemble a self-contained Windows distribution of Seed Sync.
#
# Prereqs (see docs/windows-packaging.md):
#   * MSVC toolchain (rustup default stable-msvc)
#   * GTK4 + Libadwaita built/installed via gvsbuild (default C:\gtk)
#   * cargo build --release already run
#
# Produces dist\SeedSync\ containing the exes plus the GTK runtime (DLLs,
# compiled GSettings schemas, gdk-pixbuf loaders, and the icon theme caches that
# GTK needs at runtime — omitting any of these makes the app fail silently).
#
# Usage:  pwsh -File scripts\bundle-gtk-windows.ps1 [-GtkRoot C:\gtk] [-Target release]

param(
    [string]$GtkRoot = "C:\gtk",
    [string]$Target  = "release"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root "dist\SeedSync"
$gbin = Join-Path $GtkRoot "bin"

Write-Host "Bundling Seed Sync from GTK at $GtkRoot -> $dist"
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory -Force -Path $dist | Out-Null
New-Item -ItemType Directory -Force -Path "$dist\bin" | Out-Null

# 1. Our binaries.
Copy-Item "$root\target\$Target\seed-gui.exe"    "$dist\bin\"
Copy-Item "$root\target\$Target\seed-daemon.exe" "$dist\bin\"
Copy-Item "$root\target\$Target\seed-cli.exe"    "$dist\bin\"

# 2. GTK runtime DLLs (everything the linker pulled in lives in gvsbuild's bin).
Copy-Item "$gbin\*.dll" "$dist\bin\"
# gdbus is needed by GIO at runtime.
if (Test-Path "$gbin\gdbus.exe") { Copy-Item "$gbin\gdbus.exe" "$dist\bin\" }

# 3. Compiled GSettings schemas (Adwaita + GTK) — without these libadwaita aborts.
$schemas = "$dist\share\glib-2.0\schemas"
New-Item -ItemType Directory -Force -Path $schemas | Out-Null
Copy-Item "$GtkRoot\share\glib-2.0\schemas\*.xml" $schemas -ErrorAction SilentlyContinue
& "$gbin\glib-compile-schemas.exe" $schemas

# 4. gdk-pixbuf loaders (+ cache) — required for PNG/SVG icons.
# query-loaders always emits ABSOLUTE module paths, and `--update-cache` writes
# the cache to GTK's compiled-in default location (e.g. C:\gtk\...), not here.
# So we capture stdout and rewrite the paths RELATIVE to the cache directory,
# which keeps the tree relocatable (works after the MSI installs it elsewhere).
$loaders = "$dist\lib\gdk-pixbuf-2.0\2.10.0\loaders"
New-Item -ItemType Directory -Force -Path $loaders | Out-Null
Copy-Item "$GtkRoot\lib\gdk-pixbuf-2.0\2.10.0\loaders\*.dll" $loaders
$cache = "$dist\lib\gdk-pixbuf-2.0\2.10.0\loaders.cache"
$cacheDir = (Split-Path $cache).Replace('\', '/')
[string[]]$loaderNames = Get-ChildItem "$loaders\*.dll" | Select-Object -ExpandProperty Name
Push-Location $loaders
$cacheText = & "$gbin\gdk-pixbuf-query-loaders.exe" $loaderNames
Pop-Location
($cacheText | ForEach-Object { $_.Replace("$cacheDir/", '') }) | Set-Content -Encoding ASCII $cache

# 5. Icon theme (Adwaita + hicolor) + cache.
New-Item -ItemType Directory -Force -Path "$dist\share\icons" | Out-Null
Copy-Item -Recurse "$GtkRoot\share\icons\Adwaita" "$dist\share\icons\" -ErrorAction SilentlyContinue
Copy-Item -Recurse "$GtkRoot\share\icons\hicolor" "$dist\share\icons\" -ErrorAction SilentlyContinue
if (Test-Path "$gbin\gtk-update-icon-cache.exe") {
    & "$gbin\gtk-update-icon-cache.exe" "$dist\share\icons\Adwaita"
}

Write-Host "Done. Portable tree at $dist"
Write-Host "Next: zip it, or build an MSI with cargo-wix (see docs/windows-packaging.md)."
