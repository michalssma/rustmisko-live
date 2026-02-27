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

function Stop-SystemProcesses {
    Get-Process -Name 'feed-hub' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Get-Process -Name 'alert-bot' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Get-Process -Name 'alert_bot' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Get-Process -Name 'node' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
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

$env:RUST_LOG = 'info'
$env:FEED_DB_PATH = 'data/feed.db'
$env:FEED_HUB_BIND = '0.0.0.0:8080'
$env:FEED_HTTP_BIND = '0.0.0.0:8081'
$env:TELEGRAM_BOT_TOKEN = '7611316975:AAG_bStGX283uHCdog96y07eQfyyBhOGYuk'
$env:TELEGRAM_CHAT_ID = '6458129071'
$env:FEED_HUB_URL = 'http://127.0.0.1:8081'
$env:EXECUTOR_URL = 'http://127.0.0.1:3030'
$env:PRIVATE_KEY = '0x34fb468df8e14a223595b824c1515f0477d2f06b3f6509f25c2f9e9e02ce3f7c'
$env:CHAIN_ID = '137'
$env:EXECUTOR_PORT = '3030'

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
        Write-Host "  executor OK (balance: $($exHealth.balance) USDT)" -ForegroundColor Green
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
