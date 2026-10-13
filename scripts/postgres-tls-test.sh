#!/usr/bin/env bash
# A throwaway Postgres that accepts only TLS connections, for the TLS tests of refhir-postgres.
#
#   out="$(scripts/postgres-tls-test.sh up)" && eval "$out"   # starts it, exports what the tests read
#   (not eval "$(...)": that would hide a failed `up`)
#   cargo test -p refhir-postgres tls
#   scripts/postgres-tls-test.sh down           # removes the container and the certificates
#
# What it sets up:
#   * a CA and a server certificate valid for the name `localhost` only (not for 127.0.0.1, so
#     verify-full fails and verify-ca passes when the tests connect by address);
#   * a second, unrelated CA, to prove a certificate from the wrong CA is refused;
#   * a client certificate for the role `refhir_cert`, which pg_hba accepts only with it;
#   * a pg_hba.conf that rejects every connection that is not TLS (`hostnossl ... reject`).
# Environment: REFHIR_TLS_PG_NAME (container, default refhir-tls-pg-test), REFHIR_TLS_PG_IMAGE
# (default postgres:16-alpine), REFHIR_TLS_PG_DIR (where the certificates go; the script only
# deletes a directory that holds its marker file, so it never clears one it did not create).
set -euo pipefail

name="${REFHIR_TLS_PG_NAME:-refhir-tls-pg-test}"
image="${REFHIR_TLS_PG_IMAGE:-postgres:16-alpine}"
dir="${REFHIR_TLS_PG_DIR:-${TMPDIR:-/tmp}/refhir-tls-pg-test}"

marker=".refhir-tls-pg-test"

case "${1:-}" in
  down)
    docker rm -f "$name" >/dev/null 2>&1 || true
    [ -f "$dir/$marker" ] && rm -rf "$dir"
    exit 0
    ;;
  up) ;;
  *)
    echo "usage: $0 up|down" >&2
    exit 2
    ;;
esac

docker rm -f "$name" >/dev/null 2>&1 || true
if [ -f "$dir/$marker" ]; then
  rm -rf "$dir"
elif [ -e "$dir" ] && [ -n "$(ls -A "$dir" 2>/dev/null)" ]; then
  echo "$dir exists and was not created by this script; choose another REFHIR_TLS_PG_DIR" >&2
  exit 1
fi
mkdir -p "$dir"
touch "$dir/$marker"
cd "$dir"

# Quiet on stdout, which the caller evals; openssl errors still reach stderr.
{
  ca() { # ca <prefix> <common name>
    openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$1.key"
    printf 'basicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\n' >"$1.ext"
    openssl req -new -key "$1.key" -subj "/CN=$2" -out "$1.csr"
    openssl x509 -req -in "$1.csr" -signkey "$1.key" -days 2 -extfile "$1.ext" -out "$1.pem"
  }
  leaf() { # leaf <prefix> <common name> <extensions>
    openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$1.key"
    printf '%s\n' "$3" >"$1.ext"
    openssl req -new -key "$1.key" -subj "/CN=$2" -out "$1.csr"
    openssl x509 -req -in "$1.csr" -CA ca.pem -CAkey ca.key -CAcreateserial -days 2 \
      -extfile "$1.ext" -out "$1.crt"
  }
  ca ca "refhir test CA"
  ca wrong-ca "some other CA"
  leaf server localhost "subjectAltName=DNS:localhost
extendedKeyUsage=serverAuth"
  leaf client refhir_cert "extendedKeyUsage=clientAuth"
} >/dev/null

cat >pg_hba.conf <<'EOF'
local all all trust
hostnossl all all all reject
hostssl all refhir_cert all cert
hostssl all all all scram-sha-256
EOF

# Postgres wants its key owned by it and not readable by others, which a bind mount cannot
# give, so the container copies the files first.
docker run -d --name "$name" \
  -e POSTGRES_PASSWORD=postgres \
  -p 127.0.0.1::5432 \
  -v "$dir:/tls:ro" \
  --entrypoint sh "$image" -c '
    mkdir -p /var/lib/pgtls && cp /tls/* /var/lib/pgtls/ &&
    chown -R postgres:postgres /var/lib/pgtls && chmod 600 /var/lib/pgtls/*.key &&
    exec docker-entrypoint.sh postgres \
      -c ssl=on \
      -c ssl_cert_file=/var/lib/pgtls/server.crt \
      -c ssl_key_file=/var/lib/pgtls/server.key \
      -c ssl_ca_file=/var/lib/pgtls/ca.pem \
      -c hba_file=/var/lib/pgtls/pg_hba.conf' >/dev/null

# The entrypoint starts a temporary server to run its init, then restarts: wait for the
# second one, which is the only one that listens on TCP.
for _ in $(seq 1 60); do
  docker exec "$name" pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1 && break
  sleep 1
done
docker exec "$name" pg_isready -h 127.0.0.1 -U postgres >/dev/null

docker exec -i "$name" psql -U postgres -v ON_ERROR_STOP=1 >/dev/null <<'EOF'
CREATE ROLE refhir_cert LOGIN;
CREATE DATABASE tls_cert OWNER refhir_cert;
EOF

port="$(docker port "$name" 5432/tcp | head -1 | sed 's/.*://')"
echo "export REFHIR_TLS_POSTGRES_ADDR=localhost:$port"
echo "export REFHIR_TLS_POSTGRES_DIR=$dir"
