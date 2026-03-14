param (
    [switch]$Stop
)

$ErrorActionPreference = 'Continue'
$ROOT = Split-Path -Parent $MyInvocation.MyCommand.Path

function Is-NightWatchEnabled {
    return ($env:NIGHT_WATCH -eq 'true' -or $env:NIGHT_WATCH -eq '1')
}

$FEED_HUB_EXE = Join-Path $ROOT 'target\release\feed-hub.exe'
$ALERT_BOT_EXE = Join-Path $ROOT 'target\release\alert-bot.exe'
$EXECUTOR_DIR = Join-Path $ROOT 'executor'
$EXECUTOR_SCRIPT = Join-Path $EXECUTOR_DIR 'index.js'
$DASHBOARD_DIR = Join-Path $ROOT 'dashboard'
$DASHBOARD_SCRIPT = Join-Path $DASHBOARD_DIR 'server.js'
$LOG_DIR = Join-Path $ROOT 'logs'

if (-not (Test-Path $LOG_DIR)) {
    New-Item -ItemType Directory -Path $LOG_DIR -Force | Out-Null
}

function Import-DotEnv {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path
    )

    if (-not (Test-Path $Path)) { return }

    Get-Content $Path | ForEach-Object {
        $line = $_.Trim()
        if ($line.Length -eq 0) { return }
        if ($line.StartsWith('#')) { return }

        $idx = $line.IndexOf('=')
        if ($idx -lt 1) { return }

        $key = $line.Substring(0, $idx).Trim()
        $val = $line.Substring($idx + 1).Trim()

        if ($val.StartsWith('"') -and $val.EndsWith('"') -and $val.Length -ge 2) {
            $val = $val.Substring(1, $val.Length - 2)
        }
        if ($val.StartsWith("'") -and $val.EndsWith("'") -and $val.Length -ge 2) {
            $val = $val.Substring(1, $val.Length - 2)
        }

        if ($key.Length -gt 0) {
            Set-Item -Path "Env:$key" -Value $val
        }
    }
}

function Get-ManagedProcessIds {
    param(
        [string]$ImageName,
        [string]$CommandLinePattern,
        [int]$ListeningPort = 0
    )

    $ids = New-Object System.Collections.Generic.List[int]

    if ($ImageName -and $CommandLinePattern) {
        try {
            $escapedPattern = [regex]::Escape($CommandLinePattern)
            $matches = Get-CimInstance Win32_Process -Filter "Name = '$ImageName'" -ErrorAction SilentlyContinue |
                Where-Object { $_.CommandLine -and $_.CommandLine -match $escapedPattern }
            foreach ($match in $matches) {
                if ($match.ProcessId -and -not $ids.Contains([int]$match.ProcessId)) {
                    $ids.Add([int]$match.ProcessId) | Out-Null
                }
            }
        } catch {}
    }

    if ($ListeningPort -gt 0) {
        try {
            $portOwners = Get-NetTCPConnection -LocalPort $ListeningPort -State Listen -ErrorAction SilentlyContinue |
                Select-Object -ExpandProperty OwningProcess -Unique
            foreach ($owner in $portOwners) {
                if ($owner -and -not $ids.Contains([int]$owner)) {
                    $ids.Add([int]$owner) | Out-Null
                }
            }
        } catch {}
    }

    return $ids
}

function Stop-ProcessTree {
    param([int]$ProcessId)

    if ($ProcessId -le 0) { return }

    try {
        Start-Process -FilePath 'taskkill.exe' -ArgumentList @('/PID', $ProcessId, '/T', '/F') -WindowStyle Hidden -Wait | Out-Null
    } catch {
        try {
            Stop-Process -Id $ProcessId -Force -ErrorAction SilentlyContinue
        } catch {}
    }
}

function Wait-UntilStopped {
    param(
        [scriptblock]$Probe,
        [int]$TimeoutSeconds = 12
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $remaining = & $Probe
        if (-not $remaining -or @($remaining).Count -eq 0) {
            return $true
        }
        Start-Sleep -Milliseconds 500
    }
    return $false
}

function Wait-HttpOk {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Url,
        [int]$TimeoutSeconds = 20
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $Url -TimeoutSec 5 | Out-Null
            return $true
        } catch {}
        Start-Sleep -Seconds 1
    }
    return $false
}

function Stop-SystemProcesses {
    $managedTargets = @(
        @{ Name = 'powershell.exe'; Script = (Join-Path $ROOT 'watchdog.ps1'); Port = 0 },
        @{ Name = 'pwsh.exe';       Script = (Join-Path $ROOT 'watchdog.ps1'); Port = 0 },
        @{ Name = 'powershell.exe'; Script = (Join-Path $ROOT 'night_watch.ps1'); Port = 0 },
        @{ Name = 'pwsh.exe';       Script = (Join-Path $ROOT 'night_watch.ps1'); Port = 0 },
        @{ Name = 'node.exe';       Script = $DASHBOARD_SCRIPT; Port = 7777 },
        @{ Name = 'node.exe';       Script = $EXECUTOR_SCRIPT; Port = 3030 }
    )

    foreach ($target in $managedTargets) {
        $pids = Get-ManagedProcessIds -ImageName $target.Name -CommandLinePattern $target.Script -ListeningPort $target.Port
        foreach ($processId in $pids) {
            Stop-ProcessTree -ProcessId $processId
        }
    }

    Get-Process -Name 'feed-hub' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Get-Process -Name 'alert-bot' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Get-Process -Name 'alert_bot' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

    [void](Wait-UntilStopped -Probe {
        $remaining = @()
        $remaining += Get-Process -Name 'feed-hub' -ErrorAction SilentlyContinue
        $remaining += Get-Process -Name 'alert-bot' -ErrorAction SilentlyContinue
        $remaining += Get-Process -Name 'alert_bot' -ErrorAction SilentlyContinue
        $remaining += Get-ManagedProcessIds -ImageName 'node.exe' -CommandLinePattern $EXECUTOR_SCRIPT -ListeningPort 3030
        $remaining += Get-ManagedProcessIds -ImageName 'node.exe' -CommandLinePattern $DASHBOARD_SCRIPT -ListeningPort 7777
        $remaining += Get-ManagedProcessIds -ImageName 'powershell.exe' -CommandLinePattern (Join-Path $ROOT 'watchdog.ps1')
        $remaining += Get-ManagedProcessIds -ImageName 'pwsh.exe' -CommandLinePattern (Join-Path $ROOT 'watchdog.ps1')
        $remaining += Get-ManagedProcessIds -ImageName 'powershell.exe' -CommandLinePattern (Join-Path $ROOT 'night_watch.ps1')
        $remaining += Get-ManagedProcessIds -ImageName 'pwsh.exe' -CommandLinePattern (Join-Path $ROOT 'night_watch.ps1')
        return @($remaining | Select-Object -Unique)
    })
}

if ($Stop) {
    Write-Host '[STOP] Stopping system...' -ForegroundColor Yellow
    Stop-SystemProcesses
    Write-Host '[STOP] Done.' -ForegroundColor Green
    exit 0
}

Write-Host '=== RustMiskoLive System Start ===' -ForegroundColor Cyan
Write-Host '[1/6] Cleaning old processes...' -ForegroundColor Yellow
Stop-SystemProcesses
Start-Sleep -Seconds 2

# Optional: load local secrets/config from .env (ignored by git)
$dotenvPath = Join-Path $ROOT '.env'
if (Test-Path $dotenvPath) {
    Import-DotEnv -Path $dotenvPath
}

if (-not $env:RUST_LOG) { $env:RUST_LOG = 'info' }
if (-not $env:FEED_DB_PATH) { $env:FEED_DB_PATH = 'data/feed.db' }
if (-not $env:FEED_HUB_BIND) { $env:FEED_HUB_BIND = '0.0.0.0:8080' }
if (-not $env:FEED_HTTP_BIND) { $env:FEED_HTTP_BIND = '0.0.0.0:8081' }
if (-not $env:FEED_HUB_URL) { $env:FEED_HUB_URL = 'http://127.0.0.1:8081' }
if (-not $env:EXECUTOR_URL) { $env:EXECUTOR_URL = 'http://127.0.0.1:3030' }
if (-not $env:CHAIN_ID) { $env:CHAIN_ID = '137' }
if (-not $env:EXECUTOR_PORT) { $env:EXECUTOR_PORT = '3030' }

# Legacy alert-bot WS gate is an opt-in debug fallback only.
# Ignore stale WS_STATE_GATE values from .env unless the new explicit opt-in is present.
$legacyWsGateOptIn = ($env:LEGACY_WS_GATE -eq 'true' -or $env:LEGACY_WS_GATE -eq '1')
if (($env:WS_STATE_GATE -eq 'true' -or $env:WS_STATE_GATE -eq '1') -and -not $legacyWsGateOptIn) {
    Write-Host 'WARN: Ignoring stale WS_STATE_GATE=true from environment. Use LEGACY_WS_GATE=true for explicit legacy opt-in.' -ForegroundColor Yellow
}
$env:WS_STATE_GATE = if ($legacyWsGateOptIn) { 'true' } else { 'false' }

# Secrets MUST NOT be hardcoded in repo.
# Required for live on-chain execution:
if (-not $env:PRIVATE_KEY) {
    Write-Host ''
    Write-Host 'ERROR: PRIVATE_KEY neni nastaveny.' -ForegroundColor Red
    Write-Host '  Nastav ho lokalne pred spustenim, nebo vytvor .env v rootu (gitignored) s radkem:' -ForegroundColor Yellow
    Write-Host '    PRIVATE_KEY=0x...' -ForegroundColor Yellow
    Write-Host ''
    exit 1
}

# Telegram is optional (alerts will be disabled if missing)
if (-not $env:TELEGRAM_BOT_TOKEN -or -not $env:TELEGRAM_CHAT_ID) {
    Write-Host 'WARN: TELEGRAM_BOT_TOKEN/TELEGRAM_CHAT_ID nejsou nastaveny (telegram alerty budou vypnute).' -ForegroundColor Yellow
}

Write-Host '[2/6] Starting feed-hub...' -ForegroundColor Green
$feedHubLog = Join-Path $LOG_DIR 'feed_hub.log'
$feedHubErr = Join-Path $LOG_DIR 'feed_hub_err.log'
Start-Process -FilePath $FEED_HUB_EXE -WorkingDirectory $ROOT -WindowStyle Hidden -RedirectStandardOutput $feedHubLog -RedirectStandardError $feedHubErr
Start-Sleep -Seconds 3

$feedProc = Get-Process -Name 'feed-hub' -ErrorAction SilentlyContinue
if ($feedProc) {
    Write-Host "  feed-hub OK (PID: $($feedProc.Id))" -ForegroundColor Green
} else {
    Write-Host '  ERROR: feed-hub did not start' -ForegroundColor Red
    exit 1
}

Write-Host '[3/6] Starting executor...' -ForegroundColor Green
if (Test-Path $EXECUTOR_SCRIPT) {
    $executorLog = Join-Path $LOG_DIR 'executor.log'
    $executorErr = Join-Path $LOG_DIR 'executor_err.log'
    Start-Process -FilePath 'node' -ArgumentList $EXECUTOR_SCRIPT -WorkingDirectory $EXECUTOR_DIR -WindowStyle Hidden -RedirectStandardOutput $executorLog -RedirectStandardError $executorErr
    if (Wait-HttpOk -Url 'http://127.0.0.1:3030/health' -TimeoutSeconds 25) {
        $exHealth = Invoke-RestMethod -Uri 'http://127.0.0.1:3030/health' -TimeoutSec 5
        $wallet = if ($exHealth.wallet) { $exHealth.wallet } else { 'unknown' }
        $pmAllow = if ($exHealth.paymasterAllowance) { $exHealth.paymasterAllowance } else { 'n/a' }
        Write-Host "  executor OK (wallet: $wallet, balance: $($exHealth.balance) USDT, paymasterAllowance: $pmAllow)" -ForegroundColor Green

        if ($env:EXPECTED_WALLET_ADDRESS) {
            $expected = $env:EXPECTED_WALLET_ADDRESS.Trim()
            if ($expected.Length -gt 0 -and $wallet -ne 'unknown') {
                if ($wallet.ToLowerInvariant() -ne $expected.ToLowerInvariant()) {
                    Write-Host "  ERROR: Executor wallet mismatch! expected=$expected actual=$wallet" -ForegroundColor Red
                    Write-Host "  Zkontroluj PRIVATE_KEY v .env / env promennych (NESDILEJ ho v chatu)." -ForegroundColor Yellow
                    Stop-SystemProcesses
                    exit 1
                }
            }
        }
    } else {
        Write-Host '  executor health check failed, check logs/executor.log' -ForegroundColor Yellow
        Stop-SystemProcesses
        exit 1
    }
} else {
    Write-Host "  ERROR: executor script not found ($EXECUTOR_SCRIPT)" -ForegroundColor Red
    Stop-SystemProcesses
    exit 1
}

Write-Host '[4/6] Starting dashboard...' -ForegroundColor Green
if (Test-Path $DASHBOARD_SCRIPT) {
    $dashboardLog = Join-Path $LOG_DIR 'dashboard.log'
    $dashboardErr = Join-Path $LOG_DIR 'dashboard_err.log'
    Start-Process -FilePath 'node' -ArgumentList $DASHBOARD_SCRIPT -WorkingDirectory $DASHBOARD_DIR -WindowStyle Hidden -RedirectStandardOutput $dashboardLog -RedirectStandardError $dashboardErr
    if (Wait-HttpOk -Url 'http://127.0.0.1:7777/login.html' -TimeoutSeconds 20) {
        Write-Host '  dashboard OK (http://127.0.0.1:7777/login.html)' -ForegroundColor Green
    } else {
        Write-Host '  ERROR: dashboard did not start' -ForegroundColor Red
        Stop-SystemProcesses
        exit 1
    }
} else {
    Write-Host "  ERROR: dashboard script not found ($DASHBOARD_SCRIPT)" -ForegroundColor Red
    Stop-SystemProcesses
    exit 1
}

Write-Host '[5/6] Starting alert-bot...' -ForegroundColor Green
$alertLog = Join-Path $LOG_DIR 'alert_bot.log'
$alertErr = Join-Path $LOG_DIR 'alert_bot_err.log'
Start-Process -FilePath $ALERT_BOT_EXE -WorkingDirectory $ROOT -WindowStyle Hidden -RedirectStandardOutput $alertLog -RedirectStandardError $alertErr
Start-Sleep -Seconds 3

$alertProc = Get-Process -Name 'alert-bot' -ErrorAction SilentlyContinue
if (-not $alertProc) {
    $alertProc = Get-Process -Name 'alert_bot' -ErrorAction SilentlyContinue
}

if ($alertProc) {
    Write-Host "  alert-bot OK (PID: $($alertProc.Id))" -ForegroundColor Green
} else {
    Write-Host '  ERROR: alert-bot did not start' -ForegroundColor Red
    Stop-SystemProcesses
    exit 1
}

Write-Host '[6/6] Starting watchdog...' -ForegroundColor Green
$watchdogScript = Join-Path $ROOT 'watchdog.ps1'
if (Test-Path $watchdogScript) {
    Start-Process powershell.exe -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$watchdogScript`"" -WorkingDirectory $ROOT -WindowStyle Hidden
    Start-Sleep -Seconds 1
    Write-Host '  watchdog OK (background, 30s intervals)' -ForegroundColor Green
} else {
    Write-Host '  watchdog.ps1 not found, skipping' -ForegroundColor Yellow
}

Write-Host '[7/7] Starting night-watch...' -ForegroundColor Green
$nightWatchScript = Join-Path $ROOT 'night_watch.ps1'
if (-not (Is-NightWatchEnabled)) {
    Write-Host '  night-watch skipped (set NIGHT_WATCH=true for overnight monitor mode)' -ForegroundColor Yellow
} elseif (Test-Path $nightWatchScript) {
    Start-Process powershell.exe -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$nightWatchScript`"" -WorkingDirectory $ROOT -WindowStyle Hidden
    Start-Sleep -Seconds 1
    Write-Host '  night-watch OK (background, 10min monitor loop)' -ForegroundColor Green
} else {
    Write-Host '  night_watch.ps1 not found, skipping' -ForegroundColor Yellow
}

Write-Host ''
Write-Host '=== Health Check ===' -ForegroundColor Cyan
Start-Sleep -Seconds 5

try {
    Invoke-RestMethod -Uri 'http://127.0.0.1:8081/health' -TimeoutSec 5 | Out-Null
    Write-Host '  feed-hub: ONLINE' -ForegroundColor Green
} catch {
    Write-Host '  feed-hub: OFFLINE' -ForegroundColor Red
}

try {
    Invoke-RestMethod -Uri 'http://127.0.0.1:3030/health' -TimeoutSec 5 | Out-Null
    Write-Host '  executor: ONLINE' -ForegroundColor Green
} catch {
    Write-Host '  executor: OFFLINE' -ForegroundColor Red
}

try {
    Invoke-WebRequest -UseBasicParsing -Uri 'http://127.0.0.1:7777/login.html' -TimeoutSec 5 | Out-Null
    Write-Host '  dashboard: ONLINE' -ForegroundColor Green
} catch {
    Write-Host '  dashboard: OFFLINE' -ForegroundColor Red
}

Write-Host ''
Write-Host '=== SYSTEM RUNNING ===' -ForegroundColor Cyan
Write-Host '  Feed Hub:  http://127.0.0.1:8081/state' -ForegroundColor White
Write-Host '  Executor:  http://127.0.0.1:3030' -ForegroundColor White
Write-Host '  Dashboard: http://127.0.0.1:7777/login.html' -ForegroundColor White
Write-Host "  Logs:      $LOG_DIR" -ForegroundColor White
Write-Host ''
Write-Host 'Stop: .\start_system.ps1 -Stop' -ForegroundColor Yellow
Write-Host 'Watch: Invoke-RestMethod http://127.0.0.1:8081/state' -ForegroundColor Yellow
