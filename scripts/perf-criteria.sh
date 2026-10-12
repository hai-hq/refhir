#!/usr/bin/env bash
# Checks the performance exit criteria of the multi-process work against a real Postgres.
#
#   PERF_DB_URL=postgres://user:pass@host:5432/emptydb scripts/perf-criteria.sh [--quick]
#
# Needs: a release build of refhir (REFHIR_BIN, default target/release/refhir), Go, curl, python3,
# and an EMPTY database in PERF_DB_URL (the server installs the R4 fixture into it). Writes its
# JSON results to $PERF_OUT (default: a temp directory) and exits non-zero if the criterion fails.
#
# What it checks
#   Phase 2 exit criterion: "read/search throughput rises with pool size versus Phase 0". A pool of
#   1 is the baseline (one connection, as before the pool); each workload is compared with a pool
#   of 8 over alternating pairs, and a verdict is the MEDIAN gain, because one comparison on a
#   shared machine swings by tens of percent. A workload PASSES with a median gain of at least
#   MIN_GAIN_PCT (default 3), and the script's exit code is non-zero if any of them fails.
#
#   1. Reads: read=100 at CLIENTS clients (default 32). PAIRS pairs (default 3).
#   2. The Phase 0 mix (read=60,search=25,write=15), read+search ops/s, MIXED_PAIRS pairs (default 3).
#   3. Reads and searches beside writers: read=70,search=30 from READERS clients (default 24) while
#      WRITERS clients (default 8) write at the same time; read+search ops/s of the readers only.
#      This is the Phase 0 contention without its closed-loop artifact, in which a client that draws
#      a four second write is not reading meanwhile. SPLIT_PAIRS pairs (default 3).
#   4. Search-heavy: read=20,search=80 at as many clients as the machine has CPUs. INFO_PAIRS pairs
#      (default 3).
#
#   "waited" is the total time the clients spent waiting for a connection, and "cores" is how many
#   cores the server process used (CPU time over wall time). A pool can only raise throughput that
#   the connection limits: if "cores" is near the machine's core count the server is CPU-bound and
#   a FAIL here is a CPU problem, not a pool problem. That is how a quadratic scan in the FHIRPath
#   model (Model::known_child, run for every step of every row of a search) showed up: searches used
#   ~10 of 12 cores and gained nothing from a pool until it was fixed (docs/perf/baseline.md).
set -euo pipefail

QUICK=0; [[ "${1:-}" == "--quick" ]] && QUICK=1
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${REFHIR_BIN:-$ROOT/target/release/refhir}"
DB="${PERF_DB_URL:?set PERF_DB_URL to an empty Postgres database}"
OUT="${PERF_OUT:-$(mktemp -d)}"
PORT="${PERF_PORT:-18080}"
MIN_GAIN_PCT="${MIN_GAIN_PCT:-3}"
START_TIMEOUT="${START_TIMEOUT:-240}"
CLIENTS="${CLIENTS:-32}"
CLIENTS_MATCHED="${CLIENTS_MATCHED:-$(python3 -c 'import os; print(os.cpu_count() or 8)')}"
PAIRS="${PAIRS:-3}"
MIXED_PAIRS="${MIXED_PAIRS:-3}"
INFO_PAIRS="${INFO_PAIRS:-3}"
SPLIT_PAIRS="${SPLIT_PAIRS:-3}"
READERS="${READERS:-24}"
WRITERS="${WRITERS:-8}"
if [[ $QUICK == 1 ]]; then DURATION=15s; SEED_READ=20; SEED_MIXED=100; SEED_HEAVY=150; PAIRS=${PAIRS_QUICK:-1}; MIXED_PAIRS=0; SPLIT_PAIRS=0; INFO_PAIRS=0; else DURATION="${DURATION:-30s}"; SEED_READ="${SEED_READ:-20}"; SEED_MIXED="${SEED_MIXED:-200}"; SEED_HEAVY="${SEED_HEAVY:-300}"; fi
LOADGEN="${LOADGEN:-$OUT/loadgen}"
mkdir -p "$OUT"
[[ -x "$BIN" ]] || { echo "no refhir binary at $BIN (cargo build --release -p refhir)" >&2; exit 2; }
[[ -x "$LOADGEN" ]] || (cd "$ROOT/clients/go" && go build -o "$LOADGEN" ./cmd/loadgen)

SERVER_PID=""
stop_server() { [[ -n "$SERVER_PID" ]] && kill "$SERVER_PID" 2>/dev/null && wait "$SERVER_PID" 2>/dev/null || true; SERVER_PID=""; }
trap stop_server EXIT

start_server() { # pool size
  stop_server
  if curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
    echo "something is already listening on port $PORT; refusing to measure against it" >&2; exit 2
  fi
  REFHIR_SKIP_DEFAULT_PACKAGES=1 "$BIN" --postgres "$DB" --bind "127.0.0.1:$PORT" \
    --package "$ROOT/crates/fhir/fixtures/r4" --db-pool "$1" >"$OUT/server.log" 2>&1 &
  SERVER_PID=$!
  # The first start installs the FHIR definitions into the database, which takes minutes over a
  # slow link (a 5 ms round trip made it take longer than the old fixed 240 s wait), so the wait is
  # START_TIMEOUT seconds (default 240) and running out of it is an error, not something to carry on from.
  local waited=0
  until curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; do
    kill -0 "$SERVER_PID" 2>/dev/null || { echo "the server exited; see $OUT/server.log" >&2; exit 2; }
    (( waited < START_TIMEOUT )) || { echo "the server was not ready after ${START_TIMEOUT}s (set START_TIMEOUT higher on a slow database); see $OUT/server.log" >&2; exit 2; }
    sleep 1; waited=$((waited + 1))
  done
  # Prove this is the server just started, with the pool that was asked for, not a stale one.
  local reported
  reported=$(curl -sf "http://127.0.0.1:$PORT/healthz?verbose=1" | python3 -c 'import json,sys; print(json.load(sys.stdin)["pool"]["max"])')
  if [[ "$reported" != "$1" ]]; then
    echo "asked for a pool of $1 but the server reports $reported" >&2; exit 2
  fi
}

# CPU seconds the server process has used so far.
# `ps -o cputime` prints M:SS.cc or H:MM:SS.cc on macOS and [DD-]HH:MM:SS on Linux.
server_cpu() { ps -o cputime= -p "$SERVER_PID" | awk '{ t = $1; d = 0; if (index(t, "-")) { split(t, x, "-"); d = x[1]; t = x[2] } n = split(t, p, ":"); s = 0; for (i = 1; i <= n; i++) s = s * 60 + p[i]; print d * 86400 + s }'; }

# measure LABEL POOL MIX SEED CLIENTS METRIC -> sets RESULT (METRIC "total": all ops/s; "readsearch":
# read + search ops/s; "split": read + search ops/s of READERS clients that run beside WRITERS
# clients writing, MIX being the readers' mix), leaves the JSON in $OUT/LABEL.json, the pool wait
# in WAITED and the server's busy cores in CORES.
# It runs in THIS shell, not in $(...): a subshell would lose SERVER_PID, the old server would keep
# the port, and every later "pool of 8" would silently measure the first server.
measure() {
  local label=$1 pool=$2 mix=$3 seed=$4 clients=$5 metric=$6 base="http://127.0.0.1:$PORT" cpu0 cpu1 t0 t1
  start_server "$pool"
  # A warm-up that is thrown away: the first wave of concurrent searches is slow while the server
  # warms up, and in a short window that tail dominates.
  "$LOADGEN" --base "$base" --duration 12s --concurrency "$clients" --mix "$mix" \
    --seed-patients 60 --json=false --progress 0 >/dev/null 2>&1 || true
  cpu0=$(server_cpu); t0=$(python3 -c 'import time; print(time.time())')
  if [[ $metric == split ]]; then
    "$LOADGEN" --base "$base" --duration "$DURATION" --concurrency "$WRITERS" --mix write=100 \
      --seed-patients 20 --out "$OUT/$label-writers.json" --json=false --progress 0 >/dev/null 2>&1 &
    local writers=$!
    "$LOADGEN" --base "$base" --duration "$DURATION" --concurrency "$READERS" --mix "$mix" \
      --seed-patients "$seed" --out "$OUT/$label.json" --json=false --progress 0 >/dev/null 2>&1
    wait "$writers"
  else
    "$LOADGEN" --base "$base" --duration "$DURATION" --concurrency "$clients" --mix "$mix" \
      --seed-patients "$seed" --out "$OUT/$label.json" --json=false --progress 0 >/dev/null 2>&1
  fi
  cpu1=$(server_cpu); t1=$(python3 -c 'import time; print(time.time())')
  CORES=$(python3 -c 'import sys; c0,c1,t0,t1=map(float,sys.argv[1:]); print(f"{(c1-c0)/(t1-t0):.1f}")' "$cpu0" "$cpu1" "$t0" "$t1")
  RESULT=$(python3 - "$OUT/$label.json" "$metric" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
if sys.argv[2] in ("readsearch", "split"):
    print(f'{sum(d["classes"].get(c, {}).get("ops_per_s", 0) for c in ("read", "search")):.1f}')
else:
    print(f'{d["total"]["ops_per_s"]:.1f}')
PY
)
  WAITED=$(python3 - "$OUT/$label.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
def find(x):
    if isinstance(x, dict):
        if "pool" in x and "time_ns" in x: return x
        for v in x.values():
            r = find(v)
            if r: return r
a, b = find(d["server_stats"]), find(d["server_stats_before"])
print(f'{(a["pool"]["wait_ns"] - b["pool"]["wait_ns"]) / 1e9:.1f}')
PY
)
}

ratio() { python3 -c "import sys; a,b=float(sys.argv[1]),float(sys.argv[2]); print(f'{(b-a)/a*100:+.1f}')" "$1" "$2"; }

echo "perf criteria ($([[ $QUICK == 1 ]] && echo quick || echo full)): $DURATION per run, results in $OUT"

# compare NAME MIX SEED CLIENTS METRIC PAIRS -> prints each pair, sets MEDIAN and POSITIVE and PAIRLIST.
compare() {
  local name=$1 mix=$2 seed=$3 clients=$4 metric=$5 pairs=$6 gains=() P1 P8 W1 W8 G
  for pair in $(seq 1 "$pairs"); do
    measure "$name-pool1-$pair" 1 "$mix" "$seed" "$clients" "$metric"; P1=$RESULT; W1=$WAITED; K1=$CORES
    measure "$name-pool8-$pair" 8 "$mix" "$seed" "$clients" "$metric"; P8=$RESULT; W8=$WAITED; K8=$CORES
    G=$(ratio "$P1" "$P8")
    gains+=("$G")
    echo "  pair $pair:  pool 1 = $P1 ops/s (waited ${W1}s for a connection, ${K1} cores),  pool 8 = $P8 ops/s (${W8}s, ${K8} cores)  ($G %)"
  done
  read -r MEDIAN POSITIVE PAIRLIST < <(python3 - "${gains[@]}" <<'PY'
import statistics, sys
g = [float(x) for x in sys.argv[1:]]
print(f"{statistics.median(g):+.1f}", sum(1 for x in g if x > 0), ",".join(f"{x:+.1f}" for x in g))
PY
)
}

FAILED=()
verdict() { # LABEL PAIRS  (uses MEDIAN POSITIVE PAIRLIST)
  python3 - "$1" "$2" "$MIN_GAIN_PCT" "$MEDIAN" "$POSITIVE" "$PAIRLIST" <<'PY'
import sys
label, pairs, need, median, positive, listed = sys.argv[1:]
word = "PASS" if float(median) >= float(need) else "FAIL"
print(f"{word}  {label}: median gain with a pool of 8 is {median} % ({positive} of {pairs} pairs positive; pairs: {listed.replace(',', ', ')}; needed {float(need):+.1f} %)")
PY
  if awk -v m="$MEDIAN" -v n="$MIN_GAIN_PCT" 'BEGIN { exit !(m + 0 < n + 0) }'; then FAILED+=("$1"); fi
}

echo "1. reads only (read=100), $CLIENTS clients, $PAIRS alternating pair(s)"
compare reads read=100 "$SEED_READ" "$CLIENTS" total "$PAIRS"
verdict "reads" "$PAIRS"

if (( MIXED_PAIRS > 0 )); then
  echo "2. the Phase 0 mix (read=60,search=25,write=15), $CLIENTS clients, read+search ops/s, $MIXED_PAIRS alternating pair(s)"
  compare mixed read=60,search=25,write=15 "$SEED_MIXED" "$CLIENTS" readsearch "$MIXED_PAIRS"
  verdict "Phase 0 mix" "$MIXED_PAIRS"
fi

if (( SPLIT_PAIRS > 0 )); then
  echo "3. reads and searches (read=70,search=30) from $READERS clients beside $WRITERS writing clients, $SPLIT_PAIRS alternating pair(s)"
  compare split read=70,search=30 100 "$READERS" split "$SPLIT_PAIRS"
  verdict "reads and searches beside writers" "$SPLIT_PAIRS"
fi

if (( INFO_PAIRS > 0 )); then
  echo "4. search-heavy (read=20,search=80), $CLIENTS_MATCHED clients (= CPUs), $INFO_PAIRS alternating pair(s)"
  compare heavy read=20,search=80 "$SEED_HEAVY" "$CLIENTS_MATCHED" total "$INFO_PAIRS"
  verdict "search-heavy" "$INFO_PAIRS"
fi

if (( ${#FAILED[@]} == 0 )); then
  echo "PASS  Phase 2 exit criterion: throughput rises with a pool of 8 in every workload (median gain at least $MIN_GAIN_PCT %)"
else
  echo "FAIL  Phase 2 exit criterion: no median gain of $MIN_GAIN_PCT % with a pool of 8 for: ${FAILED[*]}"
  exit 1
fi
