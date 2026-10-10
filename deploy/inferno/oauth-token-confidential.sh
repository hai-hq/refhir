#!/usr/bin/env bash
# Authorization code + PKCE for the built-in confidential client (seed / admin).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib.sh
source "${ROOT}/lib.sh"

BASE="$(inferno_base_url)"
REDIRECT="${SEED_REDIRECT_URI:-http://127.0.0.1/cb}"
CLIENT_ID="${SEED_CLIENT_ID:-confidential}"
CLIENT_SECRET="${SEED_CLIENT_SECRET:-inferno-confidential}"
SCOPE="${SEED_SCOPE:-user/Patient.read user/Patient.write}"
VERIFIER="abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
AUD="$(inferno_expected_issuer)"

challenge="$(inferno_pkce_challenge "${VERIFIER}")"

query="$(python3 - <<PY
from urllib.parse import urlencode
print(urlencode({
    "response_type": "code",
    "client_id": "${CLIENT_ID}",
    "redirect_uri": "${REDIRECT}",
    "scope": "${SCOPE}",
    "aud": "${AUD}",
    "code_challenge": "${challenge}",
    "code_challenge_method": "S256",
    "state": "seed",
}))
PY
)"

location="$(inferno_curl_location "${BASE}/oauth/authorize?${query}")"
location="$(inferno_interact_approve "${BASE}" "${location}" "seed")"
code="$(inferno_code_from_redirect "${location}")"

curl -sfS -X POST "${BASE}/oauth/token" \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  --data-urlencode "grant_type=authorization_code" \
  --data-urlencode "code=${code}" \
  --data-urlencode "redirect_uri=${REDIRECT}" \
  --data-urlencode "client_id=${CLIENT_ID}" \
  --data-urlencode "client_secret=${CLIENT_SECRET}" \
  --data-urlencode "code_verifier=${VERIFIER}" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["access_token"])'
