# Operations

## Health probes

Use these for Kubernetes probes and other orchestration, not `/metadata`: that builds a full
CapabilityStatement on every call. Both are open under `--smart` (no token, no scopes), return a
small JSON body, read no resource, and are not logged.

| Endpoint | Question | Answer |
|----------|----------|--------|
| `GET /livez` | Is the process up and the HTTP server answering? | Always `200 {"status":"ok"}` |
| `GET /readyz` | Can it take traffic? | `200 {"status":"ok"}`, or `503 {"status":"starting"\|"unavailable","reason":"..."}` |

`/readyz` is `starting` while startup runs: the database is opened, packages are installed, search
indexes are backfilled (a standby is `starting` until it holds the writer lease, a replica until
the primary's catalog exists). Every role listens from the moment it starts, so a process that is
still starting answers `503` instead of refusing connections. Once serving it is `unavailable`
when a one-row query to the database (it reads no resource) fails or takes more than two seconds,
when a replica has stopped following the primary, or when the primary cannot record its published
watermark. The `reason` is a fixed phrase. The cause is in the debug log. The database result is
reused for one second and probes that overlap share one query, so more callers cost no more queries.

```yaml
livenessProbe:  { httpGet: { path: /livez,  port: http } }
readinessProbe: { httpGet: { path: /readyz, port: http } }
```

`GET /healthz` is older and answers a load balancer's question (200 or 503, with `role`, and
`?verbose=1` for pool and commit counters, which needs a token under `--smart`). It is not a
liveness probe: a standby answers it with 503.

## Postgres and TLS

`--postgres` takes a URL (`postgresql://user:pass@host:5432/db`) or libpq `host=... user=...`
pairs. TLS is chosen with libpq's parameters in that string:

```bash
refhir --postgres 'postgresql://refhir:...@db.example.com:5432/refhir?sslmode=verify-full&sslrootcert=/etc/refhir-pg-tls/ca.crt'
```

| `sslmode` | Encrypted | Server certificate |
|-----------|-----------|--------------------|
| *(absent)*, `disable` | never | n/a. **The default**: nothing changes for URLs that never set it |
| `prefer` | if the server offers TLS | not checked |
| `require` | always | not checked, unless `sslrootcert` is given, in which case the chain is (as in libpq) |
| `verify-ca` | always | chain must lead to `sslrootcert`; the host name is not checked |
| `verify-full` | always | chain must lead to `sslrootcert` and the certificate must be valid for the host in the URL |

- **Use `verify-full` in production.** `prefer` and `require` without `sslrootcert` stop someone
  listening on the network, not someone answering in the server's place.
- `sslrootcert=<file>`: PEM file with the CA certificate(s) that signed the server's certificate
  (required by `verify-ca` and `verify-full`). There is no system trust store fallback.
- `sslcert=<file>` and `sslkey=<file>`: PEM client certificate and private key, for servers
  that authenticate with them (`hostssl ... cert` in `pg_hba.conf`). Give both or neither.
- The files are re-read whenever their modification time changes, so a rotated certificate is
  picked up without a restart. Percent-encode paths in a URL if they hold special characters.
- As in libpq, TLS is not used over a Unix socket (`host=/var/run/postgresql`), whatever the
  `sslmode`. `sslnegotiation=direct` works with Postgres 17 and later.
- The certificate must carry the host name or address in `subjectAltName`; a common name alone
  is not accepted. `allow` and `PGSSL*` environment variables are not supported.
- A mistake (an unknown mode, `verify-full` with no `sslrootcert`, certificate files with
  `sslmode=disable`) fails at startup instead of connecting without the check.

## Export jobs

**Export jobs are durable.** `$export` records each job in a `jobs` table in the same database (SQLite file or Postgres), so `GET /export/{job}` and `GET /export/{job}/file` still answer after a restart. The NDJSON output is a file under `[export].dir`. With `--memory` the table is in memory too, so nothing survives a restart.

**Any number of servers can share the job table.** A job is worked on by the server that holds its *lease*: an owner and an expiry, taken in one atomic statement (on Postgres with `FOR UPDATE SKIP LOCKED`, so servers do not queue behind each other), renewed every third of its 30 seconds while the work goes on, and released when it ends. A server that dies simply stops renewing; each server sweeps for jobs nobody holds, so another one takes the job once the lease runs out, without waiting for a restart. Only the holder can record how a job ended, so a server that lost its lease cannot overwrite the result of the one that took over. A server takes only jobs of kinds it has a handler for, so an older server in a rolling deploy leaves a newer kind of job alone. A job that has been started three times without finishing is marked failed rather than run forever. Exports write to the local `[export].dir`, so run them where that directory is shared if more than one server is serving `/export`.
