---
title: Environment variables
description: Every REFHIR_ environment variable the server reads, what it does, and when to use it.
---

The server reads these variables at startup. Where a setting also exists in the configuration file or
as a flag, the order is: the file, then the environment, then the command line, with each later
source overriding the one before. Use the environment for secrets and for values that change
between deployments, so they stay out of the config file.

## Server and database

| Variable | What it does |
|---|---|
| `REFHIR_CONFIG` | Path to the configuration file. Without it the server looks for `./refhir.toml`, then `/etc/refhir/refhir.toml`. |
| `REFHIR_POSTGRES_URL` | The Postgres connection URL. Overrides `[database].postgres_url`. TLS is chosen in the URL: with no `sslmode` there is no TLS. |
| `REFHIR_PUBLIC_URL` | The server's public URL, used as the SMART issuer. Overrides `[server].public_url`. Required for SMART on a non-loopback bind. |
| `REFHIR_ROLE` | `primary`, `standby` or `replica`. The same as `--role`. See [Several processes](/docs/operate/clustering/). |
| `REFHIR_PRIMARY_URL` | Where a replica sends writes. The same as `--primary-url`. |
| `REFHIR_DB_POOL` | A positive number: the most Postgres connections (default 8), or on SQLite the read connections next to the one writer (default 4). The same as `--db-pool`. |
| `REFHIR_TENANT` | The tenant this process serves, in a Postgres database that many tenants share. The same as `--tenant`, and `[database].tenant`. Create tenants with `refhir tenant create`. See [Tenants](/docs/operate/clustering/#tenants-on-one-database). |
| `REFHIR_DEFAULT_PAGE_SIZE` | How many matches a search `GET` returns on a page when it names no `_count`, with a `next` link for the rest. The default is 100, and `0` returns every match in one response. Overrides `[server].default_page_size`. There is no flag. |
| `REFHIR_UPDATE_AS_CREATE` | Set to `true` to let `PUT [type]/[id]` create a resource under the id the client names. The same as `--update-as-create`. |

## Packages

| Variable | What it does |
|---|---|
| `REFHIR_PACKAGE_DOWNLOAD` | `true` lets the server download FHIR packages: the release defaults when there is no prepared file, `name@version` entries of `[conformance].packages`, and `$install`. Off by default, so nothing is downloaded unless it is set. |
| `REFHIR_PACKAGE_REGISTRY` | Where FHIR packages are downloaded from once downloading is on: a registry or mirror URL, or `s3://bucket/prefix`. The default is `https://packages.fhir.org`. |
| `REFHIR_PACKAGE_BUNDLE` | The file of prepared packages (made by `refhir package-bundle`) that a first start installs. Found beside the binary when unset; `off` says not to look. A file that is named and is not there, or that is there and unusable, stops the start. |
| `REFHIR_PACKAGE_CACHE` | The on-disk cache for downloaded packages. Without it the cache is `package-cache` next to the SQLite file, or `.refhir/packages` for an in-memory or Postgres database (`.refhir/packages/<tenant>` for a tenant). |
| `REFHIR_SKIP_DEFAULT_PACKAGES` | Skips installing the release's default packages, so the server knows no resource types until you add a package with `--package`. Not for production. |
| `REFHIR_S3_ACCESS_KEY_ID` | The access key for an `s3://` package source. |
| `REFHIR_S3_SECRET_ACCESS_KEY` | The secret key for an `s3://` package source. |
| `REFHIR_S3_SESSION_TOKEN` | The session token, for temporary keys. `AWS_SESSION_TOKEN` is read too. |
| `REFHIR_S3_ENDPOINT` | The store's URL, for S3-compatible stores. `AWS_ENDPOINT_URL_S3` and `AWS_ENDPOINT_URL` are read too. |
| `REFHIR_S3_REGION` | Part of the request signature. The default is `us-east-1`; some stores want another value, for example `auto` for R2. `AWS_REGION` and `AWS_DEFAULT_REGION` are read too. |

See [Production deployment](/docs/operate/production/#package-source) for building your own package
bundle.

## SMART and sign-in

| Variable | What it does |
|---|---|
| `REFHIR_SMART_DCR_TOKEN` | The initial access token for `POST /oauth/register`. If it is not set the server generates one and prints it once, only when standard error is a terminal. |
| `REFHIR_SMART_CONFIDENTIAL_SECRET` | The secret for the built-in `confidential` client. |
| `REFHIR_OIDC_ISSUER` | The OpenID Connect issuer. Overrides `[smart.oidc].issuer`. |
| `REFHIR_OIDC_CLIENT_ID` | The OpenID Connect client id. Overrides `[smart.oidc].client_id`. |
| `REFHIR_OIDC_CLIENT_SECRET` | The OpenID Connect client secret. Keep it out of the config file. |
| `REFHIR_SIGNING_KEY_PASSPHRASE` | Encrypts the SMART signing key stored in the database. Without it the key is stored unencrypted and a warning is logged. Losing or changing it makes the stored key unreadable. |
| `REFHIR_ALLOW_REMOTE_DEMO_LOGIN` | Lets `demo_login` run on a non-loopback bind. For throwaway test servers only. |

Set the DCR token and the confidential secret explicitly whenever you run a standby or a replica: a
generated value changes on every restart, and those roles refuse to start without both pinned. See
[Register a SMART app](/docs/guides/register-smart-app/).

## Health cards

| Variable | What it does |
|---|---|
| `REFHIR_HEALTH_CARDS_KEY_FILE` | The PEM file with the key that signs SMART Health Cards: an unencrypted PKCS#8 private key on P-256. Overrides `[health_cards].key_file`. Without a key the server issues no cards. See [Health cards](/docs/guides/health-cards/). |

## Audit

| Variable | What it does |
|---|---|
| `REFHIR_AUDIT_KEY` | The key for the audit trail's hash chain: at least 16 bytes, for example `openssl rand -hex 32`, the same on every server and kept outside the database. Without it the chain catches mistakes but proves nothing against someone who can write the database. `GET /_audit/verify` checks it. |
