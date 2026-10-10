#!/usr/bin/env bash
# Standalone SMART launch + PKCE + interact approve (same flow as server unit tests).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib.sh
source "${ROOT}/lib.sh"

BASE="$(inferno_base_url)"
REDIRECT="${SMOKE_REDIRECT_URI:-http://127.0.0.1/cb}"
AUD="$(inferno_expected_issuer)"
SCOPE="launch/patient openid fhirUser offline_access patient/Patient.rs"
VERIFIER="abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"

challenge="$(inferno_pkce_challenge "${VERIFIER}")"

query="$(python3 - <<PY
from urllib.parse import urlencode
print(urlencode({
    "response_type": "code",
    "client_id": "public",
    "redirect_uri": "${REDIRECT}",
    "scope": "${SCOPE}",
    "aud": "${AUD}",
    "code_challenge": "${challenge}",
    "code_challenge_method": "S256",
    "state": "inferno-smoke",
}))
PY
)"

echo "Authorize (expect redirect to interact) ..."
location="$(inferno_curl_location "${BASE}/oauth/authorize?${query}")"
echo "Approving interact session ..."
location="$(inferno_interact_approve "${BASE}" "${location}" "inferno" "p1")"
code="$(inferno_code_from_redirect "${location}")"

echo "Token exchange ..."
token_body="$(curl -sfS -X POST "${BASE}/oauth/token" \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  --data-urlencode "grant_type=authorization_code" \
  --data-urlencode "code=${code}" \
  --data-urlencode "redirect_uri=${REDIRECT}" \
  --data-urlencode "client_id=public" \
  --data-urlencode "code_verifier=${VERIFIER}")"

export TOKEN_BODY="${token_body}"
python3 - <<'PY'
import json, os
body = json.loads(os.environ["TOKEN_BODY"])
assert body.get("token_type") == "Bearer", body
assert body.get("patient") == "p1", body
assert body.get("access_token"), body
assert body.get("refresh_token"), body
assert body.get("id_token"), body
print("OAuth standalone smoke OK")
PY
