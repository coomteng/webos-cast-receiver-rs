#!/usr/bin/env bash
# Package the TV app with the real server address substituted for the BRIDGE_HOST placeholder.
# Reads SERVER_BASE from local.env (git-ignored). The source tree is left untouched.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.local/node/bin:$PATH"
[ -f local.env ] && . ./local.env
: "${SERVER_BASE:?set SERVER_BASE=http://<server-host>:8790 in local.env}"
tools/es5check.sh
STAGE=$(mktemp -d); trap 'rm -rf "$STAGE"' EXIT
cp -r app "$STAGE/app"
sed -i "s#http://BRIDGE_HOST:8790#${SERVER_BASE}#" "$STAGE/app/js/api.js"
grep -q "http://BRIDGE_HOST" "$STAGE/app/js/api.js" && { echo "placeholder still present" >&2; exit 1; }
rm -rf dist && ares-package "$STAGE/app" -o dist | tail -1
echo "BASE in ipk: $(grep -o "'http://[^']*'" "$STAGE/app/js/api.js" | tail -1)"
ls dist
