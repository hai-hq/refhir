---
title: Introduction
description: What ReFHIR is in plain terms, how it differs from a typical FHIR server, and who it is for.
---

**FHIR** (say "fire") is the standard format and web API that healthcare software uses to store and
share patient data: patients, medications, lab results and more. A **FHIR server** is a program that
holds that data and answers questions about it.

ReFHIR is an open-source FHIR server, written in Rust, from the nonprofit [HAIHQ](https://haihq.org).
It has one difference. FHIR's standard API answers a question once, and the answer is out of date as
soon as the data changes. ReFHIR keeps the question open and sends the new answer whenever a change
affects it, so a screen or an AI agent always works from what is true now, with no polling or refresh
code of its own.

## What that looks like

A client runs a FHIR query and **stays subscribed to its result**. When a committed write changes
what the query returns, the server re-runs it and pushes the new result.

```http
GET /Observation?patient=Patient/123&status=final   <- an ordinary FHIR query: one answer
ws://host/sync   subscribe: Observation?patient=Patient/123&status=final
                 <- the matches now, then a new result whenever a commit changes them
```

The idea behind it is one sentence: **queries, not events, are the primary unit of
synchronization.** An event says that something changed. A live query says what the answer is now,
which is what a screen or an agent needs. [How live queries work](/docs/concepts/how-it-works/) shows what the server does, and
[Live queries for AI agents](/docs/concepts/agents/) covers why it matters for agents that act on state
that may have changed while they were thinking.

## How it differs from a typical FHIR server

FHIR's standard API is request and response. After the answer arrives, staying current is left to
your code: polling, notifications, caches and refresh logic, written again in every product.
ReFHIR moves that work into the server.

- **The data changes.** A typical server leaves you to ask again, or to wire up notifications, which
  say that something changed and not what the list looks like now. ReFHIR re-runs your question and
  sends the new list.
- **A medication is stopped.** It leaves the list, and the new list simply does not contain it.
- **An AI agent is working.** It can check that what it read is still true before it acts.
- **Software that already speaks FHIR** can use it, because ReFHIR is also a standard FHIR server: REST,
  search, transactions and [SMART on FHIR](/docs/guides/smart/) authorization.

See [Live queries and FHIR Subscriptions](/docs/concepts/vs-subscriptions/) for how it differs from the
notification feature FHIR already has.

## Who it is for

- **People building screens that must stay current**, such as worklists and dashboards. Start with the
  [tutorial](/docs/tutorial/).
- **People building AI agents over patient data**, who want working context that stays current and a
  way to notice that it changed. Read [Live queries for AI agents](/docs/concepts/agents/).
- **People who want a FHIR server they can run themselves**: open source under Apache-2.0, as a Docker
  image, a Helm chart or a binary. See [Install](/docs/install/).

## Versions

ReFHIR is at version 0.x, so settings and the `/sync` protocol can change between releases. The
[changelog](/docs/reference/changelog/) says what changed. Upgrades are forward-only, so back up your
database before you upgrade.

## Next

- [Install](/docs/install/) and [Getting started](/docs/getting-started/): run it with Docker or from
  source, and try it with `curl`.
- [Tutorial: a live patient list](/docs/tutorial/): build something that updates itself.
- [FHIR API](/docs/guides/fhir-api/): what the server supports.
- [Source on GitHub](https://github.com/hai-hq/refhir): issues, releases and the changelog.
