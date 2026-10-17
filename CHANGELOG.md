# Changelog

All notable changes to ReFHIR are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While the version is 0.x, a release can change configuration, the HTTP API or the `/sync`
protocol. When it does, the entry says so under **Changed**. Database upgrades are forward-only
([Schema migrations](docs/migrations.md)): back up before you upgrade.

## [Unreleased]

### Added

- **A repeated search parameter in a live query.** A `/sync` query's arguments are a JSON object, so
  a parameter that a URL would give twice (AND) had no way to be written. It is now an array of
  strings: `{"schedule.actor": ["HealthcareService/h1", "Location/l1"]}` finds the Slots whose
  Schedule has both as actors, as `schedule.actor=…&schedule.actor=…` does over REST. An empty
  array, or one with anything but strings in it, is refused (`QueryFailed`). The Go and TypeScript
  clients carry an array through `Watch`, `Query` and `search`.
- **`[server].default_page_size`** (or `REFHIR_DEFAULT_PAGE_SIZE`). See Changed.
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
- **`Prefer: handling=lenient` on a search `GET`.** A search whose parameter value cannot be read
  (see Changed) can ask for the parameter to be left out instead: the search runs on the rest, the
  Bundle carries an `OperationOutcome` entry (`search.mode` `outcome`) with a warning for each
  parameter set aside, and its `self` link no longer has them. The response says
  `Preference-Applied: handling=lenient`, and search responses now carry `Vary: Prefer`.
  `handling=strict` is the default. Conditional create, update and delete, `batch` and
  `transaction` entries, GraphQL and live queries are never lenient. See
  [FHIR API](docs/fhir-api.md#search).

### Changed

- **A search `GET` that names no `_count` returns a page of 100 matches with a `next` link.** It
  used to return every match in one response (1,520 Slots in one 0.3 s answer, which a client then
  handed on whole). FHIR leaves the page size to the server. The `self` link shows the `_count` that
  applied, `total` still counts every match, and a `_count` in the request is used as sent
  (`_count=0` is the total alone). A client that read every match from one response must now follow
  `next`, or ask for a `_count` large enough. **A page without `_sort` is in index order**, which is
  not an order to rely on: send `_sort`. This covers `GET [type]` and a compartment search. Live
  queries, GraphQL, the entries of a Bundle, conditional update and delete and the searches behind
  a chain, `_has` or `_include` are not paged. Set `[server].default_page_size` or
  `REFHIR_DEFAULT_PAGE_SIZE` to change the page, or to `0` to return every match as before.
- **`_sort` with a small `_count` no longer costs a millisecond for every match.** On a server with
  1,520 free Slots, `Slot?status=free&start=ge…&_sort=start&_count=3` took 0.9 s (5.2 s with 6,080),
  because the search worked out both sort keys again for every comparison of the sort, and kept
  every match until it was done. Each match now gets its sort key once, only the `offset + count`
  that sort first are held, and the order and the bundle are unchanged. Reading a plain path such as
  `Slot.start`, `Schedule.actor` or `Appointment.participant.actor` out of a document no longer
  copies the whole document first, which cuts what it costs to check a row and to index a write.
  The same search takes 20 ms (78 ms with 6,080 Slots), and about what the search takes without
  `_sort`. The time still grows with the number of Slots the filters leave to be read, by about
  13 µs each; narrowing by a parameter the index decides (`status`, `schedule`) keeps it small.
- **Naming more than one database on the command line is now an error.** `--sqlite`, `--memory`
  and `--postgres` are alternatives, but ReFHIR used to start without complaint and use one of
  them: `refhir --memory --sqlite x.db` ran in memory and never created `x.db`. It now exits with
  `pass only one of --sqlite, --memory, or --postgres`, as the
  [CLI reference](docs/configuration.md#cli-reference) has always said, and that includes the same
  flag twice. `--sqlite` or `--postgres` followed directly by another flag is refused too, where
  it used to take the flag as its value (`--sqlite --memory` opened a file called `--memory`).
  `refhir migrate` does the same for `--sqlite` and `--postgres` (exit status 2). A single flag
  still overrides the database the config file or the environment names. If a script passes two,
  remove the one you did not mean.
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
- A bare id given to a reference parameter that can point at any resource (`Task?subject=123`) or
  that lists no `target` is now a `400` that asks for `Type/id`; it used to find nothing.
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
  populating from nothing. Each alternative of a comma list, and each occurrence of a parameter
  given twice, is read on its own. A client that relied on the empty result can send
  `Prefer: handling=lenient`.

### Fixed

- **A comma between the values of one search parameter is OR.** `Slot?status=free,busy` and
  `HealthcareService?_id=a,b` matched nothing, because the whole value was compared as one. This
  holds for ids and every parameter type, with `\,` for a literal comma, and a value lists at most
  1000 alternatives. `:not` and `:not-in` with a list leave out every value in it.
- **`_include:iterate` and `_revinclude:iterate` are accepted** instead of refused as an unknown
  parameter. They apply a rule to the matches and to the resources already added, until none is
  added, and a search that would add more than 1000 resources that way is a `400`.
- **A reference search given just an id found nothing.** `Observation?patient=123` now returns
  what `Observation?patient=Patient/123` returns. A bare id is looked up as a reference to each
  type the search parameter can point at, so `subject=123` finds a `Patient/123` or a `Group/123`.
  Live queries over `/sync` read ids the same way, and are woken only by writes that reference
  that id.

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
