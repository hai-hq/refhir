---
title: Live queries for AI agents
description: How an agent can notice that the state it reasoned from has changed, and refuse a write that rests on an old version.
---

An agent reads a record, reasons about it, calls tools, and may act. That takes seconds or minutes,
and other systems can change the record the whole time. The usual result is an agent that acts on
a snapshot that stopped being true while it was thinking.

ReFHIR is the data layer, not an agent framework: planning, tool use and clinical safety checks belong to
the runtime above it. It gives that runtime two things to build on: a way to **hear that the state it depends on changed**, and a way to
**refuse a write that rests on an old version**.

## The pattern

1. **Keep a live query for each piece of state the plan depends on.** For example, the patient's
   active medications.
2. **Take a fingerprint of what the agent reasoned from.** Which resources, at which versions.
3. **Check the fingerprint before acting.** If the live result no longer matches it, re-plan.
4. **Write with `ifMatch`.** The server refuses a write to a resource that has changed since the
   agent read it.

```js
import { dial } from "@haihq/refhir";

const client = await dial("http://127.0.0.1:8080");
// `patient` and `med` are resources the agent has already read.

// 1. The agent keeps a live query for the state its plan depends on.
const meds = await client.search("MedicationRequest", {
  patient: `Patient/${patient.id}`,
  status: "active",
});

// 2. A fingerprint of a result: which resources, at which versions.
const fingerprint = (bundle) =>
  (bundle.entry ?? []).map((e) => `${e.resource.id}@${e.resource.meta.versionId}`).sort().join(",");

const basis = fingerprint(meds.latest());
console.log(`agent reads ${meds.latest().total} active medication(s) and starts reasoning`);

// ... the agent reasons and calls tools. Meanwhile another system stops the medication. ...

// 3. Before acting, the agent checks that what it reasoned from is still true.
if (fingerprint(meds.latest()) !== basis) {
  console.log(`context changed: now ${meds.latest().total} active medication(s), so the agent re-plans`);
}

// 4. A write that rests on the old record is refused by the server.
try {
  await client.update("MedicationRequest", med.id, { ...med, status: "completed" }, med.meta.versionId);
} catch (error) {
  console.log(`write refused: ${error.outcome.issue[0].diagnostics}`);
}
```

Run against a real server with a patient and one active medication, which another call then
stops, it prints:

```text
agent reads 1 active medication(s) and starts reasoning
context changed: now 0 active medication(s), so the agent re-plans
write refused: If-Match 12 does not match version 13
```

`latest()` is the newest result the client has received. It is synchronous and never throws, so it
is cheap to call right before every action. The refusal is a `409` with an `OperationOutcome`,
raised as an `OutcomeError` whose `outcome` property holds it.

:::note
In this example the stale `ifMatch` is the version from before the medication was stopped.
`meta.versionId` is the commit timestamp of the write that produced the version, so comparing it
is comparing positions in the server's commit order.
:::

## What ReFHIR gives an agent

- **Live working context.** A query result that keeps itself current, instead of a copy from a
  minute ago.
- **Versions you can check.** Every resource carries `meta.versionId`, and an `If-Match` that does
  not match is refused with a `409`.
- **Consistent steps.** A transaction Bundle moves live results from one snapshot to the next as a
  unit, so an agent never sees half of a change it needs both halves of.
- **Scopes that apply to live queries.** SMART scopes are enforced on `/sync` as on REST, so a
  query sees only what its token may read.

## Keep in mind

- **The check and the write are separate steps.** Another commit can land between your fingerprint
  check and your write. `ifMatch` protects the one resource you write. For the other resources your
  decision depended on, check them as late as you can.
- **`latest()` is the newest result received, not the newest committed.** A push is asynchronous, so
  a change can be committed and not yet received.
- **Results carry no timestamps.** Compare fingerprints of `meta.versionId`, as above.
- **After a reconnect** the clients resubscribe and the next result is the current one. Treat every
  result as the whole truth and re-evaluate.

## Next

- [How live queries work](/docs/concepts/how-it-works/) explains the machinery.
- [The TypeScript client](/docs/sdks/typescript/) covers `latest()`, `next()` and errors.
- [Live queries and FHIR Subscriptions](/docs/concepts/vs-subscriptions/) explains how this differs from
  notifications.
