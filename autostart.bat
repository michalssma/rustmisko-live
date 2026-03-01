@echo off
REM ============================================================
REM RustMiskoLive — Windows Startup Batch (v5.0)
REM Tento soubor se přidá do Task Scheduler pro auto-start
REM Spustí: feed-hub, executor, alert-bot
REM ============================================================

cd /d C:\RustMiskoLive

REM Create logs dir if missing
if not exist logs mkdir logs

REM ============================================================
REM Environment variables
REM ============================================================
set RUST_LOG=info
set FEED_DB_PATH=data/feed.db
set FEED_HUB_BIND=0.0.0.0:8080
set FEED_HTTP_BIND=0.0.0.0:8081

REM Telegram
set TELEGRAM_BOT_TOKEN=7611316975:AAG_bStGX283uHCdog96y07eQfyyBhOGYuk
set TELEGRAM_CHAT_ID=6458129071

REM Feed Hub + Executor URLs
set FEED_HUB_URL=http://127.0.0.1:8081
set EXECUTOR_URL=http://127.0.0.1:3030

REM Executor on-chain config (BEZ TOHO = DRY-RUN!)
set PRIVATE_KEY=0x34fb468df8e14a223595b824c1515f0477d2f06b3f6509f25c2f9e9e02ce3f7c
set CHAIN_ID=137
set EXECUTOR_PORT=3030
set WS_STATE_GATE=true

REM ============================================================
REM Kill any old processes
REM ============================================================
echo Killing old processes...
taskkill /F /IM feed-hub.exe >nul 2>&1
taskkill /F /IM alert-bot.exe >nul 2>&1
REM Don't kill ALL node processes — only executor if needed
timeout /t 2 /nobreak >nul

REM ============================================================
REM 1. Start Feed Hub (RELEASE build)
REM ============================================================
echo Starting Feed Hub...
start /B "" "target\release\feed-hub.exe" > logs\feed_hub_auto.log 2>&1

REM Wait for feed-hub to initialize and start Azuro poller
timeout /t 8 /nobreak >nul

REM ============================================================
REM 2. Start Executor (Node.js)
REM ============================================================
if exist executor\index.js (
    echo Starting Executor...
    start /B "" node executor\index.js > logs\executor_auto.log 2>&1
    timeout /t 4 /nobreak >nul
) else (
    echo ERROR: executor\index.js nenalezen!
)

REM ============================================================
REM 3. Start Alert Bot (RELEASE build)
REM ============================================================
echo Starting Alert Bot...
start /B "" "target\release\alert-bot.exe" > logs\alert_bot_auto.log 2>&1

REM ============================================================
REM Done — verify
REM ============================================================
echo.
echo ============================================================
echo   RustMiskoLive v5.0 — ALL STARTED
echo   Feed Hub:  http://127.0.0.1:8081/state
echo   Executor:  http://127.0.0.1:3030/health
echo   Telegram:  @CSLiveMiskobot
echo ============================================================
echo.
echo Waiting 10s for system warm-up...
timeout /t 10 /nobreak >nul

REM Quick health check
curl -s http://127.0.0.1:8081/state >nul 2>&1 && echo [OK] Feed Hub responding || echo [FAIL] Feed Hub not responding
curl -s http://127.0.0.1:3030/health >nul 2>&1 && echo [OK] Executor responding || echo [FAIL] Executor not responding
tasklist /FI "IMAGENAME eq alert-bot.exe" 2>nul | find "alert-bot" >nul && echo [OK] Alert Bot running || echo [FAIL] Alert Bot not running

echo.
echo System ready. Press any key to close this window.
pause >nul
