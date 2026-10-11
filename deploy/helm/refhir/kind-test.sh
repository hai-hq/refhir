#!/usr/bin/env bash
# Deploys the chart in cluster mode into a throwaway kind cluster (with a Postgres of its own),
# checks what the readiness probes and Services do, then kills the primary pod twice while writing
# through the main Service and checks that no acknowledged write is lost.
#
#   deploy/helm/refhir/kind-test.sh [--keep]
#
# Needs docker, kind, kubectl, helm, curl and python3, and the image (default refhir:cluster; build
# it with `docker build -t refhir:cluster .`). It creates its own cluster, named below, and never
# touches another kube context; the cluster is deleted afterwards unless --keep is given.
#
# What it checks:
#   - exactly one writer pod is ready (the primary) and the other is not (a standby answers /healthz
#     503), so `<release>-primary` has one endpoint and the main Service has the primary plus both
#     replicas;
#   - for each of two kills of the primary pod: every write acknowledged through the main Service
#     can be read afterwards, commit timestamps only increase, and at most FAIL_BUDGET requests fail.
set -euo pipefail

KEEP=0; [[ "${1:-}" == "--keep" ]] && KEEP=1
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
CLUSTER="${KIND_CLUSTER:-refhir-chart-test}"
NS=refhir-test
RELEASE=r
IMAGE="${IMAGE:-refhir:cluster}"
HOST_PORT="${HOST_PORT:-18080}"
WRITE_SECONDS="${WRITE_SECONDS:-40}"
KILL_AFTER="${KILL_AFTER:-10}"
FAIL_BUDGET="${FAIL_BUDGET:-5}"
CTX="kind-$CLUSTER"
K=(kubectl --context "$CTX" -n "$NS")
BASE="http://localhost:$HOST_PORT"

cleanup() {
  if [[ $KEEP == 0 ]]; then kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true; fi
}
trap cleanup EXIT

docker image inspect "$IMAGE" >/dev/null 2>&1 || { echo "no image $IMAGE; build it first" >&2; exit 2; }
if kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  echo "a kind cluster named $CLUSTER already exists; delete it or set KIND_CLUSTER" >&2; exit 2
fi

echo "creating the kind cluster $CLUSTER"
kind create cluster --name "$CLUSTER" --wait 180s --config - >/dev/null 2>&1 <<YAML
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
    extraPortMappings:
      - containerPort: 30080
        hostPort: $HOST_PORT
        listenAddress: "127.0.0.1"
YAML
kind load docker-image "$IMAGE" --name "$CLUSTER" >/dev/null 2>&1
docker image inspect postgres:17-alpine >/dev/null 2>&1 && kind load docker-image postgres:17-alpine --name "$CLUSTER" >/dev/null 2>&1 || true

wait_for() { # description timeout command...
  local what=$1 seconds=$2; shift 2
  local deadline=$((SECONDS + seconds))
  until "$@" >/dev/null 2>&1; do
    (( SECONDS < deadline )) || { echo "timed out waiting for $what" >&2; "${K[@]}" get pods -o wide >&2 || true; exit 2; }
    sleep 2
  done
}

kubectl --context "$CTX" create namespace "$NS" >/dev/null
"${K[@]}" apply -f - >/dev/null <<'YAML'
apiVersion: apps/v1
kind: Deployment
metadata: {name: pg}
spec:
  replicas: 1
  selector: {matchLabels: {app: pg}}
  template:
    metadata: {labels: {app: pg}}
    spec:
      containers:
        - name: pg
          image: postgres:17-alpine
          args: ["-c", "max_connections=200", "-c", "fsync=off"]
          env:
            - {name: POSTGRES_USER, value: refhir}
            - {name: POSTGRES_PASSWORD, value: refhir}
            - {name: POSTGRES_DB, value: refhir}
          readinessProbe:
            exec: {command: ["pg_isready", "-U", "refhir", "-d", "refhir"]}
            periodSeconds: 2
---
apiVersion: v1
kind: Service
metadata: {name: pg}
spec:
  selector: {app: pg}
  ports: [{port: 5432}]
YAML
wait_for "postgres" 180 "${K[@]}" wait --for=condition=Ready pod -l app=pg --timeout=2s

echo "installing the chart (cluster mode, 2 writers, 2 replicas)"
helm --kube-context "$CTX" -n "$NS" install "$RELEASE" "$ROOT/deploy/helm/refhir" \
  --set image.repository="${IMAGE%%:*}" --set image.tag="${IMAGE##*:}" --set image.pullPolicy=Never \
  --set cluster.enabled=true --set database.kind=postgres \
  --set database.postgresUrl=postgres://refhir:refhir@pg:5432/refhir >/dev/null
# The main Service is exposed on a fixed node port so the host reaches it through kube-proxy,
# which spreads requests over the ready pods only, as a real client would see it.
"${K[@]}" patch service "$RELEASE-refhir" --type merge \
  -p '{"spec":{"type":"NodePort","ports":[{"name":"http","port":8080,"targetPort":"http","nodePort":30080}]}}' >/dev/null

ready_addresses() { # service -> names of the pods the Service sends traffic to
  "${K[@]}" get endpoints "$1" -o jsonpath='{range .subsets[*].addresses[*]}{.targetRef.name}{"\n"}{end}' 2>/dev/null
}
primary_pod() { ready_addresses "$RELEASE-refhir-primary" | head -1; }
epoch() {
  "${K[@]}" exec deploy/pg -- psql -U refhir -d refhir -Atc \
    "SELECT json_value FROM persistence_globals WHERE key = 'writer_epoch'" 2>/dev/null || echo "?"
}
has_primary() { [[ -n "$(primary_pod)" ]]; }
steady() { # one primary endpoint, and the main Service has the primary plus both replicas
  [[ $(ready_addresses "$RELEASE-refhir-primary" | wc -l) -eq 1 && $(ready_addresses "$RELEASE-refhir" | wc -l) -eq 3 ]]
}

echo "waiting for a primary (the first start installs the FHIR packages)"
wait_for "a primary" 900 has_primary
wait_for "the primary plus two replicas behind the main Service" 300 steady
echo "--- readiness"
"${K[@]}" get pods -l app.kubernetes.io/instance="$RELEASE" \
  -o custom-columns='POD:.metadata.name,READY:.status.containerStatuses[0].ready,ROLE:.spec.containers[0].env[?(@.name=="REFHIR_ROLE")].value' | sed 's/^/    /'
writers_ready=$("${K[@]}" get pods -l refhir.io/component=writer -o jsonpath='{range .items[*]}{.status.containerStatuses[0].ready}{"\n"}{end}' 2>/dev/null | grep -c true || true)
echo "    ready writer pods: $writers_ready (want 1), primary Service endpoints: $(ready_addresses "$RELEASE-refhir-primary" | wc -l) (want 1), main Service endpoints: $(ready_addresses "$RELEASE-refhir" | wc -l) (want 3)"
[[ "$writers_ready" == 1 ]] || { echo "FAIL: expected exactly one ready writer pod" >&2; exit 1; }
wait_for "the node port to answer" 60 curl -sf "$BASE/metadata" -o /dev/null

round() {
  local label=$1 victim
  victim=$(primary_pod)
  echo "--- $label: the primary is $victim, epoch $(epoch)"
  local log; log=$(mktemp)
  python3 "$ROOT/deploy/cluster/failover_load.py" write "$BASE" "$WRITE_SECONDS" "$label" >"$log" &
  local writer=$!
  sleep "$KILL_AFTER"
  "${K[@]}" delete pod "$victim" --grace-period=0 --force >/dev/null 2>&1
  echo "    deleted $victim (a hard kill) after ${KILL_AFTER}s"
  wait "$writer"
  python3 "$ROOT/deploy/cluster/failover_load.py" verify "$BASE" "$log" "$FAIL_BUDGET"
  rm -f "$log"
  wait_for "the Deployment to replace the pod and one primary plus two replicas to serve" 300 steady
  echo "    now the primary is $(primary_pod), epoch $(epoch)"
}

round "round 1"
round "round 2"
echo "PASS  chart: one ready primary, standby not ready, no acknowledged write lost across two primary pod kills"
