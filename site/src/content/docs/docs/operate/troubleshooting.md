---
title: Troubleshooting and FAQ
description: What common errors and surprises mean, with the exact messages ReFHIR gives and how to fix each.
---

Messages below are quoted from a running server. Errors from the API are FHIR `OperationOutcome`
resources, and the message is in `issue[0].diagnostics`. Errors at startup are printed to
standard error as `refhir: <message>`, and the process exits.

## Starting the server

### `cannot install the default FHIR packages …`

```text
refhir: cannot install the default FHIR packages (hl7.fhir.r4.core@4.0.1) from https://packages.fhir.org:
download …: Connection Failed … Point [conformance].package_registry (or REFHIR_PACKAGE_REGISTRY) at a
reachable registry, an HTTPS mirror, or s3://bucket/prefix
```

This is what a server that was told it may download meets when there is no prepared file and the
registry cannot be reached. Release archives and images have the file and download nothing. A
server that was not told it may download, and has no usable file, stops with a different error that
says so (`there is no prepared file …` or `… does not hold …`). Either make `packages.fhir.org` reachable, point
`REFHIR_PACKAGE_REGISTRY` (or `[conformance].package_registry`) at a mirror or an `s3://` bundle, or
start from a persistent volume that already holds the package cache. Packages are cached after the
first successful install, and later starts skip the download. See
[Production deployment](/docs/operate/production/#package-source).

### `/readyz` answers 503 with `"status":"starting"`

This is normal during startup. The server is opening the database, installing packages and
backfilling search indexes. It becomes `200 {"status":"ok"}` when it can serve. `/livez` is `200`
the whole time, so a liveness probe will not restart a server that is only installing.

### `unknown resource type Patient`

```json
{"issue":[{"code":"not-found","diagnostics":"unknown resource type Patient","severity":"error"}],"resourceType":"OperationOutcome"}
```

The server has no packages installed, so it knows no resource types. This happens when
`REFHIR_SKIP_DEFAULT_PACKAGES` is set, or when the package set is empty.
Remove the variable, or add a package with `--package`.

### `Address already in use`

```text
refhir: Address already in use (os error 48)
```

Another process holds the port. Choose another with `--bind 127.0.0.1:8081`, or in the config
file under `[server]`. The default is `127.0.0.1:8080`.

### `invalid socket address syntax`

`--bind` takes an IP address and a port, such as `0.0.0.0:8080`. A host name such as
`localhost:8080` is not accepted.

### `SMART is enabled but no login is configured`

```text
refhir: SMART is enabled but no login is configured: set [smart.oidc] (issuer, client_id,
client_secret) for real sign-in, or pass --smart-demo-login for testing only
```

`--smart` needs a way for users to sign in. Configure an OpenID Connect provider for real use, or add
`--smart-demo-login` on a local test server. The demo login is refused on a non-loopback bind:
`SMART demo login accepts any username, so it is only allowed on a loopback bind`.

### The data is gone after a restart

`--memory` keeps the database in RAM and leaves no file, and the audit trail ends with the process.
Use `--sqlite path` to keep a file, or Postgres. In Docker, mount a volume at `/data`, where the
image keeps its SQLite file.

## Reading and writing

### A search by patient or subject finds nothing

A reference parameter matches the full reference. This finds the resource:

```text
GET /MedicationRequest?patient=Patient/482cb5ad-6cad-47da-af86-8da2ce596e21
```

and this finds nothing, with no error:

```text
GET /MedicationRequest?patient=482cb5ad-6cad-47da-af86-8da2ce596e21
```

### A search by date or token finds nothing

A value that is not well formed is not an error: `GET /Patient?birthdate=notadate` answers `200`
with an empty Bundle. Check the value. A control parameter does fail on a bad value:
`_count must be an integer`.

### `unsupported search parameter bogus on Patient`

An unknown search parameter is a `400`, not silently ignored. Check the spelling, and check
`GET /metadata` for what this server supports. Parameters from an installed package may still be
backfilling just after it is installed.

### `405` on `PUT`

```text
Patient/xyz does not exist and this server does not support update-as-create (client-defined ids);
POST to Patient to create a resource, and the server assigns its id
```

The server assigns the ids of resources it creates, so a `PUT` to an id that does not exist is
refused. `POST` to the type, or start the server with `--update-as-create` (or
`update_as_create = true` under `[conformance]`) if your data names its own ids. The mock-data
seed command needs that flag.

### `400` on a reference: `subject Patient/nope does not exist`

References are checked strictly by default: the target must already exist, and the reference must be
a relative `Type/id`. Create the target first. Inside a transaction Bundle, an entry can refer to
another by `fullUrl`. To allow absolute URLs and targets that are not on this server, set
`--references fhir`.

### `409` on `PUT`: `If-Match "999" does not match version 2`

Another write changed the resource after you read it. This is what `If-Match` is for. Read the
current version and decide again. A `409` with `OCC retries exhausted` means the server retried a
conflicting commit and gave up; send the request again.

## Authorization

### `401`: `a bearer token is required` or `unknown token`

With `--smart`, every request except discovery, `/metadata`, `/oauth/*` and the health probes needs a
bearer token. A `401` carries `WWW-Authenticate: Bearer realm="refhir", error="invalid_token"`. It
also means a token the server does not know, such as one from before a restart of an in-memory
server, or an expired one (`token expired`).

### `403`: `scope does not allow this request`

The token is valid but its scopes do not cover this resource type or interaction. Request wider
scopes when you authorize. Unsupported scopes are dropped silently at authorization, so check the
`scope` in the token response.

### `aud must be this FHIR server`

The authorize request needs `aud` set to the server's address exactly as the discovery document
reports its `issuer`. It arrives as a redirect to your redirect URI with
`error=invalid_request`. See [Register a SMART app](/docs/guides/register-smart-app/).

## Live queries

### The socket upgrade is refused with `401`

With SMART on, `/sync` needs a valid token. The clients treat a `401` or `403` as final: every
watch and mutation fails with it. Get a new token and dial again. A socket whose token expires is
sent `FatalError: the token expired or was revoked` and dropped.

### `429` and `throttled`

Rate limits and quotas are off until you configure them. When they are on, a refusal carries
`Retry-After`. On `/sync` a refused mutation is a `MutationResponse` with `success: false` and
`retryAfterSeconds`, and a refused query is a `QueryFailed` with text such as
`rate limit exceeded: try again in 3 seconds`. A connection may hold a limited number of live
queries (`too many live queries: a connection may hold 100`) and a caller a limited number of
sockets. See [Governance](/docs/guides/governance/).

### A write to a replica fails with `transient`

```text
this server is a read-only replica; send writes to the primary
```

A replica serves reads and forwards writes only when it was started with `--primary-url`. Point
writes at the primary, or give the replica `--primary-url`.

### My watcher does not know it missed changes

After a dropped connection the clients resubscribe and the next result is current, but nothing tells
a watcher that changes happened in between. If that matters, treat every result as the whole truth
and re-evaluate. See [How live queries work](/docs/concepts/how-it-works/).

## Questions

**Does my existing FHIR client work?** Yes, over REST, search, transactions and SMART. Read
[the FHIR API page](/docs/guides/fhir-api/) first for the behaviors that differ from what some
clients assume: `PUT` to a missing id, strict references, and reference parameters that need
`Type/id`.

**How do I install the client?** For TypeScript, `npm install @haihq/refhir`. For Go, `go get github.com/hai-hq/refhir/clients/go`.

**Can I run several processes?** Yes, on one Postgres database: a primary, standbys that take over,
and read replicas. See [Several processes](/docs/operate/clustering/).

**Is it ready for production?** ReFHIR is at version 0.x: configuration, the `/sync` protocol and the HTTP API can change between releases, and upgrades are forward-only, so back up before you upgrade. [Production deployment](/docs/operate/production/) covers what to decide before you run it for real, and the [changelog](/docs/reference/changelog/) lists what changed in each release.

**A database is newer than the binary.** A database that a newer ReFHIR has migrated cannot be used
by an older one. Run the same version everywhere, or restore a backup taken before the upgrade.
`refhir migrate --status` reports a database's schema version without changing anything. See
[Schema migrations](/docs/operate/migrations/).
