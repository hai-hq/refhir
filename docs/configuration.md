# Configuration

## Configuration file

Set `[fhir].version = "r4"` (default) to install that release’s defaults (**hl7.fhir.r4.core@4.0.1**, **hl7.fhir.uv.sdc@4.0.0**) from [packages.fhir.org](https://packages.fhir.org) on first startup (skipped when already in the database). Release archives and container images install them from a prepared file beside the binary and reach no registry; a binary you built yourself has none, and the server downloads nothing unless `[conformance].package_download = true`. `[conformance].package_bundle` names another prepared file, or `"off"` to ignore any file. A file that is there but unusable stops the start. Add other IGs with `[conformance].packages` (`name@version`, a `.tgz`, or a local folder). See [refhir.toml.example](../refhir.toml.example).

**Production:** [deploy/refhir.production.toml](../deploy/refhir.production.toml) and [deploy/production.md](../deploy/production.md) (Postgres, SMART secrets via env, Helm, `cargo install --git …`).

Configuration file search order: `./refhir.toml`, `/etc/refhir/refhir.toml`, or `REFHIR_CONFIG`. CLI flags override the file. In containers, set `REFHIR_CONFIG=/etc/refhir/refhir.toml` and mount the file there.

`[server].default_page_size` (or `REFHIR_DEFAULT_PAGE_SIZE`, and it has no CLI flag) is how many matches a search `GET` returns on a page when it names no `_count`: 100 by default, and `0` returns every match in one response. See [FHIR API](fhir-api.md#search).

## Upgrading a package

Start the server with a newer version of a package it already holds (`--package ./ig-0.2.0`, `name@0.2.0`, or `[conformance].packages`) and it serves what that version defines. The newest version of a package is the one in force, so the same `--package` can stay on every start and moving to a new release is a change to it.

A newer version replaces the older one: from the moment it is installed the server serves only what the newer version defines. What the new version leaves out is gone, and what it changes is what the server uses from then on. Resources of a type that only the older version defined stay in the database, but the server does not serve them. A version that is written differently but means the same, such as `0.10` for `0.10.0`, replaces the one installed.

The older version's rows are removed at the next start or the next `$install`, not at once: its profiles, value sets, code systems, search parameters and what was indexed under a search parameter that only it defined. That is how a replica can go on reading them until it has moved to the new catalog. On a database that several tenants share, a tenant gives up the older version the same way, and a package the tenants have in common leaves the shared catalog when the last tenant that had it has given it up.

For the package's search parameters:

- One that the new version defines again takes the new definition. If its expression changed, the resources already stored are indexed again for it before the server answers requests (or, for `$install`, before it returns), and every resource written after is indexed with it. One whose type, expression and targets did not change is not indexed again.
- One whose `code` changed is searched by the new code only. A search that names the old code is refused like any parameter the server does not have (`400`, with the default strict handling), and the CapabilityStatement lists the new code.
- One that the new version leaves out is no longer accepted or listed.
- Parameters of other packages are never replaced. If a parameter of the new version has the same resource type and code as a parameter of another package but another expression, the install is refused and the database stays as it was.

The same holds for a package that another package depends on: installing one that needs a newer version of it replaces the older version.

A package that replaces an older version counts as installed when it replaces it. That matters only when two packages define the same profile: the server uses the one with more elements and, if both have as many, the one installed first.

A replica keeps answering from the catalog it had until the process that installed the package has finished indexing, and only then takes up the new one. Until then it still finds the profiles, value sets and code systems of the replaced version, which stay stored, and the index of a search parameter the new version dropped, so it lists and answers that parameter as before. It answers a parameter whose expression changed from an index that is being rebuilt, so a search by it can return a mix of the old and the new until indexing is done, and it does not know a parameter the new version adds until that parameter's index is complete.

Installing the same version again, or a version older than the one the server holds, changes nothing, so a server that is started again on an earlier release of its package keeps what it has. A new version that cannot be installed leaves the database on the version it had, and the server can be started again with either.

## CLI reference

| Flag | Purpose |
|------|---------|
| `--config path` | Config file (also `REFHIR_CONFIG`) |
| `--sqlite path` | SQLite file (default `refhir.sqlite`) |
| `--memory` | SQLite in RAM; no file (not with `--sqlite` or `--postgres`) |
| `--postgres url` | Postgres (not with `--sqlite` or `--memory`; or `REFHIR_POSTGRES_URL`). TLS is set in the URL: see [Postgres and TLS](operations.md#postgres-and-tls) |
| `--bind host:port` | Default `127.0.0.1:8080` |
| `--package spec` | FHIR IG. Repeatable. Installed after `[fhir].version` defaults. Directory, `.tgz`, or `name@version`. |
| `--smart` | SMART App Launch 2.2 (`/.well-known/smart-configuration`, `/oauth/*`) |
| `--public-url url` | SMART `issuer` URL (required with `--smart` on non-loopback bind) |
| `--smart-patient id` | Patient for `launch/patient` when no EHR context |
| `--smart-encounter id` | Encounter for EHR launch |
| `--smart-fhir-user Resource/id` | `fhirUser` claim (demo login only) |
| `--oidc-issuer url`, `--oidc-client-id id` | Sign SMART users in through an OpenID Connect provider (secret: `REFHIR_OIDC_CLIENT_SECRET`) |
| `--smart-demo-login` | Testing only: accept any username, verify nothing |
| `--references strict\|fhir` | How references are checked on write (default `strict`; `reference_policy` under `[conformance]`). See [FHIR API](fhir-api.md#reference-checking) |
| `--export-dir path` | Where `$export` writes its NDJSON files (`[export].dir`) |
| `--terminology url` | Pull terminology from this URL into the internal index (`[terminology].urls`). Not consulted per request |
| `--sdc-archiver` | Store completed `QuestionnaireResponse` resources unchanged instead of validating them first (the SDC form archiver role; `[features].sdc_archiver`) |
| `--smart-dcr-token token`, `--smart-confidential-secret secret` | Dynamic client registration token and the `confidential` client's secret. Prefer `REFHIR_SMART_DCR_TOKEN` and `REFHIR_SMART_CONFIDENTIAL_SECRET` in production |
| `--version`, `-V` | Print the version and exit |
| `--update-as-create` | Let `PUT [type]/[id]` create a resource under an id the client names (also `[conformance].update_as_create`). Off by default: the server assigns ids. |
| `--role primary\|standby\|replica` | What this process does next to others on one database (default `primary`; or `REFHIR_ROLE`). See [Several processes](clustering.md#several-processes-on-one-database) |
| `--standby` | Same as `--role standby` |
| `--primary-url url` | Where a replica sends writes (or `REFHIR_PRIMARY_URL`) |
| `--db-pool n` | Most Postgres connections (default 8), or on SQLite the read connections next to its one writer (default 4); or `REFHIR_DB_POOL` |
| `--tenant name` | The tenant this process serves, in a Postgres database that many tenants share (`tenant` under `[database]`, or `REFHIR_TENANT`). See [Tenants](clustering.md#tenants-on-one-database) |
| `refhir package-bundle [--out file]` | Prepare the release's FHIR packages ahead of time into one file that a first start installs instead of downloading them. See [Preparing the packages](../deploy/production.md#preparing-the-packages) |
| `refhir tenant create\|list\|delete` | Add, list and remove the tenants of a shared Postgres database. See [Tenants](clustering.md#tenants-on-one-database) |
| `refhir migrate --status\|--dry-run` | Report the database's schema version and what startup would do to it, without changing anything. See [Schema migrations](migrations.md) |
