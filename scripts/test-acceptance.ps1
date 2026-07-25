# Acceptance gate: run the integration suites that actually exercise the app.
#
# Every integration test in this workspace is `#[ignore]`d, because each one opens
# real iroh endpoints and must run serially. That means `cargo test --workspace`
# runs ZERO of them — it compiles the world and executes only unit tests. Nothing
# about a green `cargo test` says the app syncs files. This script is the thing
# that does.
#
# Run it before cutting any release. See docs/testing.md.
#
#   .\scripts\test-acceptance.ps1              # every suite
#   .\scripts\test-acceptance.ps1 live_folder  # one or more suites by name
#
# Exit code is non-zero if any suite fails.

$ErrorActionPreference = "Continue"

# Ordered cheapest-first, so a broken build or a broken basic sync fails fast
# rather than 40 minutes in.
$AllSuites = @(
    @{ Crate = "seed-core";   Test = "docs_spike" },
    @{ Crate = "seed-core";   Test = "persistence" },
    @{ Crate = "seed-core";   Test = "keystore" },
    @{ Crate = "seed-core";   Test = "loopback" },
    @{ Crate = "seed-core";   Test = "live_folder" },
    @{ Crate = "seed-core";   Test = "health" },
    @{ Crate = "seed-core";   Test = "presence" },
    @{ Crate = "seed-core";   Test = "member_names" },
    @{ Crate = "seed-core";   Test = "discovery" },
    @{ Crate = "seed-core";   Test = "rendezvous" },
    @{ Crate = "seed-core";   Test = "isolation" },
    @{ Crate = "seed-core";   Test = "resume" },
    @{ Crate = "seed-core";   Test = "gc" },
    @{ Crate = "seed-core";   Test = "tombstone_race" },
    @{ Crate = "seed-core";   Test = "health_quiesce" },
    @{ Crate = "seed-core";   Test = "multi_master" },
    @{ Crate = "seed-daemon"; Test = "loopback_ipc" },
    @{ Crate = "seed-daemon"; Test = "health_ipc" }
)

$suites = if ($args.Count -gt 0) {
    $AllSuites | Where-Object { $args -contains $_.Test }
} else {
    $AllSuites
}

if (-not $suites) {
    Write-Host "no matching suites; known suites:" -ForegroundColor Yellow
    $AllSuites | ForEach-Object { Write-Host "  $($_.Test)" }
    exit 2
}

# Build once up front so per-suite timings reflect test time, not compile time.
Write-Host "==> building test binaries" -ForegroundColor Cyan
cargo test --workspace --no-run
if ($LASTEXITCODE -ne 0) { Write-Host "build failed" -ForegroundColor Red; exit 1 }

$results = @()
$startedAll = Get-Date

foreach ($s in $suites) {
    Write-Host ""
    Write-Host "==> $($s.Crate) :: $($s.Test)" -ForegroundColor Cyan
    $started = Get-Date
    # --test-threads 1: these open real endpoints and bind real sockets; running
    # them concurrently makes failures meaningless.
    cargo test -p $s.Crate --test $s.Test -- --ignored --nocapture --test-threads 1
    $ok = ($LASTEXITCODE -eq 0)
    $results += [pscustomobject]@{
        Suite   = "$($s.Crate)/$($s.Test)"
        Ok      = $ok
        Minutes = [math]::Round(((Get-Date) - $started).TotalMinutes, 1)
    }
    if (-not $ok) { Write-Host "FAILED: $($s.Test)" -ForegroundColor Red }
}

Write-Host ""
Write-Host "==> summary ($([math]::Round(((Get-Date) - $startedAll).TotalMinutes, 1)) min total)" -ForegroundColor Cyan
$results | ForEach-Object {
    $tag = if ($_.Ok) { "PASS" } else { "FAIL" }
    $col = if ($_.Ok) { "Green" } else { "Red" }
    Write-Host ("  {0,-4} {1,-32} {2,5} min" -f $tag, $_.Suite, $_.Minutes) -ForegroundColor $col
}

$failed = @($results | Where-Object { -not $_.Ok })
if ($failed.Count -gt 0) {
    Write-Host ""
    Write-Host "$($failed.Count) suite(s) failed — do not release." -ForegroundColor Red
    exit 1
}
Write-Host ""
Write-Host "all suites passed" -ForegroundColor Green
