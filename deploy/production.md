# Production deployment

## Golden config

Start from [refhir.production.toml](refhir.production.toml): Postgres, public SMART URL, R4 defaults via `[fhir].version`, optional IGs in `packages`.

Mount it as `/etc/refhir/refhir.toml` or set `REFHIR_CONFIG`.

### Environment variables (secrets and overrides)

| Variable | Purpose |
|----------|---------|
| `REFHIR_CONFIG` | Path to `refhir.toml` |
| `REFHIR_POSTGRES_URL` | Overrides `[database].postgres_url` when set |
| `REFHIR_PUBLIC_URL` | Overrides `[server].public_url` when set |
| `REFHIR_SMART_DCR_TOKEN` | Bearer token for `POST /oauth/register` |
| `REFHIR_SMART_CONFIDENTIAL_SECRET` | Secret for built-in `confidential` SMART client |
| `REFHIR_OIDC_ISSUER` | Overrides `[smart.oidc].issuer` |
| `REFHIR_OIDC_CLIENT_ID` | Overrides `[smart.oidc].client_id` |
| `REFHIR_OIDC_CLIENT_SECRET` | OpenID Connect client secret (keep out of the config file) |
| `REFHIR_PACKAGE_REGISTRY` | Package source: a registry or mirror URL, or `s3://bucket/prefix` (default `https://packages.fhir.org`); see Package source |
| `REFHIR_PACKAGE_CACHE` | On-disk cache for downloaded `.tgz` packages |
| `REFHIR_SIGNING_KEY_PASSPHRASE` | Encrypts the SMART signing key stored in the database. Without it the key is stored in clear and a warning is logged. Losing or changing it makes the stored key unreadable; the server refuses to start rather than replace it |
| `REFHIR_ALLOW_REMOTE_DEMO_LOGIN` | Lets `demo_login` run on a non-loopback bind (throwaway test servers only; refused otherwise) |

CLI flags still override the file when passed explicitly.

### Kubernetes example

ConfigMap: non-secret `refhir.toml` (bind, `fhir.version`, `packages`, `public_url`).

Secret:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: refhir-secrets
stringData:
  REFHIR_POSTGRES_URL: postgresql://refhir:...@postgres:5432/refhir?sslmode=require
  REFHIR_SMART_DCR_TOKEN: "..."
  REFHIR_SMART_CONFIDENTIAL_SECRET: "..."
  REFHIR_OIDC_CLIENT_SECRET: "..."
```

Deployment `envFrom`:

```yaml
envFrom:
  - secretRef:
      name: refhir-secrets
env:
  - name: REFHIR_CONFIG
    value: /etc/refhir/refhir.toml
```

Helm chart: [helm/refhir](helm/refhir/README.md) (`values.yaml` + optional `secretEnv`).

## SMART sign-in (OpenID Connect)

With `[smart].enabled`, the server needs a way to sign users in. ReFHIR does not store passwords: it delegates login to your OpenID Connect provider (Keycloak, Okta, Entra ID, Auth0, ...) and refuses to start with SMART enabled and no login configured.

1. Register ReFHIR as a confidential client at the provider with redirect URI `<public_url>/oauth/oidc/callback` (or set `[smart.oidc].redirect_uri`).
2. Set `[smart.oidc]` (`issuer`, `client_id`, optional `scopes`) and `REFHIR_OIDC_CLIENT_SECRET`.
3. Make the provider put the user's FHIR identity in an ID token claim, `fhirUser` by default (`fhir_user_claim`), as `Practitioner/123`, `Patient/456` or an absolute URL. A client that asks for the `fhirUser` scope is refused when the claim is missing.

How it works: `/oauth/authorize` sends the browser to the provider (authorization code with PKCE, `state`, `nonce`, and a cookie tying the login to that browser). The callback redeems the code server-side and checks the ID token's signature against the provider's published keys, plus issuer, audience, expiry and nonce. The verified `sub` and `fhirUser` go into the SMART tokens; nothing the user types is trusted. The user then sees the consent page (patient picker, approve or deny).

The issuer must be `https` (plain `http` only on a loopback host). Provider metadata and keys are fetched at login time and cached for an hour.

**`demo_login = true` (or `--smart-demo-login`) is for testing only.** It accepts any username and verifies nothing, so anyone who can reach the server can sign in as anyone and read patient data. It cannot be combined with `[smart.oidc]`, and the server refuses to start with it on a non-loopback bind unless `REFHIR_ALLOW_REMOTE_DEMO_LOGIN=1` is set. The Inferno profile sets that; production must not.

## FHIR version and Implementation Guides

`[fhir].version = "r4"` (default) installs **`hl7.fhir.r4.core@4.0.1`** and **`hl7.fhir.uv.sdc@4.0.0`** from [packages.fhir.org](https://packages.fhir.org) on first startup. Downloads are cached under `[conformance].package_cache` (or `REFHIR_PACKAGE_CACHE`); packages already in the conformance database are not reinstalled. ReFHIR ships no FHIR packages: if the package source cannot be reached on a first start, the server does not start and says where it looked.

Add other implementation guides with `[conformance].packages` (`name@version`, `.tgz`, or directory), or repeat `--package` on the CLI.

**First container start** needs to reach the package source (below) to install release defaults; use a persistent `package_cache` volume so later restarts do not re-download.

### Package source

`[conformance].package_registry` (or `REFHIR_PACKAGE_REGISTRY`) says where packages are downloaded from. It is used for the release defaults, their dependencies, `[conformance].packages` and `$install`:

| Value | Meaning |
|-------|---------|
| *(unset)* | `https://packages.fhir.org` |
| `https://mirror.example.com/fhir` | Any static host that serves `<base>/<name>/<version>` (CloudFront, GCS, an internal mirror) |
| `s3://bucket/prefix` | An S3 bucket (or S3-compatible store) holding the same layout, read with signed requests |

To control exactly which package versions you run, build a bundle once and host it yourself:

```bash
scripts/package-bundle.py --s3 s3://my-bucket/fhir-packages
```

The script downloads the release defaults (read from `crates/fhir/src/package.rs`) plus everything they depend on, checks each tarball, writes `<name>/<version>` and a `manifest.json` (sizes and sha256), and uploads with the `aws` CLI. `--spec name@version` adds implementation guides, `--out dir` keeps a local copy only, `--endpoint-url` targets MinIO or R2. Then:

```toml
[conformance]
package_registry = "s3://my-bucket/fhir-packages"
```

`s3://` works with AWS S3 and with any S3-compatible store (MinIO, Cloudflare R2, Ceph, Backblaze B2, Wasabi): ReFHIR signs a plain `GET` with AWS Signature V4, so there is no SDK and no extra dependency. Settings are environment variables, so credentials stay out of the config file:

| Variable | Meaning |
|----------|---------|
| `REFHIR_S3_ACCESS_KEY_ID`, `REFHIR_S3_SECRET_ACCESS_KEY` | Access keys (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` also work). Set both. With neither, requests are unsigned, for a public bucket. Grant read-only access to the bundle prefix |
| `REFHIR_S3_SESSION_TOKEN` | For temporary keys |
| `REFHIR_S3_ENDPOINT` (or `AWS_ENDPOINT_URL_S3`) | `https://host[:port]` of the store. Switches to path-style addressing, which MinIO and most others need. Unset means AWS S3 |
| `REFHIR_S3_REGION` (or `AWS_REGION`) | Part of the signature. Default `us-east-1`; some stores want a specific value, for example `auto` for R2 |

Only static keys are supported: there is no IAM-role, IRSA, SSO or profile lookup. On Kubernetes put the keys in the chart's `secretEnv`. S3 answers 403, not 404, for a missing object when the keys have no list permission, so a wrong prefix can look like an access problem.

Tested against MinIO. Installing the default packages on a fresh database took about 20 s from MinIO on a laptop (release build), most of it parsing and storing.

**Tests only:** set `REFHIR_SKIP_DEFAULT_PACKAGES=1` when spawning `refhir` so integration tests avoid registry installs (Rust unit tests install fixtures directly). Do not use in production.

**Release is pinned per database:** `[fhir].version` is fixed when the database is first populated (e.g. `r4` → FHIR `4.0.1`). Changing `version` to another release (e.g. `r5`) on an existing SQLite/Postgres data directory **fails at startup**; provision a new database instead. Individual packages also cannot mix `fhirVersion` values (`InstallError::FhirVersion` on install).

## Bulk export and object storage

`$export` runs asynchronously and writes **NDJSON to `[export].dir`** on the server filesystem (`{job-id}.ndjson`). Clients poll `GET /export/{job}` and download `GET /export/{job}/file`.

**S3, GCS, and Azure Blob are not wired in yet.** For production you can:

- Mount a persistent volume at `/data/export` and pull files out of band, or
- Front ReFHIR with a job that copies completed files to object storage (future native support).

## Install the binary without Docker

The crate is not on crates.io yet (`publish = false` in the workspace). Install from GitHub:

```bash
cargo install --git https://github.com/hai-hq/haifhir.git refhir
```

Requires Rust 1.88+. Run with a config file:

```bash
refhir --config /etc/refhir/refhir.toml
```

When the project publishes to crates.io, the same command will work with `--package refhir` from the registry.
