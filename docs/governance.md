# Rate limits, quotas, metering and audit

Five things a server that other people's software depends on needs. They share one piece of state and one idea: **work out who is asking once, at the edge, and let everything after that read it.**

| | What it does | Default |
|---|---|---|
| **Metering** | Counts requests, errors, throttled requests, bytes and time per client, per UTC day, per class of request. Read it at `GET /_usage`. | on |
| **Request log** | One structured log line per request with a request id (`X-Request-Id`, which the response carries back, and which a caller may supply), the caller, the status and the size. Recent requests are also kept in the database for `GET /_requests`, which stores parameter *names* and never their values. | on, kept 24 h |
| **Audit trail** | Who did what to which resource, and what was refused. Read it as FHIR `AuditEvent`s at `GET /_audit`. | on, kept forever |
| **Rate limit** | A token bucket per caller, charged by what the request costs. A refusal is `429` with `Retry-After`. | **off** until configured |
| **Quota** | Daily and monthly ceilings on a client's requests, writes and bytes. A refusal is `429` with `Retry-After` set to when the quota starts again. | **off** until configured |

Nothing is limited until you size a limit for your own traffic, and a `[rate_limit]` or `[quota]` section that is present is on unless it says `enabled = false`, so numbers without a switch cannot leave you believing you are protected. [refhir.toml.example](../refhir.toml.example) documents every setting; [deploy/production.md](../deploy/production.md#rate-limits-quotas-metering-and-audit) covers running it behind a proxy and with several servers.

```toml
[rate_limit]
requests_per_second = 50   # sustained
burst = 100                # the most a caller can spend at once

[rate_limit.cost]          # one request is not one request
read = 1
search = 2
write = 2
bundle = 5
operation = 5
export = 20

[rate_limit.clients.batch-loader]   # by the token's client_id
requests_per_second = 200

[quota]
requests_per_day = 100000
writes_per_day = 10000

[audit]
retention_days = 2190
```

**Who is asking.** With `--smart`, a caller is the token's client and user, so one noisy user does not use up their client's share. Without a token a caller is an address, and an IPv6 address counts as its /64. Behind a load balancer or ingress every client has the proxy's address unless `[server] trusted_proxy_hops` says how many proxies to believe: each appends the address it saw to `X-Forwarded-For`, so the client is that many entries from the right and anything left of it is whatever the client claimed. `[server] trusted_proxy_peers` lists the networks the proxies connect from, and the header is then believed only on a connection from one of them, so a client that reaches the server directly cannot choose, or frame, an address.

**What a request costs.** Every request is put in a class, and each class has its own cost (`read`, `search`, `write`, `bundle`, `operation`, `export`, `sync`, `auth`, `discovery`, `admin`). `GET /metadata` and the OAuth endpoints are cheap but not free; a `$export` is one request that is a great deal of work. The health probes (`/healthz`, `/livez`, `/readyz`) are not callers: they are never refused, counted, logged or audited, since a probe the limiter turned away would take a healthy server out of rotation (`/healthz?verbose` takes a token and is governed as discovery). Response headers `RateLimit-Limit`, `RateLimit-Remaining` and `RateLimit-Reset` let a client pace itself before it is refused.

**Failed sign-ins.** An address that keeps presenting tokens the server does not know, or wrong client secrets at `/oauth/token`, is refused (`429`) before its credential is looked up, because a guessed token costs a database read. A token the server already knows is never refused for this, so one misbehaving client does not lock out the rest of its network.

**`/sync` is governed too.** The upgrade is one HTTP request, but every mutation and live query after it is a message that no HTTP layer sees. Each one is limited, metered and audited as the request it stands for, a caller holds at most `max_sync_connections` sockets, and a connection at most `max_live_queries` live queries, because a live query is run again whenever a write overlaps it: a cost that goes on after the message. The re-runs themselves are metered (class `push`) and never refused.

**Quotas.** Per client (so they need `--smart`, and ReFHIR refuses to start with a quota that could never apply), counted against UTC days and months, from the same counters the meter keeps. A server reads a client's totals from the database when it first sees them and again every `refresh_seconds`, which is how it learns what other servers sharing the database have counted; between reads a quota is enforced on what this server knows, so with several servers a client can overshoot by about one interval's traffic. Discovery, the OAuth endpoints and the admin endpoints use no quota.

**The audit trail** is written where the data is touched, so REST, GraphQL and `/sync` are all covered, and a transaction leaves a row for **every document it wrote**, read from the committed result and not guessed from the bundle. The one exception is a write the server makes on its own, with no request behind it: after a `rest-hook` notification fails for good, or gets through again, it sets a Subscription's `status` and `error` ([Subscriptions](fhir-api.md#subscriptions)). Nobody can be named for that write, so it leaves no row; it is a version in the Subscription's `_history`. Each row has the caller (client, user, `fhirUser`, address), the interaction, the resource, the patient whose record it touches where that can be told, the outcome (a failure carries its FHIR issue code, never the error text, which can repeat what a record says), and for a search its parameters (`[audit] include_query = false` leaves them out; they can name a patient by identifier). Bodies are never recorded, and terminology lookups (`$expand`, `$lookup`, `$validate-code`, `$translate`, `$subsumes`) are not audited at all: they touch no one's record, and a form that calls them on every keystroke would be most of the trail. Token issuance, introspection, revocation and registration at the OAuth endpoints are recorded under the client whose credential held (a public client's bare id is not one). Requests the server refuses at the door, with no token or too narrow a scope, are recorded with whoever asked, and a request turned away by a limit is counted but not audited, so that a flood cannot be turned into a flood of audit rows. What a stranger puts in a request cannot make a row large either: a path segment is recorded as a resource type or id only if it is one, and the path, parameter names and search text kept in a row are cut to fixed lengths. Refusals with no caller known (no token, a bad one) are kept in full only up to `[audit] unattributed_per_second` (20 by default); past that they are counted, and one `sampled` summary row per interval says how many, so the gap is itself on record. What an identified caller does is never sampled. A flood of requests the server *answers* still leaves a row each, so with `[rate_limit]` off anything reachable from an untrusted network can grow these tables; turn the rate limit on there. `[audit] reads = false` keeps only changes, refusals and administrative access.

```bash
# Everything that touched one patient's chart, newest first; a bundle of AuditEvent.
curl -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:8080/_audit?patient=Patient/123&_count=50'
# What one client did last week that failed.
curl -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:8080/_audit?client=acme&outcome=4&date=ge2026-10-05&date=lt2026-10-12'
curl -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:8080/_usage?client=acme&from=2026-10-01'
curl -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:8080/_requests?status=500'
```

`/_audit` accepts `patient`, `entity` (`Type` or `Type/id`), `action` (`C R U D E`), `outcome` (`0`, or `4`/`8`/`12` for a failure), `client`, `user`, `request`, `date` (with `ge` `gt` `le` `lt`), `_count` and `before` (the cursor in the `next` link). A parameter it does not know is a `400`, not ignored: dropping one would hand someone reading an audit trail a wider answer than the one they asked for. With `--smart` these endpoints need a **`system/` scope that covers `AuditEvent`** (`system/AuditEvent.read`, or `system/*.read`). A user's `user/*.read` does not qualify although it covers every resource type: it is the data a clinician may see, and the audit trail is the record of every clinician's access to it. With SMART off they are open, like the rest of the server.

**Several servers.** A rate limit is divided among the servers that share the database, each taking an equal part of the rate and the bucket, never a bucket smaller than the dearest request. Servers learn how many there are from a heartbeat every ten seconds, so a limit follows a fleet as it scales. `GET /_usage` shows the number each server is dividing by (`servers`). Set `[rate_limit] servers = N` to pin it, or `1` where a balancer pins each caller to one server, since an equal share of a limit assumes the balancer spreads a caller's requests evenly. A database that cannot be reached leaves the last count in force.

**Tamper evidence.** Each audit row is stored with `hash = HMAC-SHA256(key, previous hash || the row)`, so changing a row, removing one from the middle, or putting one in shows. `GET /_audit/verify` walks the chain and lists where it breaks. Set `REFHIR_AUDIT_KEY` (at least 16 bytes, such as `openssl rand -hex 32`, the same on every server, kept **outside the database**): without a key the chain is plain SHA-256, which anyone who can write the table can recompute, so it only catches mistakes, and the server says so at start-up and in the verification. A chain cannot show that its *newest* rows were deleted, only that what is left is whole, so the server logs the newest row's id and hash as an `audit checkpoint` line (target `refhir_audit`), checked once a minute and written when it has changed. Keep those lines where the database's writers cannot reach (an append-only log store), and `GET /_audit/verify?checkpoint=<id>:<hash>` says whether the trail still holds that row (`matches`, `different`, `missing`, or `truncated` when it ends before it). Retention takes rows from the start of the trail only, which can keep a few rows longer than asked and never deletes one sooner, so that clocks that differ by a few milliseconds cannot make retention look like tampering. Rows from before the chain existed are counted as `unchained` and are not a fault, provided the chain then starts from the beginning: the first row after them must follow from the start, and a trail in which nothing could be checked (every hash stripped, or nothing but old rows) is reported as `verified: false`. Changing the key makes older rows verify only against the old one, so choose it once.

**Durability.** Usage, request-log and audit rows are written in batches every two seconds and when the server stops (`SIGINT` or `SIGTERM`), not one write per request. A crash, as opposed to a signal, can lose up to two seconds of rows. If the database stops taking writes, rows wait in memory and are written when it recovers, up to 100,000 audit rows; after that the newest are dropped, counted (`dropped` in `/_usage`) and logged as an error. Audit rows can only be appended; the one delete is the retention you set. With `--memory` they end with the process, and ReFHIR says so at start-up.

**Not covered.** The audit trail proves nothing against someone who has both write access to the database *and* the chain key, or who deletes the newest rows when no checkpoint was kept; the chain narrows the places an attacker can hide, it does not remove them. A live query is audited when it is subscribed, not each time it is re-run and pushed. A `$export` is audited as one event, when it is requested, not as the data it reads.
