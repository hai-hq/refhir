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

- **Writer lease.** The primary holds a lease on the database (a Postgres advisory lock, or a file
  lock `<db>.lock` for SQLite). If it loses the lease the process exits, and a paused primary that
  wakes up after a standby took over is fenced off and exits instead of writing.
- **Conformance changes.** Installing a package, or writing a SearchParameter, CodeSystem,
  ValueSet or ConceptMap, changes the catalog the processes serve from. A replica notices within a
  moment and reloads it in the background, answering from the old catalog until the new one is
  ready; a standby keeps its own copy current for when it takes over. A package or a
  SearchParameter reloads everything (a few seconds of one thread), and a replica does not load a
  search parameter until its backfill has finished. A CodeSystem, ValueSet or ConceptMap reloads
  only the terminology, which takes a fraction of that and does not wait for a backfill. Writing
  many of them one by one keeps replicas reloading; a `transaction` Bundle or a package is one
  change, and so one reload. A primary also tells them when it starts, in case the process before
  it stopped between writing and telling, so each replica and standby reloads everything once
  when a primary starts or a standby takes over.
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

Running it: [deploy/cluster](../deploy/cluster/README.md) (Docker Compose with HAProxy) and the Helm
chart's `cluster.enabled`.

## Tenants on one database

One Postgres database can hold many tenants. A tenant is a separate FHIR server: its resources,
SMART clients and audit trail are its own, and nothing one tenant does reaches another. Tenants
share the database's tables, so a new tenant is a registration and not a new database, and each
tenant has a writer of its own: write capacity grows with the number of tenants.

```bash
refhir tenant create acme --postgres $URL                                          # once per tenant
refhir --postgres $URL --tenant acme                                               # its primary
refhir --postgres $URL --tenant acme --standby                                     # takes over
refhir --postgres $URL --tenant acme --role replica --primary-url http://acme:8080 # reads
```

- **A database keeps its layout.** The first `refhir tenant create` in an empty database makes it a
  shared one. A database that already holds a ReFHIR made without tenants cannot hold tenants, and
  a shared database cannot be opened without naming a tenant: each is refused before a row is read.
- **Roles.** A tenant has its own primary, standbys and replicas, as above. A process never reads,
  writes or waits for another tenant's rows, and holds its own writer lease. `GET /healthz` names
  the tenant.
- **A replica writes into its own tenant only.** The primary a replica forwards to refuses the
  request (`421`) if it serves a different tenant, so a wrong `--primary-url` cannot write into a
  tenant that is not the replica's own.
- **SMART.** Each tenant has its own clients, tokens and signing key, so give each tenant its own
  `--public-url`. The tenant's processes share `--smart-dcr-token` and
  `--smart-confidential-secret`, as above.
- **Packages.** The FHIR packages that ship with ReFHIR (the core definitions, terminology and
  what they depend on) are stored once for all tenants. A new tenant is given them in moments
  instead of reading and storing its own copy, and its start-up does not grow with the number of
  tenants. What a tenant writes (its own definitions, code systems and search parameters) and
  every package installed from a registry or a directory stay its own.
- **Files.** A tenant's package cache and export directory default to a folder named for it.
- **Routing.** Route requests to a tenant's processes by host name or path in front of ReFHIR, with
  a load balancer or ingress: each tenant is its own set of processes.
- **Managing tenants.** `refhir tenant list` shows the tenants and whether a primary of each is
  running. `refhir tenant delete NAME --yes` removes a tenant and everything it holds. It refuses
  while a primary of the tenant runs, and it cannot be undone, so take a backup first.
- **Connections.** Every process opens its pool, one connection that holds the writer lease and one
  that listens for the primary's announcements. `max_connections` must cover the processes of every
  tenant. A proxy that pools connections per transaction (PgBouncer in transaction mode) cannot sit
  between ReFHIR and the database: the lease and the announcements belong to a session.
- **Upgrades.** The schema is shared, so it is migrated once for every tenant: run the same release
  in every tenant's processes (see [Schema migrations](migrations.md)).
