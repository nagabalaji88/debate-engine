@echo off
setlocal EnableDelayedExpansion

rem ============================================================
rem  Arbiter -- install prerequisites, build, and launch the UI
rem
rem  Run this from inside a clone of the debate-engine repo (it
rem  works out where it lives automatically). Double-click it, or
rem  run it from a Command Prompt / PowerShell window.
rem ============================================================

cd /d "%~dp0"
echo(
echo  Arbiter setup
echo  =============
echo  Working directory: %CD%
echo(

if not exist "Cargo.toml" (
    echo [ERROR] No Cargo.toml found here.
    echo         Run this .bat from inside your clone of the debate-engine repo.
    goto :fail
)

rem ---- 1. Rust toolchain ----------------------------------------
where cargo >nul 2>&1
if not errorlevel 1 goto :rust_ok

echo [1/3] Rust not found on PATH -- installing it now.

where winget >nul 2>&1
if errorlevel 1 goto :rust_via_rustup_init
echo       Using winget to install rustup...
winget install --id Rustlang.Rustup -e --silent --accept-package-agreements --accept-source-agreements
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
where cargo >nul 2>&1
if not errorlevel 1 goto :rust_installed

:rust_via_rustup_init
echo       Downloading rustup-init.exe...
powershell -NoProfile -Command "Invoke-WebRequest -Uri https://win.rustup.rs/x86_64 -OutFile '%TEMP%\rustup-init.exe'"
if not exist "%TEMP%\rustup-init.exe" (
    echo [ERROR] Could not download the Rust installer. Install Rust manually
    echo         from https://rustup.rs and re-run this script.
    goto :fail
)
"%TEMP%\rustup-init.exe" -y --default-toolchain stable
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
where cargo >nul 2>&1
if errorlevel 1 (
    echo [ERROR] Rust install did not finish correctly -- cargo still not on PATH.
    echo         Close this window, open a NEW Command Prompt, and re-run this
    echo         script ^(a fresh window picks up the PATH change^).
    goto :fail
)

:rust_installed
echo       Rust installed OK.
goto :rust_done

:rust_ok
echo [1/3] Rust already installed.

:rust_done
echo(

rem ---- 2. Build ---------------------------------------------------
echo [2/3] Building arbiter ^(release mode -- first build takes a few minutes^)...
cargo build --release -p arbiter-cli --bin arbiter
if not errorlevel 1 goto :buildok

echo(
echo [ERROR] The build failed. The most common cause on a fresh Windows
echo         machine is a missing C++ linker, which the default Rust
echo         toolchain needs even though this project has no C/C++ code
echo         of its own.
echo(
choice /C YN /M "Install the Visual Studio Build Tools now (large download, several GB)"
if errorlevel 2 goto :buildtools_declined
if errorlevel 1 goto :buildtools_install
goto :buildtools_declined

:buildtools_install
where winget >nul 2>&1
if errorlevel 1 goto :buildtools_manual
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --silent --accept-package-agreements --accept-source-agreements --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools"
echo       Retrying the build...
cargo build --release -p arbiter-cli --bin arbiter
if errorlevel 1 goto :buildtools_retry_failed
goto :buildok

:buildtools_retry_failed
echo [ERROR] Still failing after installing the Build Tools. Close this
echo         window, open a NEW Command Prompt ^(so the updated environment
echo         is picked up^), and re-run this script.
goto :fail

:buildtools_manual
echo [ERROR] winget is not available, so this can't be automated here.
echo         Install the "Desktop development with C++" workload from
echo         https://visualstudio.microsoft.com/visual-cpp-build-tools/
echo         then re-run this script.
goto :fail

:buildtools_declined
echo         Install the "Desktop development with C++" workload from
echo         https://visualstudio.microsoft.com/visual-cpp-build-tools/
echo         then re-run this script.
goto :fail

:buildok
echo       Build OK.
echo(

rem ---- 3. Launch ----------------------------------------------------
echo [3/3] Starting arbiter serve -- your browser will open automatically.
echo       Data is stored under: %CD%\.arbiter\runs
echo       Close this window ^(or press Ctrl+C^) to stop the server.
echo(
target\release\arbiter.exe serve --open

goto :end

:fail
echo(
pause
exit /b 1

:end
echo(
echo Arbiter has stopped.
pause
