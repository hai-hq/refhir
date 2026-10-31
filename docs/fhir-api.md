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
write, and `meta.lastUpdated` to the time it received it (see [When a resource was
written](#when-a-resource-was-written)). A version conflict (`If-Match`) is a `409`, after the server has retried optimistic
concurrency conflicts. A `transaction` Bundle is one atomic commit, which is what lets
[live queries](sync-protocol.md) move from one consistent snapshot to the next.

`PUT` does not create a resource under an id the client chooses unless you opt in; see
[Resource ids](#resource-ids).

### When a resource was written

The server sets `meta.lastUpdated` on every version it writes, to the time it received the write: a
FHIR instant in UTC, to the millisecond (`2026-10-18T04:45:36.123Z`). A value in the body of a
create or an update is replaced, never kept. A patch, a change of tags with `$meta-add`, and each
entry of a transaction are writes like any other. Each version keeps its own time, so a `vread` or
an entry of a history shows when that version was written.

- **Search by it** with `_lastUpdated` and the usual prefixes:
  `GET [type]?_lastUpdated=ge2026-10-18T04:45:36Z`. Give it twice for a window:
  `_lastUpdated=ge2026-10-01&_lastUpdated=lt2026-11-01`.
- **List by it** with `_since` on a history: `GET [type]/_history?_since=2026-10-18T04:45:36Z`, or
  `GET /_history?_since=...` for the whole server, lists the versions written at or after that time.
  A value that is all digits is read as it always was, as a version of the server's log, so write
  a date with its month and day (`2026-01-01`), never as a year alone. A deletion
  has no time of its own, so the list starts at the last change before the time and holds any
  deletion since then: a deletion from just before the time may be listed, and one after it is never
  missed.
- **Cards** for what is new: `_since` on [`$health-cards-issue`](health-cards.md).

How a date and a time are compared, in this and in any other search by a date, is under [Searching
by a date or a time](#searching-by-a-date-or-a-time).

Versions written before the server kept times have none. A search by time does not find them, and a
history by time lists them only when the time is earlier than the first version that has one. A
resource gets its time the next time it is written.

## Search

- The full R4 `SearchParameter` set is compiled into indexes. Parameters from other installed
  packages, and `SearchParameter` resources you write, are added and backfilled in the
  background.
- Chaining (`Observation?subject:Patient.name=ann`), reverse chaining (`_has`), `_include` and
  `_revinclude`, and compartment search (`GET /Patient/123/Observation`) all work. Name the target
  type in a chain when a reference parameter can point at more than one type.
- **`_has` is answered for the resources the rest of the search leaves**, not for the whole type.
  `Location?near=6.6|3.35|5|km&_has:OrganizationAffiliation:location:network=Organization/n1`
  looks up what refers to each place `near` finds, in the index of `OrganizationAffiliation.location`,
  and checks `network` on those, so it costs what the places near cost and not what every
  affiliation in the database costs, however many there are. `_include` and `_revinclude` read what
  the rules lead to for every place of the page in one request to the database for each rule, not one
  for each place. A search that
  leaves a great many resources (a few thousand or more) asks the type once for everything that
  matches instead, and so does a server that takes references as URLs (`--references fhir`); the
  answer is the same either way.
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
- **`near` finds places by distance** (`Location?near=6.45|3.40|10|km`: latitude, longitude, then
  the distance and its units). The units are kilometres when they are left out (`mi` is miles),
  and a search that names no distance reaches 10. A place is found when it is no farther from the
  point than the distance, a place exactly that far included. `near` is indexed, so a search reads the
  places round the point and not every place, however many a directory holds. It combines with any
  other parameter, and with a chain (`HealthcareService?location.near=…`).
- **`_sort=near` orders the places by distance, nearest first** (`_sort=-near`: farthest first;
  places the same distance away go by id). It needs one `near` in the search to measure from, so
  it is an error with none, with two, or with a comma's alternatives. Paging keeps the order:
  `_count` and the `next` link continue where the page ended. Every place a `near` search finds
  carries its distance in the entry's `search.extension`, sorted or not, in the units the search
  used (kilometres, or miles for `mi`):

  ```json
  "search": {"mode": "match", "extension": [{
    "url": "http://hl7.org/fhir/StructureDefinition/location-distance",
    "valueDistance": {"value": 4.98, "unit": "km",
                      "system": "http://unitsofmeasure.org", "code": "km"}}]}
  ```
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
- **A control the server cannot apply is refused too, and never shown as if it had been.** The
  `self` link of a search holds what the server used. So `_sort=bogus` (no such parameter),
  `_sort` by a number, quantity or age parameter (the server orders by text, and by distance for
  `near`), `_summary` or `_total` with a value they do not take, and an `_include` or
  `_revinclude` that names a resource type or a parameter that does not exist, or one that is not a
  reference, are a `400` that names the term. With `Prefer: handling=lenient` the search runs
  without that term, which is not in the `self` or `next` links, and the Bundle's `OperationOutcome`
  entry says what was left out. `_sort=_id` sorts by the resource id.
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
  `handling` is read as that. Lenient is for the value of a search parameter and for the controls above only: an unknown
  parameter name, a bare id that a reference parameter cannot place, a `_filter` that does not
  parse, `_count`, `_offset` and `_cursor` are a `400` either way.
  It is not available to a conditional create, update or delete (a clause set aside widens what
  they match, and a conditional delete must not delete a different resource than the one the
  client described), nor to a `batch` or `transaction` entry, GraphQL or a live query. A
  `_handling` in the query string does nothing.

### Searching by a date or a time

A `date` parameter finds a resource by the moment it names, not by the text it was written as. That
is every parameter of the type: `date` on an `Encounter` or an `Observation`, `created` on a
`Claim`, a `ClaimResponse` and a `PaymentReconciliation`, `birthdate` on a `Patient`, and
`_lastUpdated`.

- **A time with a zone is a moment, whatever zone it is written in.** `2026-10-31T23:30:00+01:00`
  and `2026-10-31T22:30:00Z` are the same moment, so these two searches find the same resources, and
  both find an Encounter stored with either:

  ```
  GET /Encounter?date=ge2026-09-30T23:00:00Z&date=lt2026-10-31T23:00:00Z
  GET /Encounter?date=ge2026-10-01T00:00:00%2B01:00&date=lt2026-11-01T00:00:00%2B01:00
  ```

  Give `date` twice for a window (a parameter given twice is AND). Write the `+` of a zone as `%2B`
  in a URL: an unencoded `+` reads as a space, and the search is refused with a message that says
  so.
- **A value says how much it names.** `2026` is the year, `2026-10` the month, `2026-10-31` the
  day, `2026-10-31T22:30` the minute, `2026-10-31T22:30:00` the second and `2026-10-31T22:30:00.5`
  the tenth of a second. A stored value says the same of itself, so a stored `2026-10-31` is the
  whole of that day. Times are compared to the millisecond.
- **A date has no zone, and is read as UTC.** `2026-10-31` is the day that starts at
  `2026-10-31T00:00:00Z`. Ask for a day in another zone with its two times:
  `date=ge2026-11-01T00:00:00%2B01:00&date=lt2026-11-02T00:00:00%2B01:00`.
- **The prefixes compare ranges.** A value stands for a range of time (a year, a month, a day, a
  minute, a second), and so does each stored date, and a `Period` for the range between its ends.
  `eq`, the default, finds what lies wholly inside the value, and `ne` what does not: `eq2026-10`
  finds an Observation of `2026-10-31` and not one of `2026`, which reaches beyond the month.
  `gt` and `lt` find what reaches above or below the value, so a year, which holds days after a
  day, is above that day; `sa` and `eb` find only what lies wholly after or before it. `ge` and
  `le` find what `gt` and `lt` find, and what lies wholly inside the value. `ap` finds what
  overlaps the value. A value to the second is a range of one second, so `gt2026-10-31T22:30:00Z`
  does not find a time stored as `2026-10-31T23:30:00+01:00`, which is the same second.
- **A time with no zone** is read as UTC when it is a search value. A stored one is refused: FHIR
  asks for a zone with every time in a `dateTime`, a time with no zone names no moment, and a search
  by a moment could only guess. Writing `"effectiveDateTime": "2026-10-31T22:30:00"` is a `400`
  that names the element. A date with no time (`2026-10-31`) needs no zone.
- **A date has to exist.** A `date`, a `dateTime` or an `instant` is refused when it is written
  unless it is a date of the calendar: a year from `0001` to `9999` with no sign, and a day that is
  one of its month in that year. `2026-02-30`, `2023-02-29`, `2026-10-00` and `0000-01-01` are each
  a `400` that names the element. A search value is held to the same rule, so `date=0000` is a
  `400` and not an empty result.
- **A period is the range between its ends**, each read in the zone it was written in. One with no
  `end` has not ended, and one with no `start` has no beginning, so each runs without limit that
  way: `gt2030-01-01` finds an Encounter that started in 2026 and has no `period.end`, as it finds a
  period that is still going. An Encounter that has finished should say so with `period.end`:
  without it, every window after its start finds it. A window of two values finds every period that
  is going on at some moment in it:
  `date=ge2026-10-30T00:00:00Z&date=lt2026-10-31T00:00:00Z`. Write a window as two times, not as
  one day: `date=2026-10-31` finds only the periods wholly inside that day, and `ge2026-10-31`
  leaves out one that ends on it, since neither reaches above the day.
- **`_sort`** on a date, `_sort=date` or `_sort=-date`, orders by the moment each one starts, so the
  order is the same whatever zones were written. Dates that start at the same moment are in the
  order of their ids.

## Operations

| Operation | Where | Notes |
|-----------|-------|-------|
| `$everything` | `Patient/[id]` | Holds only the kinds of resource the caller's scopes let them read |
| `$export` | system, type, instance | Asynchronous (`202`, then `GET /export/{job}` and `GET /export/{job}/file`). Jobs are durable; see [Operations](operations.md#export-jobs) |
| `$validate` | type, instance | Validates against the installed StructureDefinitions |
| `$match` | `Patient` | Finds the Patients that may be the person a Patient resource describes. See [Finding a patient](#finding-a-patient-patientmatch) |
| `$meta`, `$meta-add`, `$meta-delete` | instance | |
| `$document` | `Composition/[id]` | A document Bundle of the Composition and what it references; see [Documents](#documents) |
| `$summary` | `Patient`, `Patient/[id]` | An International Patient Summary. See [Patient summary](patient-summary.md) |
| `$health-cards-issue` | `Patient/[id]` | A SMART Health Card, where the server has a signing key. See [SMART Health Cards](health-cards.md) |
| `$expand`, `$lookup`, `$validate-code`, `$subsumes`, `$translate` | `ValueSet`, `CodeSystem`, `ConceptMap` | See [Terminology](#terminology). Each also answers a `GET`, and with SMART needs only `read` |
| `$expandability` | `ValueSet` | Which value sets cannot be expanded, and why |
| `$install` | `ImplementationGuide` | Install a package at runtime. With SMART it needs `system/*.write` |
| `$populate`, `$extract`, `$assemble`, `$next-question`, `$behavior` | `Questionnaire`, `QuestionnaireResponse` | Structured Data Capture 4.0.0 |
| `$apply` | `ChargeItemDefinition/[id]` | Prices a ChargeItem under a definition. See [Pricing a charge item](#pricing-a-charge-item-chargeitemdefinitionapply) |
| `$submit` | `Claim`, `CoverageEligibilityRequest` | Stores a claim, pre-authorisation or eligibility request and answers with a response that says it is queued. See [Submitting a claim](#submitting-a-claim-or-an-eligibility-request) |

### Pricing a charge item: `ChargeItemDefinition/$apply`

`POST [base]/ChargeItemDefinition/[id]/$apply` takes the FHIR R4 `Parameters` `chargeItem` (a
`valueReference` to a `ChargeItem` in this store, as `ChargeItem/[id]`) and, optionally, `account` (a
`valueReference` to an `Account`). It works out what the ChargeItem costs under the definition and
answers with a draft `Invoice` that is not stored. R4 says only that the answer is "Any" resource, so
this is the choice made here: an Invoice is what the resource page says these prices are for, its line has
price components of the shape the definition's have, and the `account` is the Invoice's.

```bash
curl -s -X POST http://localhost:8080/ChargeItemDefinition/consultation/\$apply -H 'content-type: application/fhir+json' -d '{
  "resourceType": "Parameters",
  "parameter": [{"name": "chargeItem", "valueReference": {"reference": "ChargeItem/visit-1"}}]
}'
```

```json
{
  "resourceType": "Invoice", "status": "draft",
  "subject": {"reference": "Patient/ada"},
  "lineItem": [{"sequence": 1, "chargeItemReference": {"reference": "ChargeItem/visit-1"},
    "priceComponent": [
      {"type": "base", "amount": {"value": 15000, "currency": "NGN"}},
      {"type": "surcharge", "code": {"text": "after hours"}, "factor": 0.25, "amount": {"value": 3750, "currency": "NGN"}}]}],
  "totalNet": {"value": 18750, "currency": "NGN"},
  "totalGross": {"value": 18750, "currency": "NGN"}
}
```

- **Conditions.** `applicability` of the definition and of a `propertyGroup` is FHIRPath (`text/fhirpath`,
  which is what a condition with no `language` is), with the ChargeItem as `%context` and its references
  followed with `resolve()`: `%context.subject.resolve().birthDate > today() - 14 years`. A reference is
  followed only to a type the token may read; to any other it leads nowhere, as if the resource were not
  there, so a price never tells what is in a record the caller may not see. Every condition
  of the definition must be true, or it does not apply to this ChargeItem and the answer is a `422`
  `OperationOutcome` (`business-rule`) that names the condition. A group is used when every condition of
  its own is true, and a group with none always is. A condition the server cannot decide (another
  language such as `text/cql`, no `expression`, one that does not parse or run, or one that is not true or
  false) is a `422` that says which, and is never taken as true: a price for a condition nobody checked
  would be a wrong price.
- **Amounts.** The Invoice line has the price components of the groups that apply, as the definition gives
  them. A component with a `factor` and no `amount` is that fraction of the base price, and the Invoice
  has the amount worked out beside the `factor`. `totalNet` is the base price plus surcharges, less
  deductions and discounts (written positive or negative: both are taken off), and `totalGross` adds
  tax. `informational` components are shown and never added (one that is a `factor` and has no base price
  to take it of is shown without an amount). There is one base price: two `base` components that apply,
  in one group or two, are a `422` that names both, because nothing says which is the price and adding
  them would charge for both. Amounts are added as decimals, not rounded, and keep the currency of the
  definition; they are as exact as the JSON numbers they are read from (about 15 significant digits, as
  everywhere in this server). Amounts in two currencies are a `422`, because no rate is guessed, and so
  is a sum too large to hold. The ChargeItem's `quantity` is not applied, since the definition does not
  say what its prices are for. With no base price (no group applies) there is a line and no totals: no
  price.
- **Not found.** An `[id]` that is not a definition is a `404`. A `chargeItem` or `account` that does not
  exist is a `422`: the request is well formed, and what it names is not there. A parameter that is
  missing, is not a reference, or refers to a resource of another type is a `400`. The body is JSON: XML
  is a `415`.
- **Access.** With SMART the token needs to be able to read the definition, the ChargeItem and, if one is
  named, the Account: the operation writes nothing, so reading is enough. A token that is limited to one
  patient cannot use it, since it prices a ChargeItem the path does not name.
- **Over `/sync`.** A server without SMART runs it as a `Mutation` named `ChargeItemDefinition/$apply`,
  with `{"id": "[id]", "resource": <the Parameters>}` as its `args`, and answers the Invoice as
  `result`. With SMART the scope table `/sync` checks has no entry for it, so it is refused whatever the
  token holds: use REST. See [the protocol](sync-protocol.md#mutation).

### Submitting a claim or an eligibility request

`POST [base]/Claim/$submit` takes FHIR R4's `Parameters` with one `resource`: a `Claim`, or a `Bundle` of
them. (A Claim, or a Bundle, as the body itself is accepted too, as R4 allows when the resource is the
only input.) A Claim says in `use` whether it is a claim, a pre-authorisation (`preauthorization`) or a
pre-determination (`predetermination`). `POST [base]/CoverageEligibilityRequest/$submit` is the same for
eligibility requests.

A server that stores records is not the payer that decides them, so the operation does what a server can:
it checks and stores what was submitted, as a transaction would, and answers with a response that says it
was received and is waiting. The response is a `ClaimResponse` (or a `CoverageEligibilityResponse`) with

- `outcome` `queued`, and `status` `active`;
- `request`, a reference to the stored Claim (`Claim/[id]`);
- `patient`, `insurer` and, as R4 gives them, `use`, `type` and `subType` of a Claim, or `purpose` and
  `servicedDate` or `servicedPeriod` of an eligibility request, copied from the request, and `requestor`
  from its `provider`;
- `created`, the time of the request, and a `disposition` that says it is waiting.

The response is stored like any other resource, and creating it is announced like any other create: a
[Subscription](#subscriptions) on `ClaimResponse?outcome=queued` tells whoever adjudicates that a claim
has come, and `GET [base]/ClaimResponse?request=Claim/[id]` finds the response to a claim. The
adjudicator decides by updating the response (`outcome` `complete`, the `item`s and their `adjudication`,
`preAuthRef` for an approval).

A `Bundle` may hold Claims, Bundles that each hold a Claim, and the resources they refer to (a
`Patient`, a `Coverage`). Each is stored as a new resource, and an entry refers to another by its
`fullUrl` (`urn:uuid:...`), as in a transaction; a `fullUrl` names an entry only inside its own Bundle, so
Bundles inside the Bundle may each use the same one for their own Patient (the server gives them
separate ones), and the same `fullUrl` on two entries of one Bundle is a `400`. An entry with its own
`request` (`POST` with `ifNoneExist`, or `PUT`) is stored as it says, which keeps a `Patient` from being
made again each time. A request that `ifNoneExist` finds already stored is not a new one: the answer is
the response it already has, and no second response is made for the adjudicator to decide. A submission
holds at most 500 resources, and Bundles may be nested three deep.
What comes back follows what went in: a Claim gets its `ClaimResponse`, and a Bundle gets a `collection`
Bundle with one response for each Claim, in the order they were submitted. A submission is one
transaction: if anything in it is invalid, a reference names something that does not exist, or the token
may not write it, nothing is stored, and the answer says why (`400` with the validator's `OperationOutcome`
and, in a Bundle, the position of the entry).

With SMART, the token needs to be able to create everything the submission writes: the Claim, the response
type, and every other type in the Bundle (or to update it, for a `PUT` entry; a `PUT` with a query, which
creates when nothing matches, needs to create as well). A token that is limited to
one patient cannot submit.

Over `/sync` a server without SMART runs it as a `Mutation` named `Claim/$submit` (or
`CoverageEligibilityRequest/$submit`), with `{"resource": <the Claim, or the Bundle, or the Parameters>}`
as its `args`. With SMART it is refused, as `$apply` is: use REST.

### Documents

`Composition/[id]/$document` returns a Bundle of type `document`. The Composition is its first entry,
followed by every resource it references, and every resource those reference in turn, each once.
The Bundle has an `identifier` and a `timestamp`, every entry has a `urn:uuid` `fullUrl`, and each
reference to a resource in the document points at that `fullUrl`, so the document stands on its own.
A resource the caller's scopes do not let them read is left out. A document of more than 1000
resources is refused with a `422`.

Resources are validated entry by entry. A Bundle's `entry.resource`, and any other element that
holds a whole resource, is checked as the resource it holds, against the profiles the element
names and the ones the resource states in `meta.profile`. A narrative (`text.div`) is checked for
what FHIR allows in one: a single `div` in the XHTML namespace, only the basic formatting
elements, no script, and some text.

### Finding a patient: `Patient/$match`

`POST [base]/Patient/$match` takes the FHIR R4 `Parameters`: `resource` (a Patient, required),
`onlyCertainMatches` (boolean) and `count` (integer), and nothing else. A `resource` that has a
`managingOrganization` [limits it to that organization's Patients](#one-organizations-patients). It
answers a `searchset` Bundle, best match first. Each entry has `search.mode` `match`, a
`search.score` from 0 to 1 and the `match-grade` extension (`http://hl7.org/fhir/StructureDefinition/match-grade`) on `search`:

```bash
curl -s -X POST http://localhost:8080/Patient/\$match -H 'content-type: application/fhir+json' -d '{
  "resourceType": "Parameters",
  "parameter": [
    {"name": "resource", "resource": {
      "resourceType": "Patient",
      "name": [{"family": "Okafor", "given": ["Ada", "Chinwe"]}],
      "birthDate": "1985-03-02",
      "telecom": [{"system": "phone", "value": "0803 123 4567"}]
    }},
    {"name": "onlyCertainMatches", "valueBoolean": false}
  ]
}'
```

The algorithm is the server's own; FHIR leaves it to it. A stored Patient gets the first grade whose
conditions it meets, and one that meets none is not returned.

| Grade | When | Score |
|-------|------|-------|
| `certain` | An identifier has the same `system` and the same `value`, compared exactly | 1 |
| `probable` | The name, the full birth date and a phone number or email address in `telecom` are the same | 0.75 to 0.95 |
| `possible` | The name and the full birth date are the same | 0.40 to 0.70 |

`certainly-not` is a grade FHIR defines, and the server never returns it: a Patient that cannot be
the person is left out. A birth date that differs is not a match, however much else is the same.
Whether a `probable` match may be treated as the person is for the caller to decide: the server
does not know whether a phone number or an email address was ever checked.

Each entry also says what it was found by, in `match-evidence` extensions on `search`
(`https://refhir.dev/StructureDefinition/match-evidence`), one for each thing. Each has a `kind`,
and some a `value`:

| `kind` | Means | `value` |
|--------|-------|---------|
| `identifier` | An identifier of the request is on the Patient | `valueIdentifier`: the system and value as the request wrote them. One extension for each that matched |
| `replaced-record` | The Patient is not what the request matched but what replaced it. Comes first. The other evidence on the entry is what this Patient itself shares with the request, which can be less than the replaced one had | `valueReference`: the replaced Patient. One extension for each replaced Patient it was found through |
| `name` | A name of the request is a name of the Patient | |
| `name-reordered` | Said as well as `name` when the words are the same but not in the same places (a family name written as a given name) | |
| `birth-date` | The same full birth date | |
| `phone`, `email` | A phone number or an email address of the request is the Patient's | `valueContactPoint`: the number or address **as the request wrote it**, not the Patient's copy. One extension for each that matched |
| `gender`, `gender-differs` | Both have a gender, and it is the same, or is not | |

```json
"search": {
  "mode": "match",
  "score": 0.85,
  "extension": [
    {"url": "http://hl7.org/fhir/StructureDefinition/match-grade", "valueCode": "probable"},
    {"url": "https://refhir.dev/StructureDefinition/match-evidence",
     "extension": [{"url": "kind", "valueCode": "phone"},
                   {"url": "value", "valueContactPoint": {"system": "phone", "value": "0803 123 4567"}}]},
    {"url": "https://refhir.dev/StructureDefinition/match-evidence",
     "extension": [{"url": "kind", "valueCode": "name"}]}
  ]
}
```

This is what a caller needs to decide whether a `probable` match may be treated as the person. If it
sends a phone number it has verified, the `phone` evidence with that number shows that it is the
one the Patient has, and an email address that matched but was not verified shows as `email`. Only
what matched is said: a phone number of the request that the Patient does not have is not there.

How things are compared:

- **Names** are compared by their words. Case, accents (including the dots and tone marks of Yoruba
  and Igbo), apostrophes, hyphens, spacing and the order of the words do not matter, so
  `Ọláwálé Adébáyò`, `ADEBAYO olawale` and a family name written in the given name's place are one
  name. The letters of Hausa that have no accented form (`ɓ`, `ɗ`, `ƙ`) match the letters they are
  written as. A word missing or added is a different name, and a name of fewer than two words is
  not enough to go on. Any name of the request is compared with any name of the Patient, and a
  name given as `text` alone is read word by word. A name in another order scores a little lower.
- **Birth dates** must be full dates (`YYYY-MM-DD`) on both sides. A year, or a year and month,
  is never enough, and a `birthDate` that is not a date is a `400`.
- **Phone numbers** are compared by what they dial: `+234 803 123 4567`, `0803 123 4567`,
  `2348031234567` and `00234 (0) 803-123-4567` are one number. A number written without a country
  code is taken to be Nigerian (`234`). Email addresses ignore case. Only the Patient's own
  `telecom` counts, never `Patient.contact`, and a `telecom` with `use` `old` does not count, since
  numbers are given to someone else once they are given up. A `telecom` without a `system` is read
  as an email address if it has an `@`, and as a phone number otherwise.
- **Identifiers** count only with both a `system` and a `value`, and not when `use` is `temp`.
- **Gender** only moves the score within its grade: a gender that agrees is a little higher, and
  one that differs is lower. It never decides a grade. `unknown` is no gender: it neither agrees
  nor differs.

A Patient with `active` false is never returned, and neither is one that another Patient replaced
(it has a `link` of type `replaced-by`, with `active` false or not). A match on a replaced Patient is
still a match on the person, so the server follows the link to the Patient that replaced it and
returns that one:

- Only a relative `Patient/id` in this store is followed, through up to five links, and never round a
  cycle. It ends at a Patient that is in use and has not been replaced itself. A link to a Patient
  that is gone, that was switched off, or that is on another server leads nowhere, and then nothing
  is returned for that match.
- The Patient it finds has the grade of the match on the replaced one, but at most `probable`: the
  server compared the replaced Patient with the request, not this one, and the link was made by
  whoever merged them. So it is never `certain`, and `onlyCertainMatches` never returns it.
- The entry says so with `replaced-record` evidence, a `valueReference` to each replaced Patient it
  was found through, so that a caller or the person looking can see where the match came from. The
  rest of its evidence is what the Patient it returns shares with the request, and nothing the
  replaced one had that it does not: if the replaced Patient had the birth date or the phone number
  and this one has not, the entry does not say `birth-date` or `phone`.
- A Patient found both ways, itself and through a replaced one, is one entry: whichever scores
  higher, and itself when they score the same.

`onlyCertainMatches` is for linking without a person looking. When it is `true`, the server returns
a match only if exactly one Patient is a potential match (one with any grade) and that one is
`certain`. When more than one is, nothing is returned for it, even if one of them is certain: two
records that carry the same identifier, or one that does and another with the same name and birth
date, are for a person to settle. `count` limits the entries and is applied after that, so it never
turns many potential matches into one.

#### One organization's Patients

One server can hold the Patients of many organizations, each with its `managingOrganization`. Left
alone, `$match` compares the request with all of them, so a clinic that asks about a person also
gets the same person's records at the other organizations, `count` can be used up by records that are
not the clinic's, and `onlyCertainMatches` returns nothing for a person who is at two organizations
that share an identifier (it is two certain matches). R4 gives `$match` no parameter for the
organization, so it is the Patient in `resource` that says it: give it the `managingOrganization`
that the organization's own record of the person has, and only that organization's Patients are
compared.

```bash
curl -s -X POST http://localhost:8080/Patient/\$match -H 'content-type: application/fhir+json' -d '{
  "resourceType": "Parameters",
  "parameter": [
    {"name": "resource", "resource": {
      "resourceType": "Patient",
      "identifier": [{"system": "https://boolbyte.health/fhir/NamingSystem/boolbyte-id", "value": "BB-1"}],
      "managingOrganization": {"reference": "Organization/clinic-a"}
    }},
    {"name": "onlyCertainMatches", "valueBoolean": true}
  ]
}'
```

The `managingOrganization` has to be the reference `Organization/id`. A bare id, a URL, a
`#contained` reference, a reference to another type, or one with only an `identifier` or a
`display` is a `400` rather than an answer for every organization, which is not what a sender
who named one asked for. Leave the element out to compare with every organization's Patients. There
is no `organization` parameter: it is one that `$match` does not take, in the `Parameters` or as a
query parameter, and either is a `400` like any other unknown one. A query parameter of another name
is ignored, as it is for the other operations.

The server does not look for the Organization. It compares the reference with the one the Patients
carry, so use that one: a reference no Patient has, a mistyped one included, is answered as a
person who is not there is. The `managingOrganization` only chooses the Patients to compare with.
It is not compared with theirs: it is not in the `match-evidence`, and it changes no grade or score.

- Only the Patients whose `managingOrganization.reference` is that reference are candidates. The
  reference is compared as it is written, as a search does, so `Organization/clinic-a` is not
  `Organization/clinic` or a URL that ends in it. A Patient with no `managingOrganization` is in no
  organization's set.
- Everything above then applies to that set alone: the grades, the Patients that are not in use,
  `onlyCertainMatches` (a person with one certain match at the organization is returned although
  the other organization has a certain match too) and `count`, which is applied after the limit, so
  that another organization's records cannot take the places.
- A `replaced-by` link is followed only to a Patient of the same organization. A Patient of another
  is never returned and is not a way to reach one: a link that leaves the organization, even one
  that comes back to it, leads nowhere.
- No Patient in the set is an empty `searchset` (`total` 0), not an error, as is an organization no
  Patient has.
- Only the organization's Patients count towards the 20,000 below.
- Without a `managingOrganization` on the Patient sent, nothing is different: every organization's
  Patients are compared.

The limit is a filter on what is compared and not an access check: a token that may search Patients
may leave it out, or name any organization.

A Patient with nothing to look up (no identifier with a system and a value, and no name with a full
birth date) matches nothing, and the Bundle is empty. More than 20,000 Patients sharing an
identifier or the birth date is a `422` (`too-costly`) rather than an answer that left some out.
A resource with more than 50 identifiers, names or telecoms is a `400`. The body is JSON: an XML
body is a `415`.

Only the store the request is made to is searched. The caller needs a scope that may search
Patients (`user/Patient.read` or `user/Patient.rs`). A token limited to one patient is refused,
because the operation reads every Patient there is. The audit trail records it as an operation on
`Patient`, without the Patient in the request.

Over `/sync` it is a `Mutation` named `Patient/$match`, with the same `Parameters` as its `args` (the
same three parameters as the members of a plain object work too), and the `searchset` Bundle comes back
as `result`:

```json
{"type": "Mutation", "requestId": 7, "udfPath": "Patient/$match", "args": {"resourceType": "Parameters", "parameter": [
  {"name": "resource", "resource": {"resourceType": "Patient", "identifier": [{"system": "https://id.example.org/profile", "value": "PRF-1001"}]}}
]}}
```

A server without SMART answers it as REST does, with no scopes to check. **With SMART on, `/sync` does
not offer it:** the scope table every `/sync` operation is checked against has no entry for `$match`, so
the `Mutation` answers `forbidden` (`unsupported operation $match`) whatever the token holds, a token that
may search Patients included. Send it over REST, where the scope above is what it needs. See
[the protocol](sync-protocol.md#mutation).

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

Every element that can hold a reference is checked, the ones that may hold something else instead as well: `ChargeItem.productReference`, `Invoice.lineItem.chargeItemReference` and `MedicationRequest.medicationReference` are refused when what they name does not exist, and an element that holds a `CodeableConcept` (`productCodeableConcept`) or nothing is not asked for a reference. That is every such element of every resource, not only the billing ones: `Task.input.valueReference`, `Communication.payload.contentReference`, `QuestionnaireResponse.item.answer.valueReference` and `Parameters.parameter.valueReference` must name something on this server too (or, under `fhir`, anything). As everywhere under `strict`, a reference to a contained resource (`#med1`, as in a `MedicationRequest` with its `Medication` inside it) is refused: write the resource and name it.

## Resource ids

The server assigns the id of every resource it creates. `POST [type]` ignores an `id`, and a `meta.versionId` or `meta.lastUpdated`, in the body (FHIR R4 create) and answers `201` with the new id in `Location`. GraphQL and `/sync` `create` do the same.

- **`PUT [type]/[id]`** updates a resource that exists. For an id that does not, it answers `405` with an `OperationOutcome`, because update-as-create is off. `GET /metadata` says so (`updateCreate: false` on every resource), and also reports the conditional interactions that are implemented (`conditionalCreate`, `conditionalUpdate`, `conditionalDelete: single`).
- **Transactions** give entries that create resources their ids up front. Entries that refer to one another name each other by `fullUrl`, as FHIR defines: give the creating entry `"fullUrl": "urn:uuid:..."` and use that string in the references. A `Patient/p1` written in the bundle by hand does not name the resource the server creates.
- **Update-as-create** is a deployment choice in FHIR. Start with `--update-as-create` (or `update_as_create = true` under `[conformance]`) to let `PUT [type]/[id]` create the resource under that id (`201`); `/metadata` then reports `updateCreate: true`. Data that has ids of its own, such as the demo data, is loaded this way. The environment variable `REFHIR_UPDATE_AS_CREATE=true` turns it on too. With SMART enabled, a `PUT` then needs the `create` permission as well as `update`, since it may create. An SDC `$extract` whose templates name a `resourceId` (or an `id`) returns those entries as `PUT [type]/[id]`, so the bundle it returns needs this setting to be accepted by a server that has not got it, which answers each such entry with `405`.

## Terminology

`ValueSet/$expand` takes `url`, and optionally `filter`, `count`, `offset` and `activeOnly`, as a `POST` (a JSON body or a Parameters resource) or as a `GET` with the same names in the query string:

```bash
curl 'http://localhost:8080/ValueSet/$expand?url=http://hl7.org/fhir/ValueSet/organization-type&filter=hospital&count=10'
```

The other terminology operations (`$validate-code`, `$lookup`, `$subsumes`, `$translate`) take their parameters the same way: as a `POST`, or as a `GET` with plain names in the query string (`system`, `code`, `display`, `url`, `version`, `codeA`, `codeB`, `targetsystem`, `abstract`). A parameter that is not a plain value, a `coding` or a `codeableConcept`, is sent as a `POST`. R4 also defines `$expand` and `$validate-code` on a value set, `$validate-code` and `$subsumes` on a code system, and `$translate` on a concept map at the instance level: `POST` or `GET /ValueSet/[id]/$expand`, `/ValueSet/[id]/$validate-code`, `/CodeSystem/[id]/$validate-code`, `/CodeSystem/[id]/$subsumes`, `/ConceptMap/[id]/$translate`. The instance is a resource the server stores, one written through the API, and what is asked of it is asked of the value set, code system (at its `version`) or concept map its `url` names, so no `url` or `system` is given. The body may be empty. An id the server does not store is `404`, and one whose `url` a package defines too is `409` (the terminology holds one definition of a url, and cannot tell which is meant: ask by `url`); a value set that came from a package has no id to ask for, and is expanded by `url`. (`CodeSystem/$lookup` is defined on the type only.)

These operations change nothing, so with SMART they need only the `read` permission, whichever way they are sent, and a client that may only create is refused.

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

The ValueSet an expansion comes in has the `status` of the value set it was expanded from (`active` when that says nothing), and the expansion carries the `timestamp` R4 requires, the time the server produced it for the request, and an `identifier` (a `urn:uuid` made from what the expansion says, so the same for two answers that say the same). What was asked that shaped it is listed in `expansion.parameter`: `filter`, `count`, `offset` and `activeOnly`. An entry says `"inactive": true` when its code system says the concept is inactive (no longer to be used, but kept to make sense of old data); it is still listed, unless the request said `activeOnly=true`, which leaves such codes out (and the `total` counts what is left).

An expansion marks an entry `"abstract": true` when its code system says the concept is not to be selected as a value (the `notSelectable` property, which HL7's v3 code systems use for their grouping concepts: `_ActCoverageTypeCode`, `_ActInsuranceTypeCode` and so on in `coverage-type`). The entry is still listed, for navigation, so a picker leaves out the entries that carry the flag. `$validate-code` takes R4's `abstract` input: with `abstract=false` an abstract code is not valid (`result` false, and the `message` says why), with `abstract=true` it is valid, and without the input it is valid and the `message` says it is a grouping concept.

HL7 publishes `ValueSet/currencies` (the binding of `Money.currency`) as an include of ISO 4217 and no CodeSystem for it, so ReFHIR installs one itself at start: the active alphabetic currency codes (178, with their numeric codes and English names, for example `NGN` Nigerian Naira). `$expand` of `currencies` lists them, `filter=naira` finds `NGN`, `$validate-code` accepts a real code and refuses an unknown one, and a `Money` with an unknown `currency` is refused by `$validate` and on write. It is installed once, as the package `refhir.builtin.terminology`, after the packages the server installs, so it works with packages you add with `--package` too. A server that has no package has nothing to use it, and gets it at the first start after it has one.

A code system can be installed in several versions, and each is kept. Name a version with `system|version` (or `include.version`) and the expansion uses exactly that one. A version that is not installed is refused, and the error lists the versions that are. With no version named, a value set from a package uses the versions its own package holds, and a value set you write through the API, or one whose package does not hold the system, uses the version of the FHIR release the server serves (`4.0.1` for R4) when it is installed, and the latest installed version otherwise. The expansion records the version it read. `$lookup`, `$subsumes` and `CodeSystem/$validate-code` take a `version` parameter.

A value set that includes a code system, or a value set, that another package carries is expanded when both are installed, whichever came first: after any install the server tries again the value sets that have no expansion (see `$expandability` for the ones that still have none), and it stores what it can now expand.

`POST /ValueSet/$expandability` reports, for the value sets named in `url` (a string, an array, or `url` parameters of a Parameters resource), whether each can be expanded and why not; with none named it lists every loaded value set that cannot. Use it as a deployment check for the value sets a UI depends on. `POST /ImplementationGuide/$install` adds a warning to its response for any value set the package made unexpandable, and the server logs how many there are at start-up.
