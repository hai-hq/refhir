#!/usr/bin/env bash
# How the cost of a search grows with the number of matches, for one or more refhir binaries.
#
#   scripts/perf-search.sh [--patients N] BINARY [BINARY...]
#
# Seeds a SQLite database with N patients (default 20000; the load generator gives them ten
# family names, so each name has N/10 matches), then, for each BINARY in turn, starts it on a fresh
# copy of that database and times a set of searches over the family with the most matches: p50 and
# p95 latency and the server's CPU per query. Give it the binary before and after a change to see
# the change; rounds alternate when you give several, and the machine's noise is real, so repeat
# the whole thing (ROUNDS=2) before believing a small difference.
#
# Needs Go (to build the load generator), curl and python3. Writes to a temporary directory
# (WORK, default: a new one under $TMPDIR) and removes it afterwards.
set -euo pipefail

PATIENTS=20000
if [[ "${1:-}" == "--patients" ]]; then PATIENTS=$2; shift 2; fi
(( $# >= 1 )) || { echo "usage: $0 [--patients N] BINARY [BINARY...]" >&2; exit 2; }
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${WORK:-$(mktemp -d -t refhir-search-bench.XXXXXX)}"
ROUNDS="${ROUNDS:-1}"
PORT="${PERF_PORT:-18300}"
FIXTURE="$ROOT/crates/fhir/fixtures/r4"
LOADGEN="$WORK/loadgen"
SERVER_PID=""
trap '[[ -n "$SERVER_PID" ]] && kill "$SERVER_PID" 2>/dev/null; rm -rf "$WORK"' EXIT

for bin in "$@"; do [[ -x "$bin" ]] || { echo "not an executable: $bin" >&2; exit 2; }; done
(cd "$ROOT/clients/go" && go build -o "$LOADGEN" ./cmd/loadgen)

start() { # BINARY DATABASE
  if curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
    echo "something is already listening on port $PORT" >&2; exit 2
  fi
  REFHIR_SKIP_DEFAULT_PACKAGES=1 "$1" --sqlite "$2" --bind "127.0.0.1:$PORT" --package "$FIXTURE" \
    >"$WORK/server.log" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 180); do
    kill -0 "$SERVER_PID" 2>/dev/null || { echo "the server exited; see $WORK/server.log" >&2; exit 2; }
    curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null && return 0
    sleep 1
  done
  echo "the server was not ready in 180 s" >&2; exit 2
}
stop() { kill "$SERVER_PID" 2>/dev/null && wait "$SERVER_PID" 2>/dev/null || true; SERVER_PID=""; }

echo "seeding $PATIENTS patients with $1"
start "$1" "$WORK/seed.sqlite"
"$LOADGEN" --base "http://127.0.0.1:$PORT" --duration 1s --concurrency 8 --mix search=100 \
  --seed-patients "$PATIENTS" --json=false --progress 0 >/dev/null 2>&1
stop

for round in $(seq 1 "$ROUNDS"); do
  for bin in "$@"; do
    cp "$WORK/seed.sqlite" "$WORK/run.sqlite"
    [[ -f "$WORK/seed.sqlite-wal" ]] && cp "$WORK/seed.sqlite-wal" "$WORK/run.sqlite-wal"
    start "$bin" "$WORK/run.sqlite"
    echo
    echo "== $bin (round $round)"
    python3 "$ROOT/scripts/perf-search-measure.py" "http://127.0.0.1:$PORT" "$SERVER_PID"
    stop
    rm -f "$WORK"/run.sqlite*
  done
done
