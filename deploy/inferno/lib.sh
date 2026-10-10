#!/usr/bin/env bash
# Shared helpers for Inferno smoke scripts (source from other scripts).

inferno_base_url() {
  echo "${REFHIR_URL:-http://localhost:8080}"
}

inferno_expected_issuer() {
  local base
  base="${REFHIR_PUBLIC_URL:-$(inferno_base_url)}"
  base="${base%/}"
  echo "${base}"
}

inferno_curl_location() {
  # Prints the first Location header from a response (follows one redirect chain entry).
  curl -sS -D - -o /dev/null "$@" | tr -d '\r' | awk 'tolower($1)=="location:" {print $2; exit}'
}

inferno_oauth_redirect_or_fail() {
  local location="$1"
  local step="$2"
  if [[ -z "${location}" ]]; then
    echo "${step}: missing Location header" >&2
    exit 1
  fi
  if [[ "${location}" == *"error="* ]] || [[ "${location}" == *"error?"* ]]; then
    python3 - <<PY
from urllib.parse import urlparse, parse_qs
import sys
q = parse_qs(urlparse("${location}").query)
err = q.get("error", ["unknown"])[0]
desc = q.get("error_description", [""])[0]
print(f"${step}: OAuth error={err} {desc}", file=sys.stderr)
sys.exit(1)
PY
  fi
}

inferno_interact_approve() {
  local base="$1"
  local location="$2"
  local username="$3"
  local patient_id="${4:-}"

  inferno_oauth_redirect_or_fail "${location}" "authorize"

  if [[ "${location}" != *"/oauth/interact"* ]]; then
    echo "${location}"
    return 0
  fi

  local session
  session="$(python3 - <<PY
from urllib.parse import urlparse, parse_qs
print(parse_qs(urlparse("${location}").query)["session"][0])
PY
)"

  local -a form=(
    --data-urlencode "session=${session}"
    --data-urlencode "username=${username}"
    --data-urlencode "action=approve"
  )
  if [[ -n "${patient_id}" ]]; then
    form+=(--data-urlencode "patient_id=${patient_id}")
  fi

  location="$(curl -sS -D - -o /dev/null -X POST "${base}/oauth/interact" \
    -H 'Content-Type: application/x-www-form-urlencoded' \
    "${form[@]}" | tr -d '\r' | awk 'tolower($1)=="location:" {print $2; exit}')"

  inferno_oauth_redirect_or_fail "${location}" "interact"
  echo "${location}"
}

inferno_code_from_redirect() {
  local location="$1"
  python3 - <<PY
from urllib.parse import urlparse, parse_qs
import sys
q = parse_qs(urlparse("${location}").query)
if "code" not in q:
    print(f"redirect missing code: ${location}", file=sys.stderr)
    sys.exit(1)
print(q["code"][0])
PY
}

inferno_pkce_challenge() {
  local verifier="$1"
  export VERIFIER="${verifier}"
  python3 - <<'PY'
import base64, hashlib, os
verifier = os.environ["VERIFIER"]
digest = hashlib.sha256(verifier.encode()).digest()
print(base64.urlsafe_b64encode(digest).decode().rstrip("="))
PY
}
