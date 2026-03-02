@echo off
setlocal EnableExtensions
REM ============================================================
REM RustMiskoLive — Windows Startup Batch (safe)
REM Spousti cely system pres start_system.ps1.
REM Tajne veci (PRIVATE_KEY, Telegram tokeny, ...) nesmi byt v repu.
REM Ty patri do .env v rootu (gitignored).
REM ============================================================

cd /d "%~dp0"

if not exist logs mkdir logs

if not exist .env (
    echo ERROR: chybi .env v rootu projektu.
    echo Vytvor .env podle .env.example a dopln PRIVATE_KEY (a pripadne Telegram).
    exit /b 1
)

echo Starting system via start_system.ps1...
powershell -NoProfile -ExecutionPolicy Bypass -File ".\start_system.ps1" 1>> ".\logs\autostart.log" 2>> ".\logs\autostart_err.log"

echo.
echo ============================================================
echo   RustMiskoLive — START DONE
echo   Feed Hub:  http://127.0.0.1:8081/state
echo   Executor:  http://127.0.0.1:3030/health
echo ============================================================
echo.

REM Quick health check (best-effort)
curl -s http://127.0.0.1:8081/state >nul 2>&1 && echo [OK] Feed Hub responding || echo [FAIL] Feed Hub not responding
curl -s http://127.0.0.1:3030/health >nul 2>&1 && echo [OK] Executor responding || echo [FAIL] Executor not responding
tasklist /FI "IMAGENAME eq alert-bot.exe" 2>nul | find "alert-bot" >nul && echo [OK] Alert Bot running || echo [FAIL] Alert Bot not running

endlocal
