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
  REFHIR_POSTGRES_URL: postgresql://refhir:...@postgres:5432/refhir?sslmode=verify-full&sslrootcert=/etc/refhir-pg-tls/ca.crt
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

Mount the CA that signed Postgres's certificate (a ConfigMap or Secret) at the `sslrootcert` path, not
under `/etc/refhir`, which the config ConfigMap occupies. The Helm chart does it for you:
`--set database.tlsSecret=<secret with ca.crt>` mounts that Secret at `/etc/refhir-pg-tls`.
With `verify-full` the certificate must be valid for the host name in the URL (for CloudNativePG,
the `-rw` Service name and its namespaced forms are in its server certificate; use the one you
connect with). `sslmode=require` encrypts but does not verify the server, and a URL with no
`sslmode` does not use TLS at all, so production should use `verify-full`. Client certificates
(`sslcert`, `sslkey`) are supported for servers that require them. Details:
[Postgres and TLS](../docs/operations.md#postgres-and-tls).

Helm chart: [helm/refhir](helm/refhir/README.md) (`values.yaml` + optional `secretEnv`).

## Rate limits, quotas, metering and audit

What each setting does is in the [governance guide](../docs/governance.md#rate-limits-quotas-metering-and-audit); this is what to decide when you run it. [refhir.production.toml](refhir.production.toml) turns the rate limit on and leaves quotas for you to size.

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
- **With replicas** (see "Availability and read scaling"), a write a replica receives is sent on to the primary, and the primary limits, meters and audits it as its own request, under the caller's token and the same request id. The replica logs it but does not count it or audit it as well, so a forwarded write is neither charged twice against a quota nor written twice to the audit trail. It is limited on the way in as well, by the replica's share of the caller's rate, and then by the primary's, so a caller who writes through several replicas is limited by the primary's share of the rate for those writes: set `[rate_limit] servers = 1` if that is too little. The primary sees a forwarded request come from the replica, so list the pod or node network among `[server] trusted_proxy_peers`, or the audit trail records the replica's address instead of the caller's.

### The database

Usage, the request log and the audit trail are three tables (`usage_daily`, `request_log`, `audit_log`) in the same database as the clinical data, with the table the servers count one another in (`governance_members`). Schema migration 2 creates them the first time a version of ReFHIR that has them starts on a database (see [docs/migrations.md](../docs/migrations.md)), so back the database up before that upgrade, as for any other. Writes are batched every two seconds, so they cost a transaction every two seconds and not one per request.

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

### Restrict who can sign in

By default anyone the provider authenticates is accepted. If the provider cannot be limited to your people, say so here; both settings are off until set, and when both are set a user must pass both.

```toml
[smart.oidc]
# Every named claim must carry at least one of its listed values. A claim holding an array
# matches if any element is listed. A missing claim does not match.
allowed_claims = { organization_id = ["org_abc123"] }
# Or by the domain of a verified `email` claim (exact domain, not subdomains; case-insensitive).
allowed_email_domains = ["example.com"]
```

The check runs on the verified ID token straight after the code exchange, before the consent page: a refused user never sees a consent page and no code is issued. The client application is sent back with `error=access_denied` and `error_description=this account is not permitted to sign in to this server`. Which claim failed, and what values are allowed, is written to the server log (at `warn`) and not to the user. Notes:

- Only ID token claims are checked. If the provider puts `organization_id` in its userinfo response but not the ID token, ask it to add the claim to the ID token (or request the scope that does).
- `allowed_email_domains` needs `email` and `email_verified: true` in the ID token (add `email` to `[smart.oidc].scopes`). An address with no `email_verified` claim, or one that is not `true`, is refused.
- An entry with no values, or an empty domain, is a startup error rather than a lock-out.

### Bind people to FHIR resources

A client that asks for the `fhirUser` scope needs to know which `Practitioner`, `Patient`, `RelatedPerson` or `Person` the signed-in person is. In order:

1. the ID token claim named by `fhir_user_claim` (default `fhirUser`), when it holds a relative `Type/id` or an absolute URL;
2. the `fhirUser` of an EHR launch, when there is one;
3. a lookup, when configured: ReFHIR searches its own data for the person.

```toml
[smart.oidc.fhir_user_lookup]
resource_type = "Practitioner"   # Practitioner (default), Patient, RelatedPerson or Person
claim = "sub"                    # ID token claim to search for: sub (default), email, or any other claim
search_param = "identifier"      # default
system = "https://idp.example.com/sub"   # sent as system|value (identifier only); leave out to send the value alone
```

With these settings a login by `sub` `u-123` runs `Practitioner?identifier=https://idp.example.com/sub|u-123`, and the person is bound only if **exactly one** resource comes back. No match, more than one match, a failed search, or a value that cannot be searched safely (it contains `,` `$` or `\`, or `|` when no `system` is set) binds nobody, and a `fhirUser` request then fails as it does without a lookup (`the identity provider did not supply a valid fhirUser claim, and no FHIR resource is bound to this user`); the reason is in the log. The identifier has to be put on the resource by whatever manages your practitioners, and a lookup on `email` is only used when the ID token has `email_verified: true`.

**Write access to this resource type is the trust boundary.** A lookup binds a person to whichever single resource carries the identifier, and an identifier is ordinary data: anyone who can create or edit that type (an app with `user/Practitioner.write`, say, for a Practitioner lookup) can plant a resource with someone else's identifier, which binds that person to it, or, if their own resource already exists, makes two matches and locks them out. Keep write access to the looked-up type with the system that manages your people. The search runs on the process that handles the sign-in, so on a replica a resource created in the last moments may not be visible yet and the login fails until the replica catches up (see [Several processes](../docs/clustering.md#several-processes-on-one-database)). The lookup only runs when the app asked for `fhirUser`, or when `patient_from_fhir_user` is on and the app needs a patient.

```toml
[smart.oidc]
# When the person is bound to a Patient on this server, that patient is the launch patient:
# the consent page does not offer a patient picker, and a patient_id in the form is ignored.
patient_from_fhir_user = true
```

`patient_from_fhir_user` is off by default, so existing deployments keep the picker. Turn it on when patients sign in as themselves. It applies to a Patient binding from the claim as well as from the lookup, to apps that ask for `patient/` or `launch` scopes. It does not apply to a practitioner (the picker stays), and an EHR launch for a different patient than the one who signed in ends the login with `access_denied`.

### Token introspection for a resource gateway

`POST /oauth/introspect` (RFC 7662) tells a gateway who a SMART access token was issued to, without giving it the token's internals. For an unexpired `tok-` access token it returns:

```json
{"active": true, "client_id": "my-app", "sub": "<OIDC subject>", "scope": "openid fhirUser patient/Patient.rs",
 "exp": 1760000000, "token_type": "Bearer", "iss": "https://fhir.example.com",
 "fhirUser": "https://fhir.example.com/Practitioner/dr-1", "patient": "p1"}
```

`sub` is the `sub` of the ID token the user signed in with, `client_id` the application, and `fhirUser`, `patient` and `encounter` appear when the token has them. A token that is unknown, expired or revoked gets `{"active": false}` and nothing else. (A token from the `client_credentials` grant has `sub` `backend`.)

The caller must authenticate with one of:

- a confidential client's credentials: the built-in `confidential` client (`--smart-confidential-secret` / `REFHIR_SMART_CONFIDENTIAL_SECRET`) with HTTP Basic or `client_id` and `client_secret` in the form;
- a backend client registered at `POST /oauth/register` (`private_key_jwt`), with `client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer` and a `client_assertion` RS256 JWT whose `aud` is `<public_url>/oauth/introspect` (or `<public_url>/oauth/token`). An assertion addressed to introspection is accepted only here, never at `/oauth/token`;
- a valid SMART access token as a `Bearer` header.

Anything else, and every public client, gets `401 invalid_client`. Give the gateway its own registered backend client rather than the built-in secret, so its access can be rotated or revoked alone, and keep it off the network paths apps use.

```bash
curl -s https://fhir.example.com/oauth/introspect \
  -u confidential:$REFHIR_SMART_CONFIDENTIAL_SECRET \
  -d token=tok-...
```

**`demo_login = true` (or `--smart-demo-login`) is for testing only.** It accepts any username and verifies nothing, so anyone who can reach the server can sign in as anyone and read patient data. It cannot be combined with `[smart.oidc]`, and the server refuses to start with it on a non-loopback bind unless `REFHIR_ALLOW_REMOTE_DEMO_LOGIN=1` is set.

## FHIR version and Implementation Guides

`[fhir].version = "r4"` (default) installs **`hl7.fhir.r4.core@4.0.1`** and **`hl7.fhir.uv.sdc@4.0.0`**, and the packages they depend on (`hl7.terminology.r4`, `hl7.fhir.uv.extensions.r4`, `hl7.fhir.uv.tools.r4` and `hl7.fhir.uv.xver-r5.r4`), on first startup. SDC also lists `hl7.fhir.r4.examples`, which holds example resources and a second copy of the core definitions, so a package that is only a dependency and holds examples is left out; name it in `[conformance].packages` if you want it. Packages already in the conformance database are not reinstalled.

**Release archives and container images carry these packages already**, prepared when they were built, in a file called `refhir-packages.bundle` beside the `refhir` binary (in the image, in `/usr/local/share/refhir/`). A first start stores what is in the file: it reaches no registry and needs no network and no package cache. **The server does not download packages unless you set `[conformance].package_download = true`.** Without that setting, a first start that has no prepared file, or has one it cannot use (it is damaged, or was made for another release), stops with an error that says which, and nothing is fetched from anywhere. A binary you built yourself has no such file: make one with `refhir package-bundle` (see [Preparing the packages](#preparing-the-packages)), or turn downloading on (see [Package source](#package-source)).

Add other implementation guides with `[conformance].packages` (`name@version`, `.tgz`, or directory), or repeat `--package` on the CLI.

**Memory.** Storing the default packages is the most memory the server ever asks for, so it is what sets the limit for a container: a first start needs more than a running server, and a restart on a database that already has its packages needs less than the first start. Start from 512 MiB, and watch your own first start before you tighten it; give it more if you add large implementation guides with `[conformance].packages`. A container with less than a start needs is killed (exit status 137) before the server is ready, and starts again with the same result.

### Preparing the packages

`refhir package-bundle` downloads the release defaults and everything they depend on, reads and expands them the way a first start would, and writes the result to one file:

```bash
refhir package-bundle --out refhir-packages.bundle
```

`--spec name@version` (a `.tgz` or a directory also works) prepares other packages, and may be repeated; naming any replaces the defaults. `--registry` and `--cache` say where to download from and where to keep the downloads, like `REFHIR_PACKAGE_REGISTRY` and `REFHIR_PACKAGE_CACHE`. The file does not depend on the machine it was made on.

A server uses the file when it holds the packages the release asks for, and installs from it what its database does not have. It looks beside the `refhir` binary, then in `../share/refhir/` from it, or where `[conformance].package_bundle` (or `REFHIR_PACKAGE_BUNDLE`) says; a file named there that is not a file is an error, and `off` there says not to look. A file that is there but does not hold the release's packages, or is damaged, stops the start with an error: the server does not go to a registry instead, because a server that quietly fetched its packages from somewhere else would not be the one that was deployed. To put one in your own image:

```dockerfile
RUN refhir package-bundle --out /usr/local/share/refhir/refhir-packages.bundle
```

### Package source

A server downloads packages only if `[conformance].package_download = true` (or `REFHIR_PACKAGE_DOWNLOAD=true`) says it may. It is off by default, and with it off nothing here is reached: a package that is not installed and not in the package cache is refused, whether it is a release default with no prepared file, an entry of `[conformance].packages`, or the one a `$install` asks for (that answers `403`). Packages that come from a directory or a `.tgz` need no download.

`[conformance].package_registry` (or `REFHIR_PACKAGE_REGISTRY`) says where packages are downloaded from once downloading is on. It is used for the release defaults when there is no prepared file, their dependencies, `[conformance].packages` and `$install`:

| Value | Meaning |
|-------|---------|
| *(unset)* | `https://packages.fhir.org` |
| `https://mirror.example.com/fhir` | Any static host that serves `<base>/<name>/<version>` (CloudFront, GCS, an internal mirror) |
| `s3://bucket/prefix` | An S3 bucket (or S3-compatible store) holding the same layout, read with signed requests |

To control exactly which package versions you run, build a bundle once and host it yourself:

```bash
scripts/package-bundle.py --s3 s3://my-bucket/fhir-packages
```

The script downloads the release defaults plus everything they depend on, checks each tarball, writes `<name>/<version>` and a `manifest.json` (sizes and sha256), and uploads with the `aws` CLI. `--spec name@version` adds implementation guides, `--out dir` keeps a local copy only, `--endpoint-url` targets MinIO or R2. Then:

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

**Release is pinned per database:** `[fhir].version` is fixed when the database is first populated (e.g. `r4` → FHIR `4.0.1`). Changing `version` to another release (e.g. `r5`) on an existing SQLite/Postgres data directory **fails at startup**; provision a new database instead. Individual packages also cannot mix `fhirVersion` values.

## Bulk export

`$export` runs asynchronously and writes **NDJSON to `[export].dir`** on the server filesystem (`{job-id}.ndjson`). Clients poll `GET /export/{job}` and download `GET /export/{job}/file`.

To keep the files in object storage, mount a persistent volume at `/data/export` and copy completed files out with a job of your own.

## Availability and read scaling

One process is a single point of failure. On Postgres you can run a **standby** that takes over
within a few seconds when the primary is gone, and
**replicas** that serve reads:

- `refhir --postgres $URL` is the primary. It holds a lease on the database and refuses to start
  while another primary has it.
- `refhir --postgres $URL --standby` waits for the lease and then starts serving.
- `refhir --postgres $URL --role replica --primary-url http://<primary-or-balancer>` serves reads
  and sends writes to the primary.

Put a load balancer in front that checks `GET /healthz` (200 means it can take traffic, 503 means
it cannot) and sends nothing to a process that answers 503. Give replicas a `--primary-url` that
always reaches the current primary, such as a second balancer port that routes only to the process
whose `/healthz` says `"role":"primary"`. A ready-made Compose file and HAProxy configuration are
in [cluster/](cluster/README.md), and the Helm chart has `cluster.enabled`.

Things to know before relying on it:

- **Same secrets everywhere.** With SMART on, every process needs the same
  `REFHIR_SMART_DCR_TOKEN` and `REFHIR_SMART_CONFIDENTIAL_SECRET`; standbys and replicas refuse to
  start without them. Set `REFHIR_SIGNING_KEY_PASSPHRASE` the same on every process too.
- **A sign-in can be lost to a crash.** While a user is at the identity provider, the pending
  login is held by the process that started it, and the callback takes it, asks the provider,
  and puts it back. If that process dies in those moments the user simply signs in again.
  Nothing is issued twice and no token is lost.
- **Revocation is not instant on other processes.** Each process caches token lookups for two
  seconds, so a revoked token may still work on another process for up to that long.
- **Replicas are slightly behind.** They follow the primary within tens of milliseconds. Clients
  that must read their own writes send `X-Refhir-Min-Ts` (the Go and TypeScript clients do).
  The SMART patient picker reads the replica's own snapshot, so a patient created a moment ago can
  be missing from it until the replica refreshes. A request that must see it
  sends `X-Refhir-Min-Ts` like any other: `/oauth/*` goes through the same gate, so a replica that
  is behind hands the request to the primary. A replica with no `--primary-url` cannot hand it
  over, so it answers `503` with `Retry-After: 1` instead of an older snapshot.
- **Database connections.** Each process opens up to `REFHIR_DB_POOL` (default 8) Postgres
  connections, and a writer keeps one more for its lease. Size Postgres `max_connections` for all of them.
- **Failure behaviour.** A writer that loses its lease, or finds a newer writer in the database,
  exits (codes 70 and 71) rather than risk a second writer. Run it under a supervisor that
  restarts it; it comes back as a standby if started with `--standby`.
- **Backfills resume.** A new search parameter's index is built in the background and the
  parameter is searchable only when it finishes. If the primary dies part way, the standby that
  takes over claims the job and continues from where it stopped. Replicas do not see the
  parameter until the job is complete.
- **Export jobs and Subscription delivery** belong to the primary.

## Upgrading

The database schema is versioned and the server migrates it when it starts. **Migrations are not
reversible: back up the database first**, and use one ReFHIR version for every process that shares
a database (a database from a newer version makes an older server refuse to start). Restore the
backup into a scratch database and run `refhir migrate --dry-run` against it with the new binary
before you upgrade. Details: [docs/migrations.md](../docs/migrations.md).
