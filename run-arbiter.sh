#!/usr/bin/env bash
# Arbiter -- install prerequisites, build, and launch the UI.
#
# For Linux (also fine under WSL on Windows, if you'd rather use that than
# the native path). macOS and Windows both build directly now too --
# arbiter_store::lease has a real liveness check for each
# (crates/arbiter-store/src/lease.rs) -- so this script is not the only
# way to run this project anymore, just the Linux-native one.
# run-arbiter.bat falls back to this script under WSL only if a
# native Windows build has trouble.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

echo
echo " ARBITER -- the debate & decision engine (Rust)"
echo " ============================================="
echo " Debates, and side-by-side model comparison, in one app."
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
