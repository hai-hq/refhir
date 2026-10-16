# Several refhir processes behind a load balancer

`docker-compose.yml` starts one Postgres database, two **writers** (one serves as the primary, the
other waits to take over), two **replicas** and an HAProxy load balancer. It is the smallest deployment that survives losing the primary and
spreads reads over more than one process.

```bash
docker compose -f deploy/cluster/docker-compose.yml up --build
curl http://localhost:8080/metadata        # through the load balancer
open http://localhost:8404                 # HAProxy: which processes are up
```

The first start installs the FHIR packages into the database (it needs the package registry, as
a single process does). The other processes wait for that, then follow.

## What each piece does

| Service | Role | Behaviour |
|---------|------|-----------|
| `writer1`, `writer2` | `standby` | Both start as standbys and answer `/healthz` with 503 until they hold the writer lease. The first to take it is the primary; the other waits and takes over within seconds if it goes away. |
| `replica1`, `replica2` | `replica` | Serve reads; send writes to `http://lb:8081`. |
| `lb` | HAProxy | Port 8080 for clients, 8081 only for replicas, 8404 for HAProxy's own status page. |

Neither writer is "the" primary, so it does not matter which starts first, and a writer that
is restarted after a failure comes back as the standby. The services restart after exiting.

HAProxy asks every process `GET /healthz` once a second. A standby and a replica that has fallen
behind answer 503 and receive nothing. Port 8081 routes only to a process that reports
`"role":"primary"`, so replicas always reach whichever process is the primary now, including
after a failover.

Which writer is the primary right now:

```bash
curl -s 'http://localhost:8404/;csv' | awk -F, '$1=="writer" && $18=="UP" {print $2}'
```

## Try a failover

```bash
# Terminal 1: write through the load balancer
while true; do
  curl -s -o /dev/null -w '%{http_code}\n' -X POST -H 'content-type: application/fhir+json' \
    -d "{\"resourceType\":\"Patient\",\"id\":\"p$RANDOM$RANDOM\",\"active\":true}" \
    http://localhost:8080/Patient
  sleep 0.2
done

# Terminal 2: kill whichever writer is the primary (writer1 or writer2)
docker compose -f deploy/cluster/docker-compose.yml kill writer2
```

The other writer finds the lease free within a second, loads nothing it does not already have
(a standby keeps the catalog loaded and current while it waits), and starts serving. Commit
timestamps carry on from where the old primary stopped, and every write that returned 201 is still
there. A request that was in flight on the old primary can fail; that is the most a client sees.

`docker compose kill` is a hard stop, which Docker treats as a manual stop, so start the writer
again yourself and it comes back as the standby:

```bash
docker compose -f deploy/cluster/docker-compose.yml start writer2
```

## Reading your own writes

Replicas follow the primary within tens of milliseconds, not instantly. A write response carries
`X-Refhir-Commit-Ts`. Send it back as `X-Refhir-Min-Ts` on the next read and a replica either has
that snapshot or asks the primary, so you never read a stale answer. A replica started without
`--primary-url` cannot ask, so it answers `503` with `Retry-After: 1` (a transient error) rather than
return an older snapshot; retry, or give it the primary's address. The Go and TypeScript clients
echo the header themselves.

## SMART

SMART is off in this file. To turn it on, give every service the same `command` (for example
`["--smart", "--oidc-issuer", "...", "--oidc-client-id", "..."]`, or `--smart-demo-login` on a
throwaway stack) and uncomment `REFHIR_SMART_DCR_TOKEN`, `REFHIR_SMART_CONFIDENTIAL_SECRET` and
`REFHIR_PUBLIC_URL` in `docker-compose.yml`. The two secrets must be identical on every process:
a standby or replica refuses to start without them, because otherwise each process would invent
its own and the last one to start would replace the secret the others use. Tokens, clients and the
signing key live in the database, so a token issued by one process is accepted by all of them.

## Notes

- Each process has its own package cache and export directory (`/data` volumes). An `$export` job
  belongs to the process that accepted it, and replicas send export requests to the primary.
- The writer lease needs one database and one primary at a time; do not point two clusters at the
  same database.
- `max_connections` on Postgres must cover `REFHIR_DB_POOL` times the number of processes, plus
  the one lease connection each writer keeps.
