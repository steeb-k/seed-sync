<#
.SYNOPSIS
  S.E.E.D. (SEED Sync) Windows install-or-update engine + scheduled-task helper.

  The Windows analog of packaging/linux/seed-sync-update. Compares the installed
  daemon version to the latest release in the PUBLIC steeb-k/seed-sync-binaries
  repo and, if newer, downloads the MSI and applies it silently. The MSI's
  MajorUpgrade handling does the heavy lifting (stop service -> replace files ->
  restart service), so "apply" is just `msiexec /i <msi> /qn`.

.PARAMETER Check
  Report whether an update is available; make no changes.

.PARAMETER RegisterTask
  Register the daily "SeedSyncUpdate" scheduled task (runs this script as SYSTEM).
  Invoked by the MSI on install.

.PARAMETER UnregisterTask
  Remove the "SeedSyncUpdate" scheduled task.

.NOTES
  Public repo => no auth. Version is the source of truth (compared to the release
  tag). Lives in "Program Files\SeedSync\bin"; it locates seed-daemon.exe next to
  itself via $PSScriptRoot.
#>
[CmdletBinding(DefaultParameterSetName = 'Update')]
param(
    [Parameter(ParameterSetName = 'Update')]   [switch]$Check,
    [Parameter(ParameterSetName = 'Register')] [switch]$RegisterTask,
    [Parameter(ParameterSetName = 'Unregister')][switch]$UnregisterTask
)

$ErrorActionPreference = 'Stop'

$Repo      = if ($env:SEED_BINARIES_REPO) { $env:SEED_BINARIES_REPO } else { 'steeb-k/seed-sync-binaries' }
$TaskName  = 'SeedSyncUpdate'

# Which MSI to pull. Keyed off the OS architecture, NOT the installed build's, so a
# Windows-on-ARM machine that installed the x86_64 MSI (which it will happily run under
# emulation) migrates itself to the native ARM64 build; the two MSIs share an
# UpgradeCode, which makes that a normal major upgrade.
#
# RuntimeInformation reports the OS architecture even from an emulated process, which
# PROCESSOR_ARCHITECTURE does not; the env vars are only a fallback.
function Get-OsArch {
    try {
        $a = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
        if ($a) { return $a }
    } catch { }
    if ($env:PROCESSOR_ARCHITEW6432) { return $env:PROCESSOR_ARCHITEW6432 }
    return $env:PROCESSOR_ARCHITECTURE
}

$OsArch    = Get-OsArch
$AssetGlob = if ($OsArch -match 'Arm64') { '*windows-arm64.msi' } else { '*windows-x86_64.msi' }
$BinDir    = $PSScriptRoot
$DaemonExe = Join-Path $BinDir 'seed-daemon.exe'
$ScriptPath = $PSCommandPath

# Log to the machine-wide data dir (where the LocalSystem daemon also writes).
$DataDir = Join-Path $env:ProgramData 'SeedSync'
$LogFile = Join-Path $DataDir 'update.log'

function Write-Log {
    param([string]$Message)
    $line = "{0}  {1}" -f (Get-Date -Format 's'), $Message
    Write-Host "seed-sync-update: $Message"
    try {
        if (-not (Test-Path $DataDir)) { New-Item -ItemType Directory -Force -Path $DataDir | Out-Null }
        Add-Content -Path $LogFile -Value $line -Encoding UTF8
    } catch { }
}

# ── Scheduled-task management ────────────────────────────────────────────────
function Register-UpdateTask {
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' `
        -Argument ("-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"{0}`"" -f $ScriptPath)

    # Daily (with a random spread) plus a delayed run shortly after each boot —
    # mirrors the Linux timer's "after login + daily, randomized".
    $daily = New-ScheduledTaskTrigger -Daily -At 3am
    $daily.RandomDelay = 'PT2H'
    $boot = New-ScheduledTaskTrigger -AtStartup
    $boot.Delay = 'PT5M'

    $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
    $settings  = New-ScheduledTaskSettingsSet -StartWhenAvailable `
        -DontStopOnIdleEnd -ExecutionTimeLimit (New-TimeSpan -Hours 1)

    Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger @($daily, $boot) `
        -Principal $principal -Settings $settings `
        -Description 'SEED Sync daily auto-update check' -Force | Out-Null
    Write-Log "registered scheduled task '$TaskName'"
}

function Unregister-UpdateTask {
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-Log "removed scheduled task '$TaskName'"
    }
}

# ── Service / tray cycling ───────────────────────────────────────────────────
# The MSI's ServiceControl (Stop="both" / Start="install") cycles SeedSyncDaemon,
# but nothing cycles the per-user tray GUI: it keeps running the OLD seed-gui.exe
# after an upgrade, and while it is running it holds a lock on the file msiexec is
# trying to replace (which can turn a clean upgrade into a reboot-pending 3010).
# So: stop the tray BEFORE msiexec, restart it after.
$GuiProcName = 'seed-gui'

# The owner of a running process, as DOMAIN\user, or $null.
function Get-ProcessOwner {
    param([string]$Name)
    foreach ($p in @(Get-CimInstance Win32_Process -Filter "Name='$Name'" -ErrorAction SilentlyContinue)) {
        try {
            $o = Invoke-CimMethod -InputObject $p -MethodName GetOwner -ErrorAction Stop
            if ($o.User) { return "$($o.Domain)\$($o.User)" }
        } catch { }
    }
    return $null
}

# Returns the account whose tray we stopped (DOMAIN\user), or $null if the GUI
# wasn't running. Capturing the owner *before* killing it is the whole point: it
# is by definition the user whose tray has to come back, and it is readable no
# matter what state the session is in.
function Stop-Tray {
    $procs = @(Get-Process -Name $GuiProcName -ErrorAction SilentlyContinue)
    if (-not $procs) { return $null }
    $owner = Get-ProcessOwner "$GuiProcName.exe"
    Write-Log "stopping the tray GUI ($($procs.Count) process(es), owner $(if ($owner) { $owner } else { 'unknown' })) so the MSI can replace seed-gui.exe"
    $procs | Stop-Process -Force -ErrorAction SilentlyContinue
    for ($i = 0; $i -lt 50 -and (Get-Process -Name $GuiProcName -ErrorAction SilentlyContinue); $i++) {
        Start-Sleep -Milliseconds 100
    }
    # Never return $null for a GUI that *was* running — the caller uses the return
    # value to decide whether a relaunch is owed, and "running but owner unreadable"
    # still owes one (Restart-Tray falls back to Explorer's owner).
    if ($owner) { return $owner } else { return '' }
}

function Restart-Tray {
    param([string]$Owner)

    $exe = Join-Path $BinDir 'seed-gui.exe'
    if (-not (Test-Path $exe)) { Write-Log "seed-gui.exe not found; skipping tray relaunch"; return }

    # When the daily scheduled task runs us we are SYSTEM, so Start-Process would
    # land the GUI in session 0 — invisible, no tray. Hand it to the interactive
    # user via a one-shot task instead. Run interactively (a user invoking
    # `seed-sync-update.ps1` by hand) and we can just launch it.
    if (-not [Security.Principal.WindowsIdentity]::GetCurrent().IsSystem) {
        Start-Process -FilePath $exe -ArgumentList '--hidden' | Out-Null
        Write-Log "relaunched the tray GUI (hidden)"
        return
    }

    # Who to relaunch as. This used to be `Win32_ComputerSystem.UserName`, which is
    # empty whenever the session isn't an attached console one — including a *locked*
    # workstation, which is precisely the state the machine is in at 03:00 when the
    # daily task fires. So every unattended update killed the tray, logged "no
    # interactive user logged on", and left the user with no tray icon until they
    # opened the GUI by hand; the service kept syncing, so nothing else looked wrong.
    # (It is also empty in plenty of *unlocked* states, so this was never reliable.)
    # Prefer the owner of the GUI we just stopped, then whoever owns Explorer.
    $console = $Owner
    if (-not $console) { $console = Get-ProcessOwner 'explorer.exe' }
    if (-not $console) {
        # No Explorer either: genuinely nobody logged on. Note the autostart entry
        # runs at *logon*, so this really does wait for a login — unlike the locked
        # case above, where no logon was ever going to happen.
        Write-Log "no interactive session found; the tray will start at next logon"
        return
    }

    $task = 'SeedSyncTrayRelaunch'
    try {
        $action    = New-ScheduledTaskAction -Execute $exe -Argument '--hidden'
        $principal = New-ScheduledTaskPrincipal -UserId $console -LogonType Interactive -RunLevel Limited
        Register-ScheduledTask -TaskName $task -Action $action -Principal $principal -Force | Out-Null
        Start-ScheduledTask -TaskName $task
        # `Start-ScheduledTask` only *requests* a start, so wait for the process to
        # actually exist rather than sleeping a fixed 2 s and claiming success. A
        # post-MSI cold start pulls in the whole GTK DLL closure and can take a while.
        $deadline = (Get-Date).AddSeconds(60)
        while ((Get-Date) -lt $deadline -and -not (Get-Process -Name $GuiProcName -ErrorAction SilentlyContinue)) {
            Start-Sleep -Milliseconds 250
        }
        if (Get-Process -Name $GuiProcName -ErrorAction SilentlyContinue) {
            Write-Log "relaunched the tray GUI (hidden) as $console"
        } else {
            Write-Log "WARNING: tray relaunch task started as $console but no $GuiProcName process appeared"
        }
    } catch {
        Write-Log "WARNING: could not relaunch the tray GUI: $($_.Exception.Message)"
    } finally {
        Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
    }
}

# The MSI should have restarted the service; make sure it actually did. A stale
# or stopped daemon still looks healthy in the GUI while the node is silently
# absent from every peer.
function Assert-DaemonRunning {
    $svc = Get-Service -Name 'SeedSyncDaemon' -ErrorAction SilentlyContinue
    if (-not $svc) { Write-Log "WARNING: service SeedSyncDaemon not found after upgrade"; return }
    if ($svc.Status -ne 'Running') {
        Write-Log "service is $($svc.Status) after the upgrade; starting it"
        try {
            Start-Service -Name 'SeedSyncDaemon'
            (Get-Service -Name 'SeedSyncDaemon').WaitForStatus('Running', (New-TimeSpan -Seconds 20))
        } catch {
            Write-Log "WARNING: could not start SeedSyncDaemon: $($_.Exception.Message)"
            return
        }
    }
    Write-Log "daemon service running"
}

# ── Update logic ─────────────────────────────────────────────────────────────
function Get-InstalledVersion {
    if (-not (Test-Path $DaemonExe)) { return $null }
    $out = & $DaemonExe --version 2>$null
    if ($out -match '(\d+\.\d+\.\d+)') { return $matches[1] }
    return $null
}

function Invoke-Update {
    param([switch]$CheckOnly)

    try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch { }

    $installed = Get-InstalledVersion
    if (-not $installed) {
        Write-Log "seed-daemon.exe not found next to the updater; nothing to do"
        return
    }
    Write-Log "checking $Repo for a newer release (installed: $installed)"

    $headers = @{ 'User-Agent' = 'seed-sync-update'; 'Accept' = 'application/vnd.github+json' }
    $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers $headers
    $tag = $rel.tag_name
    if (-not $tag) { Write-Log "could not determine the latest release tag"; return }
    $latest = $tag.TrimStart('v')

    if ([version]$latest -le [version]$installed) {
        Write-Log "up to date (latest: $latest)"
        return
    }
    Write-Log "update available: $installed -> $latest"
    if ($CheckOnly) { return }

    $asset = $rel.assets | Where-Object { $_.name -like $AssetGlob } | Select-Object -First 1
    if (-not $asset) {
        # Deliberately no fallback to another architecture's MSI: swapping a machine
        # between the native and the emulated build behind the user's back is a major
        # upgrade either way, and a release that is simply missing an asset is a
        # packaging mistake we would rather see in the log than paper over.
        Write-Log "release $tag has no $AssetGlob asset; staying on $installed"
        return
    }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("seed-sync-{0}.msi" -f $latest)
    Write-Log "downloading $($asset.name)"
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $tmp -UseBasicParsing -Headers @{ 'User-Agent' = 'seed-sync-update' }

    # $null = wasn't running; otherwise the owning account ('' if unreadable).
    $guiUser = Stop-Tray

    $msiLog = Join-Path $DataDir 'update-msi.log'
    Write-Log "applying $tmp (msiexec /qn)"
    $p = Start-Process -FilePath 'msiexec.exe' `
        -ArgumentList @('/i', "`"$tmp`"", '/qn', '/norestart', '/l*v', "`"$msiLog`"") `
        -Wait -PassThru
    # 0 = success, 3010 = success, reboot required.
    if ($p.ExitCode -eq 0 -or $p.ExitCode -eq 3010) {
        Write-Log "updated to $latest (msiexec exit $($p.ExitCode))"
    } else {
        Write-Log "msiexec failed (exit $($p.ExitCode)); see $msiLog"
    }

    # Unconditional: on success this is the point of the exercise, and on failure
    # we still killed the tray, so the user must not be left without a GUI.
    Assert-DaemonRunning
    # Explicit $null test: an empty string (GUI was running, owner unreadable) is
    # falsy in PowerShell, and that case still owes the user a relaunch.
    if ($null -ne $guiUser) { Restart-Tray $guiUser }

    Remove-Item $tmp -ErrorAction SilentlyContinue
}

# ── Entry point ──────────────────────────────────────────────────────────────
try {
    switch ($PSCmdlet.ParameterSetName) {
        'Register'   { Register-UpdateTask }
        'Unregister' { Unregister-UpdateTask }
        default      { Invoke-Update -CheckOnly:$Check }
    }
} catch {
    Write-Log "error: $($_.Exception.Message)"
    exit 1
}
