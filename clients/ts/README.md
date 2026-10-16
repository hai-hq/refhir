# ReFHIR TypeScript client

Node client for [ReFHIR](https://github.com/hai-hq/refhir): reactive FHIR over `/sync`, plus HTTP for metadata.

Package: `@haihq/refhir`  
**Node 20+** uses `ws` with `Authorization` on upgrade. **Browsers** use native `WebSocket` and pass SMART tokens as `?access_token=` on `/sync` (server accepts header or query).

## Install

```bash
npm install @haihq/refhir
```

## Quick start

```ts
import { dial } from "@haihq/refhir";

const client = await dial("http://127.0.0.1:8080");
await client.create("Patient", {
  resourceType: "Patient",
  active: true,
  name: [{ family: "Ann" }],
});

const watch = await client.search("Patient", { active: "true" });
console.log(watch.latest());

const updated = await watch.next();
console.log(updated);

await client.close();
```

The server assigns the id of every resource it creates and ignores one in the body; see [Resource ids](../../docs/fhir-api.md#resource-ids).

SMART (Node):

```ts
const client = await dial("http://127.0.0.1:8080", { token: accessToken });
```

Browser (or explicit runtime):

```ts
import { dial, dialBrowser } from "@haihq/refhir";

const client = await dialBrowser("https://refhir.example.com", { token: accessToken });
// equivalent: dial(url, { token, runtime: "browser" })
```

React live search:

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

## API

| Method | Purpose |
|--------|---------|
| `dial(baseURL, options?)` | Open `/sync` session |
| `client.watch(path, args?)` | Subscribe; blocks until first result |
| `watch.latest()` / `watch.next()` | Read committed JSON |
| `watch.close()` | Remove subscription |
| `client.mutate(path, args)` | Write operation |
| `client.search` / `read` / `query` | Helpers |
| `client.create` / `update` / `delete` / `transaction` | Mutations |
| `client.metadata()` | `GET /metadata` |
| `client.doRequest(method, path, body?)` | One HTTP call |
| `client.requestWithHeaders(method, path, body?)` | Same, returning `{ status, headers, body }` |
| `client.lastCommitTs` | Highest commit timestamp seen (`bigint`, `0n` until known) |

### Read-your-writes

Against a replica, a read right after a write could be served before the replica has applied that write. The client tracks the highest commit timestamp it has seen, from `X-Refhir-Commit-Ts` on REST responses and `ts` on successful `/sync` mutations, and sends it as `X-Refhir-Min-Ts` on every later REST request (`doRequest`, `metadata`, SDC calls). The server holds the read until it has caught up, or forwards it to the primary. The value only moves up and is a `bigint` because timestamps are u64.

```ts
const client = await dial(url);                          // read-your-writes on
const created = await client.create("Patient", patient);  // the server assigns the id
client.lastCommitTs;                                     // e.g. 4172n
await client.doRequest("GET", `/Patient/${created.id}`);  // sends X-Refhir-Min-Ts: 4172

await dial(url, { readYourWrites: false });              // never send the header
```

With `readYourWrites: false`, `lastCommitTs` still tracks. In browsers, `fetch` can only read `X-Refhir-Commit-Ts` if the server lists it in `Access-Control-Expose-Headers`; timestamps from `/sync` mutations always count. Watches are not affected: they follow committed state over `/sync`.

Failed mutations throw `OutcomeError` with `.outcome`. HTTP errors throw `HttpError` with `.status` and `.body`.

After a dropped socket the client redials with backoff (50 ms doubling to 1 s, with jitter) until it is back or `close()` is called. If the server answers a redial with HTTP 401 or 403 (the token expired or was revoked, which the server enforces on open sockets), the session ends instead: every watch and `mutate` rejects with an `UpgradeError` (a subclass of `HttpError`, with `.status` and `.retryAfterMs`), as does `dial` if the first upgrade is refused. Any other refusal is retried after the longer of the backoff and the server's `Retry-After`, up to a minute. Only Node can see why an upgrade failed; in a browser every failure is retried. Batching and protocol details match the Go client (see [clients/go/README.md](../go/README.md)); the reconnect backoff differs. The protocol side of reconnecting is in [Reconnect and recovery](../../docs/sync-protocol.md#reconnect-and-recovery).

## Mock data

Seed JSON from `fixtures/mock-fhir/library` while the server is running. The data names its own ids, so each resource is sent as `PUT [type]/[id]`, which needs a server started with `--update-as-create`:

```bash
cd clients/ts && npm run seed -- -url http://127.0.0.1:8080
```

Or the Go command (`clients/go/cmd/seed`) — same batches and ordering.
