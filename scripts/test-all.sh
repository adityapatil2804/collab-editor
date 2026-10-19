#!/usr/bin/env bash
# Everything CI runs: Rust unit/fuzz/integration tests, TypeScript build,
# cross-language checks (both directions), editor binding tests, end-to-end
# tests against a live server, and a short load test.
set -euo pipefail
cd "$(dirname "$0")/.."
TMP=$(mktemp -d)
trap 'kill ${SERVER:-} 2>/dev/null; rm -rf "$TMP"' EXIT

echo "== Rust tests"
FUZZ_SEEDS=${FUZZ_SEEDS:-2000} cargo test --release -q 2>&1 | grep -E "test result|FAILED|panicked"
cargo build --release -q

echo "== TypeScript build"
(cd web && (npx --no-install tsc -p . 2>/dev/null || tsc -p .))

echo "== Cross-language (Rust -> TS, TS -> Rust)"
./target/release/trace gen "$TMP/rust.json"
node web/test/crosslang.mjs check "$TMP/rust.json"
node web/test/crosslang.mjs gen "$TMP/ts.json"
./target/release/trace check "$TMP/ts.json"

echo "== Editor binding"
node web/test/editor.mjs

echo "== End-to-end"
PORT=${PORT:-8198}
./target/release/collab-server --port "$PORT" --data "$TMP/data" --static web >"$TMP/server.log" 2>&1 &
SERVER=$!
sleep 0.4
SERVER_ADDR="127.0.0.1:$PORT"

(cd web && SERVER="$SERVER_ADDR" node test/e2e.mjs)
./target/release/loadtest --addr "$SERVER_ADDR" --clients 20 --edits 50 --interval-ms 50 | grep -E "identical|p99"

if node -e "require('playwright')" 2>/dev/null || [ -d /opt/node22/lib/node_modules/playwright ]; then
  echo "== Real browser (Chromium)"
  (cd web && SERVER="$SERVER_ADDR" node test/browser.mjs)
else
  echo "== Real browser: skipped (npm i -D playwright && npx playwright install chromium)"
fi

echo "== All checks passed"
