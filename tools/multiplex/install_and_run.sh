#!/usr/bin/env bash
# Multiplex -- install dependencies and start the server (macOS/Linux).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

if ! command -v node >/dev/null 2>&1; then
    echo "[ERROR] Node.js is not installed. Get it from https://nodejs.org (18+), then re-run this script."
    exit 1
fi

if [ ! -f .env ]; then
    cp .env.example .env
    echo "Created .env from .env.example -- open it and add your API keys, then re-run this script."
    exit 0
fi

echo "Installing dependencies..."
npm install --no-fund --no-audit

echo
echo "Starting Multiplex -- open http://localhost:8787"
echo "Close this window (or press Ctrl+C) to stop the server."
echo
exec npm start
