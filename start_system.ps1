# ============================================================
# RustMiskoLive — AUTO-START SCRIPT (24/7)
# ============================================================
# Spustí celý arbitrážní systém:
#   1. Feed Hub (WS 8080 + HTTP 8081)
#   2. Alert Bot (Telegram + auto-bet)
#   3. Executor (Node.js port 3030)
#
# Použití:
#   .\start_system.ps1           — spustí vše
#   .\start_system.ps1 -Stop     — zastaví vše
# ============================================================

param (
    [switch]$Stop
)

$ErrorActionPreference = "Continue"
$ROOT = Split-Path -Parent $MyInvocation.MyCommand.Path

# === PROCESS NAMES ===
$FEED_HUB_EXE = Join-Path $ROOT "target\debug\feed-hub.exe"
$ALERT_BOT_EXE = Join-Path $ROOT "target\debug\alert_bot.exe"
$EXECUTOR_DIR = Join-Path $ROOT "executor"
$LOG_DIR = Join-Path $ROOT "logs"

if (-not (Test-Path $LOG_DIR)) { New-Item -ItemType Directory -Path $LOG_DIR -Force | Out-Null }

# === STOP MODE ===
if ($Stop) {
    Write-Host "[STOP] Zastavuji system..." -ForegroundColor Yellow
    Get-Process -Name "feed-hub" -ErrorAction SilentlyContinue | Stop-Process -Force
    Get-Process -Name "alert_bot" -ErrorAction SilentlyContinue | Stop-Process -Force
    Get-Process -Name "node" -ErrorAction SilentlyContinue | Where-Object {
        $_.MainWindowTitle -like "*executor*" -or $_.CommandLine -like "*executor*"
    } | Stop-Process -Force -ErrorAction SilentlyContinue
    Write-Host "[STOP] Hotovo." -ForegroundColor Green
    exit 0
}

# === KILL EXISTING ===
Write-Host "=== RustMiskoLive System Start ===" -ForegroundColor Cyan
Write-Host "[1/4] Cistim stare procesy..." -ForegroundColor Yellow
Get-Process -Name "feed-hub" -ErrorAction SilentlyContinue | Stop-Process -Force
Get-Process -Name "alert_bot" -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2

# === ENV VARIABLES ===
$env:RUST_LOG = "info"
$env:FEED_DB_PATH = "data/feed.db"
$env:FEED_HUB_BIND = "0.0.0.0:8080"
$env:FEED_HTTP_BIND = "0.0.0.0:8081"
$env:TELEGRAM_BOT_TOKEN = "7611316975:AAG_bStGX283uHCdog96y07eQfyyBhOGYuk"
$env:TELEGRAM_CHAT_ID = "6458129071"
$env:FEED_HUB_URL = "http://127.0.0.1:8081"
$env:EXECUTOR_URL = "http://127.0.0.1:3030"

# === START FEED HUB ===
Write-Host "[2/4] Startuji Feed Hub (WS:8080 HTTP:8081)..." -ForegroundColor Green
$feedHubLog = Join-Path $LOG_DIR "feed_hub.log"
Start-Process -FilePath $FEED_HUB_EXE -WorkingDirectory $ROOT -WindowStyle Hidden `
    -RedirectStandardOutput $feedHubLog -RedirectStandardError (Join-Path $LOG_DIR "feed_hub_err.log")
Start-Sleep -Seconds 3

# Verify feed-hub is running
$feedProc = Get-Process -Name "feed-hub" -ErrorAction SilentlyContinue
if ($feedProc) {
    Write-Host "  Feed Hub OK (PID: $($feedProc.Id))" -ForegroundColor Green
} else {
    Write-Host "  CHYBA: Feed Hub se nespustil!" -ForegroundColor Red
    exit 1
}

# === START EXECUTOR ===
Write-Host "[3/4] Startuji Executor (port 3030)..." -ForegroundColor Green
$executorScript = Join-Path $EXECUTOR_DIR "executor.js"
if (Test-Path $executorScript) {
    $executorLog = Join-Path $LOG_DIR "executor.log"
    Start-Process -FilePath "node" -ArgumentList $executorScript -WorkingDirectory $EXECUTOR_DIR -WindowStyle Hidden `
        -RedirectStandardOutput $executorLog -RedirectStandardError (Join-Path $LOG_DIR "executor_err.log")
    Start-Sleep -Seconds 2
    Write-Host "  Executor OK" -ForegroundColor Green
} else {
    Write-Host "  Executor script nenalezen ($executorScript) — preskakuji" -ForegroundColor Yellow
}

# === START ALERT BOT ===
Write-Host "[4/4] Startuji Alert Bot (Telegram + auto-bet)..." -ForegroundColor Green
$alertLog = Join-Path $LOG_DIR "alert_bot.log"
Start-Process -FilePath $ALERT_BOT_EXE -WorkingDirectory $ROOT -WindowStyle Hidden `
    -RedirectStandardOutput $alertLog -RedirectStandardError (Join-Path $LOG_DIR "alert_bot_err.log")
Start-Sleep -Seconds 3

$alertProc = Get-Process -Name "alert_bot" -ErrorAction SilentlyContinue
if ($alertProc) {
    Write-Host "  Alert Bot OK (PID: $($alertProc.Id))" -ForegroundColor Green
} else {
    Write-Host "  CHYBA: Alert Bot se nespustil!" -ForegroundColor Red
}

# === HEALTH CHECK ===
Write-Host "" -ForegroundColor White
Write-Host "=== System Health Check ===" -ForegroundColor Cyan
Start-Sleep -Seconds 5
try {
    $health = Invoke-RestMethod -Uri "http://127.0.0.1:8081/health" -TimeoutSec 5
    Write-Host "  Feed Hub: ONLINE" -ForegroundColor Green
} catch {
    Write-Host "  Feed Hub: OFFLINE!" -ForegroundColor Red
}

Write-Host ""
Write-Host "=== SYSTEM BEZI ===" -ForegroundColor Cyan
Write-Host "  Feed Hub:  http://127.0.0.1:8081/state" -ForegroundColor White
Write-Host "  Executor:  http://127.0.0.1:3030" -ForegroundColor White
Write-Host "  Logy:      $LOG_DIR" -ForegroundColor White
Write-Host ""
Write-Host "Pro zastaveni: .\start_system.ps1 -Stop" -ForegroundColor Yellow
Write-Host "Pro sledovani: Invoke-RestMethod http://127.0.0.1:8081/state" -ForegroundColor Yellow
