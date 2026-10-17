#!/usr/bin/env bash
# Run the load test against a fresh server and report server-side CPU usage
# separately from the load generator (they share the machine).
#
#   scripts/load.sh [clients] [edits] [interval_ms] [server flags...]
#   scripts/load.sh 100 100 100 --data /tmp/lt --fsync
set -euo pipefail
cd "$(dirname "$0")/.."
CLIENTS=${1:-100}; EDITS=${2:-100}; INTERVAL=${3:-100}; shift $(( $# > 3 ? 3 : $# ))
PORT=${PORT:-8199}

cargo build --release -q
./target/release/collab-server --port "$PORT" "${@:---no-persist}" >/dev/null 2>&1 &
SERVER=$!
trap 'kill $SERVER 2>/dev/null' EXIT
sleep 0.4

ticks() { awk '{print $14 + $15}' "/proc/$SERVER/stat"; }
T0=$(ticks); S0=$(date +%s.%N)
./target/release/loadtest --addr "127.0.0.1:$PORT" --clients "$CLIENTS" --edits "$EDITS" --interval-ms "$INTERVAL"
T1=$(ticks); S1=$(date +%s.%N)
HZ=$(getconf CLK_TCK)
awk -v t="$((T1 - T0))" -v hz="$HZ" -v s0="$S0" -v s1="$S1" -v n="$(nproc)" \
  'BEGIN { printf "| Server CPU | %.1f s busy over %.1f s wall (%.0f%% of one core; machine has %d cores) |\n", t/hz, s1-s0, 100*t/hz/(s1-s0), n }'
