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
  The tenancy tests make an ordinary role, `refhir_rls_app` (so the test role needs `CREATEROLE`),
  and give it the databases they create. They run as the superuser by default, which row-level
  security does not bind, so a statement that forgot its tenant fails them. Run them again with
  `REFHIR_TEST_ROW_SECURITY=1 cargo test -p refhir-postgres tenancy` to run as the ordinary role,
  which checks that everything the stores do works with the policies applied. CI runs both.
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

Migrations are forward-only. [Schema migrations](migrations.md) explains how they run. To add one:

1. Add `NNNN_short_name.sql` to **both** `crates/sqlite/migrations/` and
   `crates/postgres/migrations/`, and an entry to `MIGRATIONS` in each backend's `migrate.rs`.
   The next version number is the last one plus one, with no gaps.
2. **Never edit a migration that has shipped.** Its checksum is in every database that applied it,
   and they would refuse to start. `the_baseline_is_frozen` pins version 1's checksum; pin new
   ones the same way.
3. A migration runs inside a transaction, together with any other pending ones. Do not use
   statements Postgres refuses there (`CREATE INDEX CONCURRENTLY`, `VACUUM`). It does not have to
   be idempotent: it runs once. Take care with statements that lock a busy table, since a
   standby or replica starting up waits for them.
4. Keep it additive where you can (new tables, new nullable columns, new indexes). A migration
   that rewrites data is not undone by anything but a restore.
5. Add a test that starts from a database at the previous version.

New SQLite and Postgres migrations should leave the two backends with the same logical schema,
and neither may change what a query returns without a code change that says so.

Postgres has a third stream. A database that holds many tenants (`refhir tenant create`) is made
from `crates/postgres/migrations-shared/`, where every table has an `instance_id` leading its
primary key, its unique constraints and its indexes, and `SHARED_MIGRATIONS` in `migrate.rs` lists
them. A change to `crates/postgres/migrations/` needs the same change there, with the tenant
column added, in a migration of the same version. `the_shared_layout_is_the_classic_one_plus_the_tenant_column`
applies both streams to scratch databases and fails when they differ by anything but the tenant
column and the short list of tables and columns the shared layout adds (`ADDED_TABLES` and
`ADDED_COLUMNS` in `tenancy_tests.rs`). The same rule applies to the SQL: a statement that reads
or writes a tenant's rows takes `Scope` (`scope.rs`) and names the tenant in every table it
touches, and a new one needs a case in the `tenancy_*` tests that runs it beside another tenant
holding the same keys.

Row-level security is the shared stream's third migration: every table that holds tenants' rows
(a column `instance_id`) needs `ENABLE` and `FORCE ROW LEVEL SECURITY` and a policy in the
migration that adds it, and a test lists the tables that lack one. The policies read the tenant a
connection serves from `refhir.tenant`, which every connection that touches tenant rows sets as it
connects (`connect_client_for`). A migration that reads or rewrites tenant rows therefore runs
with no tenant and sees none: it sets `refhir.tenant` for each tenant in turn.

## Regenerating fixtures

The server binary carries no FHIR packages (a release adds them as a prepared file, made with `refhir package-bundle`), so the test suites use a copy of `hl7.fhir.r4.core` under
`crates/fhir/fixtures/r4`. To rebuild it from the published package:

```bash
python3 crates/fhir/generate-r4.py
```

## Releases

Maintainers: see [Releasing](releasing.md).
