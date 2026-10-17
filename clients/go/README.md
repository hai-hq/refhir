# ReFHIR Go client

Go client for [ReFHIR](https://github.com/hai-hq/refhir): reactive FHIR over the `/sync` WebSocket, with optional HTTP for metadata and one-off REST calls.

Module: `github.com/hai-hq/refhir/clients/go`  
Package name: `refhir`  
TypeScript sibling: [`clients/ts`](../ts) (`@haihq/refhir`)

## Requirements

- Go 1.23+
- A running `refhir` server (see [Getting started](../../docs/getting-started.md))

## Install

From another module:

```bash
go get github.com/hai-hq/refhir/clients/go
```

`go get` fetches the latest tagged version. To use a clone of this repository instead, add a `replace` directive to your `go.mod`.

## Demo data (seed script)

Loads `.json` files from a directory (single resources or Synthea-style Bundles) and commits them in transaction batches over `/sync`.

```bash
REFHIR_PACKAGE_DOWNLOAD=true cargo run -p refhir -- --memory --update-as-create
cd clients/go
go run ./cmd/seed -url http://127.0.0.1:8080
```

The data names its own ids, so each resource is sent as `PUT [type]/[id]`; the server must be started with `--update-as-create`.

Default `-dir` resolves `fixtures/mock-fhir/library` from the repo root or `../../fixtures/mock-fhir/library` when run from `clients/go`.

| Flag | Meaning |
|------|---------|
| `-dir` | Folder of `.json` files (recursive) |
| `-url` | Server origin |
| `-batch` | Resources per transaction (default 40) |
| `-token` | SMART bearer |
| `-timeout` | Overall deadline |

Generating ~100 patients with [Synthea](https://github.com/synthetichealth/synthea), custom org names, and folder layout: [fixtures/mock-fhir/README.md](../../fixtures/mock-fhir/README.md).

## Quick start

```go
package main

import (
	"context"
	"encoding/json"
	"log"

	"github.com/hai-hq/refhir/clients/go"
)

func main() {
	ctx := context.Background()
	client, err := refhir.Dial(ctx, "http://127.0.0.1:8080")
	if err != nil {
		log.Fatal(err)
	}
	defer client.Close()

	patient := json.RawMessage(`{
		"resourceType": "Patient",
		"active": true,
		"name": [{"family": "Ann"}]
	}`)
	_, err = client.Create(ctx, "Patient", patient)
	if err != nil {
		log.Fatal(err)
	}

	watch, err := client.Search(ctx, "Patient", map[string]string{"active": "true"})
	if err != nil {
		log.Fatal(err)
	}
	defer watch.Close()

	log.Printf("current: %s", watch.Latest())

	// Blocks until another client (or this one) commits a change that affects the search.
	updated, err := watch.Next(ctx)
	if err != nil {
		log.Fatal(err)
	}
	log.Printf("updated: %s", updated)
}
```

Run the server without a database file:

```bash
REFHIR_PACKAGE_DOWNLOAD=true cargo run -p refhir -- --memory
```

The server assigns the id of every resource it creates and ignores one in the body (see [Resource ids](../../docs/fhir-api.md#resource-ids)). Later examples use `p1` as a stand-in for an id you already have.

## Model

| Piece | Role |
|--------|------|
| `Client` | One WebSocket session to `/sync` |
| `Watch` | Subscription to a read-only operation; `Latest` and `Next` |
| `Mutate` | One write operation; waits for `MutationResponse` |

Reads and searches go through **watch**. Creates, updates, deletes, and transaction bundles go through **mutate**. Both use the same socket, so a watch on `Patient/search` updates when a `Patient/update` on that session (or another connection) commits.

FHIR REST CRUD is not duplicated on HTTP in this client. Use `Metadata` and `Do` when you need a single HTTP round trip without a subscription.

Resources are **JSON** (`json.RawMessage`). There are no generated R4 structs; types match whatever packages the server installed.

## Connecting

```go
client, err := refhir.Dial(ctx, "http://127.0.0.1:8080")
```

`baseURL` is the HTTP origin. The client dials `ws://host/sync` (or `wss://` for `https`).

Options:

```go
refhir.Dial(ctx, baseURL, refhir.Bearer(accessToken))
```

`Bearer` sets `Authorization` on the WebSocket upgrade and on `Metadata` / `Do`.

```go
refhir.Dial(ctx, baseURL, refhir.WithReadYourWrites(false)) // default: on
```

`WithReadYourWrites` controls [read-your-writes](#read-your-writes) for REST calls.

Always call `Close` when finished. It tears down watches and the background session loop.

## Watches

### Helpers

```go
watch, err := client.Search(ctx, "Observation", map[string]string{
	"patient": "Patient/p1",
	"status":  "final",
})
watch, err := client.Read(ctx, "Patient", "p1")
```

Search parameter values must be **strings**, matching the server. A comma inside a value is OR
(`"status": "free,busy"`).

### Low-level

```go
watch, err := client.Watch(ctx, "Observation/search", map[string]any{
	"patient": "Patient/p1",
	"status":  "final",
})
```

`Watch` blocks until the first committed result arrives, then returns the `Watch`.

A parameter that a URL would give more than once, which FHIR reads as AND, is a `[]string` in
`Watch` and `Query` arguments:

```go
watch, err := client.Watch(ctx, "Slot/search", map[string]any{
	"status":         "free",
	"schedule.actor": []string{"HealthcareService/h1", "Location/l1"},
})
```

```go
value := watch.Latest()           // last committed JSON (search Bundle or single resource)
next, err := watch.Next(ctx)      // wait for a newer committed result
err := watch.Close()              // send Remove on the socket
```

`Next` only returns values you have not already consumed via a prior `Watch` / `Next`. After `Close`, `Next` returns an error.

Two `Search` calls with the same parameters are two subscriptions (two query IDs). The client does not dedupe them.

## Writes

```go
created, err := client.Create(ctx, "Patient", resourceJSON)
updated, err := client.Update(ctx, "Patient", "p1", resourceJSON, ifMatch)
deleted, err := client.Delete(ctx, "Patient", "p1")
bundle, err := client.Transaction(ctx, transactionBundleJSON)
```

`Update` passes `ifMatch` only when non-empty (maps to `meta.versionId` / OCC on the server).

One-shot read without staying subscribed:

```go
body, err := client.Query(ctx, "Patient/read", map[string]any{"id": "p1"})
```

### Arbitrary operations

```go
result, err := client.Mutate(ctx, "Patient/update", map[string]any{
	"id":       "p1",
	"resource": resourceJSON,
})
```

Paths are `{ResourceType}/{operation}` for any type the server catalog includes, for example `Patient/create`, `Observation/search`, `Bundle/transaction`.

### Operation arguments

The server expects these JSON `args`:

| Path | `args` |
|------|--------|
| `{Type}/create` | `{"resource": <json>}` |
| `{Type}/read` | `{"id": "..."}` |
| `{Type}/update` | `{"id", "resource", "ifMatch"?}` |
| `{Type}/delete` | `{"id": "..."}` |
| `{Type}/search` | flat object, **string values only** |
| `Bundle/transaction` | `{"resource": <bundle>}` or the bundle object |

Subscribed paths must be read-only. Subscribing to a write operation yields `QueryFailed` on the socket.

### Errors

Failed mutations return `*refhir.OutcomeError`. `Outcome` is the `OperationOutcome` JSON from the server.

```go
var outcome *refhir.OutcomeError
if errors.As(err, &outcome) {
	// handle outcome.Outcome
}
```

Successful mutations return the operation result JSON (often the saved resource).

## HTTP

```go
meta, err := client.Metadata(ctx) // GET /metadata

body, err := client.Do(ctx, http.MethodGet, "/Patient/p1", nil)
```

Non-2xx responses return `*refhir.HTTPError` with `Status` and `Body`.

### Read-your-writes

Against a replica, a read right after a write could be served before the replica has applied that write. To avoid that, the client tracks the highest commit timestamp it has seen:

- `X-Refhir-Commit-Ts` on every REST response (`Do`, `Metadata`)
- `ts` on every successful `/sync` `MutationResponse` (`Create`, `Update`, `Delete`, `Transaction`, `Mutate`)

and sends it as `X-Refhir-Min-Ts` on every later REST request. The server holds the read until it has caught up to that timestamp, or forwards it to the primary. The timestamp only moves up; a stale response never lowers it.

```go
ts := client.LastCommitTs() // 0 until the server has reported one
```

This is on by default. `WithReadYourWrites(false)` stops the header from being sent; `LastCommitTs` still tracks. Watches are not affected: they follow committed state over `/sync`.

## Reconnect

If the WebSocket drops or the server sends `FatalError`, the client dials again, resets its query-set version to `0`, and re-sends **Add** for every live `Watch`.

- Watches eventually get a fresh `QueryUpdated` via `Next` (or `Latest` after that).
- **In-flight mutations are not retried.** They fail with `refhir: disconnected` so a create is never applied twice.

There is no resume token; after reconnect the first watch result is the current committed state.

**A refused upgrade.** If the server answers the redial with HTTP 401 or 403, the token will not work if asked again, so the session ends instead of dialing forever: every `Watch` and `Mutate` returns a `*refhir.UpgradeError` (`Status`, `RetryAfter`), as does `Dial` if the first upgrade is refused. Any other refusal is retried after the longer of the backoff and the server's `Retry-After` (at most a minute). The server ends a socket whose token expired or was revoked, so a long-lived session sees this when its token runs out; dial a new client with a fresh token.

## Auth (SMART)

When the server runs with `--smart`, complete the SMART App Launch code+PKCE flow (or `POST /oauth/register` with `--smart-dcr-token`, then `client_credentials`), then:

```go
client, err := refhir.Dial(ctx, baseURL, refhir.Bearer(token))
```

Dial without a token is rejected on protected routes.

## Protocol (summary)

Client → server: `ModifyQuerySet` (add/remove queries), `Mutation` (writes).  
Server → client: `Transition` (query results), `MutationResponse`, `FatalError`.

Operation names use the `udfPath` field, for example `Patient/search`, not REST URLs.

Authentication is the HTTP `Authorization` header on the WebSocket upgrade.

## REST vs sync

| Use | API |
|-----|-----|
| Live search or read | `Search`, `Read`, `Watch` |
| Write | `Create`, `Update`, `Delete`, `Transaction`, `Mutate` |
| CapabilityStatement | `Metadata` |
| Ad hoc REST | `Do` |

Standard FHIR clients can still use HTTP against the same server. This library is for staying aligned with **committed** query results over `/sync`.
