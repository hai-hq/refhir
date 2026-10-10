# SMART Inferno validation

**Local lab only.** `docker-compose.yml` ships fixed OAuth secrets (`inferno-dcr`, `inferno-confidential`) and a public URL of `http://localhost:8080`. Do not use this profile on the public internet or for production.

Run the [Inferno SMART App Launch Test Kit](https://github.com/inferno-framework/smart-app-launch-test-kit) against a local ReFHIR instance with SMART enabled.

Set `REFHIR_PUBLIC_URL` if your FHIR base differs from `REFHIR_URL` (discovery issuer is checked against `REFHIR_PUBLIC_URL`, defaulting to `REFHIR_URL`).

## Quick start (ReFHIR only)

From the repository root:

```bash
docker compose -f deploy/inferno/docker-compose.yml up --build
```

In another terminal:

```bash
chmod +x deploy/inferno/*.sh
deploy/inferno/wait-ready.sh
deploy/inferno/seed.sh
deploy/inferno/smoke-discovery.sh
deploy/inferno/smoke-oauth.sh
```

ReFHIR listens on [http://localhost:8080](http://localhost:8080). OAuth state and the signing key persist in the database on the Docker volume.

### Dev credentials (Inferno UI)

| Setting | Value |
|--------|--------|
| FHIR base URL | `http://localhost:8080` |
| Issuer (from discovery) | `http://localhost:8080` |
| Public client id | `public` (pre-registered, loopback redirects) |
| DCR initial access token | `inferno-dcr` |
| Confidential client secret | `inferno-confidential` |
| EHR launch patient (public client) | `p1` (from `smart.patient` in config) |

During standalone launch, Inferno opens `/oauth/interact`. Sign in (any username), pick patient **p1**, and approve.

## Inferno test kit (manual)

The official kit runs in Docker on port 80 and drives browser-based OAuth flows.

```bash
git clone https://github.com/inferno-framework/smart-app-launch-test-kit.git
cd smart-app-launch-test-kit
./setup.sh
```

Create `.env` (or export before `./run.sh`):

```env
INFERNO_HOST=http://localhost
```

Start ReFHIR (above), then start Inferno:

```bash
./run.sh
```

Open [http://localhost](http://localhost), choose **SMART App Launch** (STU2 / v2.2 as appropriate), and set:

- **FHIR server URL**: `http://localhost:8080`
- Run **Discovery** first, then **Standalone** or **EHR** groups as needed.

Tips:

- Use the built-in **public** client for PKCE standalone tests with redirect `http://127.0.0.1/cb` when the suite allows a custom client.
- For **dynamic registration**, use initial access token `inferno-dcr`.
- For **confidential symmetric** tests, register a client or use DCR, then use secret `inferno-confidential` only for the pre-provisioned confidential client if your suite creates one via DCR.

Document pass/fail in your release notes; CI runs the shell smoke tests only (see below).

## CI smoke

Workflow `.github/workflows/inferno-smoke.yml` builds the Inferno compose profile, seeds `Patient/p1`, and runs `smoke-discovery.sh` + `smoke-oauth.sh`. Full Inferno UI suites remain a manual or scheduled operator step because they require browser interaction at `/oauth/interact`.

## Troubleshooting

- **Wrong issuer**: `public_url` / `REFHIR_PUBLIC_URL` must match what Inferno uses (`http://localhost:8080` for local Docker).
- **Empty patient list on interact**: run `deploy/inferno/seed.sh`.
- **Redirect errors**: public clients only allow loopback redirect URIs; match Inferno’s configured redirect exactly.

See also [docs/smart-compliance.md](../../docs/smart-compliance.md).
