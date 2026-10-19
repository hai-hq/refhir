# FHIR API

ReFHIR serves FHIR R4 (4.0.1). Request and response bodies are `application/fhir+json`; a read
can also come back as XML with `_format=xml` or `Accept: application/fhir+xml`. Errors are
`OperationOutcome` resources with a 4xx or 5xx status. `GET /metadata` returns the
CapabilityStatement, built from what is installed, so it is the authority on what this server
supports.

Resource types, search parameters and profiles come from the FHIR packages the server has
installed (see [Configuration](configuration.md#configuration-file)), not from a fixed list. With
`hl7.fhir.r4.core` that is every R4 resource type (146 of them).

## Interactions

Every resource type gets the same set:

| Level | Interactions |
|-------|--------------|
| Instance | `read`, `vread`, `update`, `patch` (JSON Patch), `delete`, `history` |
| Type | `create`, `search`, `history`; conditional create, update and delete (single match) |
| System | `transaction` and `batch` (`POST /`), `history`, `search` |

Every resource is versioned, and the server sets `meta.versionId` to the commit timestamp of the
write. A version conflict (`If-Match`) is a `409`, after the server has retried optimistic
concurrency conflicts. A `transaction` Bundle is one atomic commit, which is what lets
[live queries](sync-protocol.md) move from one consistent snapshot to the next.

`PUT` does not create a resource under an id the client chooses unless you opt in; see
[Resource ids](#resource-ids).

## Search

- The full R4 `SearchParameter` set is compiled into indexes. Parameters from other installed
  packages, and `SearchParameter` resources you write, are added and backfilled in the
  background.
- Chaining (`Observation?subject:Patient.name=ann`), reverse chaining (`_has`), `_include` and
  `_revinclude`, and compartment search (`GET /Patient/123/Observation`) all work. Name the target
  type in a chain when a reference parameter can point at more than one type.
- A reference parameter takes `Type/id` or just the id: `Observation?patient=Patient/123` and
  `Observation?patient=123` return the same resources. A bare id is looked up as a reference to
  each type the parameter can point at in that resource: the types its `SearchParameter` lists as
  `target`, limited to those the element it reads allows. So `patient=123` finds `Patient/123`,
  and `subject=123`, which can point at a Patient, a Group, a Device or a Location, finds a
  reference to any of them with that id. Write `Type/id` to name the type. An absolute URL is
  compared as written, so a bare id never matches a reference that is stored as a URL. A parameter
  that can point at more than 16 types in the resource (`Task?subject` points at any resource) or
  lists no `target` cannot place a bare id: that search, and a `_filter` comparison, is a `400`
  that asks for `Type/id`. In a comma list each id is read this way, and an id counts once for
  every type it stands for toward the 1000 index ranges a search may read. A
  [live query](sync-protocol.md) re-runs the same search, so it reads ids the same way.
- A comma between the values of one parameter is OR, for every kind of parameter:
  `Slot?status=free,busy`, `HealthcareService?_id=a,b`. Write `\,` for a comma that is part of a
  value. A value lists at most 1000 alternatives. With `:not` and `:not-in`, a list leaves out
  every value in it, and `:missing` takes one value.
- Giving a parameter twice is AND (`date=ge2026-01-01&date=lt2026-02-01`) in a query string, in the
  URL of a Bundle entry and in Subscription criteria. Each occurrence is judged against the
  resource on its own, so a resource that repeats an element can satisfy both with different
  elements: `Schedule?actor=HealthcareService/h1&actor=Location/l1` finds a Schedule whose `actor`
  list holds both, and the same through a chain
  (`Slot?schedule.actor=HealthcareService/h1&schedule.actor=Location/l1`) or on a nested list
  (`Appointment?actor=Patient/p1&actor=Location/l1`). The same value twice is the same as once, and
  AND and OR combine: `actor=Location/l1,Location/l2&actor=HealthcareService/h1`. A live query's
  arguments are a JSON object, so it gives the occurrences as an array:
  `{"schedule.actor": ["HealthcareService/h1", "Location/l1"]}`. A GraphQL argument has one value
  per name and cannot say it.
- **A search that names no `_count` returns one page of 100 matches** with a `next` link, instead
  of every match in one response. The `self` link shows the `_count` that applied, and `total`
  still counts every match. A `_count` in the request is used as it is, and `_count=0` returns the
  total alone. Follow `next` (or ask for a larger `_count`) to read the rest. Set
  `[server].default_page_size` or `REFHIR_DEFAULT_PAGE_SIZE` to change the page, or to `0` to
  return every match as a search used to. Only a search a client sends with `GET` is paged this way:
  a conditional update or delete, the searches behind a chain, `_has` or `_include`, a live query,
  GraphQL, `$export` and the entries of a Bundle read every match. Without `_sort`, a page is in
  index order, which is not an order a client can rely on: send `_sort` for a stable one.
- `_sort` with a small `_count` costs about what the same search costs unsorted: it reads each
  match once and keeps only the first page. The time follows the number of matches the filters
  leave to be read, so a parameter whose index decides the match (`status`, `schedule`) keeps a
  search small, and one that has to be checked against each document (a date range) costs a little
  for every Slot it is checked against.
- `_include` and `_revinclude` take `Type:param` or `Type:param:TargetType`. The `:iterate` forms
  (`_include=Slot:schedule&_include:iterate=Schedule:actor`) apply a rule to the matches and to
  every resource the rules have added, until none is added. A search that would add more than
  1000 resources that way is a `400`; name the parameters to follow instead of `*`, or search for
  fewer resources.
- Under a patient-scoped token, `_include` and `_revinclude` leave out the resources of other
  patients, as a search does. Resource types that are in no patient's compartment (a Practitioner,
  an Organization) are still included.
- `_revinclude=*` is a `400`: name the parameter that points at the match. The escapes `\$` and
  `\|` in a search value are not interpreted.
- An unknown search parameter is a `400`, not silently ignored.
- **A value that cannot be read as its parameter's type is a `400`, not an empty result.**
  `GET /Patient?birthdate=notadate` answers `invalid value "notadate" for search parameter
  birthdate on Patient: expected a date such as 2020, 2020-01, 2020-01-31 or
  2020-01-31T10:30:00Z`, with the prefixes it takes (`eq`, `ne`, `gt`, `lt`, `ge`, `le`, `sa`,
  `eb`, `ap`), in an `OperationOutcome`. It applies to a date, number or quantity that does not
  parse (a day that does not exist, such as `1990-02-30`, included), a composite with the wrong
  number of `$` parts, a `near` that is not `latitude|longitude`, `:missing` with anything but
  `true` or `false`, and `:of-type` without exactly `system|code|value`, and to the same
  parameters behind a chain or `_has`, and in the comparisons of a `_filter` (which also refuses a
  parameter that does not exist). A string, token, uri or reference is free text, so any value
  reads for it. The check looks at the value alone, so it gives the same answer on an empty
  database, and a [live query](sync-protocol.md) with such a value fails with `QueryFailed` at once
  instead of staying empty. Conditional create, update and delete, which search the same way,
  refuse it too, and so does a query in a Questionnaire that `$populate` runs: it fails with this
  message instead of populating from nothing.
  Each alternative of a comma list is read on its own (`birthdate=1990,notadate` names
  `notadate`), and so is each occurrence of a parameter given twice. Write a `+` in a time zone as
  `%2B` in a URL, or it reads as a space.
- **`Prefer: handling=lenient`** on a search `GET` asks the server to leave such a parameter out
  and run the search on the rest, as FHIR allows. The Bundle then holds an `OperationOutcome` entry
  (`search.mode` is `outcome`, and it is not counted in `total`) with a warning for each parameter
  set aside, and its `self` link no longer has them. A value with an alternative it cannot read is
  set aside whole, and of a parameter given twice only the occurrence that cannot be read goes.
  With `_summary=count`, which has no entries, the `self` link is the only sign. A `_filter` with
  a value it cannot read is set aside whole. The
  response carries `Preference-Applied: handling=lenient`, and every search response has `Vary:
  Prefer`, since the answer depends on it. `handling=strict` is the default, and any other value of
  `handling` is read as that. Lenient is for the value of a search parameter only: an unknown
  parameter name, a bare id that a reference parameter cannot place, a `_filter` that does not
  parse, `_count`, `_offset` and `_cursor` are a `400` either way.
  It is not available to a conditional create, update or delete (a clause set aside widens what
  they match, and a conditional delete must not delete a different resource than the one the
  client described), nor to a `batch` or `transaction` entry, GraphQL or a live query. A
  `_handling` in the query string does nothing.

## Operations

| Operation | Where | Notes |
|-----------|-------|-------|
| `$everything` | `Patient/[id]` | |
| `$export` | system, type, instance | Asynchronous (`202`, then `GET /export/{job}` and `GET /export/{job}/file`). Jobs are durable; see [Operations](operations.md#export-jobs) |
| `$validate` | type, instance | Validates against the installed StructureDefinitions |
| `$meta`, `$meta-add`, `$meta-delete` | instance | |
| `$document` | `Composition/[id]` | |
| `$expand`, `$lookup`, `$validate-code`, `$subsumes`, `$translate` | `ValueSet`, `CodeSystem`, `ConceptMap` | See [Terminology](#terminology). `ValueSet/$expand` also answers a `GET` |
| `$expandability` | `ValueSet` | Which value sets cannot be expanded, and why |
| `$install` | `ImplementationGuide` | Install a package at runtime. With SMART it needs `system/*.write` |
| `$populate`, `$extract`, `$assemble`, `$next-question`, `$behavior` | `Questionnaire`, `QuestionnaireResponse` | Structured Data Capture 4.0.0 |

Beyond REST:

- **GraphQL** at `POST /graphql`.
- **Subscriptions.** `Subscription` resources with a `rest-hook` or `websocket` channel are
  delivered by the primary; the WebSocket channel is `GET /[type]/[id]/$websocket`. See
  [Subscriptions](#subscriptions) for what is and is not notified.
- **Live queries** over [`/sync`](sync-protocol.md).
- **Governance endpoints** `/_usage`, `/_requests`, `/_audit` and `/_audit/verify`; see
  [governance](governance.md).
- **Health probes** `/livez`, `/readyz` and `/healthz`; see [Operations](operations.md#health-probes).

## Subscriptions

`Subscription` is the R4 resource: a `criteria` string such as `Patient?active=true`, and a
`channel` of type `rest-hook` or `websocket` (`email` and `sms` are not delivered). Only the
primary delivers, so a replica neither sends notifications nor serves `$websocket`
([clustering](clustering.md)).

**What is notified.** A *create or update* of a resource of the criteria's type that matches its
query. The query is run against the new version, so, as R4 defines it:

- A **delete** sends nothing.
- A resource that **stops matching** (`Patient?active=true` after `active` becomes `false`) sends
  nothing, so a receiver cannot learn from a Subscription that a resource left. A
  [`/sync` live query](sync-protocol.md) reports a result gaining and losing members, and is the
  tool for that.
- A Subscription whose `status` is `off` is skipped. Any other status delivers, `requested` and
  `error` included. The server does not move `requested` to `active` when it accepts one.
- A transaction Bundle notifies once for each matching resource it wrote.

**`rest-hook`.** Each notification is a `POST` to `channel.endpoint` with
`Content-Type: application/fhir+json` (unless a `channel.header` entry sets another, in any
case) and the full resource as the body. Any `2xx` is delivery. Redirects are not followed: a
`3xx` is a failed delivery. A `rest-hook` channel with no endpoint fails the same way.

When the resource is itself a Subscription (a Subscription whose `criteria` are `Subscription` is
told about every one that is written), its `channel.header` entries are left out of the body: they
are credentials for its own receiver. Its `endpoint` is not, so do not put a secret in the URL.

`channel.header` is honored. Each entry is `Name: value` (for example
`Authorization: Bearer 7f3c…`) and is added to the request. A name can be given once. An entry
that is not a valid header, that repeats a name, or that sets `Host`, `Content-Length`,
`Transfer-Encoding`, `Connection`, `Keep-Alive`, `Upgrade`, `TE`, `Trailer`, `Expect`, `Cookie` or
a `Proxy-` header, makes the notification fail at once, with no request and no retry, and the
Subscription goes to `error` as described below. Header values are stored in the Subscription
like any other element, so anyone who can read the Subscription (or its history) can read them:
use a credential that is good only for that receiver. Whoever may write a Subscription also
chooses the URL the server posts to and the headers it sends, and nothing limits where an
endpoint may point (loopback and private addresses included), so grant that write as carefully as
you would `$install`.

`channel.payload` and `end` are **ignored**. The body is always the full resource as JSON, an
absent `payload` does not make it empty as R4 says it should, and a Subscription does not expire.

**Failures, retries and `status`.** A failed notification is tried three times a moment apart,
then again each time the retry queue runs once the notification is due, which is at least a second
and a half after its last attempt. The queue runs on every tick of a two second timer and after
each commit, so a burst of commits does not use the attempts up. That is eight attempts in all,
about ten seconds on a quiet server and never much less than seven. Before each retry the
Subscription is read again: if it has been deleted, is `off` or sends somewhere else, the
notification is dropped, and a header that was changed is the one sent. After the eighth attempt
the notification is dropped, and the Subscription is updated as R4 describes:

- `status` becomes `error` and `error` holds the time and what went wrong, at most 500
  characters: after eight failed attempts, the attempts and the last failure; for a header that
  cannot be sent, or a missing endpoint, which one.
- This is written when the status changes, not for each failed notification. An endpoint that
  stays down leaves `error` as it was first written, and costs one new version.
- Delivery goes on while the status is `error`. The next notification, or retry, that gets through
  sets `status` back to `active` and removes `error`. Set `status` to `off` to stop delivery. The
  server never changes an `off` Subscription.
- The change is an ordinary update: a new version in `_history`, and a wake-up for `/sync`
  queries over `Subscription`. It is made by the server with no request behind it, so it is not an
  `AuditEvent` (see [governance](governance.md#rate-limits-quotas-metering-and-audit)). It is
  decided on what is stored when the notification ends, not on what was stored when it was made,
  and is made only if `status` and `channel.endpoint` are still what it was decided on, so an
  edit by a client in the meantime is not overwritten.
- A Subscription whose criteria match `Subscription` is notified of this write like any other
  change. That cannot start a chain, because a write follows only a delivery result that
  disagrees with the stored `status`.

Pending retries are held **in memory** on the primary, so a restart drops them. The stored
`status` and `error` are in the database and survive it. (Only [`$export`](operations.md#export-jobs)
jobs are durable.)

**`websocket`.** `GET /{type}/{id}/$websocket` upgrades to a WebSocket, where `{id}` is the id of
a Subscription with a `websocket` channel. For each matching create or update the server sends the
resource as a raw JSON text frame. There is no handshake (R4's `bind` and `bound` messages are not
implemented) and frames from the client are ignored. The route does not check that `{id}` names a
Subscription or that `{type}` is right, and keeps one socket per id: a second connection for the
same id replaces the first, which is closed. A notification made while no socket is connected is
lost. `status` never changes for this channel, since there is no delivery to fail.

R5 topic-based Subscriptions (`SubscriptionTopic`) are not supported.

## Reference checking

`--references` (or `reference_policy` under `[conformance]`) chooses how a reference is checked when a resource
is written:

| Policy | Rule |
|--------|------|
| `strict` (default) | The reference must be a relative `Type/id`, and its target must exist in the snapshot the write sees. Inside a transaction Bundle, an entry may refer to another entry by `fullUrl` |
| `fhir` | Absolute URLs and targets that are not on this server are allowed. A type outside the element's `targetProfile` is still rejected |

## Resource ids

The server assigns the id of every resource it creates. `POST [type]` ignores an `id`, and a `meta.versionId` or `meta.lastUpdated`, in the body (FHIR R4 create) and answers `201` with the new id in `Location`. GraphQL and `/sync` `create` do the same.

- **`PUT [type]/[id]`** updates a resource that exists. For an id that does not, it answers `405` with an `OperationOutcome`, because update-as-create is off. `GET /metadata` says so (`updateCreate: false` on every resource), and also reports the conditional interactions that are implemented (`conditionalCreate`, `conditionalUpdate`, `conditionalDelete: single`).
- **Transactions** give entries that create resources their ids up front. Entries that refer to one another name each other by `fullUrl`, as FHIR defines: give the creating entry `"fullUrl": "urn:uuid:..."` and use that string in the references. A `Patient/p1` written in the bundle by hand does not name the resource the server creates.
- **Update-as-create** is a deployment choice in FHIR. Start with `--update-as-create` (or `update_as_create = true` under `[conformance]`) to let `PUT [type]/[id]` create the resource under that id (`201`); `/metadata` then reports `updateCreate: true`. Data that has ids of its own, such as the demo data, is loaded this way. The environment variable `REFHIR_UPDATE_AS_CREATE=true` turns it on too. With SMART enabled, a `PUT` then needs the `create` permission as well as `update`, since it may create. An SDC `$extract` whose templates name a `resourceId` (or an `id`) returns those entries as `PUT [type]/[id]`, so the bundle it returns needs this setting to be accepted by a server that has not got it, which answers each such entry with `405`.

## Terminology

`ValueSet/$expand` takes `url`, and optionally `filter`, `count` and `offset`, as a `POST` (a JSON body or a Parameters resource) or as a `GET` with the same names in the query string:

```bash
curl 'http://localhost:8080/ValueSet/$expand?url=http://hl7.org/fhir/ValueSet/organization-type&filter=hospital&count=10'
```

It returns the expansion, or `422` with an `OperationOutcome` when the value set cannot be expanded. Each cause is one issue, with the failing part of the value set in `expression` (for example `ValueSet.compose.include[1]`) and the code system, its `content` mode and the filter in `diagnostics`. The usual causes are a code system that is not installed, one that holds only part of its content (`fragment`, or `not-present`, as the R4 SNOMED CT stub does), or a filter this server does not evaluate. A partial expansion is never returned without saying so, because a client cannot tell it from a complete one.

One partial case is expanded, and says so. A code system whose `content` is `example` holds a few representative concepts, and HL7 publishes value sets that are nothing but a whole-system include of one (`service-category` and `service-type`, which offer choices for `HealthcareService`). Such a value set expands to the concepts the server holds for the system, and the expansion names each system that is only a sample in an `expansion.parameter` called `<system>|<version>#CodeSystem.content`, as R4 describes, with the content mode as its value:

```json
"expansion": {
  "parameter": [{
    "name": "http://terminology.hl7.org/CodeSystem/service-category|4.0.1#CodeSystem.content",
    "valueCode": "example"
  }],
  "total": 38, "offset": 0, "contains": ["…"]
}
```

A client that needs a closed list checks for such a parameter. Only an include of the whole system is expanded this way: a filter on a sample, an exclusion of one, a sample that holds no concepts, and a `fragment` are still refused. `$validate-code` against such a value set answers from the same list, with `result` false and a `message` that says the list is only a sample for a code outside it. A resource whose binding is to one is not rejected for a code the sample lacks: the validator reports that the code cannot be checked.

HL7 publishes `ValueSet/currencies` (the binding of `Money.currency`) as an include of ISO 4217 and no CodeSystem for it, so ReFHIR installs one itself at start: the active alphabetic currency codes (178, with their numeric codes and English names, for example `NGN` Nigerian Naira). `$expand` of `currencies` lists them, `filter=naira` finds `NGN`, `$validate-code` accepts a real code and refuses an unknown one, and a `Money` with an unknown `currency` is refused by `$validate` and on write. It is installed once, as the package `refhir.builtin.terminology`, and is not installed when the release's default packages are turned off (`REFHIR_SKIP_DEFAULT_PACKAGES`).

A code system can be installed in several versions, and each is kept. Name a version with `system|version` (or `include.version`) and the expansion uses exactly that one. A version that is not installed is refused, and the error lists the versions that are. With no version named, a value set from a package uses the versions its own package holds, and a value set you write through the API, or one whose package does not hold the system, uses the version of the FHIR release the server serves (`4.0.1` for R4) when it is installed, and the latest installed version otherwise. The expansion records the version it read. `$lookup`, `$subsumes` and `CodeSystem/$validate-code` take a `version` parameter.

`POST /ValueSet/$expandability` reports, for the value sets named in `url` (a string, an array, or `url` parameters of a Parameters resource), whether each can be expanded and why not; with none named it lists every loaded value set that cannot. Use it as a deployment check for the value sets a UI depends on. `POST /ImplementationGuide/$install` adds a warning to its response for any value set the package made unexpandable, and the server logs how many there are at start-up.
