# Security policy

ReFHIR stores and serves clinical data, so we take reports about its security seriously.

## Supported versions

ReFHIR is pre-1.0. Security fixes are made on the latest release and on `main`; older
releases are not patched.

## Reporting a vulnerability

**Please do not open a public issue, pull request or discussion for a vulnerability.**

Report it privately through GitHub: open the repository's **Security** tab, choose
**Report a vulnerability**, or go straight to the
[private reporting form](https://github.com/hai-hq/refhir/security/advisories/new).
Only the project maintainers can see what you submit.

Please include:

- the ReFHIR version or commit, and how you run it (Docker, Helm, from source);
- the database (SQLite, in-memory, Postgres) and whether `--smart` is on;
- the smallest configuration and request sequence that shows the problem;
- what an attacker gains: whose data, which operation, what scope or role they started with.

**Do not include real patient data** in a report, a log excerpt or a proof of concept. Use
synthetic resources such as those in [`fixtures/mock-fhir`](fixtures/mock-fhir/README.md).

We will acknowledge your report, tell you whether we consider it a vulnerability, keep you
informed while we fix it, and agree a disclosure date with you. We credit reporters in the
release notes unless you prefer not to be named.

## What is in scope

- Bypassing authentication or SMART scope and compartment enforcement, on REST, GraphQL or
  `/sync`, including reading or writing another patient's resources.
- Evading, falsifying or silently truncating the audit trail
  ([governance](docs/governance.md)), or evading rate limits and quotas.
- Injection through search parameters, GraphQL, FHIRPath, CQL or the mapping language.
- Server-side request forgery or arbitrary file access through package installation,
  terminology or export.
- Remote code execution, memory-safety problems, and requests that make the server do
  disproportionate work for their size.
- Secrets leaking into logs, responses or the database.

## What is out of scope

- `--smart-demo-login`. It accepts any username and verifies nothing, and exists only for local
  testing and conformance runs.
- A server running **without** `--smart` on an untrusted network. Without it every endpoint is
  open, as documented.
- Attacks that need write access to the database. The audit chain narrows where such an
  attacker can hide but does not remove the places, as
  [docs/governance.md](docs/governance.md#rate-limits-quotas-metering-and-audit) says.
- Findings in dependencies that do not affect ReFHIR. Report those upstream; we track
  advisories with Dependabot and `cargo deny`.

For how to run ReFHIR safely, see [deploy/production.md](deploy/production.md).
