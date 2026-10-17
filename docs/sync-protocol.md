# The /sync protocol

`/sync` is a WebSocket on which a client subscribes to the result of a FHIR query. When a committed write changes that result, the server re-runs the query and sends the new result. The unit of subscription is a query (an operation name plus arguments), not an event: the client never receives "Patient p1 changed", it receives the current answer to the question it asked. The same socket also carries writes.

This page is for someone writing a client in a language other than Go or TypeScript. The shipped clients are [Go](../clients/go/README.md) and [TypeScript](../clients/ts/README.md). Where the server does not specify something, this page says so.

`/sync` is not `/{type}/{id}/$websocket`, which is a different endpoint that delivers FHIR Subscription notifications.

## Messages at a glance

All frames are WebSocket text frames holding one JSON object with a `type` field.

| Direction | `type` | Purpose |
|-----------|--------|---------|
| client to server | `ModifyQuerySet` | Add or remove live queries |
| client to server | `Mutation` | Run one operation, normally a write |
| server to client | `Transition` | Carries `QueryUpdated`, `QueryFailed` and `QueryRemoved` items |
| server to client | `MutationResponse` | The answer to one `Mutation` |
| server to client | `FatalError` | The server is about to drop the connection |

These are all the message types. There is no hello or connect message, no authenticate message (authentication happens on the HTTP upgrade), no application-level ping, no acknowledgement other than the `Transition` that answers a `ModifyQuerySet`, and no resume message. The server ignores binary frames, answers a Ping with a Pong, and treats a Close frame as the end of the session. It never sends pings of its own.

The server reads fields with default JSON rules: unknown extra fields are ignored, and every field listed below as required must be present.

## Connecting

The URL is the server origin with the path `/sync`: `ws://host:port/sync`, or `wss://` when the server is behind TLS. The shipped clients accept an `http`, `https`, `ws` or `wss` base URL and add `/sync` themselves.

### Authentication

The server reads a bearer token from the HTTP upgrade request, in this order:

1. The header `Authorization: Bearer <token>`. The prefix is case-sensitive.
2. The query parameter `access_token=<token>`, percent-decoded.

The Go client and the TypeScript client on Node send the header. The TypeScript client in a browser sends the query parameter, because the browser WebSocket API cannot set headers. A token in a URL can end up in proxy logs; the server redacts `access_token` in its own request logs.

The token is checked when the socket opens, and only when the server runs with `--smart`. It is checked again while the socket is open (see [Token lifetime](#token-lifetime)).

| Server mode | What happens |
|-------------|--------------|
| SMART off | No token is needed, and any token sent is ignored, not validated. The connection has no scopes and every operation is allowed, except `ImplementationGuide/$install`, which `/sync` refuses unless the connection is SMART-authenticated (so always, with SMART off). A client's `_readScopes`, `_sdcArchiver`, `_patientCompartment`, `_handling` and `_defaultCount` arguments are stripped. |
| SMART on, no token | The upgrade is refused with HTTP 401 (`a bearer token is required`). |
| SMART on, unknown, expired, or scopeless token | HTTP 401 (`unknown token`, `token expired`, `token has no scopes`). |
| SMART on, valid token | The upgrade succeeds. The token's scopes are copied into the connection. |

A 401 carries a `WWW-Authenticate` header and an `OperationOutcome` body. No WebSocket exists after a refused upgrade, so none of the messages below apply. Rate limits can refuse an upgrade with HTTP 429; see [Limits](#limits-and-rate-limiting). The server does not check the `Origin` header.

With SMART on, scopes are checked per message, not at the upgrade. A token that may open the socket can still be refused every operation.

- Each `udfPath` is mapped to an interaction (`search`, `read`, `create`, `update`, `delete`, `install`) and checked against the token's scopes. `search` needs `search`. The read-type operations (`read`, `vread`, `history` and the `$` operations such as `$everything`) need `read`. `create` and `conditional-create` need `create`. `update`, `patch`, `conditional-update`, `$meta-add` and `$meta-delete` need `update`.
- `Bundle/transaction` and `Bundle/batch` are checked entry by entry, from each entry's resource type and `request.method`.
- A patient-scoped token is limited to its patient's compartment: operations on another patient's data are refused, and a pushed result that falls outside the compartment removes the query (see [Errors](#errors)).
- The server never trusts the arguments it sets itself from a client: `_readScopes`, `_sdcArchiver`, `_patientCompartment` (the compartment of a patient-scoped token), `_handling` (the `Prefer: handling=lenient` of a REST search) and `_defaultCount` (the page size of a REST search). It removes them from every message and, with SMART on, sets `_readScopes` (and `_patientCompartment`, for a patient-scoped token) itself. A live query is not paged unless its own arguments say `_count`.

### Token lifetime

A token is looked at again while its socket is open: before the server acts on each message, when the token reaches its expiry time, and every 30 seconds in between. A socket whose token has expired or been revoked is sent `{"type": "FatalError", "message": "the token expired or was revoked"}` and dropped. A client that only listens is cut off too, and a revoked token cannot go on writing over a socket it opened earlier. A revocation is noticed within the 30 seconds by a socket that is neither sending nor receiving, and at once by the next message it sends.

Opening a new socket with the same token is refused with HTTP 401. The shipped clients take a 401 or 403 on a redial as final: the session ends, and every watch and mutation fails with the refusal (see [Reconnect and recovery](#reconnect-and-recovery)).

## Client-to-server messages

### ModifyQuerySet

Adds and removes live queries in one batch.

| Field | Type | Meaning |
|-------|------|---------|
| `type` | string | `"ModifyQuerySet"` |
| `baseVersion` | unsigned integer | Must equal the connection's current version (see [Versions](#versions-timestamps-and-ordering)). |
| `newVersion` | unsigned integer | The version the connection moves to. The shipped clients always send `baseVersion + 1`. |
| `modifications` | array | Zero or more `Add` or `Remove` items, all fields required. |

An `Add` item has `type: "Add"`, `queryId` (unsigned integer chosen by the client), `udfPath` (string) and `args` (any JSON value; the field is required, and the shipped clients send `{}` when there are no arguments). A `Remove` item has `type: "Remove"` and `queryId`.

```json
{
  "type": "ModifyQuerySet",
  "baseVersion": 0,
  "newVersion": 1,
  "modifications": [
    {"type": "Add", "queryId": 1, "udfPath": "Patient/search", "args": {"active": "true"}},
    {"type": "Add", "queryId": 2, "udfPath": "Patient/read", "args": {"id": "p1"}}
  ]
}
```

```json
{"type": "ModifyQuerySet", "baseVersion": 1, "newVersion": 2, "modifications": [{"type": "Remove", "queryId": 1}]}
```

Rules:

- The server processes the items in order and answers with one `Transition` that holds exactly one item per modification, in the same order. An `Add` yields `QueryUpdated` or `QueryFailed`. A `Remove` yields `QueryRemoved`, even for a `queryId` the server does not hold.
- `queryId` is scoped to the connection and has no meaning to the server beyond matching. The server does not deduplicate identical queries: two `Add` items with the same `udfPath` and `args` and different ids are two live queries.
- An `Add` with a `queryId` that is already live replaces it. The old query is dropped only once the new one has been accepted by the limits, so a refused replacement leaves the old query running. If the replacement is accepted but then fails scope checks or execution, the old query is already gone.
- A failed `Add` registers nothing.
- The server runs the `Add` operation immediately, and its result is the first `QueryUpdated`. If a write lands while it takes its snapshot, it retries up to 8 times.

### Mutation

Runs one operation and returns one `MutationResponse`.

| Field | Type | Meaning |
|-------|------|---------|
| `type` | string | `"Mutation"` |
| `requestId` | unsigned integer | Chosen by the client. Echoed in the response. The server does not require it to be unique. |
| `udfPath` | string | An operation name, for example `Patient/update`. |
| `args` | any JSON value | Required. The operation's arguments. |

```json
{"type": "Mutation", "requestId": 4, "udfPath": "Patient/update", "args": {"id": "p1", "resource": {"resourceType": "Patient", "id": "p1", "active": true, "name": [{"family": "Bee"}]}}}
```

The server handles one incoming message at a time per connection and finishes it, including the commit, before it reads the next. Answers therefore come back in the order the requests were sent, with pushes possibly between them, and a slow mutation delays later messages and pushes on the same socket.

### Operation names

`udfPath` is `{ResourceType}/{operation}`. Arguments are read from the JSON object `args`.

Names you may subscribe to (read-only):

| Operation | Arguments |
|-----------|-----------|
| `{Type}/read` | `id`. The value is the resource. |
| `{Type}/search` | A flat object whose values are strings, such as `{"active": "true"}` or `{"_id": "o1", "_include": "Observation:subject"}`. A parameter that a URL would give more than once, which FHIR reads as AND, is an array of strings: `{"schedule.actor": ["HealthcareService/h1", "Location/l1"]}`. A comma inside one string is OR. The array must not be empty. The value is a `searchset` Bundle, and it is not paged unless the arguments say `_count`. |
| `{Type}/vread` | `id`, `versionId` |
| `{Type}/history`, `system/history` | `id` (optional), `_since`, `_count`, `_offset` as strings |
| `{Type}/$validate`, `$meta`, `$everything`, `$document` | `id`, or `resource` for `$validate` |
| `{Type}/$expand`, `$expandability`, `$validate-code`, `$lookup`, `$translate`, `$subsumes` | Terminology operations. They read no documents, so their read set is empty and no write invalidates them. |
| `{Type}/$populate`, `$assemble`, `$next-question`, `$behavior`, `$extract` | SDC operations. SMART treats them as reads. |

Names you may send as a `Mutation`:

| Operation | Arguments |
|-----------|-----------|
| `{Type}/create` | `resource`. The server assigns the id and ignores one in the body. |
| `{Type}/update` | `id`, `resource`, optional `ifMatch` |
| `{Type}/delete` | `id` |
| `{Type}/patch` | `id`, `patch`, optional `ifMatch` |
| `{Type}/conditional-create`, `conditional-update`, `conditional-delete` | Respectively: `resource` with `ifNoneExist`; `resource` with `query`; `query` |
| `{Type}/$meta-add`, `$meta-delete` | `id`, `resource` |
| `Bundle/transaction`, `Bundle/batch` | `resource` (the Bundle), or the Bundle itself as `args` |
| `ImplementationGuide/$install` | Needs authentication, and runs on the primary. |

A `Mutation` is not limited to writes: any name above is accepted and returns its result with a `ts`. An unknown operation fails with a message such as `unsupported operation Patient/frobnicate`. With SMART on, a name the scope table does not know is refused as `unsupported operation {op}`, and a name with no `/` as `bad operation {path}`.

Only read-only names can be subscribed. A write name, or a Bundle, is refused before it runs, with SMART on or off (`{op} writes and cannot be subscribed`, `bundle cannot be subscribed`), and nothing is written. An operation that is read-only by name but writes anyway is answered `QueryFailed` (`{path} writes and cannot be subscribed`) after it ran, and its write is not undone.

## Server-to-client messages

### Transition

```json
{
  "type": "Transition",
  "startVersion": 0,
  "endVersion": 1,
  "modifications": [
    {"type": "QueryUpdated", "queryId": 2, "value": {"resourceType": "Patient", "id": "p1", "active": true, "meta": {"versionId": "7"}}, "logLines": []}
  ]
}
```

That is the answer to a `ModifyQuerySet` that added one `Patient/read` query as query 2. `startVersion`, `endVersion` and `modifications` are always present. A `Transition` answers a `ModifyQuerySet`, or is pushed because a write changed a live query. Each item in `modifications` is one of:

| `type` | Fields | Meaning |
|--------|--------|---------|
| `QueryUpdated` | `queryId`, `value`, `logLines` | The query's current result. `value` is whatever the operation returned: a Bundle for a search, the resource for a read. It is always the full result, never a diff. `logLines` is always `[]`. |
| `QueryFailed` | `queryId`, `error`, `logLines` | The query failed. `error` is plain text, not an `OperationOutcome`, and its wording is not a contract. `logLines` is always `[]`. |
| `QueryRemoved` | `queryId` | Acknowledges a `Remove`. |

A pushed `Transition` can hold several items: every query invalidated since the last push is re-run and reported together, and several commits can be folded into one push. Do not assume one `Transition` per commit.

### MutationResponse

| Field | Type | Meaning |
|-------|------|---------|
| `type` | string | `"MutationResponse"` |
| `requestId` | unsigned integer | From the `Mutation` |
| `success` | boolean | |
| `result` | JSON | On success, the operation's result: the stored resource for create and update, the response Bundle for a transaction, `null` for delete. On failure, an `OperationOutcome`. |
| `ts` | string or null | On success, the commit timestamp as a decimal string. `null` on failure. |
| `retryAfterSeconds` | integer | Only present when the mutation was refused by a rate limit or quota. |

```json
{"type": "MutationResponse", "requestId": 4, "success": true, "result": {"resourceType": "Patient", "id": "p1", "active": true, "name": [{"family": "Bee"}], "meta": {"versionId": "12"}}, "ts": "12"}
```

`ts` is a string because it is a 64-bit integer, which a JavaScript number cannot hold exactly. The shipped clients also tolerate a bare number.

### FatalError

```json
{"type": "FatalError", "message": "baseVersion 0 does not match 1"}
```

Sent when the server cannot continue: a frame it cannot parse, or a `baseVersion` mismatch. The server drops the connection right after sending it.

## Versions, timestamps and ordering

### Query-set version

Each connection has one counter on the server, starting at 0. It is the only sequencing the protocol has.

- A `ModifyQuerySet` is accepted when `baseVersion` equals the counter. The server then sets the counter to `newVersion` and answers with a `Transition` from the old value to `newVersion`. The server does not check that `newVersion` is larger.
- A `baseVersion` below the counter is also accepted, provided it is not below the version the previous `ModifyQuerySet` left the counter at. That can only be a client that sent its change before it had read a push that moved the counter on. The server then ignores `newVersion` and answers with a `Transition` from the counter to the counter plus one, which carries on from the push the client is about to read.
- Any other `baseVersion` (ahead of the counter, or behind the version the previous change left it at) gets a `FatalError`, the connection is dropped, and none of the modifications run.
- A push adds 1 to the counter and sends a `Transition` from the old value to the new one. A push with nothing to report sends nothing and leaves the counter alone.

A client keeps the `endVersion` of the last `Transition` it applied. Each new `Transition` must have `startVersion` equal to that value and `endVersion` greater than `startVersion`. Both shipped clients treat anything else as a broken stream: they drop the socket and reconnect. They keep one `ModifyQuerySet` in flight at a time, waiting for its answer before sending the next batch, and send `newVersion = baseVersion + 1`.

Because a push can land between a change being sent and its answer, the answer is recognised by what it holds, one item for each modification in the change, and not by its `startVersion`. The server answers a change before it pushes anything for the queries that change added, so the first `Transition` that holds the items a client is waiting for is the answer.

### Commit timestamps

A commit timestamp is a logical counter that rises with each commit, starting at 1. It is not a wall-clock time. Compare timestamps numerically.

- `MutationResponse.ts` is the commit timestamp of a successful mutation. For an operation that wrote nothing it is the timestamp of the snapshot the operation read.
- Every resource the commit wrote carries the same value as `meta.versionId`, a decimal string, in the response and in later reads.
- `Transition` and `QueryUpdated` carry no timestamp. A client cannot tell from the message at which snapshot a result was computed.
- `queryId` and `requestId` are only labels chosen by the client. Keep them small enough for your language's JSON numbers.

### Ordering

- Apply `Transition` messages in the order received, and the items inside one in array order. For one query the latest `QueryUpdated` wins. There is no per-query sequence number.
- Match a `MutationResponse` to its request by `requestId`.
- On one socket, the server writes the `MutationResponse` for a write before it writes the `Transition` that the write caused, because it finishes one incoming message before it looks at invalidations. Across two sockets nothing orders the two messages.
- The server re-runs each query in a push separately, so it does not promise that all items in one `Transition` were computed at the same snapshot. If a commit lands in between, it triggers another push.

### Transactions

`Bundle/transaction` runs as one database transaction. It commits at one timestamp or, if any entry fails, not at all, and the error comes back as a failed `MutationResponse`. Live queries are told about the commit once, with all the keys it wrote, and each affected query is re-run once afterwards. A subscriber therefore sees all of a transaction's effects on a query result, or none of them. The transaction shows up as one `QueryUpdated` per affected query, normally in one `Transition`. The resources it wrote all carry the same `meta.versionId`, equal to the response's `ts`.

`Bundle/batch` is different: each entry is its own commit, so a subscriber can see a batch half applied, and `ts` is that of the last entry that ran without error. On a replica, see [Replicas](#replicas-and-read-your-writes).

## Errors

| Situation | Reported as | Connection |
|-----------|-------------|------------|
| Not JSON, unknown `type`, unknown modification `type`, or a missing or mistyped field | `FatalError`, with the parser's message | Dropped |
| `baseVersion` does not match | `FatalError` | Dropped |
| An `Add` fails: unknown or write operation, scope, bad arguments, not found, rate limit, query cap | `QueryFailed` in the `Transition` that answers the `ModifyQuerySet` | Stays open. The query is not registered. |
| A re-run fails after the query was registered | `QueryFailed` in a pushed `Transition` | Stays open. See below. |
| A mutation fails: scope, validation, conflict, rate limit, no primary | `MutationResponse` with `success: false` and an `OperationOutcome` in `result` | Stays open |
| The upgrade is refused | HTTP 401 or 429 before any WebSocket exists | None |

When the server ends a connection itself, as after a `FatalError`, it does not send a Close frame. It drops the socket, so a client usually sees an abnormal closure with no close code.

`OperationOutcome` issue codes the server produces for mutations: `forbidden` (scope), `throttled`, `transient` (a replica that cannot reach the primary), `not-found`, `deleted`, `conflict`, `invalid`, `not-supported`, `exception`, or the issues of a validation outcome. Scope refusals are plain text in `diagnostics`, such as `scope does not allow Patient/create`.

`QueryFailed.error` strings written by the server itself include `{path} writes and cannot be subscribed`, `scope does not allow {path}`, `invalid value "{value}" for search parameter {name} on {type}: {what was expected}` (a search value that cannot be read as its type, such as a date that is not one: a live query is never lenient, so it fails now rather than staying empty, and `Prefer: handling=lenient` has no say in it), `token is limited to a different patient`, `too many live queries: a connection may hold {cap}`, `query stayed stale` (the server gave up after 8 attempts to take a consistent snapshot), and the rate and quota messages in the next section.

After a pushed `QueryFailed` the server drops the query, as it does when the result left a patient-scoped token's compartment. A failed re-run is typically a watched resource that was deleted. A `QueryFailed` is final for that `queryId`: nothing revives it if a later write would make the query succeed again, and it no longer counts under `max_live_queries`. Both shipped clients forget the query too. To watch again, send an `Add`.

## Limits and rate limiting

The limits in this section apply only when the operator has turned on `[rate_limit]`, and quotas only with `[quota]`. Without them nothing on `/sync` is limited, though metering and audit, which `refhir.toml.example` enables, still record `/sync` messages. [Rate limits, quotas, metering and audit](governance.md) covers configuration. The defaults quoted below are those of an enabled `[rate_limit]`.

- **The upgrade** is a request of class `sync`, charged against the caller's bucket. A refused upgrade is HTTP 429 with a `Retry-After` header, an `OperationOutcome` with code `throttled`, and `RateLimit-*` headers where a bucket decided it.
- **`max_sync_connections`** (default 10, `0` for no limit) is the number of sockets one caller may hold. The caller is the token's client and user, or the address when there is no token. When a caller is at the limit the upgrade is refused with 429 and `Retry-After: 5`. Servers sharing a database divide the limit between them.
- **`max_live_queries`** (default 100, `0` for no limit) is the number of live queries one connection may hold. An `Add` over the limit fails with `QueryFailed` (`too many live queries: a connection may hold 100`). Replacing a `queryId` does not count as another query.
- **Each `Add`** is charged as class `sync`, whatever its operation, so `Add` items in one frame are charged one by one. A refused `Add` becomes a `QueryFailed` with text such as `rate limit exceeded: try again in 3 seconds`, or for a quota `{what} used up ({used} of {limit}); it starts again at {time}`. It carries no `retryAfterSeconds`.
- **Each `Mutation`** is charged by the class of its operation: `write`, `bundle`, `read`, `search` or `operation`. A refused one gets a `MutationResponse` with `success: false`, an `OperationOutcome` with code `throttled`, and `retryAfterSeconds`.
- **Not charged:** `Remove`, pings, and the pushes themselves. Pushes are metered under class `push` and are never refused, which is why `max_live_queries` exists: each live query costs a re-run whenever a write overlaps it.
- **Audit,** when on: a mutation and the first answer of an `Add` are audited, and a refused scope check is recorded as a 403. Re-runs are not audited one by one.
- **Message size:** the server sets no limit of its own, so the WebSocket library's defaults apply. The Go client sets a 16 MiB read limit, so a larger single message closes its connection.

```json
{"type": "MutationResponse", "requestId": 3, "success": false, "result": {"resourceType": "OperationOutcome", "issue": [{"severity": "error", "code": "throttled", "diagnostics": "rate limit exceeded: try again in 3 seconds"}]}, "retryAfterSeconds": 3, "ts": null}
```

The shipped clients do not retry a refused mutation, so `retryAfterSeconds` is for the caller to use. They do honor a `Retry-After` on a refused upgrade when they dial again (for at most a minute). A custom client should honor both.

## Reconnect and recovery

The server remembers nothing about a connection once it closes. The version counter and the list of live queries live only in the connection's handler. When the socket ends for any reason, the server drops every query that connection held. There is no session id, no resume token and no replay buffer. A new connection starts at version 0 with no queries, and writes committed while the client was away are not replayed.

This works without replay because results are never diffs. After resubscribing, the first `QueryUpdated` for each query is the current full result.

Both shipped clients do the following, and a custom client has to do the same:

1. Treat a socket close, an unparseable frame, an unknown message `type`, a `FatalError` or an out-of-sequence `Transition` as a dropped connection.
2. Fail every mutation in flight. Neither client retries them. The outcome of such a mutation is unknown: it may have committed before the drop. `create` assigns a new id each time, so retrying one blindly can create a duplicate.
3. Throw away any unsent or in-flight `ModifyQuerySet`.
4. Dial again with backoff until it works or the client is closed. TypeScript starts at 50 ms, doubles to 1 s, and adds jitter. Go waits 50, 100, 200, 400, 800, then 1600 ms between attempts and repeats the last, with no jitter, and each dial attempt times out after 10 s. A network error is retried for as long as it lasts. A refused upgrade is read by its status: HTTP 401 or 403 is final, because the token will not work if asked again, so the session ends and every watch and mutation fails with the refusal (`UpgradeError` in TypeScript, `*UpgradeError` in Go); any other refusal is retried after the longer of the backoff and its `Retry-After` header, at most a minute. A browser cannot see why a handshake failed, so the TypeScript client there retries every failure.
5. Reset the version to 0, then send one `ModifyQuerySet` with `baseVersion: 0`, `newVersion: 1` and an `Add` for every live query, in ascending `queryId`, reusing the same ids, paths and arguments.
6. Keep each query's last value until the new `QueryUpdated` arrives. The shipped clients do not tell a watcher that a gap happened.

The server sends no pings and sets no idle timeout, and neither shipped client sends pings, so a half-open connection is only noticed when TCP notices. A client that needs faster detection can send WebSocket pings, which the server answers.

## Replicas and read-your-writes

A replica serves `/sync` like a primary, using the process roles described in [Several processes on one database](clustering.md).

- **Queries** run against the replica's own snapshot. They are invalidated when the replica applies a commit that the primary made, so a live query on a replica trails the primary by the replica's refresh lag.
- **Mutations** are tried locally. When the commit fails because the process is read-only and `--primary-url` is set, the replica opens a new WebSocket to the primary's `/sync` for that one mutation, sends it with the caller's bearer token as an `Authorization` header (taken from the header or the query parameter), and relays the primary's `MutationResponse` unchanged. The primary applies the same scopes and limits and audits the message as its own. The replica connects with a 10 s timeout and waits up to 120 s for the answer.
- **Before it relays the answer,** the replica waits up to 500 ms for its own snapshot to reach the commit's `ts`. A live query on the replica then reflects the write by the time the caller hears of it. If the wait times out the response is sent anyway.
- **Failures:** with no `--primary-url`, the mutation fails with code `transient` and `this server is a read-only replica; send writes to the primary`. If the primary cannot be reached, it fails with code `transient` and `could not reach the primary: ...`. Both have `ts: null`.
- **`Bundle/batch`** is sent to the primary when it has an entry that is not a read, like any other write. A batch of reads is answered by the replica. (Each entry of a batch commits on its own, so a replica that ran one itself would refuse its writes entry by entry inside a successful response.)

The read-your-writes headers `X-Refhir-Commit-Ts` and `X-Refhir-Min-Ts` belong to REST. The server skips them for `/sync`: the upgrade response carries no `X-Refhir-Commit-Ts`, and a `X-Refhir-Min-Ts` sent on the upgrade is ignored. On `/sync` the commit timestamp is `MutationResponse.ts`.

The shipped clients use it like this: they remember the highest `ts` from successful mutations, and from `X-Refhir-Commit-Ts` on REST responses, and send it as `X-Refhir-Min-Ts` on later REST calls so a replica waits or forwards instead of answering from an older snapshot. They do not use it for subscriptions. The first result of an `Add` on a replica reflects the replica's snapshot at that moment, which may predate a write the same client made over REST. A later `Transition` follows once the replica applies the commit. The protocol has no way to ask an `Add` to wait for a timestamp, and `QueryUpdated` has no timestamp to check afterwards.

## Worked exchanges

Messages go one per line. `...` marks a part left out. Versions continue across exchanges 1 to 3 on the subscriber's socket.

### 1. Subscribe to a search

The server holds one patient, `p1`, which is active.

```text
client -> server
{"type":"ModifyQuerySet","baseVersion":0,"newVersion":1,"modifications":[{"type":"Add","queryId":1,"udfPath":"Patient/search","args":{"active":"true"}}]}

server -> client
{"type":"Transition","startVersion":0,"endVersion":1,"modifications":[{"type":"QueryUpdated","queryId":1,"value":{"resourceType":"Bundle","type":"searchset","total":1,"entry":[...]},"logLines":[]}]}
```

### 2. A write on another connection, and the push

A second connection updates `p1`. The writer gets its answer, and the subscriber's query is re-run and pushed.

```text
writer -> server
{"type":"Mutation","requestId":4,"udfPath":"Patient/update","args":{"id":"p1","resource":{"resourceType":"Patient","id":"p1","active":true,"name":[{"family":"Bee"}]}}}

server -> writer
{"type":"MutationResponse","requestId":4,"success":true,"result":{...},"ts":"<commit timestamp>"}

server -> subscriber
{"type":"Transition","startVersion":1,"endVersion":2,"modifications":[{"type":"QueryUpdated","queryId":1,"value":{"resourceType":"Bundle","type":"searchset","total":1,"entry":[{"resource":{"resourceType":"Patient","id":"p1","name":[{"family":"Bee"}],...}}]},"logLines":[]}]}
```

The push is a version step of one (1 to 2), as the client expects. A `Patient/create` mutation has the same shape, with `args` of `{"resource": {...}}`. The server assigns the new resource's `id`, which is in `result`. A `Bundle/transaction` with `args` of `{"resource": <Bundle>}` is the same, and its `result` is a `transaction-response` Bundle.

### 3. Remove a query

The subscriber stops caring about query 1. Its `baseVersion` is the `endVersion` of the last `Transition` it applied.

```text
client -> server
{"type":"ModifyQuerySet","baseVersion":2,"newVersion":3,"modifications":[{"type":"Remove","queryId":1}]}

server -> client
{"type":"Transition","startVersion":2,"endVersion":3,"modifications":[{"type":"QueryRemoved","queryId":1}]}
```

Later writes no longer produce pushes for query 1.

### 4. A write the token may not make

With SMART on and a token that holds only `user/Patient.read`. The socket stays open.

```text
client -> server
{"type":"Mutation","requestId":1,"udfPath":"Patient/create","args":{"resource":{"resourceType":"Patient",...}}}

server -> client
{"type":"MutationResponse","requestId":1,"success":false,"result":{"resourceType":"OperationOutcome","issue":[{"severity":"error","code":"forbidden","diagnostics":"scope does not allow Patient/create"}]},"ts":null}
```

For a live query refused by the connection's query cap, the first `Add` (query 1) was accepted at version 1. A second `Add` (query 2, `baseVersion` 1) on a connection limited to one query returns:

```text
server -> client
{"type":"Transition","startVersion":1,"endVersion":2,"modifications":[{"type":"QueryFailed","queryId":2,"error":"too many live queries: a connection may hold 1","logLines":[]}]}
```

## Related

- [SMART on FHIR](smart.md) for tokens and scopes.
- [Rate limits, quotas, metering and audit](governance.md) for the limits that apply to `/sync`.
