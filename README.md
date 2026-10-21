<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/refhir-logo-reversed.svg">
    <img src="docs/brand/refhir-logo.svg" alt="ReFHIR" height="40">
  </picture>
</h1>

> **FHIR defines healthcare state. ReFHIR keeps software and AI synchronized with it.**

[![CI](https://github.com/hai-hq/refhir/actions/workflows/ci.yml/badge.svg)](https://github.com/hai-hq/refhir/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

ReFHIR is an open-source **reactive FHIR R4 server** from [HAIHQ](https://github.com/hai-hq).
It treats FHIR data as live application state: a client runs a FHIR query and *stays subscribed
to its result*. When a committed write changes what the query returns, the server re-runs it and
pushes the new result. Web apps and AI agents get "what is true now?" from the data layer instead
of rebuilding polling, caching and invalidation themselves.

It is also an ordinary FHIR server. Existing clients use standard REST, search and transactions,
and [SMART on FHIR](docs/smart.md) for authorization.

```http
GET /Observation?patient=Patient/123&status=final        <- a snapshot, as with any FHIR server
ws://host/sync   subscribe: Observation?patient=Patient/123&status=final
                 <- the matches now, then a new result whenever a commit changes them
```

> **Queries, not events, are the primary unit of synchronization.** Read
> [Design](docs/design.md) for the reasoning, including why this matters for AI agents that act
> on state that may have changed while they were thinking.

## Quick start

With Docker, from a clone of this repository:

```bash
git clone https://github.com/hai-hq/refhir.git
cd refhir
docker compose up --build
```

Or with Rust 1.88 (installed by `rustup` from `rust-toolchain.toml`):

```bash
REFHIR_PACKAGE_DOWNLOAD=true cargo run -p refhir -- --memory      # a throwaway server; use --sqlite refhir.sqlite to keep data
```

A build from source has no prepared packages file, so `REFHIR_PACKAGE_DOWNLOAD=true` lets its first
start download `hl7.fhir.r4.core`, the SDC package and what they depend on from
[packages.fhir.org](https://packages.fhir.org); nothing is downloaded without it. The Docker image
carries them and needs neither the setting nor the network. Then:

```bash
curl -s http://127.0.0.1:8080/metadata | head -c 300

curl -s -X POST http://127.0.0.1:8080/Patient \
  -H 'Content-Type: application/fhir+json' \
  -d '{"resourceType":"Patient","active":true,"name":[{"family":"Ann"}]}'

curl -s 'http://127.0.0.1:8080/Patient?name=ann'
```

More in [Getting started](docs/getting-started.md): the Docker image, configuration, demo data.

## Live queries from a client

```ts
import { dial } from "@haihq/refhir";

const client = await dial("http://127.0.0.1:8080");
const watch = await client.search("Patient", { active: "true" });

console.log(watch.latest());      // the current matches
console.log(await watch.next());  // the next committed result
```

In React, `useFHIRQuery` from `@haihq/refhir/react` does the same and re-renders on every
change. A Go client is in [`clients/go`](clients/go/README.md). Both share the
[`/sync` wire protocol](docs/sync-protocol.md), so a client in another language is small. The Go
client is published (`go get github.com/hai-hq/refhir/clients/go@v0.1.0`), and so is the
TypeScript client (`npm install @haihq/refhir`).

## What is in it

| | |
|---|---|
| **FHIR** | R4 (4.0.1), JSON and XML reads. Every resource type in the installed packages (146 with `hl7.fhir.r4.core`) with read, vread, create, update, patch, delete, history, search, conditional writes, `batch` and atomic `transaction`. See [FHIR API](docs/fhir-api.md) |
| **Search** | The full R4 search parameter set, plus parameters from any installed package. Chaining, `_has`, `_include`, `_revinclude`, compartments |
| **Operations** | `$everything`, `$export` (durable jobs), `$validate`, terminology (`$expand`, `$lookup`, `$validate-code`, `$subsumes`, `$translate`), SDC 4.0.0 (`$populate`, `$extract`, `$assemble`, `$next-question`), `$install` for packages |
| **Also** | GraphQL, Subscriptions (`rest-hook`, `websocket`), Implementation Guide packages installed at start-up or at runtime |
| **Live sync** | Reactive queries over `/sync`. REST reads that follow a write wait for a replica to catch up (read-your-writes) |
| **Security** | SMART App Launch 2.2 (authorization code with PKCE, EHR launch, backend services, OIDC sign-in), scope and patient-compartment enforcement |
| **Governance** | Metering, request logs and a tamper-evident audit trail (on by default); rate limits and quotas (when configured). See [governance](docs/governance.md) |
| **Storage** | SQLite (file or in memory) or Postgres, with TLS and forward-only [schema migrations](docs/migrations.md) |
| **Running it** | Docker image, Helm chart, one primary with standbys and replicas on one database, health probes. See [Operations](docs/operations.md) and [deploy/](deploy) |

## Documentation

| | |
|---|---|
| [Getting started](docs/getting-started.md) | Run it with Docker or from source, and try it |
| [Configuration](docs/configuration.md) | The config file and every flag |
| [FHIR API](docs/fhir-api.md) | What the server supports, resource ids, terminology |
| [Live sync protocol](docs/sync-protocol.md) | The `/sync` WebSocket, for client authors |
| [SMART on FHIR](docs/smart.md) | Authorization, sign-in, [SMART support](docs/smart-compliance.md) |
| [Rate limits, quotas, metering and audit](docs/governance.md) | Governing and recording access |
| [Operations](docs/operations.md) | Health probes, Postgres and TLS, export jobs |
| [Several processes](docs/clustering.md) | Failover and read replicas on one database |
| [Production deployment](deploy/production.md) | Postgres, secrets, Helm, a reverse proxy |
| [Schema migrations](docs/migrations.md) | Upgrading safely |
| [Design](docs/design.md) | Why it exists and how the engine works |
| [Development](docs/development.md) | Building, testing, changing it |
| [Brand](docs/brand/README.md) | Logo files, colors and how to use them |

## Status

ReFHIR is **version 0.x**. Configuration, the `/sync` protocol and the HTTP API can still change
before 1.0; [CHANGELOG.md](CHANGELOG.md) says when they do. Upgrades run forward-only schema
migrations, so **back up the database before upgrading**. It is not a certified product.

Not built yet: commit-time revalidation, where the assumptions an agent read with must still hold
when it commits. It is the direction the dependency tracking points at, and it is not part of the
server today.

## Contributing

Bug reports, fixes and docs are welcome. Start with [CONTRIBUTING.md](CONTRIBUTING.md), and see
[SECURITY.md](SECURITY.md) to report a vulnerability privately. Everyone taking part is expected to
follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE), which also credits the HL7
material used in the tests.

FHIR® is the registered trademark of HL7 and is used with the permission of HL7. Use of the FHIR
trademark does not constitute endorsement of this project by HL7.
