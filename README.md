# HaiFHIR

**FHIR defines the healthcare data. HaiFHIR keeps applications synchronized with it.**

Open-source reactive FHIR server from [HAIHQ](https://github.com/hai-hq).  
Repository: [github.com/hai-hq/haifhir](https://github.com/hai-hq/haifhir)

---

## Overview

HaiFHIR explores a different programming model for healthcare applications. Instead of treating FHIR mainly as a request-response API over data, it treats FHIR data as **live application state**.

Applications run FHIR queries and stay subscribed to their results. When underlying resources change, HaiFHIR finds which active queries are affected, recomputes those results, and pushes updated state to connected clients.

The goal is to make real-time apps on FHIR simpler while staying compatible with the FHIR ecosystem.

## The problem

Most FHIR servers are request-response.

```http
GET /Observation?patient=123&status=final
```

returns a snapshot at one moment. Staying current usually means polling, Subscriptions, WebSockets, webhooks, caches, invalidation, refetch, reconciliation, reconnect handling, and careful UX so users never see inconsistent combinations.

That works, but **keeping application state in sync** is largely the app developer’s job.

HaiFHIR asks whether the server can offer a higher-level abstraction.

## Product thesis

FHIR queries should behave like **reactive queries**.

You subscribe to the **result of a query**, not only to individual resource-change events.

```ts
const observations = useFHIRQuery({
  resourceType: "Observation",
  patient: patientId,
  status: "final",
  _sort: "-date",
});
```

You get the current matches. The query stays active. Creates, updates, deletes, and membership changes update the result automatically. The app works with a live view of FHIR state instead of wiring HTTP, events, and cache by hand.

### Core principle

HaiFHIR is not simply “FHIR with WebSockets.” FHIR already has notifications and Subscriptions.

The abstraction is different:

> **Queries, not events, are the primary unit of synchronization.**

Events may exist inside the server, but applications should not have to turn every event back into current query state by themselves.

## Product goals

HaiFHIR aims to provide:

- a standards-compatible FHIR server;
- reactive FHIR queries;
- automatic query dependency tracking;
- real-time synchronization of query results;
- consistent results across transactions;
- reconnect and recovery semantics;
- an ergonomic developer SDK;
- standard FHIR REST for existing clients alongside reactive clients.

The intent is less infrastructure and less glue code for live healthcare applications.

## Non-goals

HaiFHIR is not meant to become:

- an electronic medical record or hospital management system;
- a workflow, analytics, or clinical decision-support product;
- an AI agent framework;
- a replacement for the FHIR specification or a proprietary fork of FHIR.

Scope is the **FHIR data and synchronization layer**. Other products build on top.

## Architecture (conceptual)

```text
FHIR resources (Patient, Observation, …)
        ↓
Standard FHIR REST (GET/POST/PUT/DELETE, search, transactions)
        ↓
Mutations → commit → new logical version of state (Sₙ → Sₙ₊₁)
        ↓
Dependency tracking (what each query read)
        ↓
On change: affected queries → recompute → push updates
        ↓
Clients (HTTP and/or WebSocket sync)
```

**Mutation layer.** Writes are committed transitions. A FHIR transaction Bundle is one atomic commit. Reactive queries observe committed state, not half-finished writes.

**Dependency engine.** Each search or read records which index ranges and resources it depended on. When a commit touches overlapping data, only those subscriptions need to rerun.

**Synchronization.** Clients receive an initial result, then transitions when commits invalidate their read set. Ordering and version semantics follow the sync protocol (see [This repository](#this-repository) below).

**FHIR Subscriptions (future).** Standard Subscriptions remain valuable for system-to-system events and async workflows. Reactive queries address **keeping an app’s current view** in sync. Both can coexist:

```text
FHIR Subscription  →  another system learns something happened
Reactive query     →  this app’s view stays current
```

### Reactive query semantics (target behavior)

For `Observation?patient=123&status=final`:

| Change | Expected effect |
|--------|------------------|
| Status `preliminary` → `final` | Resource appears in the result |
| Status `final` → `entered-in-error` | Resource leaves the result |
| Delete | Resource disappears |
| New matching Observation | Resource appears |
| Sort key changes (`_sort=-date`) | Order updates |
| `_count` window | Result reflects the query, not a naive append |

### Transaction consistency

Multi-resource updates (Encounter + Observation + Task in one Bundle) should not expose arbitrary mixes of old and new state. Subscribed queries should advance **S100 → S101** as a unit after commit.

### Developer experience (target)

Today many apps combine FHIR HTTP, WebSockets, subscriptions, filtering, cache, invalidation, refetch, reconciliation, and reconnect logic. HaiFHIR’s direction is closer to:

```text
declare query → receive current result → keep receiving current result
```

---

## This repository

This tree is the **first implementation** of that model: a Rust server with a Convex-shaped engine (document log, index log, snapshot transactions, OCC committer, read sets, write log, invalidation). It is developed openly; not every product goal above is finished yet.

### Implemented today

| Area | Status |
|------|--------|
| FHIR R4 JSON | Yes |
| Resource types | `Patient`, `Observation` only |
| CRUD, `_history`, `vread` | Yes |
| Search (selected params, index-backed) | Yes |
| Transaction `Bundle` | Yes (atomic commit) |
| FHIR REST | Yes (`application/fhir+json`) |
| Reactive sync | Yes (`/sync` WebSocket, query rerun on invalidation) |
| TypeScript SDK (`useFHIRQuery`, …) | **Not in this repo yet** |
| SMART / auth | **Not yet** |
| Full spec, all search params, `_include`, `$everything` | **Deferred** |
| FHIR Subscription delivery | **Deferred** (distinct from live query sync) |

**Persistence:** SQLite default (`haifhir.sqlite`); optional Postgres.

**Notable rules in this build:**

- `Observation.subject` must be `Patient/{id}` with that Patient present in the same snapshot (stricter than allowing dangling references).
- `meta.versionId` is the commit timestamp; optimistic locking via `If-Match` / `meta.versionId`.
- Concurrent writes use OCC with bounded retry, then HTTP 409.

**Workspace crates:** `types`, `persistence`, `sqlite`, `postgres`, `database`, `fhir`, `server`, `haifhir` (binary). Engine shape follows [get-convex/convex-backend](https://github.com/get-convex/convex-backend) at the database layer, without customer-defined schema or UDF hosting.

### Initial scope (project)

**Include (roadmap):** subset of R4, CRUD, selected search, transactions, reactive search, WebSocket sync, dependency tracking, invalidation, reconnect handling, TS SDK, basic auth, benchmarks.

**Defer:** full spec, every search parameter, terminology, bulk export, analytics, workflow, AI, full certification.

**Success criteria** we care about: update latency, stale duration, membership correctness, transactional consistency, reconnect behavior, resource use, traffic, concurrent queries, and how little client sync code apps need.

---

## Quick start

**Rust 1.85** ([rust-toolchain.toml](rust-toolchain.toml)).

```bash
cargo run -p haifhir -- --sqlite haifhir.sqlite
```

- **http://127.0.0.1:8080**
- `curl -s http://127.0.0.1:8080/metadata`

| Flag | Purpose |
|------|---------|
| `--sqlite path` | SQLite file (default `haifhir.sqlite`) |
| `--postgres url` | Postgres (do not use with `--sqlite`) |
| `--bind host:port` | Listen address (default `127.0.0.1:8080`) |

Optional: `just run` ([justfile](justfile)) if you use [just](https://github.com/casey/just).

### HTTP examples

```bash
# Create Patient
curl -s -X POST http://127.0.0.1:8080/Patient \
  -H 'Content-Type: application/fhir+json' \
  -d '{"resourceType":"Patient","id":"p1","active":true,"name":[{"family":"Ann"}]}'

curl -s http://127.0.0.1:8080/Patient/p1
curl -s 'http://127.0.0.1:8080/Patient?name=ann'
curl -s http://127.0.0.1:8080/Patient/p1/_history

# Observation (Patient must exist)
curl -s -X POST http://127.0.0.1:8080/Observation \
  -H 'Content-Type: application/fhir+json' \
  -d '{"resourceType":"Observation","id":"o1","subject":{"reference":"Patient/p1"},"code":{"coding":[{"code":"8480-6"}]},"effectiveDateTime":"2020-01-02"}'
```

| Method | Path | Notes |
|--------|------|--------|
| GET | `/metadata` | CapabilityStatement |
| GET | `/sync` | WebSocket |
| POST | `/` | Transaction Bundle |
| * | `/{type}`, `/{type}/{id}`, search, `_history` | Standard FHIR-style routes |

Errors: `OperationOutcome` with 4xx/5xx as appropriate.

### Live sync

Connect to `ws://127.0.0.1:8080/sync`. Messages follow a Convex-style shape with FHIR operation names (`Patient/search`, `Patient/update`, …): **ModifyQuerySet**, **Mutation**, **Transition**. Read-only operations can be subscribed; writes cannot. Example payloads: [crates/server/src/tests.rs](crates/server/src/tests.rs).

---

## Development

```bash
cargo test --workspace
cargo fmt --all
```

CI: [.github/workflows/ci.yml](.github/workflows/ci.yml) (format, build, test, release binary).  
Postgres conformance: set `HAIFHIR_POSTGRES_URL` to run the postgres crate test against a real database.

---

## Open source

HaiFHIR is developed openly under HAIHQ. Implementation, experiments, and architectural choices are public so others can use it as infrastructure or as a reference for reactive healthcare data systems.

## Long-term direction

> What would FHIR look like if it were designed as a reactive application data layer?

Preserve FHIR interoperability. Explore a better way for modern applications to stay continuously aligned with healthcare state.

**FHIR defines the healthcare data. HaiFHIR keeps applications synchronized with it.**
