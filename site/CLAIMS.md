# Landing page claims

Every factual statement on the landing page (`src/pages/index.astro` and `src/components/landing/`)
and where the repository backs it. If a source changes, change the page; if you cannot find a
source for a sentence, remove the sentence.

`scripts/check-built.mjs` enforces the negative half of this file at build time: the built page
may not use certain words (certification, benchmark, throughput and similar claims) and may not
load anything from another origin.

Code on the page is not copied by hand. `src/lib/repo.ts` reads the snippets from the README when
the site builds, so they are what the project ships.

The page is visual first. At the project owner's request the page no longer carries its caveats: the "where it
stands" section, the hero's facts line and feature strip, the "simulation" and "illustration" captions, the
npm note and the agents note are all gone. That makes the "must not say" table below more important, not less:
the page still must not claim certification, stable APIs, performance figures or a comparison with another FHIR
server. The TypeScript client is on npm (`@haihq/refhir` 0.0.1; `npm view @haihq/refhir` shows it). The page
carries its message in a terminal, two timelines, a comparison grid, an interactive demo, a figure and a loop,
and keeps prose to a line or two per section, because nobody reads a wall of text on a landing page. Each
claim below has a source.

## What the page must not say

| Do not claim | Why | Source |
|---|---|---|
| Commit-time revalidation exists | It is not built | `README.md` (Status), `docs/design.md` ("Commit-time revalidation (direction)") |
| ReFHIR is certified or "SMART conformant" | It is "not a certified product"; SMART validation is a CI smoke run | `README.md` (Status) |
| Stable APIs | 0.x, no stability promise | `README.md` (Status) |
| Any throughput, latency or capacity figure | The measurements come from one loaded laptop and say they are "not capacity" | `docs/perf/baseline.md` (Setup) |
| A comparison with another FHIR server | The repository makes none; the page contrasts only with FHIR's standard API and its Subscriptions | `docs/design.md` |
| That existing FHIR clients keep working unchanged | A few defaults are strict (405 on PUT to a missing id, 400 on unknown search parameters, strict references); the page says software that speaks FHIR "can use it" | `docs/fhir-api.md` |
| That a client is told it missed changes after a reconnect | It is not told | `docs/sync-protocol.md` ("Reconnect and recovery", step 6) |
| The FHIR flame, HL7 or FHIR logos, or anything implying HL7 endorsement | FHIR® is HL7's trademark | `NOTICE`, `docs/brand/README.md` |

## What the page says

| Section | Claim | Source |
|---|---|---|
| Hero | FHIR is the standard way healthcare software shares patient data | hl7.org/fhir: "FHIR is a standard for health care data exchange, published by HL7" |
| Hero | ReFHIR is an open-source FHIR server that keeps query results live: ask once, and the new answer arrives when the data changes | `README.md` (intro), `docs/design.md` ("Product thesis") |
| Hero terminal | The five printed lines and what each change was (create Ann, create Bee, deactivate Ann, create Cho and Dee in one transaction). The reveal is staged and the card says it is output from the tutorial | `site/src/content/docs/docs/tutorial.mdx`, run against a live server |
| Problem | A plain query shows the old list after the record changes; a live query updates at once. The times are an example | `docs/design.md` ("The problem") |
| Playground | The simulation's four changes and what each panel shows; a transaction arrives as one step. The demo runs in the browser with example data and no server; the page no longer says so | `docs/design.md` ("Transaction consistency"), `docs/concepts/vs-subscriptions` |
| Queries, not events | An event says something changed; a live query says what the answer is now | `docs/design.md`, `README.md` ("Queries, not events") |
| Queries, not events | The grid: a plain query receives one answer; a Subscription sends a ping per matching record created or updated, nothing when a record stops matching, one per record in a transaction; a live query sends the whole answer each time and a transaction as one step. The rows are the tutorial's scenario | `docs/concepts/vs-subscriptions` (side-by-side table), `site/src/content/docs/docs/tutorial.mdx`, checked against a running server |
| How it works | A write commits; only queries that read what it changed wake; each gets the whole new answer. The figure uses example queries | `docs/design.md` ("How a write becomes a live-query update"), `docs/sync-protocol.md` |
| Run it, watch it | `git clone`, `docker compose up --build`; first start needs network for the FHIR packages; the printed lines | `README.md` ("Quick start", read at build time), `docs/tutorial` run against a live server |

| Run it, watch it | TypeScript, React, Go and raw `/sync` examples, read at build time | `README.md`, `clients/ts/README.md`, `clients/go/README.md` |
| AI agents | The four-step loop, and that a write is refused if the record moved; the printed lines | `docs/concepts/agents` (run against a live server) |
| Features | R4, search, transactions, `$everything`, `$export`, GraphQL, Subscriptions, SMART 2.2, validation, SDC, SQLite or Postgres with TLS and forward-only migrations, Docker, Helm, binary, health probes, standby and replicas, audit trail, metering, every version kept | `README.md` ("What is in it"), `docs/fhir-api.md`, `docs/clustering.md`, `docs/governance.md`, `docs/migrations.md`, `docs/design.md` |
| Closing | Open source under Apache-2.0, from HAIHQ, a nonprofit | `LICENSE`, haihq.org |
| Footer | Copyright line and the FHIR trademark sentence | `NOTICE`, `README.md` (License) |

## Search and sharing metadata

The landing page's `<head>` carries structured data, and `scripts/check-built.mjs` checks it against the repository.

| Where | Claim | Source |
|---|---|---|
| Title and description | ReFHIR is an open-source FHIR server, written in Rust, that keeps query results live | `README.md` (intro), `rust-toolchain.toml` |
| JSON-LD `SoftwareApplication` | Version, Apache-2.0 license, free, application category, a Linux container image or a build from source, the GitHub repository | `Cargo.toml` (compared at build time), `LICENSE`, `docs/releasing.md` (artifacts) |
| JSON-LD `Organization` | HAIHQ's own description of itself | haihq.org |
| Share image | The wordmark, the headline with a faded echo behind "updates itself.", and "An open-source FHIR server with live queries" | `README.md` (intro) |
