#!/usr/bin/env bash
# Kills the live primary twice, once in each direction, while writing through the load balancer,
# and checks that no acknowledged write is lost and commit timestamps never go backwards.
#
#   deploy/cluster/failover-test.sh [--keep]
#
# Needs docker compose, curl and python3. It builds the image from the repository's Dockerfile
# if there is none (the first start also installs the FHIR packages, so it needs the package
# registry), starts the stack under its own project name, and removes it afterwards unless --keep
# is given.
#
# What it checks, for each kill:
#   - every write the load balancer acknowledged (2xx) can be read afterwards, asking for at
#     least the commit timestamp it was acknowledged with, as the clients do;
#   - the commit timestamps of consecutive acknowledged writes only ever increase;
#   - at most FAIL_BUDGET requests fail (default 3): a request in flight on the old primary may.
# It prints the longest gap between two acknowledged writes, which bounds the takeover time.
set -euo pipefail

KEEP=0; [[ "${1:-}" == "--keep" ]] && KEEP=1
HERE="$(cd "$(dirname "$0")" && pwd)"
PROJECT="${COMPOSE_PROJECT:-refhir-failover}"
COMPOSE=(docker compose -p "$PROJECT" -f "$HERE/docker-compose.yml")
LB="http://localhost:${LB_PORT:-8080}"
STATS="http://localhost:${STATS_PORT:-8404}"
WRITE_SECONDS="${WRITE_SECONDS:-30}"
KILL_AFTER="${KILL_AFTER:-8}"
FAIL_BUDGET="${FAIL_BUDGET:-3}"

cleanup() {
  if [[ $KEEP == 0 ]]; then "${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true; fi
}
trap cleanup EXIT

# The writer that HAProxy has UP in the primary-only backend is the primary.
primary() {
  curl -sf "$STATS/;csv" | awk -F, '$1=="writer" && $2 ~ /^writer[0-9]+$/ && $18=="UP" {print $2; exit}'
}

wait_for() { # description timeout command...
  local what=$1 seconds=$2; shift 2
  local deadline=$((SECONDS + seconds))
  until "$@" >/dev/null 2>&1; do
    (( SECONDS < deadline )) || { echo "timed out waiting for $what" >&2; "${COMPOSE[@]}" ps >&2 || true; exit 2; }
    sleep 1
  done
}

has_primary() { [[ -n "$(primary)" ]]; }
lb_ready() { curl -sf "$LB/metadata" -o /dev/null; }
epoch() {
  "${COMPOSE[@]}" exec -T postgres psql -U refhir -d refhir -Atc \
    "SELECT json_value FROM persistence_globals WHERE key = 'writer_epoch'" 2>/dev/null || echo "?"
}

# The image is built from the repository's Dockerfile when it is missing, or when REBUILD=1.
# Otherwise an existing refhir:cluster is used as it is (rebuild after changing the code).
if [[ "${REBUILD:-0}" == 1 ]] || ! docker image inspect refhir:cluster >/dev/null 2>&1; then
  echo "building the image"
  "${COMPOSE[@]}" build writer1 >/dev/null 2>&1
fi
echo "starting the stack (project $PROJECT)"
"${COMPOSE[@]}" up -d >/dev/null 2>&1
# A first start installs the default FHIR packages before the primary serves (replicas already
# answer from the first catalog), so the primary is the real signal, and it can take minutes.
wait_for "the load balancer to answer" 900 lb_ready
wait_for "a primary (the first start installs the FHIR packages)" 900 has_primary

# One round: write for WRITE_SECONDS, kill the primary KILL_AFTER seconds in, then check.
round() {
  local label=$1 victim
  victim=$(primary)
  echo "--- $label: the primary is $victim, epoch $(epoch)"
  local log; log=$(mktemp)
  python3 "$HERE/failover_load.py" write "$LB" "$WRITE_SECONDS" "$label" >"$log" &
  local writer=$!
  sleep "$KILL_AFTER"
  local killed_at=$SECONDS
  "${COMPOSE[@]}" kill "$victim" >/dev/null 2>&1
  echo "    killed $victim after ${KILL_AFTER}s"
  wait "$writer"
  # Bring the killed writer back; it returns as the standby.
  "${COMPOSE[@]}" start "$victim" >/dev/null 2>&1
  python3 "$HERE/failover_load.py" verify "$LB" "$log" "$FAIL_BUDGET"
  rm -f "$log"
  wait_for "the restarted writer to be up as the standby, with a primary serving" 120 has_primary
  echo "    now the primary is $(primary), epoch $(epoch)"
}

round "round 1"
round "round 2"
echo "PASS  failover: no acknowledged write lost, commit timestamps only increased, in both directions"
