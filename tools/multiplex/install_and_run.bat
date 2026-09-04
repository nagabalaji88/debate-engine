@echo off
setlocal

rem Multiplex -- install dependencies and start the server (Windows).
cd /d "%~dp0"

where node >nul 2>&1
if errorlevel 1 (
    echo [ERROR] Node.js is not installed. Get it from https://nodejs.org ^(18+^),
    echo         then re-run this script.
    pause
    exit /b 1
)

if not exist ".env" (
    copy /y ".env.example" ".env" >nul
    echo Created .env from .env.example -- open it and add your API keys,
    echo then re-run this script.
    pause
    exit /b 0
)

echo Installing dependencies...
call npm install --no-fund --no-audit
if errorlevel 1 (
    echo [ERROR] npm install failed. See the output above.
    pause
    exit /b 1
)

echo(
echo Starting Multiplex -- open http://localhost:8787
echo Close this window ^(or press Ctrl+C^) to stop the server.
echo(
call npm start
pause
