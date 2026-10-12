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

`failover-test.sh` does this for you and checks the result: it writes through HAProxy, kills
the live primary twice (once in each direction), and then reads back every acknowledged write
asking for at least the commit timestamp it was acknowledged with.

```bash
deploy/cluster/failover-test.sh          # builds the image if there is none; --keep leaves the stack up
```

`failover-test.sh` and `deploy/helm/refhir/kind-test.sh` share the write-and-check code in
`failover_load.py`. Last run, on the final code, with the full FHIR R4 core installed:

| | round 1 (writer2 killed) | round 2 (writer1 killed) |
|---|---|---|
| Writes acknowledged | 187 | 182 |
| Requests that failed | 0 | 1 (a 502 in flight on the killed primary) |
| Longest gap between two acknowledgements | 4.2 s | 3.3 s |
| Acknowledged writes missing afterwards | 0 | 0 |
| Commit timestamps strictly increasing | yes | yes |
| Writer epoch afterwards | 2 | 3 |

An earlier run, on a machine that was also running other heavy programs, had gaps of 7.2 s and
3.8 s, so expect a few seconds and more when the machine is busy.

Where the gap goes, from one more run that logged the standby's clock against the moment of the
kill (same loaded machine):

| Time after the kill | What happened |
|---|---|
| +2.74 s | the standby took the writer lease (it checks once a second; the rest is how long Postgres took to release the dead process's lock, which was not measured separately) |
| +2.79 s | it was serving: its own start after winning took 55 ms, because it keeps its catalog loaded while it waits (an earlier run on a quieter machine: 58 ms and 71 ms, against about 17 s before the catalog was kept warm) |
| +4.72 s | HAProxy had put it in rotation (two good checks one second apart) |

So a client sees a gap of about 4 to 7 seconds on this machine, a little under two thirds of it the
standby noticing and the rest the load balancer. Lower `inter` and `rise` in `haproxy.cfg` to
shorten the HAProxy part, at the price of more health-check traffic.

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
echo the header themselves. The load generator shows the effect:

```bash
go run ./clients/go/cmd/loadgen --base http://localhost:8080 --duration 20s \
  --read-your-writes --verify-ryw
```

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
