# Changelog

All notable changes to ReFHIR are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While the version is 0.x, a release can change configuration, the HTTP API or the `/sync`
protocol. When it does, the entry says so under **Changed**. Database upgrades are forward-only
([Schema migrations](docs/migrations.md)): back up before you upgrade.

## [Unreleased]

### Added

- **`Prefer: handling=lenient` on a search `GET`.** A search whose parameter value cannot be read
  (see Changed) can ask for the parameter to be left out instead: the search runs on the rest, the
  Bundle carries an `OperationOutcome` entry (`search.mode` `outcome`) with a warning for each
  parameter set aside, and its `self` link no longer has them. The response says
  `Preference-Applied: handling=lenient`, and search responses now carry `Vary: Prefer`.
  `handling=strict` is the default. Conditional create, update and delete, `batch` and
  `transaction` entries, GraphQL and live queries are never lenient. See
  [FHIR API](docs/fhir-api.md#search).

### Changed

- **A search whose parameter value cannot be read is now a `400`, where it used to return an empty
  result.** `GET /Patient?birthdate=notadate` and `GET /Observation?value-quantity=abc` answered
  `200` with an empty Bundle, which looks the same as a search that matches nothing; they now answer
  `400` with an `OperationOutcome` that names the parameter, the value and what was expected. It
  covers a date, number or quantity that does not parse, a composite with the wrong number of
  parts, a `near` that is not `latitude|longitude`, `:missing` without `true` or `false`, and
  `:of-type` without exactly `system|code|value`, also behind a chain or `_has` and inside a
  `_filter` (where a parameter that does not exist is now refused too). Strings, tokens, uris and
  references are free text and are not affected. The same check now refuses a conditional
  create, update or delete that used such a value, which used to match nothing (and so create), a
  live query over `/sync`, which answers `QueryFailed` at once instead of staying empty, and a
  query in a Questionnaire that `$populate` runs, which now fails with the message instead of
  populating from nothing. A client
  that relied on the empty result can send `Prefer: handling=lenient`. A date range written as a
  parameter given twice (`date=ge2020&date=le2021`) was read as one value joined by a comma, and
  matched nothing; it now gets this error, with a note that value lists are not supported.

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
