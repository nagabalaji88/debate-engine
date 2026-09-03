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

rem ---- 0. Windows isn't a supported build target yet ---------------
rem Arbiter's run-safety check (crates\arbiter-store\src\lease.rs) proves
rem one process still owns a run by checking /proc/<pid> and
rem /proc/sys/kernel/random/boot_id -- both Linux-only. Off Linux that
rem check would always report "no such process," so every lease would
rem look abandoned and a second process could steal a live run out from
rem under its owner. The crate refuses to compile anywhere but Linux
rem rather than risk that silently, so a native build here fails 100%
rem of the time today, not just sometimes -- WSL is a real Linux kernel,
rem so it isn't affected.
echo [NOTE] This project's run-safety check only works on Linux by design
echo        and won't compile natively on Windows -- see
echo        crates\arbiter-store\src\lease.rs for why. WSL runs a real
echo        Linux kernel, so it isn't affected.
echo(
where wsl >nul 2>&1
if errorlevel 1 goto :native_notice

choice /C YN /M "Run Arbiter under WSL instead (recommended)"
if errorlevel 2 goto :native_notice
if errorlevel 1 goto :run_via_wsl

:native_notice
echo(
echo        Continuing with a native Windows build attempt below -- this is
echo        expected to fail today with a "Linux-only" compile error.
echo(

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
set "BUILD_LOG=%TEMP%\arbiter_build.log"
cargo build --release -p arbiter-cli --bin arbiter > "%BUILD_LOG%" 2>&1
set "BUILD_RC=%ERRORLEVEL%"
type "%BUILD_LOG%"
if "%BUILD_RC%"=="0" goto :buildok

findstr /C:"Linux-only" "%BUILD_LOG%" >nul 2>&1
if not errorlevel 1 goto :linux_only_failure

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
cargo build --release -p arbiter-cli --bin arbiter > "%BUILD_LOG%" 2>&1
set "BUILD_RC=%ERRORLEVEL%"
type "%BUILD_LOG%"
if "%BUILD_RC%"=="0" goto :buildok
findstr /C:"Linux-only" "%BUILD_LOG%" >nul 2>&1
if not errorlevel 1 goto :linux_only_failure
goto :buildtools_retry_failed

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

:linux_only_failure
echo(
echo [ERROR] That's the expected "Linux-only" failure from
echo         crates\arbiter-store\src\lease.rs ^(see the note at the top of
echo         this run^) -- not a linker problem, so installing the VS Build
echo         Tools would not help. Run this script again and choose WSL
echo         when asked, or install WSL yourself ^(elevated prompt^):
echo             wsl --install
echo         then restart Windows, open the "Ubuntu" app once to finish
echo         its setup, and re-run this .bat.
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

rem ---- WSL delegation --------------------------------------------
rem install_and_run.sh does the same three steps (Rust check, release
rem build, launch) against a real Linux kernel, where lease.rs's
rem liveness check is genuinely correct.
:run_via_wsl
echo(
echo Switching to WSL...
set "WSLDIR="
for /f "usebackq delims=" %%W in (`wsl wslpath -a "%CD%"`) do set "WSLDIR=%%W"
if not defined WSLDIR (
    echo [ERROR] Could not resolve a WSL path for this folder. Open a WSL
    echo         terminal yourself and run: bash install_and_run.sh
    goto :fail
)
if not exist "install_and_run.sh" (
    echo [ERROR] install_and_run.sh is missing from this folder.
    goto :fail
)
wsl bash "%WSLDIR%/install_and_run.sh"
echo(
echo Note: --open likely couldn't find a browser inside WSL. If nothing
echo opened, copy the "Open: http://127.0.0.1:<port>/?token=..." line
echo above into your normal Windows browser -- WSL2 forwards localhost
echo automatically, so it just works.
goto :end

:fail
echo(
pause
exit /b 1

:end
echo(
echo Arbiter has stopped.
pause
