#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib.sh
source "${ROOT}/lib.sh"

BASE="$(inferno_base_url)"
EXPECTED_ISSUER="$(inferno_expected_issuer)"
deadline="${DEADLINE_SECS:-300}"

echo "Waiting for ReFHIR at ${BASE} (up to ${deadline}s) ..."
for ((i = 0; i < deadline; i += 2)); do
  if curl -sfS "${BASE}/metadata" >/dev/null 2>&1; then
    if curl -sfS "${BASE}/.well-known/smart-configuration" >/dev/null 2>&1; then
      export EXPECTED_ISSUER
      if curl -sfS "${BASE}/.well-known/smart-configuration" | python3 -c '
import json, os, sys
d = json.load(sys.stdin)
expected = os.environ["EXPECTED_ISSUER"].rstrip("/")
issuer = (d.get("issuer") or "").rstrip("/")
sys.exit(0 if issuer == expected and d.get("authorization_endpoint") else 1)
'; then
        echo "ReFHIR is ready (FHIR + SMART discovery)."
        exit 0
      fi
    fi
  fi
  sleep 2
done

echo "Timed out waiting for ${BASE} (metadata + SMART discovery)" >&2
exit 1
