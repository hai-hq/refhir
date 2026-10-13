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

## Rate limits, quotas, metering and audit

What each setting does is in the [README](../README.md#rate-limits-quotas-metering-and-audit); this is what to decide when you run it. [refhir.production.toml](refhir.production.toml) turns the rate limit on and leaves quotas for you to size.

### Tell it how many proxies are in front

Per-address limits, the failed-sign-in limit and the audit trail all need the client's address. Behind a load balancer or ingress the connection's address is the proxy's, so **every client shares one bucket** until `[server] trusted_proxy_hops` says how many proxies to believe. Count the hops that append to `X-Forwarded-For`: a Kubernetes ingress is 1; a cloud load balancer in front of an ingress is 2.

Also say where the proxies connect from, with `[server] trusted_proxy_peers = ["10.0.0.0/8"]` (the Helm value is `server.trustedProxyPeers`). `X-Forwarded-For` is then believed only on a connection from one of those networks. Without it a client that reaches the server directly, such as through a node port or a pod address, can write the header itself: it gets a fresh bucket for every address it invents, and can burn another address's failed-sign-in allowance by naming it. ReFHIR warns at start-up when hops are set and peers are not.

Check it rather than trusting your count. Call the server from an address you know, then read back what it recorded:

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" 'https://refhir.example.com/_requests?_count=1'   # "remoteIp"
```

If `remoteIp` is a proxy's address, the number is too low. If it is whatever you put in a forged `X-Forwarded-For`, it is too high, and a client can choose its own address.

### Sizing a limit

Start by observing: leave `[rate_limit]` and `[quota]` off, run for a week, and read `GET /_usage?client=...` for what each client actually uses (`requests`, `bytesOut`, and by class). Set a client's rate at a few times its busiest period, and the burst for the largest batch it sends at once. A cost is in the same units as the rate: with `requests_per_second = 50` and `export = 20`, a caller can start two exports a second at most, and one in a burst of 100 leaves room for 80 reads.

`burst` must be at least the cost of the dearest request a caller can make, or that request could never be served; ReFHIR refuses to start otherwise. It also refuses a `[quota]` without `--smart` (a quota is per client, and without SMART there is none) and a `[rate_limit.clients.*]` for the same reason.

### Several servers

- **Rate limits are divided automatically.** Servers sharing a database count one another with a heartbeat every ten seconds, and each takes an equal part of every rate limit, bucket and connection cap (never a bucket smaller than the dearest request), so you configure the limit you want for the *fleet*. `GET /_usage` shows the number a server is dividing by (`servers`). Scaling up or down adjusts within about ten seconds, and a server that stops removes itself at once. This assumes the balancer spreads a caller's requests about evenly: where it pins each caller to one server (sticky sessions), set `[rate_limit] servers = 1`, or each caller gets one Nth of what you meant. `servers = N` pins the count if you would rather not have it measured.
- **Quotas are shared** through the database, so all servers must use the same one. Each rereads a client's totals every `refresh_seconds` (default 30), so a client can overshoot by about one interval of what the *other* servers serve. For a hard cap, set the quota a little under the real one.
- **Audit and usage rows** from all servers land in the same tables; each server adds its own counters.

### The database

Usage, the request log and the audit trail are three tables (`usage_daily`, `request_log`, `audit_log`) in the same database as the clinical data, created on first start. Writes are batched every two seconds, so they cost a transaction every two seconds and not one per request.

- `audit_log` grows by about a row per resource touched (a few hundred bytes). It is kept forever unless `[audit] retention_days` is set; set it to what your regulations require (6 years is 2190). **Back it up with the rest of the database**, and do not truncate it.
- `request_log` is pruned to `[request_log] retention_hours` every minute. At a sustained 50 requests a second, 72 hours is about 13 million rows; shorten it, or set `store = false` and keep only the structured log lines, if you ship logs elsewhere.
- Nothing updates or deletes an audit row except the retention you set.
- Rows are bounded in size however a caller phrased the request, but a flood of requests the server answers still leaves a row each. If the server is reachable from an untrusted network, switch the rate limit on.

### Keep the audit trail honest

The audit trail is hash-chained so that an edit, a deletion from the middle, or a forged row shows. Make that mean something:

1. **Set `REFHIR_AUDIT_KEY`** (`openssl rand -hex 32`) in your secret store, the same value on every server, and **not** in the database or the config file. Without it anyone who can write the table can recompute the chain, and ReFHIR warns at start-up. Choose it once: after a change, older rows verify only against the old key.
2. **Keep the checkpoints.** A chain cannot show that its newest rows were deleted. The server logs `audit checkpoint` lines (target `refhir_audit`, with `id` and `hash`) whenever the newest row changes; ship them to somewhere append-only, such as your log platform with a retention lock.
3. **Verify on a schedule.** `GET /_audit/verify?checkpoint=<id>:<hash>` with a recent checkpoint reports `verified: true` only if every link holds and the trail still contains that row. It reads the whole trail, so run it nightly and not on a dashboard; one runs at a time.

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  "https://refhir.example.com/_audit/verify?checkpoint=18342:9f2c...(64 hex characters)"
```

An alert on `"verified": false` is worth paging for: `breaks` names the rows (`edited`, `detached`, `unhashed`), and `checkpoint` says `different`, `missing` or `truncated`.

### Watch for lost rows

If the database stops taking writes, rows wait in memory (up to 100,000 audit rows), are written when it recovers, and after that the newest are dropped. Alert on these:

- the log lines `could not write the audit trail`, `could not write usage` and `could not write the request log` (the database is refusing writes and the server is retrying);
- the error `buffer is full because the database is not taking writes` (rows are now being lost);
- `dropped` in `GET /_usage`, which counts rows lost since the server started;
- `sampled` in the same response, which counts refusals that were counted and not stored one by one, and `sampled` rows in `/_audit`: a steady stream means the server is being probed or flooded.

Also alert on throttled requests (`throttled` in `/_usage`, or the `429` rate at the proxy): a client that is refused often either needs a bigger limit or is misbehaving.

### Reading it

`/_audit`, `/_usage` and `/_requests` need a token with a **`system/` scope that covers `AuditEvent`**. Register a backend client for whoever reads them (`private_key_jwt`, scope `system/AuditEvent.read`) and keep it apart from your application clients: a `user/*.read` token does not qualify.

To feed a SIEM, poll `/_audit?date=ge<last>` and follow the `next` link, or ship the structured log lines (every line has `request_id`, which is the `request` filter on `/_audit` and `/_requests`).

### Stopping

`SIGTERM` and `SIGINT` write what is still buffered before the process exits, so a rolling deploy loses nothing. If the database does not take them within eight seconds, or the process is signalled a second time, it exits with the rows unwritten (and says so in the log). A hard kill (`SIGKILL`, an out-of-memory kill, a node failure) can lose up to two seconds of rows. Give the pod a termination grace period of at least ten seconds.

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
