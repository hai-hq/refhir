#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib.sh
source "${ROOT}/lib.sh"

BASE="$(inferno_base_url)"
EXPECTED_ISSUER="$(inferno_expected_issuer)"

echo "Checking CapabilityStatement ..."
meta="$(curl -sfS "${BASE}/metadata")"
echo "${meta}" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d.get("resourceType") == "CapabilityStatement", d
rest = d.get("rest") or []
assert rest, "missing rest"
oauth = (rest[0].get("security") or {}).get("extension") or []
uris = [e.get("url") for e in oauth if isinstance(e, dict)]
assert any(u and "oauth-uris" in u for u in uris), "missing oauth-uris extension"
print("CapabilityStatement OK")
'

echo "Checking SMART discovery ..."
disc="$(curl -sfS "${BASE}/.well-known/smart-configuration")"
export EXPECTED_ISSUER
echo "${disc}" | python3 -c '
import json, os, sys
d = json.load(sys.stdin)
expected = os.environ["EXPECTED_ISSUER"].rstrip("/")
issuer = (d.get("issuer") or "").rstrip("/")
assert issuer == expected, f"unexpected issuer: {issuer!r} (expected {expected!r})"
for key in (
    "authorization_endpoint",
    "token_endpoint",
    "registration_endpoint",
    "introspection_endpoint",
    "revocation_endpoint",
    "jwks_uri",
):
    assert d.get(key), f"missing {key}"
caps = set(d.get("capabilities") or [])
for needed in (
    "launch-ehr",
    "launch-standalone",
    "client-public",
    "sso-openid-connect",
    "permission-patient",
):
    assert needed in caps, f"missing capability {needed}"
print("SMART discovery OK")
'

echo "All discovery smoke checks passed."
