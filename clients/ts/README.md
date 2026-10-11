# ReFHIR TypeScript client

Node client for [ReFHIR](https://github.com/hai-hq/haifhir): reactive FHIR over `/sync`, plus HTTP for metadata.

Package: `@hai-hq/refhir`  
**Node 20+** uses `ws` with `Authorization` on upgrade. **Browsers** use native `WebSocket` and pass SMART tokens as `?access_token=` on `/sync` (server accepts header or query).

## Install

```bash
npm install @hai-hq/refhir
```

From this repo:

```bash
cd clients/ts
npm install
npm run build
```

## Quick start

```ts
import { dial } from "@hai-hq/refhir";

const client = await dial("http://127.0.0.1:8080");
await client.create("Patient", {
  resourceType: "Patient",
  id: "p1",
  active: true,
  name: [{ family: "Ann" }],
});

const watch = await client.search("Patient", { active: "true" });
console.log(watch.latest());

const updated = await watch.next();
console.log(updated);

await client.close();
```

SMART (Node):

```ts
const client = await dial("http://127.0.0.1:8080", { token: accessToken });
```

Browser (or explicit runtime):

```ts
import { dial, dialBrowser } from "@hai-hq/refhir";

const client = await dialBrowser("https://refhir.example.com", { token: accessToken });
// equivalent: dial(url, { token, runtime: "browser" })
```

React live search:

```tsx
import { useFHIRQuery } from "@hai-hq/refhir/react";

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

## API (parity with Go)

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

Failed mutations throw `OutcomeError` with `.outcome`. HTTP errors throw `HttpError` with `.status` and `.body`.

Reconnect, batching, and protocol details match the Go client. See [clients/go/README.md](../go/README.md).

## Mock data

Seed JSON from `fixtures/mock-fhir/library` while the server is running. The data names its own ids, so each resource is sent as `PUT [type]/[id]`, which needs a server started with `--update-as-create`:

```bash
cd clients/ts && npm run seed -- -url http://127.0.0.1:8080
```

Or the Go command (`clients/go/cmd/seed`) — same batches and ordering.

## Tests

```bash
cargo build -p refhir
cd clients/ts
npm test
```

Set `REFHIR_BIN` if the binary is not at `../../target/debug/refhir`.
