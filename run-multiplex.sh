#!/usr/bin/env bash
# =====================================================================
#  MULTIPLEX -- one prompt, five models, side by side.
#
#  This is NOT the Arbiter debate engine. For that, run ./run-arbiter.sh
#  instead. This just hands off to tools/multiplex.
# =====================================================================
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo
echo " Starting MULTIPLEX (multi-model comparison)"
echo " For the Arbiter debate engine instead, run ./run-arbiter.sh"
echo

if [ ! -f "$here/tools/multiplex/package.json" ]; then
    echo "[ERROR] tools/multiplex is missing. Run: git pull"
    exit 1
fi

exec "$here/tools/multiplex/install_and_run.sh"
