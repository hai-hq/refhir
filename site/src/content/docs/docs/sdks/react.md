---
title: React hooks
description: "useFHIRQuery gives a React component a FHIR query result that stays current: its options, what it returns, and how it behaves on reconnects and changes."
---

`useFHIRQuery` subscribes a component to a live query and re-renders it every time the result
changes. It is exported from `@haihq/refhir/react`, and `react` 18 or newer is an optional peer
dependency of the package.

Install it with `npm install @haihq/refhir`.

## A live list

```tsx
import { useFHIRQuery } from "@haihq/refhir/react";

function PatientList() {
  const { data, loading, error } = useFHIRQuery({
    baseURL: "http://127.0.0.1:8080",
    resourceType: "Patient",
    params: { active: "true" },
    runtime: "browser",
  });
  if (loading) return null;
  if (error) return <pre>{error.message}</pre>;
  return <pre>{JSON.stringify(data, null, 2)}</pre>;
}
```

For a `resourceType` and `params` query, `data` is the whole `searchset` Bundle, the same value
`watch.latest()` returns in the other clients, and it is `null` until the first result arrives.

## Options

Every option is optional except the query itself, which is either a `resourceType` or a `path`.

| Option | Meaning |
|---|---|
| `resourceType` | A search over this type. Use with `params`. |
| `params` | Search parameters. A flat object whose values are all strings. |
| `path` | Any live-query operation by name, such as `Patient/read`. Use with `args`. |
| `args` | Arguments for `path`. |
| `client` | An existing client from `dial`. The hook uses it and never closes it. |
| `baseURL` | The server, when you do not pass a `client`. Defaults to `http://127.0.0.1:8080`. |
| `token` | A SMART access token, when you do not pass a `client`. |
| `runtime` | `"browser"` or `"node"`. The default follows where the code runs. |
| `enabled` | `false` leaves the hook idle. Defaults to `true`. |

A `resourceType` query and a `path` query are exclusive: pass one or the other.

## What it returns

```ts
{ data: JsonValue | null, error: Error | null, loading: boolean }
```

- `loading` is `true` until the first result arrives, or until an error ends the wait.
- `data` is the newest result. It updates on every push.
- `error` is set when connecting, subscribing or waiting for a result fails for good. The hook then
  stops, closes what it opened, and does not retry. `data` keeps its last value.

## Share one connection

Without a `client`, every hook instance opens its own WebSocket. In an app with several live
components, dial once and pass the client to each:

```tsx
import { dialBrowser } from "@haihq/refhir";

const client = await dialBrowser("http://127.0.0.1:8080", { token });

function Worklist() {
  const { data } = useFHIRQuery({
    client,
    resourceType: "Task",
    params: { status: "requested" },
  });
  return <pre>{JSON.stringify(data, null, 2)}</pre>;
}
```

When the component unmounts, the hook closes the watch it opened, and closes the client only if it
dialed that client itself. A client that finishes connecting after unmount is closed too.

## When things change

- **A different query.** The hook resubscribes when the query, the base URL, the token or the
  runtime changes. `data` goes back to `null` while the new query loads.
- **A reconnect.** The client reconnects on its own with backoff and resubscribes, and the hook does
  nothing special. `data` keeps its last value until the fresh result arrives, and neither
  `loading` nor `error` changes. The component is not told that it missed changes in between.
- **A refused sign-in.** A `401` or `403` when connecting ends the session for good, so `error` is
  set. Get a new token and render the hook with it.
- **`enabled: false`.** Use it to wait for something, such as a token or a selected patient.

## Writes

The hook is read-only. To change data, use the client's `create`, `update`, `delete` and
`transaction` methods, or plain REST. The live result updates when the commit lands, which is
what makes an optimistic update unnecessary for most screens. See
[The TypeScript client](/docs/sdks/typescript/).

## Browsers and tokens

A browser cannot set headers on a WebSocket, so the browser build sends the token as an
`access_token` query parameter on `/sync`. The server accepts either form and redacts the
parameter in its own request logs. Prefer a short-lived token, because a URL can end up in a
proxy log.
