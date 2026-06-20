# Attach the signed Windows MSI to the matching release in the PUBLIC
# steeb-k/seed-sync-binaries repo — the same release the Linux CI (release.yml)
# creates for the tag. softprops/action-gh-release on the Linux side and this
# upload both target release "v<version>", so order doesn't matter.
#
# Prereqs:
#   * gh CLI authenticated (gh auth status) with write access to the binaries repo.
#   * The release "v<version>" already exists (push the tag so release.yml runs,
#     or create it manually). The asset name matches the Linux convention:
#       seed-sync-<version>-windows-x86_64.msi
#
# Usage:  pwsh -File scripts\publish-msi.ps1 [-Version <ver>] [-Repo owner/name]

param(
    [string]$Version,
    [string]$Repo = "steeb-k/seed-sync-binaries"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

if (-not $Version) {
    $line = Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $line) { throw "could not read version from Cargo.toml" }
    $Version = $line.Matches[0].Groups[1].Value
}

$msi = Join-Path $root "target\wix\seed-sync-$Version-windows-x86_64.msi"
if (-not (Test-Path $msi)) {
    throw "MSI not found: $msi`nBuild it first: pwsh -File scripts\build-msi.ps1"
}

$tag = "v$Version"
Write-Host "Uploading $(Split-Path -Leaf $msi) to $Repo release $tag" -ForegroundColor Cyan
& gh release upload $tag $msi -R $Repo --clobber
if ($LASTEXITCODE -ne 0) {
    throw "gh release upload failed. Confirm the release '$tag' exists on $Repo (push the tag to run release.yml) and that gh is authenticated with write access."
}
Write-Host "Done — $tag on $Repo now carries the Windows MSI." -ForegroundColor Green
