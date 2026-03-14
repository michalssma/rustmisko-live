# watchdog.ps1 — RustMiskoLive process watchdog
#
# Design:
#   - 30s check interval      → CPU ≈ 0 %
#   - Log ONLY on state change → 0 disk I/O on healthy ticks
#   - Rolling 200-line log     → logs\watchdog.log stays ≤ 15 KB forever
#   - Backoff: 5 crashes       → 10-min cooldown (prevents restart-loop)
#   - GC every 100 iterations  → no PS memory drift in long-running loops
#
# Usage:
#   .\watchdog.ps1            (foreground, Ctrl+C to stop)
#   .\watchdog.ps1 -Background (start as hidden background process)

param([switch]$Background)

$ErrorActionPreference = 'SilentlyContinue'
$ROOT            = Split-Path -Parent $MyInvocation.MyCommand.Path
$LOG_FILE        = Join-Path $ROOT 'logs\watchdog.log'
$MAX_LOG_LINES   = 200
$CHECK_SEC       = 30
$MIN_UPTIME_SEC  = 60     # ignore if restarted < 60s ago (let process bind ports)
$MAX_CONSECUTIVE = 5      # crashes before cooldown
$COOLDOWN_MIN    = 10

function Is-NightWatchEnabled {
    return ($env:NIGHT_WATCH -eq 'true' -or $env:NIGHT_WATCH -eq '1')
}

# ── Re-launch self as hidden process ─────────────────────────────────────────
if ($Background) {
    $me = $MyInvocation.MyCommand.Path
    Start-Process powershell.exe `
        -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$me`"" `
        -WorkingDirectory $ROOT -WindowStyle Hidden
    Write-Host "[watchdog] Started in background." -ForegroundColor Cyan
    exit 0
}

# ── Rolling log (≤ MAX_LOG_LINES lines, log only on state change) ─────────────
function Write-WD([string]$level, [string]$msg) {
    $line = "$(Get-Date -Format 'yyyy-MM-ddTHH:mm:ss') [$level] $msg"
    $color = switch ($level) { 'ERROR' { 'Red' } 'WARN' { 'Yellow' } default { 'Cyan' } }
    Write-Host $line -ForegroundColor $color

    $existing = @()
    if (Test-Path $LOG_FILE) { $existing = Get-Content $LOG_FILE -ErrorAction SilentlyContinue }
    $keep = if ($existing.Count -ge $MAX_LOG_LINES) { $existing[-($MAX_LOG_LINES - 1)..-1] } else { $existing }
    ($keep + $line) | Set-Content $LOG_FILE -ErrorAction SilentlyContinue
}

# ── .env loader (mirrors start_system.ps1) ───────────────────────────────────
function Import-DotEnv([string]$path) {
    if (-not (Test-Path $path)) { return }
    Get-Content $path | ForEach-Object {
        $line = $_.Trim()
        if ($line.Length -eq 0 -or $line.StartsWith('#')) { return }
        $idx = $line.IndexOf('=')
        if ($idx -lt 1) { return }
        $k = $line.Substring(0, $idx).Trim()
        $v = $line.Substring($idx + 1).Trim()
        if ($v.Length -ge 2 -and (($v.StartsWith('"') -and $v.EndsWith('"')) -or ($v.StartsWith("'") -and $v.EndsWith("'")))) {
            $v = $v.Substring(1, $v.Length - 2)
        }
        if ($k.Length -gt 0) { Set-Item -Path "Env:$k" -Value $v -ErrorAction SilentlyContinue }
    }
}

# ── Telegram alert (only if tokens set) ─────────────────────────────────────
function Send-Alert([string]$text) {
    if (-not $env:TELEGRAM_BOT_TOKEN -or -not $env:TELEGRAM_CHAT_ID) { return }
    try {
        $body = @{ chat_id = $env:TELEGRAM_CHAT_ID; text = $text } | ConvertTo-Json
        Invoke-RestMethod -Method Post `
            -Uri "https://api.telegram.org/bot$($env:TELEGRAM_BOT_TOKEN)/sendMessage" `
            -ContentType 'application/json' -Body $body -TimeoutSec 5 | Out-Null
    } catch {}
}

# ── Process definitions (mirrors start_system.ps1) ───────────────────────────
$procs = @(
    @{
        Id      = 'feed-hub'
        Name    = 'feed-hub'
        Exe     = Join-Path $ROOT 'target\release\feed-hub.exe'
        Args    = $null
        WorkDir = $ROOT
        LogOut  = Join-Path $ROOT 'logs\feed_hub.log'
        LogErr  = Join-Path $ROOT 'logs\feed_hub_err.log'
    },
    @{
        Id      = 'alert-bot'
        Name    = 'alert-bot'
        Exe     = Join-Path $ROOT 'target\release\alert-bot.exe'
        Args    = $null
        WorkDir = $ROOT
        LogOut  = Join-Path $ROOT 'logs\alert_bot.log'
        LogErr  = Join-Path $ROOT 'logs\alert_bot_err.log'
    },
    @{
        Id      = 'executor'
        Name    = 'node'       # matched by cmdline, not bare name
        Exe     = 'node'
        Args    = @((Join-Path $ROOT 'executor\index.js'))
        WorkDir = Join-Path $ROOT 'executor'
        LogOut  = Join-Path $ROOT 'logs\executor.log'
        LogErr  = Join-Path $ROOT 'logs\executor_err.log'
    },
    @{
        Id      = 'dashboard'
        Name    = 'node'
        Exe     = 'node'
        Args    = @((Join-Path $ROOT 'dashboard\server.js'))
        WorkDir = Join-Path $ROOT 'dashboard'
        LogOut  = Join-Path $ROOT 'logs\dashboard.log'
        LogErr  = Join-Path $ROOT 'logs\dashboard_err.log'
    }
)

# ── Per-process state ─────────────────────────────────────────────────────────
$state = @{}
foreach ($p in $procs) {
    $state[$p.Id] = @{
        Restarts    = 0
        LastRestart = [DateTime]::MinValue
        PausedUntil = [DateTime]::MinValue
    }
}

# ── Liveness check ────────────────────────────────────────────────────────────
function Test-Alive([hashtable]$proc) {
    if ($proc.Id -eq 'executor') {
        $hit = Get-CimInstance Win32_Process -Filter "Name='node.exe'" -ErrorAction SilentlyContinue |
               Where-Object { $_.CommandLine -match 'executor\\index\.js' }
        return ($null -ne $hit -and @($hit).Count -gt 0)
    }
    if ($proc.Id -eq 'dashboard') {
        $hit = Get-CimInstance Win32_Process -Filter "Name='node.exe'" -ErrorAction SilentlyContinue |
               Where-Object { $_.CommandLine -and $_.CommandLine -match [regex]::Escape((Join-Path $ROOT 'dashboard\server.js')) }
        return ($null -ne $hit -and @($hit).Count -gt 0)
    }
    if ($proc.Id -eq 'night-watch') {
        $hit = Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
               Where-Object { ($_.Name -eq 'powershell.exe' -or $_.Name -eq 'pwsh.exe') -and $_.CommandLine -match 'night_watch\.ps1' }
        return ($null -ne $hit -and @($hit).Count -gt 0)
    }
    return ($null -ne (Get-Process -Name $proc.Name -ErrorAction SilentlyContinue))
}

# ── Restart process (same flags as start_system.ps1) ─────────────────────────
function Start-Proc([hashtable]$proc) {
    $params = @{
        FilePath            = $proc.Exe
        WorkingDirectory    = $proc.WorkDir
        WindowStyle         = 'Hidden'
        RedirectStandardOutput = $proc.LogOut
        RedirectStandardError  = $proc.LogErr
        ErrorAction         = 'SilentlyContinue'
    }
    if ($proc.Args) { $params.ArgumentList = $proc.Args }
    Start-Process @params
}

# ── Init ──────────────────────────────────────────────────────────────────────
Import-DotEnv (Join-Path $ROOT '.env')

if (-not $env:PRIVATE_KEY) {
    Write-WD 'ERROR' 'PRIVATE_KEY not set — watchdog cannot safely restart processes. Exiting.'
    exit 1
}

# Fill defaults (same as start_system.ps1)
if (-not $env:RUST_LOG)       { $env:RUST_LOG = 'info' }
if (-not $env:FEED_HUB_BIND)  { $env:FEED_HUB_BIND = '0.0.0.0:8080' }
if (-not $env:FEED_HTTP_BIND) { $env:FEED_HTTP_BIND = '0.0.0.0:8081' }
if (-not $env:FEED_HUB_URL)   { $env:FEED_HUB_URL = 'http://127.0.0.1:8081' }
if (-not $env:EXECUTOR_URL)   { $env:EXECUTOR_URL = 'http://127.0.0.1:3030' }
if (-not $env:CHAIN_ID)       { $env:CHAIN_ID = '137' }
if (-not $env:EXECUTOR_PORT)  { $env:EXECUTOR_PORT = '3030' }
$legacyWsGateOptIn = ($env:LEGACY_WS_GATE -eq 'true' -or $env:LEGACY_WS_GATE -eq '1')
if (($env:WS_STATE_GATE -eq 'true' -or $env:WS_STATE_GATE -eq '1') -and -not $legacyWsGateOptIn) {
    Write-WD 'WARN' 'Ignoring stale WS_STATE_GATE=true from environment. Use LEGACY_WS_GATE=true for explicit legacy opt-in.'
}
$env:WS_STATE_GATE = if ($legacyWsGateOptIn) { 'true' } else { 'false' }

if (Is-NightWatchEnabled) {
    $procs += @{
        Id      = 'night-watch'
        Name    = 'powershell'
        Exe     = 'powershell.exe'
        Args    = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $ROOT 'night_watch.ps1'))
        WorkDir = $ROOT
        LogOut  = Join-Path $ROOT 'logs\night_watch_stdout.log'
        LogErr  = Join-Path $ROOT 'logs\night_watch_monitor.log'
    }
}

Write-WD 'INFO' "Watchdog started. interval=${CHECK_SEC}s backoff=${MAX_CONSECUTIVE}x->${COOLDOWN_MIN}min log-cap=${MAX_LOG_LINES}lines"
$watchList = @('feed-hub', 'alert-bot', 'executor', 'dashboard')
if (Is-NightWatchEnabled) {
    $watchList += 'night-watch'
}
Send-Alert "[Watchdog] STARTED. Monitoring: $($watchList -join ', ')"

# ── Main loop ─────────────────────────────────────────────────────────────────
$iter = 0

while ($true) {
    $iter++

    foreach ($proc in $procs) {
        $st  = $state[$proc.Id]
        $now = [DateTime]::UtcNow

        # In backoff cooldown?
        if ($now -lt $st.PausedUntil) { continue }

        # Process alive → nothing to do (no log, no I/O)
        if (Test-Alive $proc) { continue }

        # ── Process is dead ──────────────────────────────────────────────────

        # Too soon after last restart? Give it time to fully bind ports.
        if (($now - $st.LastRestart).TotalSeconds -lt $MIN_UPTIME_SEC) { continue }

        $st.Restarts++

        if ($st.Restarts -gt $MAX_CONSECUTIVE) {
            # Enter cooldown — stop hammering a broken process
            $st.PausedUntil = $now.AddMinutes($COOLDOWN_MIN)
            $st.Restarts    = 0
            $m = "$($proc.Id) crashed $MAX_CONSECUTIVE times. Cooling down ${COOLDOWN_MIN}min."
            Write-WD 'WARN' $m
            Send-Alert "[Watchdog] WARN: $m"
            continue
        }

        Write-WD 'WARN' "$($proc.Id) dead - restarting (attempt $($st.Restarts) of $MAX_CONSECUTIVE)"
        Send-Alert "[Watchdog] $($proc.Id) restarted (attempt $($st.Restarts)/$MAX_CONSECUTIVE)"

        Start-Proc $proc
        $st.LastRestart = [DateTime]::UtcNow

        # Brief pause to let the new process initialize before next check
        Start-Sleep -Seconds 3
    }

    # Prevent PowerShell memory drift in long while-loops
    if ($iter % 100 -eq 0) {
        [GC]::Collect()
        [GC]::WaitForPendingFinalizers()
    }

    Start-Sleep -Seconds $CHECK_SEC
}
