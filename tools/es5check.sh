#!/usr/bin/env bash
# ES5 gate for the TV app (webOS 4.4 = Chrome 53; one ES2015 token kills the whole script).
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.local/node/bin:$PATH"
ACORN=$(node -e "try{console.log(require.resolve('acorn'))}catch(e){process.exit(1)}" 2>/dev/null || true)
if [ -z "$ACORN" ]; then
  for d in /tmp/cdp "$HOME/.local/node/lib/node_modules/npm/node_modules"; do
    [ -d "$d/node_modules/acorn" ] && ACORN="$d/node_modules/acorn" && break
  done
fi
[ -n "$ACORN" ] || { echo "acorn not found; run: npm i --prefix /tmp/cdp acorn" >&2; exit 2; }
fail=0
for f in app/js/*.js; do
  node -e "require('$ACORN').parse(require('fs').readFileSync('$f','utf8'),{ecmaVersion:5})" 2>/dev/null \
    && echo "ok   $f" || { echo "FAIL $f"; fail=1; }
done
exit $fail
