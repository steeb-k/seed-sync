<#
.SYNOPSIS
  Prove the Windows Trusted Signing chain actually works: compile a never-
  signed, two-second probe binary and sign it, asserting Unsigned-before and
  Valid-after.

.DESCRIPTION
  A copied system binary is already signed by Microsoft (and that signature
  can't be stripped), so it would prove nothing short of the real thing; this
  compiles a throwaway one instead.

  Used by the windows job in .github/workflows/build.yml, AFTER azure/login
  and BEFORE the MSIs are signed. See docs/ci-release.md §5: unlike a build
  that logs in to Azure first, SEED Sync's Windows job builds both
  architectures BEFORE azure/login at all — the OIDC token behind it lives
  about five minutes and a service principal has no refresh token, and the
  two release builds together run far longer than that. So the login (and
  this probe) happen only once the binaries already exist, followed by a
  second, fresh login immediately before the MSIs are actually signed.

.PARAMETER MetadataPath
  Forwarded to scripts\sign-artifacts.ps1 -MetadataPath. Defaults to
  $env:ARTIFACT_SIGNING_METADATA / the repo-root file, same as that script.
#>
param(
    [string]$MetadataPath
)

$ErrorActionPreference = "Stop"
# scripts\ci\windows-signing-probe.ps1 -> scripts -> repo root.
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)

$dir = Join-Path $env:RUNNER_TEMP "signprobe"
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$src = Join-Path $dir "probe.cs"
Set-Content $src "class P { static void Main() { } }"
$probe = Join-Path $dir "probe.exe"

$csc = "$env:WINDIR\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
if (-not (Test-Path $csc)) { throw "no C# compiler at $csc" }
& $csc /nologo /out:$probe $src
if (-not (Test-Path $probe)) { throw "the probe did not compile" }

$before = Get-AuthenticodeSignature $probe
if ($before.Status -eq "Valid") { throw "the probe is already signed; the check would prove nothing" }

$signArgs = @{ Files = $probe }
if ($MetadataPath) { $signArgs.MetadataPath = $MetadataPath }
& "$root\scripts\sign-artifacts.ps1" @signArgs

$after = Get-AuthenticodeSignature $probe
Write-Host "probe: $($after.Status) by $($after.SignerCertificate.Subject)"
if ($after.Status -ne "Valid") { throw "the signing chain produced $($after.Status)" }
