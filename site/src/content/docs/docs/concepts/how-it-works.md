---
title: How live queries work
description: "What happens between a subscription and a pushed result: the query, the read set, the snapshot and the transition."
---

A live query keeps a client current on the answer to a FHIR query. This page follows one from
subscription to push. [The /sync protocol](/docs/sdks/sync-protocol/) is the exact specification.

## A live query is a question

A live query is an operation name plus arguments: `Patient/search` with `{"active": "true"}`, or
`Patient/read` with an id. The client never hears "Patient p1 changed". It receives the current
answer to the question it asked.

A client subscribes by sending `ModifyQuerySet` on the `/sync` WebSocket:

```json
{
  "type": "ModifyQuerySet",
  "baseVersion": 0,
  "newVersion": 1,
  "modifications": [
    {"type": "Add", "queryId": 1, "udfPath": "Patient/search", "args": {"active": "true"}}
  ]
}
```

The server runs the query at once and answers with the first result (the Bundle is shortened here):

```json
{
  "type": "Transition",
  "startVersion": 0,
  "endVersion": 1,
  "modifications": [
    {"type": "QueryUpdated", "queryId": 1, "value": {"resourceType": "Bundle", "type": "searchset", "total": 0}, "logLines": []}
  ]
}
```

For a search, `value` is a `searchset` Bundle. For a read, it is the resource. Only read-only
operations can be subscribed; writes go on the same socket as a separate `Mutation` message.

## What the server remembers

When the engine runs a query it records what the query read: the index ranges it touched, not
just the ids of the resources it returned. That record is the query's _read set_, and it is kept
for as long as the query is live. It is how the server can tell, later, whether a write could have
changed the answer.

## What happens on a write

1. A write commits. A transaction Bundle is one atomic commit, so everything in it lands at once.
2. The committer publishes the commit and wakes the live queries whose read sets overlap what was
   written. Queries the write cannot have changed are left alone.
3. Each woken query is re-run at the new snapshot.
4. The client receives the new full result.

Because the re-run happens at one snapshot, a transaction appears to a client as one step, never as
a half-applied set of resources. `Bundle/batch` is different: each entry is its own commit, so a
subscriber can see a batch half applied.

## What the client receives

- **The full result, every time.** A push is never a diff, and for one query the latest
  `QueryUpdated` wins.
- **Membership, not just edits.** A resource that stops matching leaves the result. A resource that
  starts matching enters it. The sort order is part of the query, so it stays correct as data
  changes.
- **Commits can be folded.** Every query invalidated since the last push is re-run and reported
  together, and several commits can become one push. Do not expect one message per commit.

## Good to know when you build a client

- A `Transition` and a `QueryUpdated` carry **no timestamp**, so a client cannot tell at which snapshot
  a result was computed.
- There is **no per-query sequence number**. Apply transitions in the order they arrive.
- Items in one `Transition` are re-run separately, so they do not all share a snapshot. If a commit
  lands in between, it triggers another push.
- After a **reconnect**, the shipped clients resubscribe and the first result for each query is the
  current full result.

If you need to know which version of a resource you are looking at, every resource carries
`meta.versionId`, the commit timestamp of the write that produced it, and an `If-Match` that no
longer matches is refused with a `409`. [Live queries for AI agents](/docs/concepts/agents/) builds
on that.

## Next

- [Tutorial: a live patient list](/docs/tutorial/) puts this to work in a few lines of code.
- [The /sync protocol](/docs/sdks/sync-protocol/) lists every message, error and limit.
- [Glossary](/docs/concepts/glossary/) defines the terms used here.
