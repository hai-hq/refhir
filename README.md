# ReFHIR

> **FHIR defines healthcare state. ReFHIR keeps software and AI synchronized with it.**

Open-source reactive FHIR server from [HAIHQ](https://github.com/hai-hq).  
Repository: [github.com/hai-hq/haifhir](https://github.com/hai-hq/haifhir)

---

## Overview

ReFHIR explores a different programming model for healthcare applications and AI systems. Instead of treating FHIR mainly as a request-response API over healthcare data, it treats FHIR data as **live application state**.

Applications and AI agents can run FHIR queries and stay subscribed to their results. When underlying resources change, ReFHIR finds which active queries are affected, updates those results, and keeps connected clients aligned with **committed** state.

The goal is to make real-time healthcare applications and healthcare AI systems simpler to build while staying compatible with the FHIR ecosystem.

## The problem

FHIR servers are usually request-response.

```http
GET /Observation?patient=123&status=final
```

returns one snapshot. Healthcare state keeps moving: new results, discontinued orders, closed encounters, updated diagnoses, tasks completed elsewhere, authorization or care-team changes while software or an agent is still working.

Staying current typically means polling, Subscriptions, WebSockets, webhooks, caches, invalidation, refetch, reconciliation, and reconnect logic. The app or agent must decide what changed, whether it matters, which queries to rerun, whether caches are stale, whether related writes were one transaction, and how to recover after gaps.

That burden is especially heavy for **AI agents**. An agent may read state at `t₀`, reason and call tools for seconds or minutes, then act at `t₂` on a snapshot that is no longer true. ReFHIR asks whether the **data layer** can offer a better primitive than every runtime rebuilding its own sync stack.

## Product thesis

FHIR queries should behave like **reactive queries**.

Subscribe to the **result of a query**, not only to per-resource change events.

```ts
const observations = useFHIRQuery({
  resourceType: "Observation",
  patient: patientId,
  status: "final",
  _sort: "-date",
});
```

The client gets current matches. The query stays active. Membership, ordering, creates, updates, and deletes update the result automatically.

### Core principle

ReFHIR is not simply “FHIR with WebSockets.” FHIR already has notifications and Subscriptions.

> **Queries, not events, are the primary unit of synchronization.**

For applications, that means live UI state. For agents, it means **live working context**. Events may exist inside the server, but callers should not have to turn every event back into “what is true now?” by themselves.

## Why this matters for AI and agents

Agents often chain: read patient state, inspect meds and labs, check tasks, reason, call tools, act. Each FHIR call is a snapshot. The usual fix is more glue: refetch, poll, listen, invalidate context, rerun tools, check versions, reconcile.

ReFHIR shifts generic sync into the healthcare data layer so runtimes can ask **what is current now?** instead of only **what did I last fetch?** That does not make an agent clinically safe by itself; it gives a stronger primitive to build on.

**Reactive context.** A live query over e.g. active `MedicationRequest`s can update while the agent works, so the runtime can refresh context, reconsider a plan, or block actions on stale assumptions.

**Events vs state.** Event streams say “something changed.” Agents still rebuild context. Reactive queries say “the state you depend on changed; here is the updated result.”

**Context as a dependency graph.** Instead of one-off `GET Patient`, `GET Observation`, …, a task can depend on a set of live queries (patient, encounter, open tasks). The runtime can tell **context still valid** from **context changed, re-evaluate**.

**Long-running work.** Agents may span many steps while clinicians, devices, or other agents change the same data. ReFHIR assumes **the world can change while software is acting.**

**Commit-time revalidation (direction).** For consequential actions, the same dependency tracking that drives query sync can eventually support “assumptions at read time still hold at commit time” (not required in the first server release).

## Product goals

ReFHIR aims to provide:

- a standards-compatible FHIR server;
- reactive FHIR queries and automatic dependency tracking;
- real-time synchronization of query results;
- consistent results across transactions;
- reconnect and recovery semantics;
- an ergonomic developer SDK (Go: `github.com/hai-hq/haifhir/clients/go`; TypeScript: `@hai-hq/refhir` in `clients/ts`);
- standard FHIR REST for existing clients;
- primitives for AI-agent context synchronization;
- ways to detect when state an agent depends on has changed.

Less infrastructure for live apps and state-aware healthcare AI.

## Non-goals

ReFHIR is not:

- an EMR, hospital management system, or workflow product;
- an **AI agent framework**, autonomous clinical agent, or CDS platform;
- analytics, model inference, or a replacement for the FHIR spec.

Scope is the **FHIR data and synchronization layer**. Agent frameworks, clinical apps, and workflows build on top.

## Architecture (conceptual)

```text
FHIR resources (Patient, Observation, Encounter, …)
        ↓
Standard FHIR REST
        ↓
Mutations → commit → Sₙ → Sₙ₊₁
        ↓
Dependency tracking (reads + index ranges per query)
        ↓
Affected queries → recompute → push updates
        ↓
Web apps, backends, AI runtimes (/sync)
```

| Layer | Role |
|--------|------|
| **FHIR data** | Resources validated and stored per supported R4 profiles and catalog |
| **REST** | `GET/POST/PUT/DELETE`, search, transactions for interoperability |
| **Mutations** | Committed transitions; transaction Bundle = one atomic commit |
| **Dependencies** | What each query read; which subscriptions to rerun on overlap |
| **Reactive engine** | Recompute only queries whose results may have changed |
| **Sync** | Initial result, ordered transitions, versions, reconnect (target: full recovery semantics) |

**FHIR Subscriptions (future).** Still valuable for system-to-system events and async workflows.

```text
FHIR Subscription  →  another system learns something happened
Reactive query     →  a client stays synchronized with what is true now
```

### Reactive query semantics (target)

For `Observation?patient=123&status=final`:

| Change | Effect |
|--------|--------|
| `preliminary` → `final` | Appears in result |
| `final` → `entered-in-error` | Leaves result |
| Delete | Disappears |
| New match | Appears |
| Sort key changes | Order updates |
| `_count` window | Result matches the query, not a naive append stream |

### Transaction consistency

Multi-resource updates in one Bundle must not expose impossible mixes (e.g. new Observation + old MedicationRequest). Subscribed queries should advance **S100 → S101** as a unit. That matters for agents reasoning over combinations that never existed as a committed snapshot.

### Client SDK (target)

```ts
const observations = useFHIRSearch("Observation", {
  patient: patientId,
  status: "final",
});

// Agent-oriented (conceptual, not shipped yet):
const context = await fhir.watch({
  resourceType: "MedicationRequest",
  patient: patientId,
  status: "active",
});
context.onChange(() => agent.invalidate("medication-context"));
```

Sync protocol today: WebSocket `/sync`, Convex-style messages, FHIR operation names (`Patient/search`, …). See [Live sync](#live-sync) and [crates/server/src/tests.rs](crates/server/src/tests.rs).

---

## This repository

First **Rust** implementation: Convex-shaped engine (document log, index log, snapshot transactions, OCC committer, read sets, write log, invalidation). Developed openly; product goals above are not all done yet.

### Implemented today

| Area | Status |
|------|--------|
| FHIR R4 JSON | Yes |
| Resource types | `Patient`, `Observation` |
| CRUD, `_history`, `vread` | Yes |
| Search (selected params, index-backed) | Yes |
| Transaction `Bundle` | Yes (atomic commit) |
| FHIR REST | Yes (`application/fhir+json`) |
| Reactive sync | Yes (`/sync`, invalidation + query rerun) |
| Go SDK | Yes ([clients/go](clients/go/README.md), module `github.com/hai-hq/haifhir/clients/go`) |
| TypeScript SDK | Yes (`clients/ts`, npm `@hai-hq/refhir`) |
| SMART / auth | Yes (App Launch 2.2, Backend Services, PKCE, OIDC) |
| Full spec, `_include`, `$everything`, all search params | **Deferred** |
| FHIR Subscription delivery | **Deferred** (vs live query sync) |
| Commit-time agent revalidation | **Deferred** (architecture direction) |

**Persistence:** SQLite (`refhir.sqlite`), in-memory SQLite (`--memory`), or Postgres.

**Export jobs are durable.** `$export` records each job in a `jobs` table in the same database (SQLite file or Postgres), so `GET /export/{job}` and `GET /export/{job}/file` still answer after a restart. The NDJSON output is a file under `[export].dir`. With `--memory` the table is in memory too, so nothing survives a restart.

**Any number of servers can share the job table.** A job is worked on by the server that holds its *lease*: an owner and an expiry, taken in one atomic statement (on Postgres with `FOR UPDATE SKIP LOCKED`, so servers do not queue behind each other), renewed every third of its 30 seconds while the work goes on, and released when it ends. A server that dies simply stops renewing; each server sweeps for jobs nobody holds, so another one takes the job once the lease runs out, without waiting for a restart. Only the holder can record how a job ended, so a server that lost its lease cannot overwrite the result of the one that took over. A server takes only jobs of kinds it has a handler for, so an older server in a rolling deploy leaves a newer kind of job alone. A job that has been started three times without finishing is marked failed rather than run forever. Exports write to the local `[export].dir`, so run them where that directory is shared if more than one server is serving `/export`.

**This build:**

- `Observation.subject` must be `Patient/{id}` with Patient present in the same snapshot.
- `meta.versionId` = commit timestamp; `If-Match` / version conflicts → 409 after OCC retries.
- Engine reference: [get-convex/convex-backend](https://github.com/get-convex/convex-backend) (database layer only).

**Crates:** `types`, `persistence`, `sqlite`, `postgres`, `database`, `fhir`, `server`, `refhir` (binary).

### Roadmap vs defer

**Toward initial release:** R4 subset, CRUD, selected search, transactions, reactive search, WebSocket sync, dependency tracking, invalidation, reconnect, version identifiers, TS SDK, SMART App Launch 2.2, benchmarks.

**Defer:** full spec, terminology, bulk export, analytics, workflow orchestration, agent framework, inference, autonomous clinical actions, full certification.

**Success measures:** update latency, stale duration, membership correctness, transactional consistency, reconnect behavior, resource and traffic cost, concurrent queries, unnecessary invalidation, less client sync code, detecting when agent context is stale.

---

## Setup

Pick one path. **Docker** is for running a server (your machine, VM, or cluster) without installing Rust. **Local development** is for hacking on ReFHIR or using repo fixtures and tests.

| | Docker | Local development |
|---|--------|-------------------|
| **You need** | Docker (and optionally Docker Compose) | Rust 1.88 ([rust-toolchain.toml](rust-toolchain.toml)), Git |
| **You get** | A `refhir` process in a container | `cargo run -p refhir` from a clone |
| **Config** | `refhir.toml` on the host (recommended) | `refhir.toml` in the repo root and/or CLI flags |
| **Data** | Docker volume (e.g. `/data`) | `refhir.sqlite` in the clone, or `--memory` |

Set `[fhir].version = "r4"` (default) to install that release’s defaults (**hl7.fhir.r4.core@4.0.1**, **hl7.fhir.uv.sdc@4.0.0**) from [packages.fhir.org](https://packages.fhir.org) on first startup (cached on disk; skipped when already in the database). Add other IGs with `[conformance].packages` (`name@version`, a `.tgz`, or a local folder). See [refhir.toml.example](refhir.toml.example).

**Production:** [deploy/refhir.production.toml](deploy/refhir.production.toml) and [deploy/production.md](deploy/production.md) (Postgres, SMART secrets via env, Helm, `cargo install --git …`).

Configuration file search order: `./refhir.toml`, `/etc/refhir/refhir.toml`, or `REFHIR_CONFIG`. CLI flags override the file. In containers, set `REFHIR_CONFIG=/etc/refhir/refhir.toml` and mount the file there.

### Docker

Use this when you only want to **run** ReFHIR, not build it from source.

**1. Get an image**

- **Release image** (after a `v*` tag on GitHub): `ghcr.io/hai-hq/refhir:latest`
- **Build from the repo** (no registry needed):

  ```bash
  git clone https://github.com/hai-hq/haifhir.git
  cd haifhir
  docker build -t refhir:local .
  ```

**2. Create configuration (recommended)**

```bash
cp refhir.toml.example refhir.toml
```

Edit `refhir.toml` for bind address, database path under `/data`, optional `packages` (IGs), SMART, and terminology. The image ships a minimal default at `/etc/refhir/refhir.toml`; mounting your own file replaces it.

**3. Run**

Compose (builds the image from the clone and mounts the example config):

```bash
docker compose up --build
```

Or run the release image with persistent data and your config:

```bash
docker run --rm -p 8080:8080 \
  -v refhir-data:/data \
  -v "$PWD/refhir.toml:/etc/refhir/refhir.toml:ro" \
  -e REFHIR_PACKAGE_CACHE=/data/package-cache \
  ghcr.io/hai-hq/refhir:latest
```

Use `refhir:local` instead of `ghcr.io/hai-hq/refhir:latest` if you built locally.

**Kubernetes:** [deploy/helm/refhir](deploy/helm/refhir/README.md).

### Local development

Use this when you work **in the repository** (server code, `cargo test`, seed scripts, clients).

**1. Clone and toolchain**

```bash
git clone https://github.com/hai-hq/haifhir.git
cd haifhir
# rustup installs 1.88 from rust-toolchain.toml
```

**2. Configuration (optional)**

```bash
cp refhir.toml.example refhir.toml
```

For a quick try you can skip the file and pass flags only (next step).

**3. Run the server**

Persistent SQLite file in the repo:

```bash
cargo run -p refhir -- --sqlite refhir.sqlite
```

Ephemeral (no file, good for tests):

```bash
cargo run -p refhir -- --memory
```

Equivalent with [just](https://github.com/casey/just): `just run` or `just memory`.

**4. Load demo data (optional)**

With the server up on port 8080 and Go 1.23+ installed:

```bash
cd clients/go && go run ./cmd/seed -url http://127.0.0.1:8080
```

See [fixtures/mock-fhir/README.md](fixtures/mock-fhir/README.md) and [clients/go/README.md](clients/go/README.md#demo-data-seed-script).

**Packages at runtime:** `[conformance].packages` or repeat `--package` to add IGs after the `[fhir].version` defaults. After startup, `POST /ImplementationGuide/$install` can install `name@version` from the registry when SMART is enabled (`system/*.write`). Override the package source with `REFHIR_PACKAGE_REGISTRY`: a registry or mirror URL, or `s3://bucket/prefix` for a bundle you host yourself, built with `scripts/package-bundle.py` (see [deploy/production.md](deploy/production.md#package-source)). ReFHIR ships no packages, so a first start needs that source to be reachable.

Regenerate the R4 fixtures used by the test suites (the server itself ships no packages): `python3 crates/fhir/generate-r4.py`.

### Verify the server

Base URL defaults to **http://127.0.0.1:8080** (or the host/port in `refhir.toml` / `--bind`).

```bash
curl -s http://127.0.0.1:8080/metadata
```

### Health probes

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

### CLI reference

| Flag | Purpose |
|------|---------|
| `--config path` | Config file (also `REFHIR_CONFIG`) |
| `--sqlite path` | SQLite file (default `refhir.sqlite`) |
| `--memory` | SQLite in RAM; no file (not with `--sqlite` or `--postgres`) |
| `--postgres url` | Postgres (not with `--sqlite` or `--memory`; or `REFHIR_POSTGRES_URL`). TLS is set in the URL: see [Postgres and TLS](#postgres-and-tls) |
| `--bind host:port` | Default `127.0.0.1:8080` |
| `--package spec` | FHIR IG. Repeatable. Installed after `[fhir].version` defaults. Directory, `.tgz`, or `name@version`. |
| `--smart` | SMART App Launch 2.2 (`/.well-known/smart-configuration`, `/oauth/*`) |
| `--public-url url` | SMART `issuer` URL (required with `--smart` on non-loopback bind) |
| `--smart-patient id` | Patient for `launch/patient` when no EHR context |
| `--smart-encounter id` | Encounter for EHR launch |
| `--smart-fhir-user Resource/id` | `fhirUser` claim (demo login only) |
| `--oidc-issuer url`, `--oidc-client-id id` | Sign SMART users in through an OpenID Connect provider (secret: `REFHIR_OIDC_CLIENT_SECRET`) |
| `--smart-demo-login` | Testing only: accept any username, verify nothing |
| `--update-as-create` | Let `PUT [type]/[id]` create a resource under an id the client names (also `[conformance].update_as_create`). Off by default: the server assigns ids. |
| `--role primary\|standby\|replica` | What this process does next to others on one database (default `primary`; or `REFHIR_ROLE`). See [Several processes](#several-processes-on-one-database) |
| `--standby` | Same as `--role standby` |
| `--primary-url url` | Where a replica sends writes (or `REFHIR_PRIMARY_URL`) |
| `--db-pool n` | Most Postgres connections (default 8), or on SQLite the read connections next to its one writer (default 4); or `REFHIR_DB_POOL` |
| `refhir migrate --status\|--dry-run` | Report the database's schema version and what startup would do to it, without changing anything. See [Schema migrations](docs/migrations.md) |

### Postgres and TLS

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

### Several processes on one database

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

Running it: [deploy/cluster](deploy/cluster/README.md) (Docker Compose with HAProxy), the Helm
chart's `cluster.enabled`, and measurements in [docs/perf](docs/perf/baseline.md).

### HTTP examples

```bash
# The server assigns the id of a resource it creates (see "Resource ids" below).
ID=$(curl -s -X POST http://127.0.0.1:8080/Patient \
  -H 'Content-Type: application/fhir+json' \
  -d '{"resourceType":"Patient","active":true,"name":[{"family":"Ann"}]}' | jq -r .id)

curl -s "http://127.0.0.1:8080/Patient/$ID"
curl -s 'http://127.0.0.1:8080/Patient?name=ann'
curl -s "http://127.0.0.1:8080/Patient/$ID/_history"

curl -s -X POST http://127.0.0.1:8080/Observation \
  -H 'Content-Type: application/fhir+json' \
  -d "{\"resourceType\":\"Observation\",\"subject\":{\"reference\":\"Patient/$ID\"},\"code\":{\"coding\":[{\"code\":\"8480-6\"}]},\"effectiveDateTime\":\"2020-01-02\"}"
```

### Resource ids

The server assigns the id of every resource it creates. `POST [type]` ignores an `id`, and a `meta.versionId` or `meta.lastUpdated`, in the body (FHIR R4 create) and answers `201` with the new id in `Location`. GraphQL and `/sync` `create` do the same.

- **`PUT [type]/[id]`** updates a resource that exists. For an id that does not, it answers `405` with an `OperationOutcome`, because update-as-create is off. `GET /metadata` says so (`updateCreate: false` on every resource), and also reports the conditional interactions that are implemented (`conditionalCreate`, `conditionalUpdate`, `conditionalDelete: single`).
- **Transactions** give entries that create resources their ids up front. Entries that refer to one another name each other by `fullUrl`, as FHIR defines: give the creating entry `"fullUrl": "urn:uuid:..."` and use that string in the references. A `Patient/p1` written in the bundle by hand does not name the resource the server creates.
- **Update-as-create** is a deployment choice in FHIR. Start with `--update-as-create` (or `update_as_create = true` under `[conformance]`) to let `PUT [type]/[id]` create the resource under that id (`201`); `/metadata` then reports `updateCreate: true`. Data that has ids of its own, such as the demo data under `fixtures/mock-fhir` and the Inferno profile, is loaded this way. The environment variable `REFHIR_UPDATE_AS_CREATE=true` turns it on too. With SMART enabled, a `PUT` then needs the `create` permission as well as `update`, since it may create. An SDC `$extract` whose templates name a `resourceId` (or an `id`) returns those entries as `PUT [type]/[id]`, so the bundle it returns needs this setting to be accepted by a server that has not got it, which answers each such entry with `405`.
- Rust code that embeds the server and needs a known id can call `refhir_fhir::import_resource`. It is a function, not an operation: nothing a client sends reaches it.

### Terminology

`ValueSet/$expand` returns the expansion, or `422` with an `OperationOutcome` when the value set cannot be expanded. Each cause is one issue, with the failing part of the value set in `expression` (for example `ValueSet.compose.include[1]`) and the code system, its `content` mode and the filter in `diagnostics`. The usual causes are a code system that is not installed, one that holds only part of its content (`example`, `fragment`, or `not-present`, as the R4 SNOMED CT stub does), or a filter this server does not evaluate. A partial expansion is never returned, because a client cannot tell it from a complete one.

A code system can be installed in several versions, and each is kept. HL7's v2 tables carry theirs in the URL (`.../v2-0006|2.1`), other packages in `CodeSystem.version`; a code system written through the API with a `version` is stored the same way. A value set that names a version (`system|version`, or `include.version`) gets exactly that one; with none named, a value set from a package reads the versions of that system **its own package** holds (installing the package expanded it with those, and a rebuild gives the same answer), and a value set written through the API, or one whose package does not hold the system, reads the version of the FHIR release the server serves (`4.0.1` for R4) when it is installed and the latest installed version otherwise. The expansion records the version it read. A value set that includes another by URL reads that one's definition from its own package too, when the package has one. When several packages define the same value set URL, the definition at the server's release version is the one in use (the extensions package carries `5.3.0-ballot-tc1` copies of R4's value sets over R5's code system URLs, and an R4 server means R4's `4.0.1`); with none at the release, the one installed last, because version strings in different styles (`2018-08-12`, `2.9`, `3.0.0`) do not order against each other. If the one in use cannot be expanded, the next that can answers, so a newer terminology package that needs a code system nothing carries does not take away the older expansion. A start, a rebuild and an incremental change choose the same definition: each is tried first as installing its own package tried it, against what that package holds alone, and only when none can be expanded that way is the rest of the store read, which is how a value set that waits on another package's code system gets an expansion (a start reads what installing each package stored, so it has none for such a value set until the next rebuild). A rebuild stores the expansion under the package whose definition is in use. Stored expansions carry the rules they were made under, and a database written by an older version has them rebuilt once, at its next start. Cross-version and newer packages are why: `hl7.fhir.uv.xver-r5.r4`, a dependency of SDC, has `5.0.0` of `http://hl7.org/fhir/encounter-status`, and `hl7.terminology.r4` carries newer copies of systems the R4 core package also carries; an R4 value set that includes them still means R4's codes (`arrived`, `finished`). A version that is not installed is refused, naming the versions that are, rather than answered from another one. `$lookup`, `$subsumes` and `CodeSystem/$validate-code` take a `version` parameter.

`POST /ValueSet/$expandability` reports, for the value sets named in `url` (a string, an array, or `url` parameters of a Parameters resource), whether each can be expanded and why not; with none named it lists every loaded value set that cannot. Use it as a deployment check for the value sets a UI depends on. `POST /ImplementationGuide/$install` adds a warning to its response for any value set the package made unexpandable, and the server logs how many there are at start-up.

| Method | Path | Notes |
|--------|------|--------|
| GET | `/metadata` | CapabilityStatement |
| GET | `/sync` | WebSocket |
| POST | `/` | Transaction Bundle |
| * | `/{type}`, `/{type}/{id}`, search, `_history` | FHIR-style routes |

Errors: `OperationOutcome` (4xx/5xx).

### SMART on FHIR

**Inferno:** SMART-enabled Docker profile and runbook — [deploy/inferno/README.md](deploy/inferno/README.md). CI runs discovery + OAuth smoke via `.github/workflows/inferno-smoke.yml`.

`--smart` turns the server into a SMART App Launch 2.2 authorization server and resource server:

- Discovery at `/.well-known/smart-configuration` and JWKS at `/.well-known/jwks.json`
- Authorization code + PKCE (`S256`), EHR launch (`/oauth/ehr-launch`), standalone `launch/patient`
- Token endpoint: `authorization_code`, `refresh_token`, and `client_credentials` (asymmetric JWT)
- OpenID Connect `id_token`, token introspection, revocation, and dynamic client registration
- SMART v1 (`patient/*.read`) and v2 (`patient/*.rs`) scopes, plus patient-compartment enforcement

Default clients: `public` (no secret) and `confidential` (`--smart-confidential-secret`), with loopback redirect URIs `/cb` and `/app`. App-launch tokens cannot include `system/` scopes.

**Sign-in.** `--smart` needs a login and will not start without one. For real use, configure an OpenID Connect provider (`[smart.oidc]` or `--oidc-issuer`/`--oidc-client-id` plus `REFHIR_OIDC_CLIENT_SECRET`): the authorize step sends users to the provider and the verified `sub` and `fhirUser` claim go into the tokens. Optional settings under `[smart.oidc]` limit who may sign in (`allowed_claims`, `allowed_email_domains`), bind a person to a `Practitioner` or `Patient` when the provider has no `fhirUser` claim (`fhir_user_lookup`), and fix the launch patient to the signed-in patient (`patient_from_fhir_user`); all are off by default. A gateway in front of ReFHIR can ask `POST /oauth/introspect` (confidential or backend client credentials) for the `client_id`, `sub`, `scope` and `exp` of an access token. See [deploy/production.md](deploy/production.md#smart-sign-in-openid-connect). `--smart-demo-login` skips authentication entirely (any username works) and exists only for local testing and conformance runs such as Inferno.

`POST /oauth/register` requires `--smart-dcr-token`. If either flag is omitted, a value is generated and printed once to stderr when the process has a terminal; secrets are never written to logs. Redirect URIs must be `https` or loopback `http`. Backend clients register with `private_key_jwt`, inline `jwks`, and `system/` scopes. EHR launch context comes from `--smart-patient` / `--smart-encounter`, not from query parameters. `--public-url` is required when `--smart` is combined with a non-loopback `--bind`.

### Live sync

`ws://127.0.0.1:8080/sync`: **ModifyQuerySet**, **Mutation**, **Transition** with paths like `Patient/search`, `Patient/update`. Subscribe read-only operations only. Examples: [crates/server/src/tests.rs](crates/server/src/tests.rs).

---

## Development

```bash
cargo test --workspace
cargo fmt --all
```

CI: [.github/workflows/ci.yml](.github/workflows/ci.yml).  
Postgres tests: set `HAIFHIR_POSTGRES_URL` (a URL or `host=... user=...`) to a role that may
`CREATE DATABASE`. They include `crates/refhir/tests/cluster.rs`, which starts real primary,
standby and replica processes and kills the primary while writes are in flight; the lease tests
make a database of their own each, so they do not disturb each other.  
TLS tests: `out="$(scripts/postgres-tls-test.sh up)" && eval "$out"` starts a throwaway TLS-only Postgres (a
container named `refhir-tls-pg-test`) and exports the two variables they read;
`cargo test -p refhir-postgres tls`; then `scripts/postgres-tls-test.sh down`. Without those
variables the TLS server tests are skipped, unless `REFHIR_REQUIRE_POSTGRES_TLS` is set.

---

## Open source

ReFHIR is developed openly under HAIHQ: implementation, experiments, benchmarks, and architecture in public. Use it as **infrastructure** or as **research** on reactive healthcare data, consistency, and agent state.

## Long-term direction

> What should the healthcare data layer look like when applications and AI agents continuously operate over changing clinical state?

FHIR keeps interoperability. ReFHIR explores making that state **reactive**: apps stay synchronized; agents can work against context that can stay current, surface dependency changes, and eventually support stronger checks before consequential commits.

**FHIR defines healthcare state. ReFHIR keeps software and AI synchronized with it.**
