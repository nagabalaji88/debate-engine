#!/usr/bin/env bash
# Arbiter -- install prerequisites, build, and launch the UI.
#
# For Linux, and for Windows via WSL (arbiter_store::lease reads
# /proc/<pid> and /proc/sys/kernel/random/boot_id for its run-ownership
# check -- see crates/arbiter-store/src/lease.rs -- so this only runs
# where a real Linux kernel is underneath; native Windows/macOS builds
# refuse to compile on purpose rather than silently mis-detecting a
# live run as abandoned). install_and_run.bat delegates to this script
# under WSL automatically when it finds one.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

echo
echo " Arbiter setup"
echo " ============="
echo " Working directory: $(pwd)"
echo

if [ ! -f Cargo.toml ]; then
    echo "[ERROR] No Cargo.toml found here."
    echo "        Run this from inside your clone of the debate-engine repo."
    exit 1
fi

echo "[1/3] Rust toolchain"
if ! command -v cargo >/dev/null 2>&1; then
    echo "      Not found -- installing via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    # shellcheck disable=SC1090
    source "$HOME/.cargo/env"
else
    echo "      Already installed."
fi
echo

echo "[2/3] Building arbiter (release mode -- first build takes a few minutes)..."
cargo build --release -p arbiter-cli --bin arbiter
echo "      Build OK."
echo

echo "[3/3] Starting arbiter serve..."
echo "      Data is stored under: $(pwd)/.arbiter/runs"
echo "      Under WSL, --open likely won't find a browser inside the Linux"
echo "      side -- copy the 'Open: http://127.0.0.1:<port>/?token=...' URL"
echo "      below into your normal Windows browser; WSL2 forwards localhost"
echo "      automatically, so it just works."
echo "      Close this window (or press Ctrl+C) to stop the server."
echo
exec target/release/arbiter serve --open
