# Design

Why ReFHIR exists and how it works. To run it, start with [Getting started](getting-started.md).

## The problem

FHIR servers are usually request-response.

```http
GET /Observation?patient=Patient/123&status=final
```

returns one snapshot. Healthcare state keeps moving: new results, discontinued orders, closed encounters, updated diagnoses, tasks completed elsewhere, authorization or care-team changes while software or an agent is still working.

Staying current typically means polling, Subscriptions, WebSockets, webhooks, caches, invalidation, refetch, reconciliation, and reconnect logic. The app or agent must decide what changed, whether it matters, which queries to rerun, whether caches are stale, whether related writes were one transaction, and how to recover after gaps.

That burden is especially heavy for **AI agents**. An agent may read state at `t₀`, reason and call tools for seconds or minutes, then act at `t₂` on a snapshot that is no longer true. ReFHIR asks whether the **data layer** can offer a better primitive than every runtime rebuilding its own sync stack.

## Product thesis

FHIR queries should behave like **reactive queries**.

Subscribe to the **result of a query**, not only to per-resource change events.

```ts
const observations = useFHIRQuery({
  baseURL: "http://127.0.0.1:8080",
  resourceType: "Observation",
  params: { patient: patientId, status: "final", _sort: "-date" },
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
- an ergonomic developer SDK (Go: `github.com/hai-hq/refhir/clients/go`; TypeScript: `@haihq/refhir` in `clients/ts`);
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

**FHIR Subscriptions.** ReFHIR delivers these too (`rest-hook` and `websocket` channels), for system-to-system events and async workflows. They notify creates and updates only, as R4 does; see [Subscriptions](fhir-api.md#subscriptions).

```text
FHIR Subscription  →  another system learns something happened
Reactive query     →  a client stays synchronized with what is true now
```

### Reactive query semantics (target)

For `Observation?patient=Patient/123&status=final`:

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

## The engine

ReFHIR is a Cargo workspace. Each crate has one job, and the arrows below only point downwards:
a crate never depends on one above it.

| Crate | Role |
|-------|------|
| `refhir` | The binary: configuration, startup, `refhir migrate` |
| `refhir-server` | HTTP (axum), the `/sync` WebSocket, the SMART authorization server, GraphQL, rate limits, metering and audit, background jobs |
| `refhir-fhir` | The resource catalog, package installation, search, operation handlers, Bundles, history, terminology |
| `refhir-sdc`, `refhir-cql`, `refhir-map` | Structured Data Capture 4.0.0 operations, CQL for SDC, and the FHIR mapping language |
| `refhir-sdc-wasm` | A WebAssembly build of the questionnaire behavior engine, for browsers |
| `refhir-validate`, `refhir-fhirpath` | Validation of JSON instances against StructureDefinitions, and a FHIRPath 2.0 evaluator that search indexing, validation and SDC share |
| `refhir-database` | Snapshots, transactions, read sets, the committer, the write log and subscriptions |
| `refhir-sqlite`, `refhir-postgres` | Storage backends for the contract below |
| `refhir-persistence` | The storage contract: a document log and an index log |
| `refhir-types` | Identifiers, timestamps and key intervals |

### How a write becomes a live-query update

1. A handler runs inside a `Transaction` that reads at a `begin_ts` snapshot. Every read records
   the index intervals it touched in a `ReadSet`; writes are buffered. Reads see the snapshot
   plus the transaction's own buffer.
2. At commit, the **committer** picks the next commit timestamp and checks the read set against
   the write log: any commit published since `begin_ts`, and any commit validated but not yet
   published, that wrote a key inside a read interval is a conflict. A conflict is retried by
   re-running the handler, which is safe because handlers only touch the `Transaction`. When the
   retries run out the client gets a `409`.
3. A commit with no conflict is written to storage as documents and index rows in **one SQL
   transaction**. Read-only transactions skip this step. If the process cannot tell whether the
   write reached disk it exits rather than carry on with an unknown state.
4. The committer then publishes: it appends the write log, advances the snapshot readers may use,
   and wakes the subscriptions whose read sets overlap what was written.
5. A woken `/sync` query re-runs its FHIR operation and sends the client the new result. Because
   the query re-runs at the new snapshot, a transaction Bundle appears to the client as one
   step, never as a half-applied set of resources.

Storage is append-only: every version of a document is a row in the document log, a delete is a
tombstone row, and each search parameter value is a row in the index log. A read at timestamp
*t* takes the newest row at or before *t*, which is what makes snapshots cheap and `vread` and
`_history` free.

Work that should follow a commit but is not part of it runs after `publish_commit`: Subscription
delivery, bulk export and remote terminology. Only `$export` is a durable job, a row in the `jobs`
table that survives a restart (see [Operations](operations.md#export-jobs)). Subscription delivery
is a loop in the primary process, and a notification that has failed waits in memory for its
retries, so a restart drops it; what delivery learned about a Subscription (`status` and `error`)
is written to the Subscription, which is stored. See [Subscriptions](fhir-api.md#subscriptions).

## Client SDKs

The Go ([`clients/go`](../clients/go/README.md)) and TypeScript ([`clients/ts`](../clients/ts/README.md))
clients open a `/sync` session, subscribe to a query, and hand you the latest committed result and
each change after it. The TypeScript package also has a React hook, `useFHIRQuery`. They follow
the [wire protocol](sync-protocol.md), so a client in another language is a small amount of work.
