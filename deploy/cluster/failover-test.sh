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
  python3 - "$LB" "$WRITE_SECONDS" "$label" >"$log" <<'PY' &
import json, sys, time, urllib.request, urllib.error, uuid
lb, seconds = sys.argv[1], float(sys.argv[2])
label = "".join(c for c in sys.argv[3] if c.isalnum())  # a FHIR id has no spaces
acked, failed = [], []
last_ack, longest_gap, gap_at = None, 0.0, 0.0
end = time.time() + seconds
n = 0
while time.time() < end:
    n += 1
    pid = f"{label}-{n}-{uuid.uuid4().hex[:6]}"
    body = json.dumps({"resourceType": "Patient", "id": pid, "active": True}).encode()
    req = urllib.request.Request(f"{lb}/Patient", data=body, method="POST",
                                 headers={"content-type": "application/fhir+json"})
    started = time.time()
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            ts = resp.headers.get("X-Refhir-Commit-Ts")
            now = time.time()
            if last_ack is not None and now - last_ack > longest_gap:
                longest_gap, gap_at = now - last_ack, now - (end - seconds)
            last_ack = now
            acked.append({"id": pid, "ts": int(ts) if ts else None})
    except urllib.error.HTTPError as err:
        failed.append({"id": pid, "status": err.code, "at": round(started - (end - seconds), 2)})
    except Exception as err:  # connection reset, timeout
        failed.append({"id": pid, "status": str(err)[:60], "at": round(started - (end - seconds), 2)})
    time.sleep(0.1)
print(json.dumps({"acked": acked, "failed": failed,
                  "longest_gap_s": round(longest_gap, 2), "gap_at_s": round(gap_at, 1)}))
PY
  local writer=$!
  sleep "$KILL_AFTER"
  local killed_at=$SECONDS
  "${COMPOSE[@]}" kill "$victim" >/dev/null 2>&1
  echo "    killed $victim after ${KILL_AFTER}s"
  wait "$writer"
  # Bring the killed writer back; it returns as the standby.
  "${COMPOSE[@]}" start "$victim" >/dev/null 2>&1
  python3 - "$LB" "$log" "$FAIL_BUDGET" <<'PY'
import json, sys, time, urllib.request, urllib.error
lb, path, budget = sys.argv[1], sys.argv[2], int(sys.argv[3])
r = json.load(open(path))
acked, failed = r["acked"], r["failed"]
problems = []
stamped = [a["ts"] for a in acked if a["ts"] is not None]
if len(stamped) != len(acked):
    problems.append(f"{len(acked) - len(stamped)} acknowledged writes carried no X-Refhir-Commit-Ts")
backwards = sum(1 for a, b in zip(stamped, stamped[1:]) if b <= a)
if backwards:
    problems.append(f"{backwards} acknowledged writes got a commit timestamp not above the one before")
missing = []
for a in acked:
    headers = {"X-Refhir-Min-Ts": str(a["ts"])} if a["ts"] else {}
    deadline = time.time() + 20
    while True:
        try:
            req = urllib.request.Request(f"{lb}/Patient/{a['id']}", headers=headers)
            with urllib.request.urlopen(req, timeout=10) as resp:
                if resp.status == 200:
                    break
        except Exception:
            pass
        if time.time() > deadline:
            missing.append(a["id"])
            break
        time.sleep(0.2)
if missing:
    problems.append(f"{len(missing)} acknowledged writes cannot be read: {missing[:5]}")
if len(failed) > budget:
    problems.append(f"{len(failed)} requests failed (budget {budget})")
print(f"    {len(acked)} writes acknowledged, {len(failed)} failed {[(f['at'], f['status']) for f in failed]}, "
      f"longest gap between acknowledgements {r['longest_gap_s']}s (at t={r['gap_at_s']}s)")
print(f"    timestamps strictly increasing: {not backwards}; acknowledged writes missing: {len(missing)}")
if problems:
    for p in problems:
        print("    FAIL:", p)
    sys.exit(1)
PY
  rm -f "$log"
  wait_for "the restarted writer to be up as the standby, with a primary serving" 120 has_primary
  echo "    now the primary is $(primary), epoch $(epoch)"
}

round "round 1"
round "round 2"
echo "PASS  failover: no acknowledged write lost, commit timestamps only increased, in both directions"
