@echo off
setlocal

rem =====================================================================
rem  MULTIPLEX -- one prompt, five models, side by side.
rem
rem  This is NOT the Arbiter debate engine. For that, run run-arbiter.bat
rem  instead. This just hands off to tools\multiplex.
rem =====================================================================

echo(
echo  Starting MULTIPLEX ^(multi-model comparison^)
echo  For the Arbiter debate engine instead, run run-arbiter.bat
echo(

cd /d "%~dp0tools\multiplex"
if not exist "package.json" (
    echo [ERROR] tools\multiplex is missing. Run: git pull
    pause
    exit /b 1
)

call install_and_run.bat
