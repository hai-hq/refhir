# Several processes on one database

By default there is one process. To survive a crash or serve more reads, run several against one
**Postgres** database (a SQLite file can also be shared by a primary and standbys on one machine):

| Role | What it does |
|------|--------------|
| `primary` | The one process that writes. It holds a lease on the database and refuses to start while another primary has it. |
| `standby` (`--standby`) | Waits for the lease and starts serving the moment the primary is gone. |
| `replica` | Serves reads from what the primary has published and sends everything it cannot do (writes, `$export`, `$install`) to `--primary-url`. |

```bash
refhir --postgres $URL                                                  # primary
refhir --postgres $URL --standby                                        # takes over
refhir --postgres $URL --role replica --primary-url http://primary:8080 # reads
```

- **Writer lease.** A session-level Postgres advisory lock on a dedicated connection (a file lock
  `<db>.lock` for SQLite), kept alive every two seconds. If the connection is lost the process
  exits. Each takeover bumps a `writer_epoch`, and every write checks it inside its own
  transaction, so a paused primary that wakes up after its successor took over is fenced and
  exits instead of writing.
- **Load balancers.** `GET /healthz` is 200 when a process can take traffic and 503 when it
  cannot: a standby, a process still starting, or a replica that has stopped following the
  primary. `?verbose=1` adds connection pool and commit timing counters.
- **Reading your own writes.** Write responses carry `X-Refhir-Commit-Ts` (reads carry the
  snapshot they ran at). Send the largest value you have seen as `X-Refhir-Min-Ts` and a replica
  waits briefly for its snapshot to reach it, then asks the primary. A replica that has no
  `--primary-url` to ask answers `503` with `Retry-After: 1` instead of an older snapshot. The Go
  and TypeScript clients do this on their own.
- **`/sync`** works on a replica: queries are answered and invalidated locally; mutations go to
  the primary on a connection of their own. `$websocket` subscriptions, Subscription delivery and
  `$export` live on the primary.
- **SMART.** Tokens, clients and the signing key are in the database, so any process accepts a
  token another issued. Every process must be given the same `--smart-dcr-token` and
  `--smart-confidential-secret`; standbys and replicas refuse to start without them.
- **Catalog.** Search parameters and profiles are loaded into memory, so two counters in the
  database tell other processes to reload. A replica follows the *version*, which moves only when
  no search parameter is waiting for its backfill: it is bumped when a backfill completes (or at
  once for a change with nothing to backfill), so a replica never loads a parameter whose index is
  incomplete. A standby follows the *revision*, which moves on every change, because after a
  takeover it may have to finish the backfill itself.
- **Backfills.** Each backfill job records the writer epoch that claims it. A standby that takes
  over resumes a job its predecessor left half done from the stored cursor and takes the claim;
  an older writer is refused a job a newer one holds. Backfill progress, the claim, installs and
  the catalog counters are written inside a transaction that first checks the writer epoch, like
  document commits, so a replaced primary cannot alter them.

Running it: [deploy/cluster](../deploy/cluster/README.md) (Docker Compose with HAProxy), the Helm
chart's `cluster.enabled`, and measurements in [docs/perf](perf/baseline.md).
