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
| Rate limits, quotas, metering, request log, audit trail | Yes ([below](#rate-limits-quotas-metering-and-audit)); the last three are on by default, the first two when configured |
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

### CLI reference

| Flag | Purpose |
|------|---------|
| `--config path` | Config file (also `REFHIR_CONFIG`) |
| `--sqlite path` | SQLite file (default `refhir.sqlite`) |
| `--memory` | SQLite in RAM; no file (not with `--sqlite` or `--postgres`) |
| `--postgres url` | Postgres (not with `--sqlite` or `--memory`; or `REFHIR_POSTGRES_URL`) |
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

A code system can be installed in several versions, and each is kept. HL7's v2 tables carry theirs in the URL (`.../v2-0006|2.1`), other packages in `CodeSystem.version`; a code system written through the API with a `version` is stored the same way. A value set that names a version (`system|version`, or `include.version`) gets exactly that one; with none named, the latest installed version is used, and the expansion records the version it read. A version that is not installed is refused, naming the versions that are, rather than answered from another one. `$lookup`, `$subsumes` and `CodeSystem/$validate-code` take a `version` parameter.

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

**Sign-in.** `--smart` needs a login and will not start without one. For real use, configure an OpenID Connect provider (`[smart.oidc]` or `--oidc-issuer`/`--oidc-client-id` plus `REFHIR_OIDC_CLIENT_SECRET`): the authorize step sends users to the provider and the verified `sub` and `fhirUser` claim go into the tokens. See [deploy/production.md](deploy/production.md#smart-sign-in-openid-connect). `--smart-demo-login` skips authentication entirely (any username works) and exists only for local testing and conformance runs such as Inferno.

`POST /oauth/register` requires `--smart-dcr-token`. If either flag is omitted, a value is generated and printed once to stderr when the process has a terminal; secrets are never written to logs. Redirect URIs must be `https` or loopback `http`. Backend clients register with `private_key_jwt`, inline `jwks`, and `system/` scopes. EHR launch context comes from `--smart-patient` / `--smart-encounter`, not from query parameters. `--public-url` is required when `--smart` is combined with a non-loopback `--bind`.

### Rate limits, quotas, metering and audit

Five things a server that other people's software depends on needs. They share one piece of state and one idea: **work out who is asking once, at the edge, and let everything after that read it.**

| | What it does | Default |
|---|---|---|
| **Metering** | Counts requests, errors, throttled requests, bytes and time per client, per UTC day, per class of request. Read it at `GET /_usage`. | on |
| **Request log** | One structured log line per request with a request id (`X-Request-Id`, which the response carries back, and which a caller may supply), the caller, the status and the size. Recent requests are also kept in the database for `GET /_requests`, which stores parameter *names* and never their values. | on, kept 24 h |
| **Audit trail** | Who did what to which resource, and what was refused. Read it as FHIR `AuditEvent`s at `GET /_audit`. | on, kept forever |
| **Rate limit** | A token bucket per caller, charged by what the request costs. A refusal is `429` with `Retry-After`. | **off** until configured |
| **Quota** | Daily and monthly ceilings on a client's requests, writes and bytes. A refusal is `429` with `Retry-After` set to when the quota starts again. | **off** until configured |

Nothing is limited until you size a limit for your own traffic, and a `[rate_limit]` or `[quota]` section that is present is on unless it says `enabled = false`, so numbers without a switch cannot leave you believing you are protected. [refhir.toml.example](refhir.toml.example) documents every setting; [deploy/production.md](deploy/production.md#rate-limits-quotas-metering-and-audit) covers running it behind a proxy and with several servers.

```toml
[rate_limit]
requests_per_second = 50   # sustained
burst = 100                # the most a caller can spend at once

[rate_limit.cost]          # one request is not one request
read = 1
search = 2
write = 2
bundle = 5
operation = 5
export = 20

[rate_limit.clients.batch-loader]   # by the token's client_id
requests_per_second = 200

[quota]
requests_per_day = 100000
writes_per_day = 10000

[audit]
retention_days = 2190
```

**Who is asking.** With `--smart`, a caller is the token's client and user, so one noisy user does not use up their client's share. Without a token a caller is an address, and an IPv6 address counts as its /64. Behind a load balancer or ingress every client has the proxy's address unless `[server] trusted_proxy_hops` says how many proxies to believe: each appends the address it saw to `X-Forwarded-For`, so the client is that many entries from the right and anything left of it is whatever the client claimed. `[server] trusted_proxy_peers` lists the networks the proxies connect from, and the header is then believed only on a connection from one of them, so a client that reaches the server directly cannot choose, or frame, an address. The address is the connection's, which `refhir_server::serve` provides; a router from `app_with` served some other way has none, and its callers with no token share one limit (the server warns).

**What a request costs.** Every request is put in a class, and each class has its own cost (`read`, `search`, `write`, `bundle`, `operation`, `export`, `sync`, `auth`, `discovery`, `admin`). `GET /metadata` and the OAuth endpoints are cheap but not free; a `$export` is one request that is a great deal of work. Response headers `RateLimit-Limit`, `RateLimit-Remaining` and `RateLimit-Reset` let a client pace itself before it is refused.

**Failed sign-ins.** An address that keeps presenting tokens the server does not know, or wrong client secrets at `/oauth/token`, is refused (`429`) before its credential is looked up, because a guessed token costs a database read. A token the server already knows is never refused for this, so one misbehaving client does not lock out the rest of its network.

**`/sync` is governed too.** The upgrade is one HTTP request, but every mutation and live query after it is a message that no HTTP layer sees. Each one is limited, metered and audited as the request it stands for, a caller holds at most `max_sync_connections` sockets, and a connection at most `max_live_queries` live queries, because a live query is run again whenever a write overlaps it: a cost that goes on after the message. The re-runs themselves are metered (class `push`) and never refused.

**Quotas.** Per client (so they need `--smart`, and ReFHIR refuses to start with a quota that could never apply), counted against UTC days and months, from the same counters the meter keeps. A server reads a client's totals from the database when it first sees them and again every `refresh_seconds`, which is how it learns what other servers sharing the database have counted; between reads a quota is enforced on what this server knows, so with several servers a client can overshoot by about one interval's traffic. Discovery, the OAuth endpoints and the admin endpoints use no quota.

**The audit trail** is written where the data is touched, so REST, GraphQL and `/sync` are all covered, and a transaction leaves a row for **every document it wrote**, read from the committed result and not guessed from the bundle. Each row has the caller (client, user, `fhirUser`, address), the interaction, the resource, the patient whose record it touches where that can be told, the outcome (a failure carries its FHIR issue code, never the error text, which can repeat what a record says), and for a search its parameters (`[audit] include_query = false` leaves them out; they can name a patient by identifier). Bodies are never recorded, and terminology lookups (`$expand`, `$lookup`, `$validate-code`, `$translate`, `$subsumes`) are not audited at all: they touch no one's record, and a form that calls them on every keystroke would be most of the trail. Token issuance, introspection, revocation and registration at the OAuth endpoints are recorded under the client whose credential held (a public client's bare id is not one). Requests the server refuses at the door, with no token or too narrow a scope, are recorded with whoever asked, and a request turned away by a limit is counted but not audited, so that a flood cannot be turned into a flood of audit rows. What a stranger puts in a request cannot make a row large either: a path segment is recorded as a resource type or id only if it is one, and the path, parameter names and search text kept in a row are cut to fixed lengths. Refusals with no caller known (no token, a bad one) are kept in full only up to `[audit] unattributed_per_second` (20 by default); past that they are counted, and one `sampled` summary row per interval says how many, so the gap is itself on record. What an identified caller does is never sampled. A flood of requests the server *answers* still leaves a row each, so with `[rate_limit]` off anything reachable from an untrusted network can grow these tables; turn the rate limit on there. `[audit] reads = false` keeps only changes, refusals and administrative access.

```bash
# Everything that touched one patient's chart, newest first; a bundle of AuditEvent.
curl -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:8080/_audit?patient=Patient/123&_count=50'
# What one client did last week that failed.
curl -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:8080/_audit?client=acme&outcome=4&date=ge2026-10-05&date=lt2026-10-12'
curl -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:8080/_usage?client=acme&from=2026-10-01'
curl -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:8080/_requests?status=500'
```

`/_audit` accepts `patient`, `entity` (`Type` or `Type/id`), `action` (`C R U D E`), `outcome` (`0`, or `4`/`8`/`12` for a failure), `client`, `user`, `request`, `date` (with `ge` `gt` `le` `lt`), `_count` and `before` (the cursor in the `next` link). A parameter it does not know is a `400`, not ignored: dropping one would hand someone reading an audit trail a wider answer than the one they asked for. With `--smart` these endpoints need a **`system/` scope that covers `AuditEvent`** (`system/AuditEvent.read`, or `system/*.read`). A user's `user/*.read` does not qualify although it covers every resource type: it is the data a clinician may see, and the audit trail is the record of every clinician's access to it. With SMART off they are open, like the rest of the server.

**Turning a request away costs less than serving it.** The limiter is a mutex and some arithmetic in memory, checked before any handler or database work; a failed sign-in is refused before its token is looked up; a quota is checked from memory and re-read from the database once per client every `refresh_seconds`. Throttled requests are counted in the meter, which is memory, and get a row in the request log only up to `[request_log] refused_per_second` (20), so a flood of refusals is a handful of database writes a second and not one each.

**Several servers.** A rate limit is divided among the servers that share the database, each taking an equal part of the rate and the bucket, never a bucket smaller than the dearest request. Servers learn how many there are from a heartbeat every ten seconds (a row each in `governance_members`; a server that is stopping removes its own, and one that is lost expires in 35 s), which follows a fleet as it scales and is nowhere near a request. `GET /_usage` shows the number each server is dividing by (`servers`). Set `[rate_limit] servers = N` to pin it, or `1` where a balancer pins each caller to one server, since an equal share of a limit assumes the balancer spreads a caller's requests evenly. A database that cannot be reached leaves the last count in force.

**Tamper evidence.** Each audit row is stored with `hash = HMAC-SHA256(key, previous hash || the row)`, so changing a row, removing one from the middle, or putting one in shows. `GET /_audit/verify` walks the chain and lists where it breaks. Set `REFHIR_AUDIT_KEY` (at least 16 bytes, such as `openssl rand -hex 32`, the same on every server, kept **outside the database**): without a key the chain is plain SHA-256, which anyone who can write the table can recompute, so it only catches mistakes, and the server says so at start-up and in the verification. A chain cannot show that its *newest* rows were deleted, only that what is left is whole, so the server logs the newest row's id and hash as an `audit checkpoint` line (target `refhir_audit`), checked once a minute and written when it has changed. Keep those lines where the database's writers cannot reach (an append-only log store), and `GET /_audit/verify?checkpoint=<id>:<hash>` says whether the trail still holds that row (`matches`, `different`, `missing`, or `truncated` when it ends before it). Retention takes rows from the start of the trail only, which can keep a few rows longer than asked and never deletes one sooner, so that clocks that differ by a few milliseconds cannot make retention look like tampering. Rows from before the chain existed are counted as `unchained` and are not a fault, provided the chain then starts from the beginning: the first row after them must follow from the start, and a trail in which nothing could be checked (every hash stripped, or nothing but old rows) is reported as `verified: false`. Changing the key makes older rows verify only against the old one, so choose it once.

**Durability.** Usage, request-log and audit rows are written in batches every two seconds and on `SIGINT`/`SIGTERM`, never one write per request: SQLite and Postgres each serve the clinical data and these tables over a single connection, so a write per request would queue behind (and in front of) patient reads. A backlog is written a thousand rows at a time, so catching up after an outage does not hold the database against clinical work. A database that is not taking writes does not lose rows: they are kept and written when it recovers, up to 100,000 audit rows, after which the newest are dropped, counted (`dropped` in `/_usage`) and logged as an error. A database that does not take them within eight seconds, or a second signal, ends the process without them. A crash, as opposed to a signal, can lose up to two seconds. Audit rows can only be appended; the one delete is the retention you set. With `--memory` they end with the process, and ReFHIR says so at start-up.

**Not covered.** The audit trail proves nothing against someone who has both write access to the database *and* the chain key, or who deletes the newest rows when no checkpoint was kept; the chain narrows the places an attacker can hide, it does not remove them. A live query is audited when it is subscribed, not each time it is re-run and pushed. A `$export` is audited as one event, when it is requested, not as the data it reads.

### Live sync

`ws://127.0.0.1:8080/sync`: **ModifyQuerySet**, **Mutation**, **Transition** with paths like `Patient/search`, `Patient/update`. Subscribe read-only operations only. Examples: [crates/server/src/tests.rs](crates/server/src/tests.rs).

---

## Development

```bash
cargo test --workspace
cargo fmt --all
```

CI: [.github/workflows/ci.yml](.github/workflows/ci.yml).  
Postgres test: set `HAIFHIR_POSTGRES_URL`.

---

## Open source

ReFHIR is developed openly under HAIHQ: implementation, experiments, benchmarks, and architecture in public. Use it as **infrastructure** or as **research** on reactive healthcare data, consistency, and agent state.

## Long-term direction

> What should the healthcare data layer look like when applications and AI agents continuously operate over changing clinical state?

FHIR keeps interoperability. ReFHIR explores making that state **reactive**: apps stay synchronized; agents can work against context that can stay current, surface dependency changes, and eventually support stronger checks before consequential commits.

**FHIR defines healthcare state. ReFHIR keeps software and AI synchronized with it.**
