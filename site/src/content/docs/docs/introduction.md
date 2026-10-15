---
title: Introduction
description: What ReFHIR is, what it does, and where it stands today.
---

ReFHIR is an open-source **reactive FHIR R4 server**, written in Rust, from the nonprofit
[HAIHQ](https://haihq.org). It treats FHIR data as live application state.

A client runs a FHIR query and **stays subscribed to its result**. When a committed write changes
what the query returns, the server re-runs it and pushes the new result. Web apps and AI agents get
"what is true now?" from the data layer, instead of each one rebuilding polling, caching and
invalidation.

```http
GET /Observation?patient=123&status=final        <- a snapshot, as with any FHIR server
ws://host/sync   subscribe: Observation?patient=123&status=final
                 <- the matches now, then a new result whenever a commit changes them
```

The idea behind it is one sentence: **queries, not events, are the primary unit of
synchronization.** [Design](/docs/concepts/design/) explains the reasoning, including why it matters for AI
agents that act on state that may have changed while they were thinking.

## It is also an ordinary FHIR server

Existing clients keep working. ReFHIR speaks standard FHIR REST, search and transactions, and
[SMART on FHIR](/docs/guides/smart/) for authorization. See the [FHIR API](/docs/guides/fhir-api/)
page for exactly what is supported.

## Where it stands

:::caution[Early software]
ReFHIR is **version 0.x**. Configuration, the `/sync` protocol and the HTTP API can still change
before 1.0. It is not a certified product. Back up your database before upgrading.
:::

One thing is not built yet: commit-time revalidation, where the assumptions an agent read with must
still hold when it commits. It is the direction the dependency tracking points at, and it is not
part of the server today.

## Next

- [Getting started](/docs/getting-started/): run it with Docker or from source, and try it with `curl`.
- [FHIR API](/docs/guides/fhir-api/): what the server supports.
- [Source on GitHub](https://github.com/hai-hq/refhir): issues, releases and the changelog.
