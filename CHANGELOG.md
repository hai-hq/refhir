# Changelog

All notable changes to ReFHIR are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While the version is 0.x, a release can change configuration, the HTTP API or the `/sync`
protocol. When it does, the entry says so under **Changed**. Database upgrades are forward-only
([Schema migrations](docs/migrations.md)): back up before you upgrade.

## [Unreleased]

### Added

- **Subscriptions send `channel.header`.** Each entry (R4 says `Name: value`, for example
  `Authorization: Bearer …`) is added to the `rest-hook` request, so a receiver that needs a
  token can be served. A name can be given once. An entry that is not a valid header, that
  repeats a name, or that sets a header the server controls (`Host`, `Content-Length`,
  `Transfer-Encoding`, `Connection`, `Keep-Alive`, `Upgrade`, `TE`, `Trailer`, `Expect`,
  `Cookie`, `Proxy-*`) fails the notification and puts the Subscription in `error` with the
  reason. A `Content-Type` entry replaces the default, whatever its case.
- **A Subscription's `status` follows its deliveries.** When a `rest-hook` notification still
  fails after its eight attempts (about ten seconds), the server sets `status` to `error` and
  `error` to the time and the reason. The next notification that gets through sets `status` back
  to `active` and clears `error`. Delivery continues while the status is `error`; only `off`
  stops it, and the server never changes an `off` Subscription.

### Changed

- **A `rest-hook` endpoint that redirects has not received the notification.** Redirects are no
  longer followed, and a `3xx` is a failed delivery. A `301`, `302` or `303` used to turn the
  `POST` into a `GET`, and the notification counted as delivered.
- **Retries are spaced.** A failed notification waits at least a second and a half between
  attempts. The retry queue also runs after every commit, so on a busy server all eight attempts
  could be used up in a moment. Before each retry the Subscription is read again: if it has been
  deleted, is `off` or sends somewhere else the notification is dropped, and a changed header is
  the one sent.
- **A `rest-hook` channel with no endpoint ends in `error`.** It was skipped without a word.
- **A notification for a Subscription leaves out its `channel.header`.** A Subscription whose
  `criteria` are `Subscription` is told about every one written, and would otherwise have been
  sent their credentials.
- **The server now writes to Subscriptions.** The `status` and `error` change above is an
  ordinary update, so a Subscription can gain a version you did not write (visible in its
  `_history`, not in the audit trail: there is no caller to name). An `If-Match` that names an
  older version fails with `409`, as for any concurrent edit.
- **Subscription behavior is documented in full** ([Subscriptions](docs/fhir-api.md#subscriptions)):
  only creates and updates are notified, as R4 defines it (a delete, or a resource that stops
  matching, sends nothing); `channel.payload` and `end` are ignored; nothing limits where an
  endpoint may point; retries are in memory and a restart drops them; the WebSocket channel has
  no handshake and keeps one socket per id. `docs/design.md` no longer lists Subscription
  delivery with the durable jobs: only `$export` is durable.

## [0.0.1] - 2026-10-15

The first release. Everything below is new.

### Added

- **A FHIR R4 (4.0.1) server.** Every resource type in the installed packages gets read, vread,
  create, update, JSON Patch, delete, history, search, conditional create, update and delete, and
  `batch` and atomic `transaction` Bundles. Reads can be returned as XML. Resources are validated
  against the installed StructureDefinitions, and references are checked on write (`strict` by
  default, `fhir` optionally). The server assigns resource ids; `--update-as-create` lets `PUT`
  create under a client-chosen id.
- **Search** over the full R4 search parameter set, compiled from the installed packages and
  backfilled in the background when a package or a `SearchParameter` adds one. Chaining, `_has`,
  `_include`, `_revinclude` and compartment search.
- **Operations:** `$everything`, `$export` (system, type and instance, as durable jobs),
  `$validate`, `$meta`, `$meta-add`, `$meta-delete`, `$document`, `$install` for packages,
  terminology (`$expand`, `$lookup`, `$validate-code`, `$subsumes`, `$translate`,
  `$expandability`), and Structured Data Capture 4.0.0 (`$populate`, `$extract`, `$assemble`,
  `$next-question`, `$behavior`, with an optional form-archiver mode).
- **GraphQL** at `/graphql`, and **Subscriptions** with `rest-hook` and `websocket` channels.
- **Reactive queries** over a `/sync` WebSocket: subscribe to the result of a FHIR query and
  receive each committed change to it, with a transaction Bundle arriving as one step. Go and
  TypeScript clients (the latter with a React hook, `useFHIRQuery`). REST reads that follow a
  write can wait for a replica to catch up, through `X-Refhir-Commit-Ts` and `X-Refhir-Min-Ts`.
- **SMART App Launch 2.2** as authorization server and resource server: authorization code with
  PKCE, EHR launch, refresh tokens, backend services (`private_key_jwt`), dynamic client
  registration, token introspection and revocation, v1 and v2 scopes, and patient-compartment
  enforcement on REST and `/sync`. Users sign in through OpenID Connect, with an allowlist,
  binding to a `Practitioner` or `Patient`, and token introspection for a gateway.
- **Governance:** per-client metering (`/_usage`), a request log (`/_requests`) and an audit trail
  of `AuditEvent`s (`/_audit`) that is hash-chained and verifiable (`/_audit/verify`), all on by
  default; rate limits and quotas that you configure. `/sync` is governed like HTTP.
- **Storage** in SQLite (a file, or in memory) or Postgres, with Postgres TLS (`sslmode`,
  `sslrootcert`, `sslcert`, `sslkey`) and forward-only schema migrations. `refhir migrate
  --status` and `--dry-run` report what a start would do.
- **Several processes on one database:** a writer lease with fencing, standbys that take over,
  and read replicas that forward writes. `/livez`, `/readyz` and `/healthz` probes.
- **Packages** installed from packages.fhir.org, a mirror, an S3 bucket, a `.tgz` or a folder, at
  start-up or through `$install`. Value sets are expanded with their own package's terminology,
  and cross-version copies do not shadow R4's.
- **Deployment:** a container image, a Helm chart (single process or a Postgres cluster), a
  Docker Compose cluster behind HAProxy, and an Inferno profile for SMART conformance runs.
- `refhir --version`.

[Unreleased]: https://github.com/hai-hq/refhir/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/hai-hq/refhir/releases/tag/v0.0.1
