# Changelog

All notable changes to ReFHIR are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While the version is 0.x, a release can change configuration, the HTTP API or the `/sync`
protocol. When it does, the entry says so under **Changed**. Database upgrades are forward-only
([Schema migrations](docs/migrations.md)): back up before you upgrade.

## [Unreleased]

### Fixed

- **A comma between the values of one search parameter is OR.** `Slot?status=free,busy` and
  `HealthcareService?_id=a,b` matched nothing, because the whole value was compared as one. This
  holds for ids and every parameter type, with `\,` for a literal comma, and a value lists at most
  1000 alternatives. `:not` and `:not-in` with a list leave out every value in it.
- **`_include:iterate` and `_revinclude:iterate` are accepted** instead of refused as an unknown
  parameter. They apply a rule to the matches and to the resources already added, until none is
  added, and a search that would add more than 1000 resources that way is a `400`.

### Changed

- A comma in a search value now separates alternatives wherever the value comes from: a query
  string, `If-None-Exist`, a conditional update or delete, a Bundle entry. A client that sent a
  value holding a comma without escaping it (`identifier=a,b` for the identifier `a,b`) now matches
  the resources with `a` or with `b` instead, so a conditional create can be blocked by, and a
  conditional update or delete can act on, a different resource than before. Write `\,` for a
  comma that is part of a value.
- A search parameter given more than once is AND, as FHIR defines it, in a query string, in the
  URL of a Bundle entry and in Subscription criteria (only the last one counted in the last two).
  The values were joined with a comma, which now means OR, so
  `date=ge2026-01-01&date=lt2026-02-01` would have become a match on either bound.
- `_revinclude=*` is a `400` naming the problem. It used to return no reverse includes and say
  nothing.
- `_include` and `_revinclude` no longer add another patient's resources for a patient-scoped
  token, as a search does not return them. A match that another match refers to is no longer
  repeated as an include. The CapabilityStatement's `rest.documentation` states how search treats
  commas, repeated parameters, `:iterate` and the limits above.

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
