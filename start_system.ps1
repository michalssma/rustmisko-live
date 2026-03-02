param (
    [switch]$Stop
)

$ErrorActionPreference = 'Continue'
$ROOT = Split-Path -Parent $MyInvocation.MyCommand.Path

$FEED_HUB_EXE = Join-Path $ROOT 'target\release\feed-hub.exe'
$ALERT_BOT_EXE = Join-Path $ROOT 'target\release\alert-bot.exe'
$EXECUTOR_DIR = Join-Path $ROOT 'executor'
$EXECUTOR_SCRIPT = Join-Path $EXECUTOR_DIR 'index.js'
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

function Stop-SystemProcesses {
    Get-Process -Name 'feed-hub' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Get-Process -Name 'alert-bot' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Get-Process -Name 'alert_bot' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

    # Stop only the executor node process (avoid killing unrelated node.exe)
    try {
        $executorNodes = Get-CimInstance Win32_Process |
            Where-Object { $_.Name -eq 'node.exe' -and $_.CommandLine -match 'executor\\index\.js' }

        foreach ($p in $executorNodes) {
            try {
                Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
            } catch {}
        }
    } catch {}
}

if ($Stop) {
    Write-Host '[STOP] Stopping system...' -ForegroundColor Yellow
    Stop-SystemProcesses
    Write-Host '[STOP] Done.' -ForegroundColor Green
    exit 0
}

Write-Host '=== RustMiskoLive System Start ===' -ForegroundColor Cyan
Write-Host '[1/4] Cleaning old processes...' -ForegroundColor Yellow
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
if (-not $env:WS_STATE_GATE) { $env:WS_STATE_GATE = 'true' }

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

Write-Host '[2/4] Starting feed-hub...' -ForegroundColor Green
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

Write-Host '[3/4] Starting executor...' -ForegroundColor Green
if (Test-Path $EXECUTOR_SCRIPT) {
    $executorLog = Join-Path $LOG_DIR 'executor.log'
    $executorErr = Join-Path $LOG_DIR 'executor_err.log'
    Start-Process -FilePath 'node' -ArgumentList $EXECUTOR_SCRIPT -WorkingDirectory $EXECUTOR_DIR -WindowStyle Hidden -RedirectStandardOutput $executorLog -RedirectStandardError $executorErr
    Start-Sleep -Seconds 3
    try {
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
    } catch {
        Write-Host '  executor health check failed, check logs/executor.log' -ForegroundColor Yellow
    }
} else {
    Write-Host "  ERROR: executor script not found ($EXECUTOR_SCRIPT)" -ForegroundColor Red
}

Write-Host '[4/4] Starting alert-bot...' -ForegroundColor Green
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

Write-Host ''
Write-Host '=== SYSTEM RUNNING ===' -ForegroundColor Cyan
Write-Host '  Feed Hub:  http://127.0.0.1:8081/state' -ForegroundColor White
Write-Host '  Executor:  http://127.0.0.1:3030' -ForegroundColor White
Write-Host "  Logs:      $LOG_DIR" -ForegroundColor White
Write-Host ''
Write-Host 'Stop: .\start_system.ps1 -Stop' -ForegroundColor Yellow
Write-Host 'Watch: Invoke-RestMethod http://127.0.0.1:8081/state' -ForegroundColor Yellow
