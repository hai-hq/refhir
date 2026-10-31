# Changelog

All notable changes to ReFHIR are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While the version is 0.x, a release can change configuration, the HTTP API or the `/sync`
protocol. When it does, the entry says so under **Changed**. Database upgrades are forward-only
([Schema migrations](docs/migrations.md)): back up before you upgrade.

## [Unreleased]

### Added

- **A client can outlive its token.** The Go and TypeScript clients take a token source
  (`WithTokenSource`, or a function as the TypeScript `token` option) that they ask for the first
  `/sync` upgrade, for every redial and for every REST call. The server ends a socket whose token
  expired, and the client dials again with a renewed one, so a service that holds live queries open
  for hours behind a gateway that issues short-lived tokens keeps its watches and its session. On a
  401 at a redial the client asks the source once more before it gives up; a second 401, a source
  that returns the token just refused, and a 403 still end the session, and an error from the source
  is retried with the dial backoff. `Bearer(token)` and a plain `token` string are a source that
  always gives the same token, so they behave as before. A token is renewed by redial: there is no
  message that replaces the token of an open socket.
- **`QueryFailed` carries a `code`.** It is the FHIR IssueType that applies (`deleted`, `not-found`,
  `forbidden`, `throttled`, `too-costly`, `invalid`, `transient`, `exception`), so a client can tell a
  watched resource that was deleted from a scope refusal, a rate limit or a cap without reading the
  text of `error`, which is still sent and still not a contract. The Go client returns it as a
  `*QueryError` from `Watch.Next`, and the TypeScript client as a `QueryError`.
- **A client can tell a mutation that was never sent from one in doubt.** `Mutate` on a Go client that
  is dialling again fails at once with `ErrNotSent` (`NotSentError` in TypeScript), and
  `Connected()` (`connected`) says whether it has a socket. A mutation lost with the socket fails
  with `ErrDisconnected` (`DisconnectedError`), which may have been applied. TypeScript `mutate` and
  `watch` take an `AbortSignal`.

### Changed

- **A call whose caller gave up is not sent after a redial.** The Go client's session goroutine
  redials without reading its commands, so a `Mutate` or `Watch` made meanwhile used to wait and go
  out when the socket was back, whatever its context said; a caller that had timed out could not
  know the write was applied later. A command whose context has ended is now dropped unsent, and a
  `Mutate` made while the client dials again fails at once instead of waiting.

### Fixed

- **A data race in the Go client's `Watch.Close`.** A watch cancelled while its `Add` was still being
  sent read its id while the session goroutine wrote it. The id is now handed over under the watch's
  lock, and a query whose watch was closed first is never registered, so none is left running on
  the server.

### Documentation

- `Patient/$match` is available over `/sync`, as a `Mutation` with the REST `Parameters` as its
  `args`, on a server without SMART; the docs said it was not. With SMART on `/sync` still refuses it
  (and `$apply` and `$submit`), as it has no scope entry for them. The page also gives the argument
  shape of the terminology operations over `/sync` (flat names) and of instance-level operations such
  as `ChargeItemDefinition/$apply` (`{id, resource}`).

## [0.0.4] - 2026-10-27

### Added

- **`Patient/$match` can be limited to one organization, by the Patient sent.** One server holds
  many organizations' Patients, and `$match` compared the request with all of them: a clinic that
  asked about a person also got the same person's records at the other organizations, `count` could
  be filled by records that were not the clinic's, and `onlyCertainMatches` returned nothing for a
  person at two organizations that share an identifier, which is two certain matches. R4 gives
  `$match` no parameter for it, so the Patient in `resource` says it: when it has a
  `managingOrganization` (`Organization/id`), only the Patients whose `managingOrganization` is that
  reference are candidates, as `Patient?organization=` finds them in a search. The grades,
  `onlyCertainMatches` and `count` then apply to that set alone, and a `replaced-by` link is not
  followed to a Patient of another organization, nor through one. The `managingOrganization` is not
  compared: it is no evidence and changes no grade or score. One that is not `Organization/id` (a
  URL, a `#contained` reference, only an `identifier`) is a `400`; an organization with no match is
  an empty `searchset`; without one nothing changes, and the CapabilityStatement says so with the
  other `$match` notes. `$match` still takes only `resource`, `onlyCertainMatches` and `count`.
  See [FHIR API](docs/fhir-api.md#one-organizations-patients).

### Changed

- **`Patient/$match` reads the `managingOrganization` of the Patient sent.** In 0.0.3 it was ignored
  and every organization's Patients were compared. Now a `managingOrganization` of `Organization/id`
  limits the answer to that organization's Patients, and one that is not (an absolute URL, a
  `#contained` reference, only an `identifier` or a `display`) is a `400`. A caller that sends a
  stored Patient as `resource` should leave the element out to compare with every organization.

## [0.0.3] - 2026-10-21

### Added

- **`POST [base]/ChargeItemDefinition/[id]/$apply`.** It answered `405`. It now takes `chargeItem`
  (and optionally `account`) as FHIR R4's `Parameters` and answers with a draft `Invoice`, not stored,
  that prices the ChargeItem under the definition: the definition's `applicability` conditions are
  evaluated as FHIRPath with the ChargeItem as `%context` (every one of the definition's must be true,
  or the answer is a `422` that names it), and the price components of each `propertyGroup` whose
  conditions are true are put on the line. `totalNet` is the base price plus surcharges less
  deductions and discounts, and `totalGross` adds tax; `informational` components are only shown.
  Amounts are added as decimals, keep the definition's currency, and are never converted: two
  currencies are a `422`, as are two base prices that apply and a sum too large to hold. A condition
  follows a reference only to a type the token may read. R4 leaves the answer as "Any" resource, so the Invoice is a choice, made
  because its line has price components shaped like the definition's. A definition that is not there is
  a `404`, a ChargeItem or Account that is not there a `422`. With SMART the token needs to read the
  definition, the ChargeItem and the Account; a token limited to one patient cannot use it. See
  [FHIR API](docs/fhir-api.md#pricing-a-charge-item-chargeitemdefinitionapply).
- **`POST [base]/Claim/$submit` and `POST [base]/CoverageEligibilityRequest/$submit`.** Both answered
  `405`. They now take FHIR R4's `Parameters` with a `resource`, a `Claim` or `CoverageEligibilityRequest`
  or a `Bundle` of them (or that resource as the body), store them with whatever the Bundle brings
  (entries refer to each other by `fullUrl`, as in a transaction), and answer with a `ClaimResponse` or
  `CoverageEligibilityResponse` that has `outcome` `queued`, `status` `active`, `request` pointing at the
  stored request, and `patient`, `insurer` and `use` (or `purpose`) copied from it. FHIR leaves the
  deciding to the payer, and a server is not one, so this is the choice made here: the response is stored
  and announced like any other create, a Subscription on `ClaimResponse?outcome=queued` tells whoever
  adjudicates, and they update the response with the decision. A submission is one transaction, so a
  request that is invalid, or names something that does not exist, stores nothing and is answered with the
  validator's errors. A request that `ifNoneExist` finds already stored is answered with the response it
  has, and no second one is made. Bundles inside the Bundle each have their own `fullUrl`s. The token
  needs to be able to create every type the submission writes (a `PUT` with a query creates, so it counts). The
  CapabilityStatement lists the operations. See
  [FHIR API](docs/fhir-api.md#submitting-a-claim-or-an-eligibility-request).
- **The terminology operations answer a `GET`.** R4 defines `$expand`, `$validate-code`, `$lookup`,
  `$subsumes` and `$translate` as having no side effects, and they were `POST` only (a `GET`
  answered `405`). Their parameters go in the query string (`url`, `filter`, `count` and `offset`
  for `$expand`; `system`, `code`, `display`, `abstract` and so on for the rest), and the answer is
  the one the `POST` gives. A `coding` or `codeableConcept` is still sent as a `POST`. See
  [FHIR API](docs/fhir-api.md#terminology).
- **Expansions are complete R4 ValueSets.** The ValueSet an expansion comes in has the `status` R4
  requires (that of the value set expanded, `active` when it says nothing), and the expansion has
  the required `timestamp` (the time the server produced it for the request), an `identifier`
  (a `urn:uuid` made from what the expansion says), and in `expansion.parameter` what was asked that shaped
  it (`filter`, `count`, `offset`, `activeOnly`). An entry carries `"inactive": true` when its code
  system says the concept is inactive.
- **`$expand` takes `activeOnly`.** `activeOnly=true` leaves out the codes whose code system calls
  them inactive; the `total` counts what is left.
- **The terminology operations answer at the instance.** `ValueSet/[id]/$expand` and
  `/$validate-code`, `CodeSystem/[id]/$validate-code` and `/$subsumes`, and
  `ConceptMap/[id]/$translate`, as `POST` or `GET`, with R4's instance forms (`CodeSystem/$lookup`
  is the type only). The instance is a resource the server stores, and what is asked of it is asked
  of the value set, code system or concept map its `url` names (`409` when a package defines that url
  too, since the terminology cannot tell which is meant). A value set that came from a package
  is expanded by `url`.
- **HL7's service categories and service types expand.** `http://hl7.org/fhir/ValueSet/service-category`
  and `service-type` (and `program` and `appointment-cancellation-reason`) are a whole-system include
  of a code system whose `content` is `example`, and were refused for that. They now expand to the
  concepts the server holds, and the expansion says it is only a sample with an `expansion.parameter`
  named `<system>|<version>#CodeSystem.content` whose value is `example`, as R4 describes. A filter on
  such a system, an exclusion of one, one with no concepts installed, and a `fragment` are still
  refused. `$validate-code` agrees with `$expand`; a code outside the sample is `false` with a
  message that says the list is only a sample, and the resource validator reports the code as one it
  cannot check, not as an error.
- **Expansions mark abstract codes.** An entry of `$expand` carries `"abstract": true` when its code
  system marks the concept `notSelectable`, as HL7's v3 code systems do for their grouping concepts
  (R4's `ValueSet.expansion.contains.abstract`). `coverage-type` flags `_ActCoverageTypeCode`,
  `_ActHealthInsuranceTypeCode`, `_ActInsurancePolicyCode`, `_ActInsuranceTypeCode` and
  `_ActProgramTypeCode`, and no other entry. `ValueSet/$validate-code` takes R4's `abstract` input:
  `false` refuses an abstract code, `true` accepts it, and without it the code is valid and the
  `message` says it is a grouping concept.
- **A value set that waits on another package's code system is expanded when that package
  arrives.** A package stores the expansions of its own value sets, made from its own resources,
  so a value set that includes a code system (or a value set) that another package carries was
  left with no expansion, and installing that other package later, or earlier, did not give it
  one until the next rebuild. After any install, the value sets that have no expansion are tried
  again, and the ones that can now be expanded are expanded and stored under the package whose
  definition they are of. It is one attempt at each value set that has none (94 for the R4 and SDC
  packages, about a sixth of a second) and not a rebuild.
- **ISO 4217 currency codes are built in.** HL7 publishes `ValueSet/currencies` (the required
  binding of `Money.currency`) as an include of `urn:iso:std:iso:4217` and no CodeSystem for it, so
  the value set could not be expanded and a currency could not be checked. The server now installs the
  code system itself, at start, once: the 178 active alphabetic codes with their numeric codes and
  English names. `$expand` of `currencies` lists them, `$validate-code` accepts a real code and
  refuses an unknown one, and a `Money` with an unknown `currency` is rejected. A database that
  already has the R4 packages gets it at its next start, as the package `refhir.builtin.terminology`.
- **`Patient/$summary`: an International Patient Summary.** `GET` or `POST` it on a patient (or on
  `Patient`, with an `identifier`) and it returns an IPS document Bundle: a Composition with
  allergies, problems and medications (always there, with `emptyReason` when there is nothing),
  and immunizations, laboratory results and procedures (left out when empty), each with a table
  in its text, together with the patient's own resources and what they point at. It holds only
  what the caller's scopes let them read, and never another patient. With the IPS package
  installed, the summary validates against the IPS profiles. See
  [Patient summary](docs/patient-summary.md).
- **`Patient/[id]/$health-cards-issue`: SMART Health Cards.** With a P-256 key in
  `[health_cards].key_file` (or `REFHIR_HEALTH_CARDS_KEY_FILE`), the server signs cards that hold the
  patient's name and birth date and the resources asked for by `credentialType`, shrunk as the card
  format asks, and publishes the public key at `/health-cards/.well-known/jwks.json`. `_since` holds
  a card to what was written at or after it. See [SMART Health Cards](docs/health-cards.md).
- **`meta.lastUpdated`, `_lastUpdated`, and history by date.** The server sets `meta.lastUpdated` on
  every version it writes, to the millisecond, in UTC. `_lastUpdated` finds resources by it (it
  matched nothing before, because nothing was ever set), and `_since` on a type's or the server's
  history takes a date and time as FHIR has it (it was a `400` for anything but a version id). A
  version id still works. Versions written before this release have no time: they are not found by
  `_lastUpdated`, and get one the next time they are written. A deletion has no time, so a history
  by date lists a deletion from just before the date, never leaves one out. See
  [When a resource was written](docs/fhir-api.md#when-a-resource-was-written).
- **A guide to SMART Health Links**, with a script that fetches a summary, encrypts it and
  makes the link, and a host for the links (`link-host`, below). The server does not host links: a
  separate host holds the encrypted files, so the key is never on a server that holds the record.
  See [SMART Health Links](docs/health-links.md).
- **A link host to run, as a package of its own.** `link-host` is a [Hono](https://hono.dev) server in
  TypeScript that stores the encrypted files of SMART Health Links in Postgres, or in a SQLite file for
  one machine with no database server, and answers whoever opens a link. It never has a key, so a host
  that is broken into holds nothing that can be read. `link-host/deploy` starts it with Caddy, which
  gets and renews an HTTPS certificate, and Postgres (`compose.yaml`) or a SQLite volume
  (`compose.sqlite.yaml`), from one `.env` file. The host:
  - **Keeps nothing on its own disk** with Postgres, so any number of copies run against one
    database, and a new one may start before the old one stops. With SQLite several copies share the
    file on one machine, and `refhir-link-host backup` copies it while they run. Either way a guess at
    a passcode is counted once whichever copy receives it: in a transaction on the link's row in
    Postgres, and by running again a change that finds the link changed under it in SQLite. The schema
    is made on first start.
  - **Clears away what has ended.** The encrypted files of a link that expired, was switched off or ran
    out of attempts are deleted, and the record is kept for 30 days so its owner can read who asked.
    A link without an `expiresAt` lasts 30 days, and the answer says when.
  - **Limits how often it is asked**: by address, by link, for owners, for making links, and a caller
    who keeps getting a token wrong is shut out. Requests, headers, time and concurrency are bounded.
  - **Says who may make links**: a clients file with a token (or its hash) and limits for each,
    addresses the owner routes or a client are tied to, and `REQUIRE_PASSCODE`. Owner routes cannot
    be called from another site's page unless it is named.
  - **Says what it does**: a JSON line for each request and event, naming a link by its first eight
    characters and logging nothing that opens one; the address, browser and outcome of each caller in
    the link's access list; `/readyz`, which fails when the database cannot be written; `/metrics`;
    and a stop that tells the balancer first and then finishes what it is answering.
  - **Can be changed in code**: `authenticate`, `authorizeCreate`, `authorizeManifest` (to check who a
    recipient is), `onEvent`, and a `store` that another database can stand in for.

- **`POST [base]/Patient/$match`.** A server that is asked whether it already holds a person used
  to answer `405`. It now takes FHIR R4's `Parameters` (`resource`, `onlyCertainMatches`, `count`)
  and answers a `searchset` Bundle, best first, with a `search.score` and the `match-grade`
  extension on each entry: `certain` for an identifier with the same system and value, `probable`
  for the same name, full birth date and a phone number or email address, `possible` for the same
  name and full birth date. Names ignore case, accents and order (a family name written as a given
  name matches), phone numbers ignore spacing and whether they have a country code (`+234`, or the
  `0` of a national number, is Nigeria's), and a different birth date is not a match. A Patient that
  is not active is never returned, and a match on one that another replaced (`link` type
  `replaced-by`) is the Patient that replaced it, followed through up to five links in the store,
  `probable` at the most and never `certain`. `onlyCertainMatches` returns nothing when more than
  one Patient is a potential match. Each entry says what it was found by in `match-evidence`
  extensions on `search`: the name, birth date, identifier, phone number or email address that
  matched (the last three as the request wrote them, so a caller can tell which of its own phone
  numbers matched), and the replaced Patient a match came through. The request is JSON: an XML body
  is refused with a `415`. The CapabilityStatement lists the operation on `Patient`. A scope that
  may search Patients may use it; a token limited to one patient may not. See
  [FHIR API](docs/fhir-api.md#finding-a-patient-patientmatch).

### Changed

- **A newer version of a package takes the place of the older version.** A package at a higher
  version used to be stored next to the one the database held, and what both defined stayed in
  force: a version that changed a search parameter's expression stopped the server from starting
  (`search parameter ChargeItem.status expression conflicts with an installed package`), one that
  gave a parameter another code left the old code answering beside the new, one that left a
  parameter out kept it, and a profile that a new version changed could lose to the older one's.
  The newest version of a package is now the one in force, and the database holds one version of
  each package: installing a newer one replaces the older one, and the older one's profiles, value
  sets, code systems and search parameters are removed at the next start, which is how a replica
  can go on reading them until it has moved to the new catalog. A search parameter the new version
  defines again takes its definition, and the resources already stored are indexed again for it
  when its expression, type or targets changed (before the server answers requests, or before
  `$install` returns); one that did not change is left as it was. A parameter whose code changed is searched by the new code only,
  a parameter the version leaves out is no longer accepted or listed in the CapabilityStatement,
  and what was indexed under a code that is gone is cleared. The same holds for a package that
  another package depends on. Other packages are never replaced, and a clash between a parameter
  and one of theirs still refuses the install. Installing the same version again, or a version
  older than the one the database holds, changes nothing, so a server can be started again on an
  earlier release of its package. An install that fails leaves the database on the version it had,
  and a search parameter that an install left waiting for its index (it stopped before the
  backfill was queued) is queued at the next start. A version written differently but meaning the
  same, such as `0.10` for `0.10.0`, replaces the one installed. A replica keeps answering from its
  previous catalog until the primary has finished indexing: it still finds what the replaced
  version held, and answers a parameter whose expression changed from an index that is being
  rebuilt. A shared Postgres package leaves the shared catalog when the last tenant that had it has
  given it up. Resources of a type that only the older
  version defined stay in the database but are not served.
  See [Upgrading a package](docs/configuration.md#upgrading-a-package).
- **A date search follows R4 for ranges: `eq`, `ne`, `ge`, `le`, and a period as a range.** A value
  and a stored date each stand for a range of time (a year, a day, a second), and `eq` took any
  overlap, so a stored `2026` was found by `eq2026-10-31`. `eq` now finds what lies wholly inside
  the value and `ne` what does not, and `ge` and `le` find what `gt` and `lt` find and what lies
  wholly inside the value (a month that ends with the day is no longer `ge` that day). A `Period`
  was read as two separate points, its start and its end; it is now the range between them, and
  one with no `end` has not ended and one with no `start` has no beginning, as R4 reads them. **An
  Encounter stored with only `period.start` is now found by every window after its start**, since
  it is going on; write `period.end` when it finishes. `ap` finds what overlaps the value. A
  period with no end, or no start, that was saved before this release is found by `gt` and `ge`
  (no end) or by `lt` and `le` (no start) only near the date it does have until it is written
  again, since the index holds a key for the open end only from now on (see
  [Searching by a date or a time](docs/fhir-api.md#searching-by-a-date-or-a-time)).
- **A date that does not exist is refused when it is stored.** A `date`, `dateTime` or `instant`
  with day `00` (`2026-10-00`), year `0000`, a sign before the year, or a day that is not one of its
  month (`2026-02-30`, `2023-02-29`) was accepted, and every one of them but year `0000` was a date
  that no search could read, so the resource was stored and nothing found it. It is now a `400`
  that names the element, as R4 asks. `date=0000` in a search is a `400` too. Such a date that is
  already stored stays stored, and is still found by no search.
- **A time with no zone is refused when it is stored.** `"effectiveDateTime": "2026-10-31T22:30:00"`
  was accepted, though FHIR asks for a zone with every time in a `dateTime`, and a search by a date
  read it as UTC whatever zone its writer meant. It is now a `400` that names the element. A date
  with no time (`2026-10-31`) still needs no zone, and a search value with no zone is still read as
  UTC. A system that sends times with no zone has to say which zone they are in.
- **`_sort` on a date orders by the moment, not by the text.** `_sort=date` put
  `2026-10-31T22:30:00-05:00` (a time on 1 November, UTC) among the times of 31 October, and
  `…T23:30:00+01:00` after `…T23:00:00Z` though it is earlier. It now orders by when each date
  starts, so the order is the same whatever zone the times were written in.
- **With SMART, the terminology operations need the `read` permission, not `create`.** `$expand`,
  `$expandability`, `$validate-code`, `$lookup`, `$subsumes` and `$translate` change nothing, but a
  `POST` of one was authorised as a create, so a client with only `read` could not use them and
  one with only `create` could. They are reads, as a `GET` of them is. A client that was given only
  `create` for these resources to call them needs `read` now.
- **Stored expansions are rebuilt once at the first start after this version**, so a database
  that holds the R4 packages gains the expansions above. It is the same one-time rebuild an upgrade
  has done before, and the next start finds nothing to do.
- **A date search compares to the millisecond and in the zone a time is written in.** It compared
  in whole seconds, so two times in one second could not be told apart (`lt` on a time missed a time
  33 ms before it), and it read `+01:00` and `Z` as the same, so a time with an offset was found an
  hour or more away from where it is. A value that is a date, or that has no fraction, still
  stands for all of the day or the second it names.
- **`gt` and `lt` on a date follow FHIR.** `gt2026-10-18` found a resource that ended after the
  day's end, and `lt` one that began before the day's start, as `sa` and `eb` did: a period or a
  day that straddles the value was left out. `gt` and `lt` now take what reaches beyond the value;
  `sa` and `eb` take only what lies wholly beyond it.
- **A slice's minimum and maximum are checked.** A profile that asks for at least one value in a
  slice (an IPS section a summary must have, say) now fails a resource that has none, and one that
  allows a slice once fails a resource that has it twice. A slice that is told apart by a profile
  is still not checked.
- **A health card's `iss` is an address a verifier can reach.** The server refuses to start with a
  signing key on a non-loopback address when no `public_url` is set, and says so when `public_url`
  is not `https`. `credentialType` takes the clinical types the summary holds, and no other.
- **`Patient/$summary` and `$health-cards-issue` read a bounded part of a record.** Each kind is
  read newest first, up to 2,000 resources, and a section holds the 100 most recently written.
- **`$document` for a token tied to one patient holds nothing of another patient.** A reference
  that names a host (`http://other.example/Patient/1`) is no longer taken as a resource of this
  server.
- **A date search no longer misses a time because of how the time is written.** The index was
  read in the order of the text, which is not the order of time: `ge2026-10-18T04:45:53Z` found
  nothing where a time was stored as `...:53.065Z` (a `.` sorts before a `Z`), and a time in another
  zone was cut off the same way. A date search now reads every day it could reach, and a date
  written to the year or the month is read by its own key, and the comparison above decides.
- **A resource inside a Bundle entry is validated as the resource it is.** `entry.resource` (and
  any other element that holds a whole resource, such as a Parameters part) was checked as the
  abstract `Resource`, so every element of every entry was "unknown" and a Bundle with entries
  could not be validated. It is now checked against its own type, the profiles the entry's slice
  names and the ones it states in `meta.profile`. HL7's own example IPS summaries now validate.
- **A narrative is checked, not refused.** `txt-1` and `txt-2` use FHIR's `htmlChecks()`, which
  the server did not implement, so `$validate` of any resource with a narrative failed with
  "unsupported function htmlChecks". It now checks that the narrative is one `div` in the XHTML
  namespace with only the basic formatting elements, no script and some text.
- **A complex extension is validated by the values inside it,** as is anything else a profile
  slices: the `value[x]` of a part of an extension (the `lang` and `content` of a translation)
  was looked for under that literal name and reported missing. Slices of an element that sits
  inside a slice of its parent (each section of a Composition slicing its `entry`) are now told
  apart, and no longer applied to every value of the element.
- **`$document` returns a valid document.** It has an `identifier`, a `timestamp` and a `urn:uuid`
  `fullUrl` for each entry, references that point at them, and holds what the Composition's
  resources reference as well as what it references itself, each once. A document of more than
  1000 resources is a `422`.
- **`$everything` and `$document` hold only what the caller's scopes let them read.** They used to
  read every kind of resource of the compartment or the document regardless of the token.
- The built-in Patient compartment holds `MedicationStatement` and `Immunization`, for a server
  with no `CompartmentDefinition` installed.

### Fixed

- **A reference inside a choice element is checked like every other.** `--references strict` (the
  default) refused a missing target for `Invoice.account` or `Observation.focus`, and saved
  `Invoice.lineItem.chargeItemReference` = `ChargeItem/nope` and `ChargeItem.productReference` =
  `Medication/nope`, because an element that FHIR lets hold a reference or something else
  (`xxx[x]`) was never looked at. Every such element is now checked: the target must exist (or, in a
  transaction Bundle, be named by the `fullUrl` of an entry), and be a type the element allows. As
  everywhere else under `strict`, a reference to a contained resource (`#id`) is refused, so a
  `MedicationRequest` whose `medicationReference` is `#med1` is now refused too. This is every
  such element of every resource, not only the billing ones: a `Task` input, a `Communication`
  payload, a `QuestionnaireResponse` answer or a `Parameters` value that is a reference must name
  something on this server as well.
  `--references fhir` stays lenient. A resource that already holds a dangling reference of this
  kind is not changed, but a later update that keeps it is refused. See
  [FHIR API](docs/fhir-api.md#reference-checking).


## [0.0.2] - 2026-10-19

### Added

- **A first start installs prepared packages, with no registry and little memory.** Release archives
  and container images now carry the release's FHIR packages (R4 core, SDC and what they depend on)
  in a file, `refhir-packages.bundle`, that is made when they are built: the packages are
  downloaded, read and expanded once, there. A server finds the file beside its binary (or in
  `../share/refhir/` from it, or at `[conformance].package_bundle` / `REFHIR_PACKAGE_BUNDLE`; `off`
  says not to look) and stores what is in it, so the first start needs no network and no package
  cache, and a first start peaks at a small fraction of the memory it did. A file that is there and
  cannot be used (damaged, or made for another release) stops the start with an error that says so.
  `refhir package-bundle` makes the file for a binary you built yourself, or for other packages
  (`--spec`). See [Preparing the packages](deploy/production.md#preparing-the-packages).
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
- **Many tenants in one Postgres database.** A process started with `--tenant NAME` (or
  `REFHIR_TENANT`, or `tenant` under `[database]`) serves one tenant of a database that many tenants
  share. Each tenant is a separate FHIR server: its own resources, SMART clients, audit trail,
  writer, and standbys and replicas. `refhir tenant create | list | delete` manages them, and a
  process whose tenant the database does not have refuses to start. The first `create` makes an
  empty database a shared one, and a database keeps the layout it was made with: a database that
  already holds a ReFHIR without tenants cannot hold them, and the reverse. **Nothing changes
  without `--tenant`**: the same tables, migrations and SQL as before.
  - **The packages tenants have in common are stored once.** The FHIR core and what it depends on
    are read from a prepared file (see above) into the database one time, and each tenant is given
    them, with its own search parameters and its own value set expansions. Packages installed from a
    registry or a directory, and everything a tenant writes, stay the tenant's own.
  - **The database keeps each tenant to its own rows.** Every table that holds tenants' rows has
    row-level security, enabled and forced, so a connection sees and writes only the tenant it
    serves. A superuser and a role with `BYPASSRLS` are never bound by it, so a process that connects
    as one works and logs a warning. Take backups with such a role: `pg_dump` run as the ordinary
    role refuses the database, and `--enable-row-security` dumps no tenant's rows.
  - **A replica cannot write into the wrong tenant.** It sends its tenant with everything it
    forwards, over REST and over `/sync`, and a primary of another tenant answers `421`.
    `/healthz` names the tenant, `refhir migrate --status` says when a database is shared, and a
    tenant's package cache and export directory default to a folder named for it. The Helm chart
    takes `database.tenant` (one release per tenant).
  - **A tenant announces its commits to its replicas at most every 20 ms.** Every tenant's replicas
    listen in one database, so an announcement for each commit woke all of them and made busy
    tenants slow each other down. The first commit after a quiet spell is announced with the commit
    itself, and a burst ends with one announcement after its last commit. A database without tenants
    announces every commit as before.

  Each tenant adds a writer, so give the Postgres server room for all of them together. See
  [Tenants on one database](docs/clustering.md#tenants-on-one-database).

- **`_sort=near`, and the distance of each place `near` finds.** `Location?near=6.45|3.40|10|km&_sort=near`
  returns the places nearest first (`-near`: farthest first, places the same distance away by id),
  and every place a `near` search finds carries its distance in `entry.search.extension`, as the
  FHIR `location-distance` extension (`valueDistance`, in kilometres, or miles when the search said
  `mi`). Paging keeps the order across `next` links. A search that cannot be measured from one
  point (no `near`, two of them, or a comma's alternatives) is refused as any other `_sort` that
  cannot be applied (see Changed). See [FHIR API](docs/fhir-api.md#search).

### Changed

- **`near` reads the places round the point, not every place.** The search used to read and
  measure every Location, so its time grew with the table: `Location?near=6.60|3.35|10|km&_count=20`
  took 0.4 s over 40,000 Locations and 0.7 s for 50 km. `Location.position` now has an index (a
  Z-order curve over latitude and longitude, in the byte-ordered keys SQLite and Postgres both
  hold, so no extension is needed), a search reads the few ranges of it that cover the circle, and
  the places found are measured exactly as before (a search that also names an `identifier` reads
  that identifier and only checks the circle, as it always read it). Over 40,000 Locations the same searches take
  5 ms on SQLite and 16 ms on Postgres (a 50 km search that finds 209 places, 14 ms and 42 ms).
  On Postgres the ranges are read in one statement, not one for each: a search sent 33 statements
  and now sends 2, and any search that is checked row by row over several index ranges gets the
  same. What a search finds is unchanged: the distance is still measured with the same formula, and
  a place exactly at the distance is still in. **Schema migration 3 (4 in the shared tenant layout)
  makes the first start after upgrading build the index** for the places already stored: it puts
  `near` back to pending and queues a backfill, which the server runs as it starts, before it
  answers requests. That takes about 5 seconds for 40,000 places on SQLite and about 3 minutes on
  Postgres, which commits 16 places at a time. Until it is done `near` answers `400` (unsupported
  search parameter) on a replica, and a replica waits for the migration as for any other. Back the
  database up first, as for any migration ([Schema migrations](docs/migrations.md)); there is no
  going back to an earlier release afterwards.
- **`_has` is answered for the places the rest of the search leaves, and `_include` and `_revinclude`
  are read for the whole page at once.** `Location?near=…&_has:OrganizationAffiliation:location:network=…` ran the
  `OrganizationAffiliation` search over every affiliation in the database, built a Bundle of all of
  them and read their references, whatever few places `near` had left: with 10,000 places it took
  10 to 30 seconds, the same for a circle of 6 places as of 223, and the directory search that adds a
  second `_has`, an `_include` and two `_revinclude` took about twenty. When the other parameters
  leave up to a few thousand resources, each is now looked up in the index of the reference
  (`OrganizationAffiliation.location`), and what refers to it is checked against `network=…`, so the
  cost follows the places near and not the size of the database. An `_include` and a `_revinclude` read
  what their rules lead to for every match of the page in one request to the database for each
  rule, where each used one for each match. SQLite and Postgres answer such a read in one statement for each 200 and 50 index ranges
  instead of one each. The answers are the same: a search that leaves many resources, a `_has` that is
  not a plain parameter of the type (a chain, `_id`, a parameter the type lacks), and a server run with
  `--references fhir` (where a reference can be a URL that an index lookup would not find) take the
  search of the whole type, as before. **Numbers.** A directory of 10,000 places (and as many
  organizations, services and affiliations) in a SQLite file, the releases before and after started
  on copies of it, one request at a time on a busy machine (load average 90 to 240, so read the
  ratios): `Location?near=6.6|3.35|100|km&_sort=near&_count=100` with a `_has` on the service
  category took 4.5 s and now takes 52 ms; with a `_has` on the network, 8.7 s and now 61 ms; a 5 km
  circle with the `_has` on the network, 10.0 s and now 3 ms; and the directory search (`near`, both
  `_has`, an `_include` and two `_revinclude`), 15.0 s and now 199 ms, its CPU time falling from 4.2 s
  to 0.15 s. Every Bundle is identical entry for entry. On Postgres 16 in a local container, with 2,000 places and the same
  data for both, one request at a time on a machine at load about 15, the same searches went from
  145 ms to 12 ms, 161 ms to 13 ms, and 446 ms to 38 ms for the directory search (the statements that
  read many ranges are prepared once for each connection, so they are planned a few times and not
  on every request). A `_has` that nothing else narrows still costs what the whole type costs,
  about 9 s at 10,000 places on SQLite: it is asked of its places one at a time only while that is
  25 statements or fewer (5,000 places on SQLite, 1,250 on Postgres), and past that is answered as
  before.

- **A control the server cannot apply is refused, not echoed.** `Location?_sort=bogus` answered
  `200` and its `self` link still said `_sort=bogus`, as if the order had been applied, though FHIR
  says that link holds the parameters the server used. The same held for `_sort` by a parameter
  the server cannot order by (a number, a quantity, an age), `_summary` or `_total` with a value
  they do not take, and an `_include` or `_revinclude` naming a resource type or parameter that
  does not exist or is not a reference. Each is now a `400` that names it, unless the request has
  `Prefer: handling=lenient`, which runs the search without it: it is not in the `self` or `next`
  link, and the `OperationOutcome` entry says what was left out. `_sort=_id` sorts by the resource
  id. **A client that sent such a term and relied on its being ignored now gets a `400`:** remove
  it, or send `Prefer: handling=lenient`. See [FHIR API](docs/fhir-api.md#search).

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
- **A request that cannot read a definition or a code system from the database fails.** With the
  reads above, a database error while reading a profile, an extension or a code system used to be
  logged and the request answered as if it did not exist: a write refused as invalid, or one
  accepted because nothing was left to check it by. It is now a `500` (`exception`) that names what
  could not be read, nothing is committed, and the same request is answered once the database is.
- **A first start stores each package as it reads it.** The prepared file (see below) holds a
  package as values of about 4 MB of rows, and the database takes them one at a time inside one
  transaction, so a package is stored whole or not at all, as before. The largest default package
  is over 120 MB of JSON, and a first start used to hold all of it; it now holds a few megabytes
  of it at a time, on SQLite and on Postgres. On SQLite the heap peaks at about 42 MB where it
  peaked at about 129 MB (resident memory 120 MB where it was 300 MB); on Postgres at about
  51 MB where it peaked at about 164 MB. Postgres keeps a buffer as large as the biggest
  statement a connection has sent for as long as the connection lives, which after storing the
  default packages would be about 16 MB for the life of the server, so a connection that stored
  a large package is closed afterwards and the pool opens another. A prepared file made by an
  earlier build of this version has another format and is refused with an error that says so:
  make it again with `refhir package-bundle`.
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
  nothing. It is left out when a package only depends on it, with a line in the log. Name it in
  `[conformance].packages` (or give it to `$install`) to install it, and a database that has it keeps
  it. The one visible difference is which package holds the value sets that core and the examples
  package both define: core, where it was the examples package, with the same codes in each
  expansion.
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
- **A first start installs the default packages more than twice as fast on PostgreSQL.** The
  time went to round trips and commits, not to work: every definition, search parameter and
  piece of terminology was a statement of its own, every resource type a package adds search
  parameters to cost four committed transactions of bookkeeping (even with no resource stored to
  index), the catalog was rebuilt a second time only to mark those parameters ready, and the
  packages one install needs were downloaded and unpacked one after another. A package now goes
  in a few statements per table, the bookkeeping is queued and finished in one step per package
  (a type with no stored resources is not scanned), only the search parameters are recompiled
  after it, and the packages a dependency walk needs at the same time are fetched side by side.
  The log has a new line, `fetched N package(s) to install in …`, for the part before the first
  package is stored. With the default R4 packages from a local mirror (so no download time) the
  start after the lease took about 70 s on PostgreSQL and about 25 s now, and on SQLite about
  26 s and about 24 s; peak memory did not rise. A database whose packages are already installed
  starts as before.
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
- **A replica and a standby now follow terminology written through the API.** Creating, updating
  or deleting a CodeSystem, ValueSet or ConceptMap changed the catalog of the process that took
  the write and told no other. A replica went on validating and expanding against the
  terminology it loaded at start, and a standby took over with it, until something else (a package
  install, a SearchParameter write) happened to make them reload; a restart also fixed it. The
  write now tells them, and each reloads the terminology within a moment of it.
  - **What a replica does now.** It reloads only the terminology (the value sets, code systems and
    concept maps and the expansions kept for them) in the background, and answers from the old
    one until the new one is ready. It does not read the structure definitions or the search
    parameters, so it takes a fraction of a full reload: about a third of the time on the R4
    package alone, and less saved where a lot of terminology is installed. A package install or a
    SearchParameter write still reloads everything. A script that writes many code systems or
    value sets one by one keeps every replica reloading until it stops; send them in one
    `transaction` Bundle, or install them as a package, to have each reload once.
  - **A running backfill does not hold it back.** A replica still never loads a search parameter
    whose index is incomplete, and a terminology reload loads none, so the replica hears of a
    terminology write at once even while a backfill is running.
  - **Upgrading.** There is no migration: the change is a new counter, `terminology_version`, in
    the `persistence_globals` table, created by the first write. It takes effect when the
    primary, the replicas and the standbys all run this version: an older replica or standby does
    not follow it, and an older primary does not move it, so those behave as before.
- **A write whose announcement failed is announced again, and a primary announces when it
  starts.** When the server has committed a CodeSystem, ValueSet, ConceptMap or SearchParameter
  and then fails to tell the other processes (the counter write after the tables were in order),
  the repair that follows used to find nothing to fix and say nothing, so replicas and standbys
  stayed on the old catalog until the next change. The repair now tells them again, and they
  reload everything, unless it published, which tells them. The request succeeds either way,
  because the write is committed and right: a failure to tell the others is logged, and they
  catch up at the next change to the catalog or at their next start. The repair that runs as a
  primary starts does the same, so a primary that stopped between writing and telling is made up
  for. The cost is that every replica and standby loads the whole catalog (a few seconds of one
  thread each) when a primary starts or a standby takes over.
- **Claiming jobs on Postgres can no longer take more than was asked for.** The statement that
  picks the waiting jobs (`$export` and the other durable jobs) let the planner run its locking
  subquery again for each row it updated, skipping the rows it had just locked, so one claim could
  take more jobs than the worker asked for. It now picks once, whatever the plan.

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

[Unreleased]: https://github.com/hai-hq/refhir/compare/v0.0.4...HEAD
[0.0.4]: https://github.com/hai-hq/refhir/compare/v0.0.3...v0.0.4
[0.0.3]: https://github.com/hai-hq/refhir/compare/v0.0.2...v0.0.3
[0.0.2]: https://github.com/hai-hq/refhir/compare/v0.0.1...v0.0.2
[0.0.1]: https://github.com/hai-hq/refhir/releases/tag/v0.0.1
