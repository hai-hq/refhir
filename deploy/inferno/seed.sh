#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=lib.sh
source "${ROOT}/lib.sh"

BASE="$(inferno_base_url)"

echo "Obtaining write token (confidential client) ..."
token="$("${ROOT}/oauth-token-confidential.sh")"

status="$(curl -sS -o /dev/null -w "%{http_code}" \
  -H "Authorization: Bearer ${token}" \
  "${BASE}/Patient/p1")"
if [[ "${status}" == "200" ]]; then
  echo "Patient/p1 already present at ${BASE}"
  exit 0
fi
if [[ "${status}" != "404" ]]; then
  echo "unexpected GET Patient/p1 status: ${status}" >&2
  exit 1
fi

# The server assigns the id of a resource it creates, so Patient/p1 is created with PUT, which
# refhir.toml allows with update_as_create.
echo "Seeding Patient/p1 at ${BASE} ..."
curl -sfS -X PUT "${BASE}/Patient/p1" \
  -H 'Content-Type: application/fhir+json' \
  -H "Authorization: Bearer ${token}" \
  --data-binary @"${ROOT}/patient-p1.json"
echo "OK"
