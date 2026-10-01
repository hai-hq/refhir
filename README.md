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
- an ergonomic developer SDK (Go first: `github.com/hai-hq/haifhir/clients/go`; TypeScript later);
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
| Go SDK | Yes (`clients/go`, `github.com/hai-hq/haifhir/clients/go`) |
| TypeScript SDK | **Not yet** |
| SMART / auth | **Not yet** |
| Full spec, `_include`, `$everything`, all search params | **Deferred** |
| FHIR Subscription delivery | **Deferred** (vs live query sync) |
| Commit-time agent revalidation | **Deferred** (architecture direction) |

**Persistence:** SQLite (`refhir.sqlite`); optional Postgres.

**This build:**

- `Observation.subject` must be `Patient/{id}` with Patient present in the same snapshot.
- `meta.versionId` = commit timestamp; `If-Match` / version conflicts → 409 after OCC retries.
- Engine reference: [get-convex/convex-backend](https://github.com/get-convex/convex-backend) (database layer only).

**Crates:** `types`, `persistence`, `sqlite`, `postgres`, `database`, `fhir`, `server`, `refhir` (binary).

### Roadmap vs defer

**Toward initial release:** R4 subset, CRUD, selected search, transactions, reactive search, WebSocket sync, dependency tracking, invalidation, reconnect, version identifiers, TS SDK, basic auth, benchmarks.

**Defer:** full spec, terminology, bulk export, analytics, workflow orchestration, agent framework, inference, autonomous clinical actions, full certification.

**Success measures:** update latency, stale duration, membership correctness, transactional consistency, reconnect behavior, resource and traffic cost, concurrent queries, unnecessary invalidation, less client sync code, detecting when agent context is stale.

---

## Quick start

**Rust 1.85** ([rust-toolchain.toml](rust-toolchain.toml)).

```bash
cargo run -p refhir -- --sqlite refhir.sqlite
```

That command installs the bundled `hl7.fhir.r4.core` 4.0.1 package, so the concrete R4 resource types are available on a fresh database. Pass `--package dir` to install a package directory instead. Repeat the flag for more than one package on the same FHIR version.

Regenerate the bundled package with `python3 crates/fhir/generate-r4.py`. It downloads the published core package and writes StructureDefinitions and SearchParameters into `crates/fhir/fixtures/r4`.

- **http://127.0.0.1:8080**
- `curl -s http://127.0.0.1:8080/metadata`

| Flag | Purpose |
|------|---------|
| `--sqlite path` | SQLite file (default `refhir.sqlite`) |
| `--postgres url` | Postgres (not with `--sqlite`) |
| `--bind host:port` | Default `127.0.0.1:8080` |
| `--package dir` | FHIR package directory. Repeatable. Replaces the bundled R4 core default. |

Optional: `just run` ([justfile](justfile)) if you use [just](https://github.com/casey/just).

### HTTP examples

```bash
curl -s -X POST http://127.0.0.1:8080/Patient \
  -H 'Content-Type: application/fhir+json' \
  -d '{"resourceType":"Patient","id":"p1","active":true,"name":[{"family":"Ann"}]}'

curl -s http://127.0.0.1:8080/Patient/p1
curl -s 'http://127.0.0.1:8080/Patient?name=ann'
curl -s http://127.0.0.1:8080/Patient/p1/_history

curl -s -X POST http://127.0.0.1:8080/Observation \
  -H 'Content-Type: application/fhir+json' \
  -d '{"resourceType":"Observation","id":"o1","subject":{"reference":"Patient/p1"},"code":{"coding":[{"code":"8480-6"}]},"effectiveDateTime":"2020-01-02"}'
```

| Method | Path | Notes |
|--------|------|--------|
| GET | `/metadata` | CapabilityStatement |
| GET | `/sync` | WebSocket |
| POST | `/` | Transaction Bundle |
| * | `/{type}`, `/{type}/{id}`, search, `_history` | FHIR-style routes |

Errors: `OperationOutcome` (4xx/5xx).

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
