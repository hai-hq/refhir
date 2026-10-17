# Configuration

## Configuration file

Set `[fhir].version = "r4"` (default) to install that release’s defaults (**hl7.fhir.r4.core@4.0.1**, **hl7.fhir.uv.sdc@4.0.0**) from [packages.fhir.org](https://packages.fhir.org) on first startup (skipped when already in the database). Release archives and container images install them from a prepared file beside the binary and reach no registry; a binary you built yourself has none, and the server downloads nothing unless `[conformance].package_download = true`. `[conformance].package_bundle` names another prepared file, or `"off"` to ignore any file. A file that is there but unusable stops the start. Add other IGs with `[conformance].packages` (`name@version`, a `.tgz`, or a local folder). See [refhir.toml.example](../refhir.toml.example).

**Production:** [deploy/refhir.production.toml](../deploy/refhir.production.toml) and [deploy/production.md](../deploy/production.md) (Postgres, SMART secrets via env, Helm, `cargo install --git …`).

Configuration file search order: `./refhir.toml`, `/etc/refhir/refhir.toml`, or `REFHIR_CONFIG`. CLI flags override the file. In containers, set `REFHIR_CONFIG=/etc/refhir/refhir.toml` and mount the file there.

`[server].default_page_size` (or `REFHIR_DEFAULT_PAGE_SIZE`, and it has no CLI flag) is how many matches a search `GET` returns on a page when it names no `_count`: 100 by default, and `0` returns every match in one response. See [FHIR API](fhir-api.md#search).

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
