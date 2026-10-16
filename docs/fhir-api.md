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
- An unknown search parameter is a `400`, not silently ignored.
- A reference parameter matches the full reference, so write `Observation?patient=Patient/123`. A
  bare id such as `patient=123` matches nothing, and neither does a date that is not well formed:
  `birthdate=notadate` answers `200` with an empty Bundle.

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
- **Update-as-create** is a deployment choice in FHIR. Start with `--update-as-create` (or `update_as_create = true` under `[conformance]`) to let `PUT [type]/[id]` create the resource under that id (`201`); `/metadata` then reports `updateCreate: true`. Data that has ids of its own, such as the demo data, is loaded this way. The environment variable `REFHIR_UPDATE_AS_CREATE=true` turns it on too. With SMART enabled, a `PUT` then needs the `create` permission as well as `update`, since it may create. An SDC `$extract` whose templates name a `resourceId` (or an `id`) returns those entries as `PUT [type]/[id]`, so the bundle it returns needs this setting to be accepted by a server that has not got it, which answers each such entry with `405`.

## Terminology

`ValueSet/$expand` returns the expansion, or `422` with an `OperationOutcome` when the value set cannot be expanded. Each cause is one issue, with the failing part of the value set in `expression` (for example `ValueSet.compose.include[1]`) and the code system, its `content` mode and the filter in `diagnostics`. The usual causes are a code system that is not installed, one that holds only part of its content (`example`, `fragment`, or `not-present`, as the R4 SNOMED CT stub does), or a filter this server does not evaluate. A partial expansion is never returned, because a client cannot tell it from a complete one.

A code system can be installed in several versions, and each is kept. Name a version with `system|version` (or `include.version`) and the expansion uses exactly that one. A version that is not installed is refused, and the error lists the versions that are. With no version named, a value set from a package uses the versions its own package holds, and a value set you write through the API, or one whose package does not hold the system, uses the version of the FHIR release the server serves (`4.0.1` for R4) when it is installed, and the latest installed version otherwise. The expansion records the version it read. `$lookup`, `$subsumes` and `CodeSystem/$validate-code` take a `version` parameter.

`POST /ValueSet/$expandability` reports, for the value sets named in `url` (a string, an array, or `url` parameters of a Parameters resource), whether each can be expanded and why not; with none named it lists every loaded value set that cannot. Use it as a deployment check for the value sets a UI depends on. `POST /ImplementationGuide/$install` adds a warning to its response for any value set the package made unexpandable, and the server logs how many there are at start-up.
