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
- A comma between the values of one parameter is OR, for every kind of parameter:
  `Slot?status=free,busy`, `HealthcareService?_id=a,b`. Write `\,` for a comma that is part of a
  value. A value lists at most 1000 alternatives. With `:not` and `:not-in`, a list leaves out
  every value in it, and `:missing` takes one value.
- Giving a parameter twice is AND (`date=ge2026-01-01&date=lt2026-02-01`) in a query string, in the
  URL of a Bundle entry and in Subscription criteria. A GraphQL argument or a live query's
  arguments have one value per name, so they cannot say it.
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

## Operations

| Operation | Where | Notes |
|-----------|-------|-------|
| `$everything` | `Patient/[id]` | |
| `$export` | system, type, instance | Asynchronous (`202`, then `GET /export/{job}` and `GET /export/{job}/file`). Jobs are durable; see [Operations](operations.md#export-jobs) |
| `$validate` | type, instance | Validates against the installed StructureDefinitions |
| `$meta`, `$meta-add`, `$meta-delete` | instance | |
| `$document` | `Composition/[id]` | |
| `$expand`, `$lookup`, `$validate-code`, `$subsumes`, `$translate` | `ValueSet`, `CodeSystem`, `ConceptMap` | See [Terminology](#terminology) |
| `$expandability` | `ValueSet` | Which value sets cannot be expanded, and why |
| `$install` | `ImplementationGuide` | Install a package at runtime. With SMART it needs `system/*.write` |
| `$populate`, `$extract`, `$assemble`, `$next-question`, `$behavior` | `Questionnaire`, `QuestionnaireResponse` | Structured Data Capture 4.0.0 |

Beyond REST:

- **GraphQL** at `POST /graphql`.
- **Subscriptions.** `Subscription` resources with a `rest-hook` or `websocket` channel are
  delivered by the primary; the WebSocket channel is `GET /[type]/[id]/$websocket`.
- **Live queries** over [`/sync`](sync-protocol.md).
- **Governance endpoints** `/_usage`, `/_requests`, `/_audit` and `/_audit/verify`; see
  [governance](governance.md).
- **Health probes** `/livez`, `/readyz` and `/healthz`; see [Operations](operations.md#health-probes).

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
- **Update-as-create** is a deployment choice in FHIR. Start with `--update-as-create` (or `update_as_create = true` under `[conformance]`) to let `PUT [type]/[id]` create the resource under that id (`201`); `/metadata` then reports `updateCreate: true`. Data that has ids of its own, such as the demo data under `fixtures/mock-fhir` and the Inferno profile, is loaded this way. The environment variable `REFHIR_UPDATE_AS_CREATE=true` turns it on too. With SMART enabled, a `PUT` then needs the `create` permission as well as `update`, since it may create. An SDC `$extract` whose templates name a `resourceId` (or an `id`) returns those entries as `PUT [type]/[id]`, so the bundle it returns needs this setting to be accepted by a server that has not got it, which answers each such entry with `405`.
- Rust code that embeds the server and needs a known id can call `refhir_fhir::import_resource`. It is a function, not an operation: nothing a client sends reaches it.

## Terminology

`ValueSet/$expand` returns the expansion, or `422` with an `OperationOutcome` when the value set cannot be expanded. Each cause is one issue, with the failing part of the value set in `expression` (for example `ValueSet.compose.include[1]`) and the code system, its `content` mode and the filter in `diagnostics`. The usual causes are a code system that is not installed, one that holds only part of its content (`example`, `fragment`, or `not-present`, as the R4 SNOMED CT stub does), or a filter this server does not evaluate. A partial expansion is never returned, because a client cannot tell it from a complete one.

A code system can be installed in several versions, and each is kept. HL7's v2 tables carry theirs in the URL (`.../v2-0006|2.1`), other packages in `CodeSystem.version`; a code system written through the API with a `version` is stored the same way. A value set that names a version (`system|version`, or `include.version`) gets exactly that one; with none named, a value set from a package reads the versions of that system **its own package** holds (installing the package expanded it with those, and a rebuild gives the same answer), and a value set written through the API, or one whose package does not hold the system, reads the version of the FHIR release the server serves (`4.0.1` for R4) when it is installed and the latest installed version otherwise. The expansion records the version it read. A value set that includes another by URL reads that one's definition from its own package too, when the package has one. When several packages define the same value set URL, the definition at the server's release version is the one in use (the extensions package carries `5.3.0-ballot-tc1` copies of R4's value sets over R5's code system URLs, and an R4 server means R4's `4.0.1`); with none at the release, the one installed last, because version strings in different styles (`2018-08-12`, `2.9`, `3.0.0`) do not order against each other. If the one in use cannot be expanded, the next that can answers, so a newer terminology package that needs a code system nothing carries does not take away the older expansion. A start, a rebuild and an incremental change choose the same definition: each is tried first as installing its own package tried it, against what that package holds alone, and only when none can be expanded that way is the rest of the store read, which is how a value set that waits on another package's code system gets an expansion (a start reads what installing each package stored, so it has none for such a value set until the next rebuild). A rebuild stores the expansion under the package whose definition is in use. Stored expansions carry the rules they were made under, and a database written by an older version has them rebuilt once, at its next start. Cross-version and newer packages are why: `hl7.fhir.uv.xver-r5.r4`, a dependency of SDC, has `5.0.0` of `http://hl7.org/fhir/encounter-status`, and `hl7.terminology.r4` carries newer copies of systems the R4 core package also carries; an R4 value set that includes them still means R4's codes (`arrived`, `finished`). A version that is not installed is refused, naming the versions that are, rather than answered from another one. `$lookup`, `$subsumes` and `CodeSystem/$validate-code` take a `version` parameter.

`POST /ValueSet/$expandability` reports, for the value sets named in `url` (a string, an array, or `url` parameters of a Parameters resource), whether each can be expanded and why not; with none named it lists every loaded value set that cannot. Use it as a deployment check for the value sets a UI depends on. `POST /ImplementationGuide/$install` adds a warning to its response for any value set the package made unexpandable, and the server logs how many there are at start-up.
