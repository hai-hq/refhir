---
title: Live queries and FHIR Subscriptions
description: How ReFHIR's live queries differ from the FHIR Subscriptions it also delivers, and when to use each.
---

FHIR already has a way to be told that something happened: the `Subscription` resource. ReFHIR
delivers those too. A live query does a different job, and this page says which job is which.

```text
FHIR Subscription  →  another system learns something happened
Live query         →  a client stays synchronized with what is true now
```

## The difference in one example

Subscribe to `Patient?active=true`, then deactivate a patient who matches. A live query re-runs
and sends the new list without that patient. A rest-hook or websocket `Subscription` sends
nothing, because the patient no longer matches the criteria. It sent a notification when the
patient first matched, and it never says that the patient left.

That is the shape of the whole difference. A notification tells a receiver about one resource that
matched. It does not tell it what the list looks like now, so whoever receives it has to work that
out, and any change that makes a resource stop matching, or that deletes it, never arrives.

## Side by side

| | Subscription (`rest-hook`, `websocket`) | Live query (`/sync`) |
|---|---|---|
| You ask for | A notification when a matching resource is created or updated | The current result of a query, kept current |
| You receive | The matching resource, POSTed to an endpoint or sent on a socket | The full result: a `searchset` Bundle for a search |
| A resource stops matching | Nothing is sent | The next result is without it |
| A resource is deleted | Nothing is sent | The next result is without it |
| A transaction | One notification per matching resource | One step: the result before, then the result after |
| Who receives it | Another system, at an endpoint it names | A connected client, over one WebSocket |
| If delivery fails | A rest-hook is retried up to 8 times, from memory | The client reconnects and gets the current result |
| Typical use | System-to-system events and asynchronous workflows | Live UI state, and context for an agent |

Both are available on the same server, on different endpoints. `/sync` is not
`/{type}/{id}/$websocket`; the second is the Subscription socket.

## Which to use

- Use a **live query** when something needs to show or reason about the current state: a screen, a
  worklist, an agent's working context. See [How live queries work](/docs/concepts/how-it-works/).
- Use a **Subscription** when another system should be told that a resource was created or
  updated and can act on that one resource. See [Subscriptions](/docs/guides/subscriptions/).

ReFHIR implements the R4 `Subscription` resource. It does not implement R5 topic-based
subscriptions.
