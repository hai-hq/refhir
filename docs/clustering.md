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
- **Replica latency.** A tenant's primary tells its replicas about new commits at most every 20 ms:
  the first commit after a quiet spell at once, and the rest of a burst together. Announcing every
  commit would have each one take a lock that the whole Postgres server shares and wake the
  listening connections of the database, so many busy tenants would slow each other down. A replica
  can be up to 20 ms later than it would have been inside a burst of writes, and not at all when
  writes are sparse.
- **A replica writes into its own tenant only.** The primary a replica forwards to refuses the
  request (`421`) if it serves a different tenant, so a wrong `--primary-url` cannot write into a
  tenant that is not the replica's own.
- **Row-level security.** Every table that holds tenants' rows has a policy that shows a connection
  only the rows of the tenant it serves and lets it write only its own, so a process cannot read or
  change another tenant's rows even if a statement asks for them. Superusers, and roles with
  `BYPASSRLS`, are never bound by policies, so run ReFHIR as an ordinary role that owns its
  database (`CREATE ROLE refhir LOGIN PASSWORD '…'`, then `CREATE DATABASE refhir OWNER refhir`).
  The policies bind the owner too. A process that connects as a role the policies do not bind
  works, and logs a warning at start-up that the database is not keeping it to its tenant. Take
  backups as a superuser or a role with `BYPASSRLS`: `pg_dump` run as the ordinary role refuses,
  because it would not see every tenant's rows, and `--enable-row-security` would let it run and
  dump none of them.
- **SMART.** Each tenant has its own clients, tokens and signing key, so give each tenant its own
  `--public-url`. The tenant's processes share `--smart-dcr-token` and
  `--smart-confidential-secret`, as above.
- **Packages.** The FHIR packages that ship with ReFHIR (the core definitions, terminology and
  what they depend on) are stored once for all tenants. A new tenant is given them in moments
  instead of reading and storing its own copy, and its start-up does not grow with the number of
  tenants. What a tenant writes (its own definitions, code systems and search parameters) and
  every package installed from a registry or a directory stay its own.
- **Files.** A tenant's package cache and export directory default to a folder named for it.
- **Routing.** With one process per tenant, route requests to a tenant's processes by host name or
  path in front of ReFHIR, with a load balancer or ingress. To have no process per tenant at all,
  serve every tenant from one process: [Many tenants in one process](#many-tenants-in-one-process).
- **Managing tenants.** `refhir tenant list` shows the tenants, whether each is deleted, and whether
  a primary of each is running; `refhir tenant show NAME` shows one. `refhir tenant delete NAME
  --yes` deletes a tenant: it is not served from then on and its rows are removed after a grace
  period (7 days unless `--grace` says otherwise), and `refhir tenant restore NAME` undoes it until
  then. `--grace 0` removes it and everything it holds at once; that refuses while a primary of
  the tenant runs and cannot be undone, so take a backup first. `refhir tenant purge` removes the
  tenants whose grace period is over. The same can be done over HTTP: see
  [the admin API](#the-admin-api).
- **Connections.** Every process opens its pool. A primary also keeps a connection that holds the
  writer lease and, for a tenant, another that announces its commits; a replica keeps one that
  listens for those announcements. `max_connections` must cover all of them for the processes of
  every tenant. (A process that serves every tenant needs far fewer: see
  [Connections](#connections-and-limits-per-tenant).) A proxy that pools connections per transaction (PgBouncer in transaction mode) cannot sit
  between ReFHIR and the database: the lease and the announcements belong to a session.
- **Sizing.** Each tenant adds a writer, so write capacity grows with the number of tenants until
  the Postgres server is the limit: its CPU, its disk, and the one lock a cluster takes to announce
  commits to listeners, which is shared by every database in it. Size the server for all its
  tenants together, and give some tenants a cluster of their own when it is full.
- **Upgrades.** The schema is shared, so it is migrated once for every tenant: run the same release
  in every tenant's processes (see [Schema migrations](migrations.md)).

## Many tenants in one process

`refhir --all-tenants` serves **every tenant of the database from one process** (and from each of
its replicas), so a deployment needs no process, container or Kubernetes object per tenant, and a
tenant is created with one HTTP call. Each tenant is still a separate FHIR server: it has its own
resources, SMART clients and signing key, health card key, audit trail, `/sync` sessions,
Subscriptions and export files, and its own writer, and nothing one tenant does reaches another.

```bash
export REFHIR_ADMIN_TOKEN=$(openssl rand -hex 32)
refhir --postgres $URL --all-tenants --public-url https://fhir.example.com              # primary
refhir --postgres $URL --all-tenants --standby    --public-url https://fhir.example.com # takes over
refhir --postgres $URL --all-tenants --role replica --primary-url http://primary:8080 \
       --public-url https://fhir.example.com                                           # reads
```

An empty database is given the shared layout by the first process that starts on it, so there is
nothing to create first. A database made without tenants is refused. Every process runs the same
release and the same settings.

- **Choosing the tenant.** By path, `/t/{tenant}/…` (the base is `--tenant-path-prefix`, or
  `[tenants] path_prefix`), and/or by host name, `{tenant}.{domain}` (`--tenant-domain`, or
  `[tenants] domain`). The tenant's server sees the path without the prefix. A request that names
  no tenant, or one the process does not serve, is a `404`; one for a tenant that is still opening
  is a `503` with `Retry-After`, and for a deleted tenant a `410`. What a replica forwards to its
  primary carries the tenant it is for. Naming a tenant grants nothing: each tenant checks its own
  SMART tokens, and a token issued for one tenant names that tenant as its audience.
- **Each tenant's address.** `[tenants] public_url` (or `--public-url`) is the address tenants are
  reached at from outside, with `{tenant}` where the name goes (`https://{tenant}.fhir.example.com`,
  `https://fhir.example.com/t/{tenant}`), or the base that the path prefix is added to
  (`https://fhir.example.com` gives `https://fhir.example.com/t/acme`). From it each tenant's
  server makes its `Location` headers, Bundle links, `/export` links, SMART discovery and token
  issuer and audience, and the issuer its SMART Health Cards name
  (`…/health-cards/.well-known/jwks.json`, served per tenant). Tenants told apart by host name need
  `{tenant}` in it, since every tenant would otherwise get the same address. On a loopback bind it
  defaults to the address requests arrive at; on any other bind it is required.
- **What a tenant is created with.** Packages to install (as `--package` takes them: `name@version`,
  a directory or a `.tgz`), `update_as_create`, SMART on or off, health cards on or off, and its
  limits. They are kept in the registry, so every process serves the tenant the same way and a
  restart serves it as it was. A tenant's SMART registration token and `confidential` client secret
  are made from the process's `--smart-dcr-token` and `--smart-confidential-secret` and the tenant's
  name, so every process that shares those two settings agrees on them, and no tenant's opens
  another's. SMART needs a login configured for the process (`[smart.oidc]`).
- **The registry.** The tenants and their settings are a table in the database. A process reads it
  at start, and again whenever any process changes it (the change is announced to all of them) and
  every few seconds in case an announcement was missed. A tenant created, deleted or restored by any
  process, or by the `refhir tenant` command, is served or dropped by every process within moments. A tenant that cannot be opened (a package that cannot be installed, say)
  is reported by its own state in `GET /healthz` and `GET /admin/tenants`, and does not stop the
  others.
- **Probes.** `GET /livez` and `GET /readyz` are the process's: `/readyz` is 200 once the registry
  is loaded, whatever state a tenant is in, since one tenant's failure must not take the others out
  of rotation. `GET /healthz` lists each tenant's state, and `/t/{tenant}/healthz` is that tenant's
  own, as a process that serves one gives.
- **Roles.** Each tenant has a primary, standbys and replicas as above, and they are the processes
  of the deployment: a process that is the primary holds the writer lease of every tenant it opened,
  and a standby takes the leases over as the primary's go. A tenant whose lease another process
  holds is `unavailable` here and tried again every second. A tenant whose lease is taken from a
  running process is dropped by that process alone, and opened again as a standby does.

### Connections and limits per tenant

A process that serves one tenant keeps a pool of its own, a connection that holds its writer lease,
another that announces its commits, and (on a replica) one that listens for them. A process that
serves many does not repeat that for each tenant, because `max_connections` would be spent long
before the server's CPU was. It keeps:

| What | How many | Shared by |
|------|----------|-----------|
| The pool | `--db-pool` (default 16) | every tenant. A connection says which tenant it serves, for row-level security, when it is checked out for a tenant other than the one it served last |
| The writer leases | 1 session | every tenant the process is the primary of. An advisory lock lives as long as its session, and one session can hold any number |
| Announcements | 1 connection | every tenant's commits. `NOTIFY` names its channel and needs no tenant |
| Listening (replicas) | 1 connection | every tenant's channel |

So a process needs the pool's size plus three connections however many tenants it serves, and
`max_connections` has to cover that for each process of the deployment and nothing more. Row-level
security binds every one of these connections to the tenant it is serving at that moment, so a pool
that is shared is not a way across tenants.

**Memory.** Each tenant a process serves keeps its own copy of the catalog (the definitions,
search parameters and terminology index it validates and searches with) in memory, so the process
grows with the number of tenants it serves: plan on roughly 40 to 60 MB for each, with the R4 core
packages, and give the process memory for all of them. A process opens a few tenants at a time
when it starts, so a restart does not load every catalog at once, but it does end up holding them
all.

A tenant may hold at most its **share** of the pool at once, so one busy tenant cannot keep the
others waiting for a connection: `connections_per_tenant` (default half the pool, and at most all
of it), or the tenant's own `limits.connections`. A request that finds its tenant's share in use
waits for one of its own to come back, and the pool's other connections stay free. Size the pool for
the tenants that are busy at the same time, not for all of them; the shares may add up to more than
the pool, since not every tenant is busy together. `GET /healthz?verbose=1` and `/admin/metrics`
show each tenant's connections in use and how often it waited.

A tenant can also be limited in what it asks of the process, in `[tenants.limits]` for all of them
and in its own `limits` when it is created:

| Limit | Effect |
|-------|--------|
| `requests_per_second`, `burst` | Past it, a request is refused with `429` and `Retry-After`. Each process counts its own, so a deployment of several allows up to that many times the rate. A tenant's own [rate limits and quotas](operations.md) (`[rate_limit]`, `[quota]`) apply as well, on top |
| `storage_bytes` | The rows of the tenant's document log and search index, measured every `storage_check_interval` (5 minutes). Past it, requests that could store more are refused with `507` until it is under again; reads and deletes are not |
| `connections` | The tenant's share of the pool, above |

Counters for each tenant (responses by class, throttled and refused requests, requests in flight,
time spent, storage, connections) are at `GET /admin/metrics` in the Prometheus text format, with
a `tenant` label.

### The admin API

The admin API is authenticated apart from FHIR, with a bearer token of its own: `REFHIR_ADMIN_TOKEN`
(or `[tenants] admin_token`). No SMART token stands in for it, and without a token the API refuses
everything (`403`); a wrong or missing one is a `401`. Treat the token as a database password.
`--admin-bind host:port` (or `[tenants] admin_bind`) serves the admin API and the probes on a
listener of their own, so that only the network that manages tenants reaches them; put a proxy that
requires client certificates in front of that listener for mutual TLS, and leave `/admin` unrouted
on the public one.

| Request | Does |
|---------|------|
| `POST /admin/tenants` | Creates a tenant and opens it before it answers: `201` with the tenant. Idempotent: the same body again is `200` with the tenant that is there, and a different body for an existing name is `409`. A tenant that cannot be opened is not kept (`422`), so the request can be sent again once it is fixed |
| `GET /admin/tenants` | Every tenant, with its status and how this process serves it |
| `GET /admin/tenants/{name}` | One tenant: status, settings, its public URL, the packages and versions it holds, its size (documents and index rows, in rows and bytes), and this process's counters for it |
| `DELETE /admin/tenants/{name}` | Soft delete: the tenant is not served from then on, and its rows are removed once the grace period is over. `?grace=24h` (`30s`, `15m`, `24h`, `7d`; default `[tenants] delete_grace`, 7 days) and `?export=false` change that. `grace=0` removes it at once |
| `POST /admin/tenants/{name}/restore` | Brings back a deleted tenant whose rows have not been removed |
| `GET /admin/metrics` | Per-tenant counters, as above |

`POST /admin/tenants` takes a JSON object; only `name` is required, and an unknown field is a `400`:

```json
{
  "name": "acme",
  "packages": ["hl7.fhir.uv.ips@1.1.0", "/etc/refhir/packages/acme-profiles"],
  "update_as_create": false,
  "smart": true,
  "health_cards": true,
  "limits": {"requests_per_second": 200, "burst": 400, "storage_bytes": 53687091200, "connections": 4}
}
```

With `smart` on, the response also carries the tenant's `registration_token` and the
`confidential` client's secret, which are the only time the API hands them out (they can be made
again from the process's two secrets and the name). A tenant name is lowercase letters, digits and
`-`, up to 63 characters, and starts with a letter or digit.

**Deleting.** A delete does these things in order: the tenant is refused new requests (`410`),
what it is answering finishes, every resource it holds is written to
`{export_dir}/{tenant}/deleted-{time}.ndjson` (unless `export=false`; if that fails the tenant is
not deleted), the registry marks it deleted, its sockets, background work and writer lease are
given up, and the hook, if there is one, is told. `[tenants] delete_hook` is a URL that is sent a
`POST` with `{"event": "tenant.deleted", "tenant", "deleted_at", "purge_after", "export"}`: the
place to copy the export somewhere safe, or to tell whatever else holds data for the tenant. Its
answer is reported in the response and is never a reason to undo the deletion. The primary
removes the rows of every deleted tenant whose grace period is over, once a minute; until then the
name stays taken, and `restore` brings the tenant back with everything it held.

On a replica, a request that changes tenants is sent on to the primary (`--primary-url`), as a
write is; a standby answers `503` until it has taken over.

`refhir tenant` does the same against the database, without the process: `create` takes the same
settings as flags, `delete` the grace period, an export directory and `--yes`, and the processes
that serve every tenant follow what it changes. (It does not open the tenant, so a `create` that
names a package the process cannot install shows up as the tenant being `unavailable` instead of
being refused.)

### Upgrading

The schema is shared, so it is migrated once for every tenant by the first process of the new
release that starts, however many tenants the database holds (the migrations that touch tenants'
rows take them in turn, each under its own row-level security setting). Start the processes of a
deployment one release at a time as for [any upgrade](migrations.md). The release that added the
registry's settings column adds it with an empty settings object to every tenant that was already
there, and each is served as it was.
