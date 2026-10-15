# Development

How to build, test and change ReFHIR. For the architecture see [Design](design.md); for the
rules about contributing see [CONTRIBUTING.md](../CONTRIBUTING.md).

## Prerequisites

| Tool | Version | For |
|------|---------|-----|
| Rust | 1.88, installed by `rustup` from [rust-toolchain.toml](../rust-toolchain.toml) | the server |
| Go | 1.23+ | the Go client, `seed` and `loadgen` |
| Node | 22.12+ (the TypeScript client alone needs 20+) | the website, and the TypeScript client |
| Docker | any recent | the image, and the Postgres TLS tests |
| [just](https://github.com/casey/just) | optional | shortcuts in the [justfile](../justfile) |
| Python 3 | optional | `scripts/` and the fixture generator |

The pinned toolchain is installed without `clippy` and `rustfmt`. Add them once with
`rustup component add clippy rustfmt`. Use that toolchain for the checks: CI runs 1.88.0, and a
newer clippy reports lints that one does not know.

## Layout

```text
crates/     the Rust workspace (see design.md for what each crate does)
clients/    Go and TypeScript clients, and the seed and load-generator commands
deploy/     Docker, the Helm chart, an HAProxy cluster, the Inferno SMART profile, production notes
docs/       this documentation
fixtures/   mock FHIR data for demos
scripts/    performance checks, the package bundler, the Postgres TLS test helper
site/       the website: landing page and documentation, built with Astro
```

## The website

The landing page and the documentation site live in [`site/`](../site/README.md). It is Astro and
Starlight, fully static. The Markdown under `docs/` stays the source of truth for the pages the
site shows: the build copies it, so edit the original file, not a copy.

```bash
(cd site && npm ci && npm run build && npm test)
```

## Build and test

```bash
just check          # formatting, clippy (a warning fails), and the Rust tests: what CI runs first
cargo test --workspace
cargo fmt --all
```

The Go and TypeScript clients start a real `refhir` process, so build it first:

```bash
cargo build -p refhir
(cd clients/go && REFHIR_BIN=../../target/debug/refhir REFHIR_SKIP_DEFAULT_PACKAGES=1 go test ./...)
(cd clients/ts && npm ci && npm run build && REFHIR_BIN=../../target/debug/refhir REFHIR_SKIP_DEFAULT_PACKAGES=1 npm test)
```

`REFHIR_SKIP_DEFAULT_PACKAGES=1` keeps those processes from downloading the R4 package; the
Rust tests use the copies under `crates/fhir/fixtures`.

Check that the Markdown links still resolve:

```bash
python3 scripts/check-doc-links.py
```

## Tests that need a database

The Postgres tests skip themselves when no database is configured. CI runs them with a Postgres
service, and sets `REFHIR_REQUIRE_POSTGRES=1` so that a missing database fails the build instead.

- **Postgres.** Set `REFHIR_TEST_POSTGRES_URL` (a URL or `host=... user=...`) to a role that may
  `CREATE DATABASE`. The tests include `crates/refhir/tests/cluster.rs`, which starts real
  primary, standby and replica processes and kills the primary while writes are in flight. The
  lease tests make a database of their own each, so they do not disturb each other.
- **Postgres over TLS.** `out="$(scripts/postgres-tls-test.sh up)" && eval "$out"` starts a
  throwaway TLS-only Postgres (a container named `refhir-tls-pg-test`) and exports the two
  variables the tests read. Then run `cargo test -p refhir-postgres tls`, and finish with
  `scripts/postgres-tls-test.sh down`. Without those variables the TLS tests are skipped, unless
  `REFHIR_REQUIRE_POSTGRES_TLS` is set.

The workflow that runs all of this is [.github/workflows/ci.yml](../.github/workflows/ci.yml).
Which jobs run where: a pull request runs formatting, clippy, build and test, `cargo deny`, the
Helm chart and the docs links. The jobs marked "main only" there (Postgres tests, the release
build, the container image) run on every push to `main`, and so does the Inferno conformance
workflow when SMART code changes. To run them on a branch, use *Run workflow* on the Actions tab;
tick *perf* to include the 45-minute performance check as well.

## Changing the schema

Read [Schema migrations](migrations.md#adding-a-migration) first. The short version: add a new
numbered migration to both backends and never edit one that has shipped.

## Regenerating fixtures

The server ships no FHIR packages, so the test suites use a copy of `hl7.fhir.r4.core` under
`crates/fhir/fixtures/r4`. To rebuild it from the published package:

```bash
python3 crates/fhir/generate-r4.py
```

## Releases

Maintainers: see [Releasing](releasing.md).
