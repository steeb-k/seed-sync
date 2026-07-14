# Assemble a self-contained Windows distribution of SEED Sync, for x86_64 or arm64.
#
# Produces dist\SeedSync[-arm64]\ containing the exes plus the GTK runtime (DLLs,
# compiled GSettings schemas, gdk-pixbuf loaders, and the icon theme caches that
# GTK needs at runtime — omitting any of these makes the app fail silently).
#
# The two architectures get their GTK from different places, because they have to
# (see docs/windows-packaging.md):
#   x86_64 : gvsbuild        (C:\gtk)        — MSVC ABI, all three exes are MSVC.
#   arm64  : MSYS2 CLANGARM64 (C:\gtk-arm64) — gvsbuild is x64-only and vcpkg's gtk
#            port excludes arm64-windows, so MSYS2 is the only prebuilt GTK4 +
#            libadwaita for Windows on ARM. It is mingw-ABI, so the GUI is built for
#            aarch64-pc-windows-gnullvm while the daemon/CLI stay MSVC. That mix is
#            harmless: they are separate processes that only meet over IPC.
#
# Cross-building arm64 on an x86_64 host means the GTK helper *tools* in the arm64
# tree (glib-compile-schemas, gdk-pixbuf-query-loaders, ...) cannot run here. They
# produce arch-independent output, so we run the x86_64 build of the very same MSYS2
# packages instead (-HostToolsRoot, fetched with `fetch-gtk-msys2.ps1 -Env ucrt64`).
# Same upstream version, same loader set => same generated caches.
#
# Prereqs (see docs/windows-packaging.md):
#   x86_64: MSVC toolchain, GTK4 + libadwaita via gvsbuild (default C:\gtk)
#   arm64 : pwsh -File scripts\fetch-gtk-msys2.ps1                      # -> C:\gtk-arm64
#           pwsh -File scripts\fetch-gtk-msys2.ps1 -Env ucrt64 -Root C:\gtk-msys2-x64
#           plus llvm-mingw on PATH (aarch64-w64-mingw32-clang) for the GUI link.
#
# Usage:  pwsh -File scripts\bundle-gtk-windows.ps1 [-Arch x86_64|arm64] [-SkipBuild]

param(
    [ValidateSet("x86_64", "arm64")]
    [string]$Arch = "x86_64",
    [string]$GtkRoot,
    [string]$HostToolsRoot,
    [string]$Target = "release",
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

# The arm64 GUI and daemon come out of *different* target dirs because they are built
# for different ABIs (gnullvm vs msvc) — see the header and scripts\build-arm64.ps1.
if ($Arch -eq "arm64") {
    if (-not $GtkRoot)       { $GtkRoot = "C:\gtk-arm64" }
    if (-not $HostToolsRoot) { $HostToolsRoot = "C:\gtk-msys2-x64" }
    $dist   = Join-Path $root "dist\SeedSync-arm64"
    $guiDir = Join-Path $root "target\aarch64-pc-windows-gnullvm\release"
    $svcDir = Join-Path $root "target\aarch64-pc-windows-msvc\release"
} else {
    if (-not $GtkRoot)       { $GtkRoot = "C:\gtk" }
    if (-not $HostToolsRoot) { $HostToolsRoot = $GtkRoot }
    $dist   = Join-Path $root "dist\SeedSync"
    $guiDir = Join-Path $root "target\$Target"
    $svcDir = $guiDir
}
$gbin = Join-Path $GtkRoot "bin"
$hbin = Join-Path $HostToolsRoot "bin"

if (-not (Test-Path $gbin)) { throw "no GTK runtime at $GtkRoot (see the prereqs in this script's header)" }
if (-not (Test-Path $hbin)) { throw "no host GTK tools at $HostToolsRoot (see the prereqs in this script's header)" }

# The host tools are dynamically linked against their own tree.
$env:PATH = "$hbin;$env:PATH"

if ((-not $SkipBuild) -and ($Arch -eq "arm64")) {
    & "$root\scripts\build-arm64.ps1" -GtkRoot $GtkRoot
    if ($LASTEXITCODE -ne 0) { throw "arm64 build failed" }
}

Write-Host "Bundling SEED Sync ($Arch) from GTK at $GtkRoot -> $dist"
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory -Force -Path "$dist\bin" | Out-Null

# 1. Our binaries + the auto-update engine (installed alongside; the MSI registers
#    it as the SeedSyncUpdate scheduled task).
Copy-Item "$guiDir\seed-gui.exe"    "$dist\bin\"
Copy-Item "$svcDir\seed-daemon.exe" "$dist\bin\"
Copy-Item "$svcDir\seed-cli.exe"    "$dist\bin\"
Copy-Item "$root\packaging\windows\seed-sync-update.ps1" "$dist\bin\"

# 2. GTK runtime DLLs.
# gdbus is needed by GIO at runtime.
if (Test-Path "$gbin\gdbus.exe") { Copy-Item "$gbin\gdbus.exe" "$dist\bin\" }

if ($Arch -eq "arm64") {
    # MSYS2's bin\ is a shared prefix for all ~90 packages in the dependency closure,
    # not a purpose-built GTK tree like gvsbuild's — copying *.dll from it drags in
    # things like libpython3.14.dll that nothing in the app ever loads. So take only
    # what is actually reachable: walk the import tables from our own binaries (plus
    # the pixbuf loaders, which GTK dlopen()s rather than imports) and copy the
    # transitive closure. Anything not in the GTK tree is a system DLL and is skipped
    # by construction, because we only ever copy names we find in $gbin.
    $mingwDump = (Get-ChildItem "C:\" -Directory -Filter "llvm-mingw-*" -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending | Select-Object -First 1)
    $objdump = @(
        "C:\Program Files\LLVM\bin\llvm-objdump.exe"
        if ($mingwDump) { Join-Path $mingwDump.FullName "bin\llvm-objdump.exe" }
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $objdump) { throw "llvm-objdump not found; needed to resolve the GTK import closure" }

    $seeds = @(Get-ChildItem "$dist\bin\*.exe" -File) +
             @(Get-ChildItem "$GtkRoot\lib\gdk-pixbuf-2.0\2.10.0\loaders\*.dll" -File)

    $copied = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $queue  = [System.Collections.Generic.Queue[string]]::new()
    foreach ($s in $seeds) { $queue.Enqueue($s.FullName) }

    while ($queue.Count -gt 0) {
        $pe = $queue.Dequeue()
        $imports = & $objdump -p $pe 2>$null |
            Select-String -Pattern '^\s*DLL Name:\s*(.+)$' |
            ForEach-Object { $_.Matches[0].Groups[1].Value.Trim() }
        foreach ($imp in $imports) {
            $src = Join-Path $gbin $imp
            if (-not (Test-Path $src)) { continue }        # a system DLL, or not ours to ship
            if (-not $copied.Add($imp)) { continue }
            Copy-Item $src "$dist\bin\"
            $queue.Enqueue($src)
        }
    }
    Write-Host "  GTK import closure: $($copied.Count) DLLs" -ForegroundColor DarkGray
} else {
    # gvsbuild's tree is already just the GTK stack, so take it wholesale — this is
    # the shipping x86_64 path and is left exactly as it was.
    Copy-Item "$gbin\*.dll" "$dist\bin\"
}

# 3. Compiled GSettings schemas (Adwaita + GTK) — without these libadwaita aborts.
#    The compiler reads XML and writes a GVariant blob — no target code involved, so
#    the host build of the same glib produces the same result.
$schemas = "$dist\share\glib-2.0\schemas"
New-Item -ItemType Directory -Force -Path $schemas | Out-Null
Copy-Item "$GtkRoot\share\glib-2.0\schemas\*.xml" $schemas -ErrorAction SilentlyContinue
& "$hbin\glib-compile-schemas.exe" $schemas
if ($LASTEXITCODE -ne 0) { throw "glib-compile-schemas failed" }

# 4. gdk-pixbuf loaders (+ cache) — required for PNG/SVG icons.
# query-loaders always emits ABSOLUTE module paths, and `--update-cache` writes
# the cache to GTK's compiled-in default location (e.g. C:\gtk\...), not here.
# So we capture stdout and rewrite the paths RELATIVE to the cache directory,
# which keeps the tree relocatable (works after the MSI installs it elsewhere).
#
# It also g_module_open()s every loader to read its metadata, so it can only ever query
# loaders of its OWN architecture. We therefore query the host tree's loaders and ship
# the arm64 ones: same package, same version, same module set (asserted below), so the
# only difference between the two caches would be the paths — which we rewrite anyway.
$loaders = "$dist\lib\gdk-pixbuf-2.0\2.10.0\loaders"
New-Item -ItemType Directory -Force -Path $loaders | Out-Null
Copy-Item "$GtkRoot\lib\gdk-pixbuf-2.0\2.10.0\loaders\*.dll" $loaders

$hostLoaders = "$HostToolsRoot\lib\gdk-pixbuf-2.0\2.10.0\loaders"
$shipNames = (Get-ChildItem "$loaders\*.dll"     | Select-Object -ExpandProperty Name | Sort-Object) -join ','
$hostNames = (Get-ChildItem "$hostLoaders\*.dll" | Select-Object -ExpandProperty Name | Sort-Object) -join ','
if ($shipNames -ne $hostNames) {
    throw "the host GTK tree's pixbuf loaders differ from the ones being shipped; the generated loaders.cache would be wrong.`n  shipped: $shipNames`n  host   : $hostNames"
}

$cache = "$dist\lib\gdk-pixbuf-2.0\2.10.0\loaders.cache"
$hostCacheDir = (Split-Path $hostLoaders).Replace('\', '/')
[string[]]$loaderNames = Get-ChildItem "$hostLoaders\*.dll" | Select-Object -ExpandProperty Name
Push-Location $hostLoaders
$cacheText = & "$hbin\gdk-pixbuf-query-loaders.exe" $loaderNames
Pop-Location
if (-not $cacheText) { throw "gdk-pixbuf-query-loaders produced nothing" }
($cacheText | ForEach-Object { $_.Replace("$hostCacheDir/", '') }) | Set-Content -Encoding ASCII $cache

# 5. Icon theme (Adwaita + hicolor) + cache. gvsbuild ships gtk-update-icon-cache;
#    MSYS2 names it gtk4-update-icon-cache. The cache is arch-independent.
New-Item -ItemType Directory -Force -Path "$dist\share\icons" | Out-Null
Copy-Item -Recurse "$GtkRoot\share\icons\Adwaita" "$dist\share\icons\" -ErrorAction SilentlyContinue
Copy-Item -Recurse "$GtkRoot\share\icons\hicolor" "$dist\share\icons\" -ErrorAction SilentlyContinue
$iconTool = "$hbin\gtk4-update-icon-cache.exe", "$hbin\gtk-update-icon-cache.exe" |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if ($iconTool -and (Test-Path "$dist\share\icons\Adwaita\index.theme")) {
    & $iconTool "$dist\share\icons\Adwaita"
}

# 6. Check the bundle is complete and single-architecture. This matters most for arm64,
#    which we cannot launch on an x86_64 build host: a missing DLL or a stray x64 binary
#    would otherwise only surface as "the app won't start" on a user's machine.
& "$root\scripts\verify-bundle.ps1" -Dist $dist -Arch $Arch
if ($LASTEXITCODE -ne 0) { throw "bundle verification failed" }

Write-Host "Done. Portable tree at $dist"
Write-Host "Next: zip it, or build the MSI (scripts\bundle is run by scripts\build-msi.ps1)."
