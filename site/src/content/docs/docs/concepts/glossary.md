---
title: Glossary
description: The terms ReFHIR's documentation uses, in plain words.
---

**Audit trail.** A record of who did what to which resource, and what was refused. It is read back
as FHIR `AuditEvent` resources at `GET /_audit`, and its entries are chained with a hash so that
tampering is detectable. See [Governance](/docs/guides/governance/).

**Batch.** A `Bundle` of type `batch`. Each entry is its own commit, so a subscriber can see a
batch half applied. Compare _transaction_.

**Commit.** A write that has been validated and made durable. A commit is what wakes the live
queries it could have changed.

**Commit timestamp.** A logical counter that rises with every commit, starting at 1. It is not a
wall-clock time. Compare timestamps as numbers.

**Live query.** A subscription to the result of a query, over the `/sync` WebSocket. The server
re-runs the query when a commit could have changed its answer, and pushes the new full result. Also
called a reactive query. See [How live queries work](/docs/concepts/how-it-works/).

**`meta.versionId`.** The version of a resource. ReFHIR sets it to the commit timestamp of the
write that produced that version, so it also orders versions. A mismatched `If-Match` is refused
with a `409`.

**Operation.** The name of what a live query or a `/sync` mutation runs, written
`{ResourceType}/{operation}`, such as `Patient/search` or `Patient/update`. In the protocol it is
the `udfPath` field.

**Package.** A FHIR package, such as `hl7.fhir.r4.core`, that supplies the resource types, search
parameters and profiles a server knows. ReFHIR ships none and installs them on the first start.

**Primary, standby, replica.** The roles of several processes sharing one database. The _primary_
writes and holds the lease. A _standby_ waits for the lease and takes over when the primary is gone.
A _replica_ serves reads, and forwards writes to the primary. See
[Several processes](/docs/operate/clustering/).

**Query-set version.** A counter each `/sync` connection keeps, starting at 0. It is the only
sequencing the protocol has, and it is about the set of live queries, not about the data.

**Read-your-writes.** The guarantee that a read after your own write sees it. ReFHIR's REST reads
that follow a write wait for a replica to catch up, using the `X-Refhir-Commit-Ts` and
`X-Refhir-Min-Ts` headers. The TypeScript client tracks `lastCommitTs` for this.

**Read set.** What a query read while it ran: the index ranges it touched. A write wakes a live
query when it overlaps the query's read set.

**Resource.** A FHIR resource: a `Patient`, an `Observation`, a `MedicationRequest`.

**SMART App Launch.** The FHIR authorization framework. ReFHIR implements version 2.2 as both
authorization server and resource server. See [SMART on FHIR](/docs/guides/smart/).

**Snapshot.** A consistent view of the data as of one commit timestamp. A read at timestamp _t_
sees the newest version of each resource at or before _t_.

**Subscription.** The FHIR R4 resource that asks to be notified. ReFHIR delivers `rest-hook` and
`websocket` Subscriptions. It is not the same as a live query. See
[Live queries and FHIR Subscriptions](/docs/concepts/vs-subscriptions/).

**Transaction.** A `Bundle` of type `transaction`. It commits at one timestamp or not at all, and a
subscriber sees all of its effects on a query result or none of them.

**Transition.** The `/sync` message that carries results to a client. It holds `QueryUpdated`,
`QueryFailed` and `QueryRemoved` items.

**Watch.** The client-side handle for one live query. In the TypeScript and Go clients,
`latest()` returns the newest result and `next()` waits for a new one.
