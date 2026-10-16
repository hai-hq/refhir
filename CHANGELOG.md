# Changelog

All notable changes to ReFHIR are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While the version is 0.x, a release can change configuration, the HTTP API or the `/sync`
protocol. When it does, the entry says so under **Changed**. Database upgrades are forward-only
([Schema migrations](docs/migrations.md)): back up before you upgrade.

## [Unreleased]

### Added

- **A first start installs prepared packages, with no registry and little memory.** Release
  archives and container images now carry the release's FHIR packages (R4 core, SDC and what they
  depend on) in a file, `refhir-packages.bundle`, that is made when they are built: the packages
  are downloaded, read and expanded once, there. A server finds the file beside its binary (or in
  `../share/refhir/` from it, or at `[conformance].package_bundle` / `REFHIR_PACKAGE_BUNDLE`; `off`
  says not to look) and stores what is in it, so the first start needs no network and no package
  cache, and a first start peaks at a small fraction of the memory it did. A file that is there and cannot be used (damaged, or made for another release) stops the
  start with an error that says so. `refhir package-bundle` makes the file for a binary you built
  yourself, or for other packages (`--spec`). See [Preparing the
  packages](deploy/production.md#preparing-the-packages).
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

- **A running server holds what every request needs and an index of the rest, and reads the rest
  the first time a request uses it.** It used to hold every StructureDefinition, code system,
  value set expansion and concept map of the installed packages in memory: about 100 MB for R4
  core, SDC and what they depend on, nearly all of what the process used. It now holds the base
  types (every resource, data type and primitive), the value sets their bindings check a code
  against and the code systems behind those, which is what a request of any common kind is
  validated with, and an index of everything else, about 27 MB in all. A profile, an extension, a
  concept map or a table nothing binds is read from the database the first time a request uses it,
  and kept, up to about 8 MB of each kind, until other reads push it out; a server that has been
  asked about every value set the packages define holds about 48 MB. On macOS the running
  server's private memory went from about 90 MB to about 27 MB. A request of a common kind reads
  nothing from the database, the first time or after (a test checks it); the first request that
  names a profile waits for a read of a few milliseconds, and the next does not. What a request is
  answered does not change, and a catalog loaded this way is checked against one that holds
  everything, for every definition and expansion of R4 core. Validating a resource is about a
  third faster, because a definition is shared and no longer copied for each request. A restart
  reads what is held, which can take up to half a second longer. When the catalog is rebuilt (a
  package, a change to a search parameter) or a replica or standby reloads it after the primary
  changed it, the new catalog reads what the old one had in use before it takes its place, while
  the old one is still answering, so requests are not sent back to waiting. A replica that lags the
  primary can read a row the primary has just written that its index does not describe yet.
- **A first start stores each package as it reads it.** The prepared file (see below) holds a
  package as values of about 4 MB of rows, and the database takes them one at a time inside one
  transaction, so a package is stored whole or not at all, as before. The largest default package
  is over 120 MB of JSON, and a first start used to hold all of it; on SQLite it now holds a few
  megabytes of it at a time, and the heap peaks at about 42 MB where it peaked at about 129 MB
  (resident memory 120 MB where it was 300 MB). A prepared file made by an earlier build of this
  version has another format and is refused with an error that says so: make it again with
  `refhir package-bundle`. Postgres still reads each package whole.
- **The server downloads packages only if it is told it may.** It used to fetch the release's
  packages from packages.fhir.org on a first start, and anything named in `[conformance].packages`
  or `$install`. Now nothing is downloaded unless `[conformance].package_download = true` (or
  `REFHIR_PACKAGE_DOWNLOAD=true`; the Helm chart has `conformance.packageDownload`). Without it a
  package that is not installed and not in the package cache is refused: a first start with no
  prepared file stops with an error that says what to do, and `$install` answers `403`. Release
  archives and images are not affected, because they carry their packages. **If you build ReFHIR
  yourself and relied on the first-start download, set `package_download = true`, or make a file
  with `refhir package-bundle`.** Packages from a directory or a `.tgz` need no setting.
- **A package that is only a dependency and holds examples is not installed.** SDC lists
  `hl7.fhir.r4.examples`, which holds the example resources of R4 and a second copy of every core
  definition, none of which a server uses; it was downloaded, read, stored and loaded for
  nothing. It is left out when a package only depends on it. Name it in `[conformance].packages`
  (or give it to `$install`) to install it.
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

- **A first start needed several gigabytes of memory.** Installing the default packages (R4 core,
  SDC and the seven packages SDC depends on) peaked at over 2 GiB, so a container given 512 MiB or
  2 GiB was killed (exit status 137) while it was still installing, and a server that started
  fine kept that memory for as long as it ran. Loading the conformance tables and reading a
  package now work through them a page at a time instead of parsing every document and holding
  all of it, and what they keep is shared: the R4 packages hold about 66,000 constraints of
  which 299 differ, and each is now kept once. A first start with the default packages peaks at
  about a quarter of what it did, and a restart on a database that already has them takes about a
  tenth of the memory it did and starts faster. On Linux the memory a load frees is now handed
  back to the system, so a running server does not stay at its start-up peak. See
  [Memory](deploy/production.md#fhir-version-and-implementation-guides).
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
