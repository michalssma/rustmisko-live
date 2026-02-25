@echo off
REM ============================================================
REM RustMiskoLive — Windows Startup Batch
REM Tento soubor se přidá do Task Scheduler pro auto-start
REM ============================================================

cd /d C:\RustMiskoLive

REM Set environment
set RUST_LOG=info
set FEED_DB_PATH=data/feed.db
set FEED_HUB_BIND=0.0.0.0:8080
set FEED_HTTP_BIND=0.0.0.0:8081
set TELEGRAM_BOT_TOKEN=7611316975:AAG_bStGX283uHCdog96y07eQfyyBhOGYuk
set TELEGRAM_CHAT_ID=6458129071
set FEED_HUB_URL=http://127.0.0.1:8081
set EXECUTOR_URL=http://127.0.0.1:3030

REM Start Feed Hub
echo Starting Feed Hub...
start /B "" "target\debug\feed-hub.exe" > logs\feed_hub_auto.log 2>&1

REM Wait for feed-hub to initialize
timeout /t 5 /nobreak > nul

REM Start Executor (if exists)
if exist executor\executor.js (
    echo Starting Executor...
    start /B "" node executor\executor.js > logs\executor_auto.log 2>&1
    timeout /t 3 /nobreak > nul
)

REM Start Alert Bot
echo Starting Alert Bot...
start /B "" "target\debug\alert_bot.exe" > logs\alert_bot_auto.log 2>&1

echo System started. Check http://127.0.0.1:8081/health
timeout /t 10 /nobreak > nul

REM Open Chrome with FlashScore tabs (optional)
REM start chrome "https://www.flashscore.com/tennis/" "https://www.flashscore.com/football/" "https://www.flashscore.com/basketball/" "https://www.flashscore.com/hockey/" "https://www.flashscore.com/esports/"
