# SMART App Launch conformance

ReFHIR implements SMART App Launch 2.2 as authorization server + FHIR resource server.

## Implemented

- Discovery (`/.well-known/smart-configuration`), JWKS, PKCE (S256), authorization code + refresh + client_credentials
- Interactive authorize (`/oauth/interact`): sign-in through an OpenID Connect provider (code + PKCE, nonce, ID token signature check), patient picker, consent
- EHR launch with per-launch patient (public clients use configured `smart_patient` only)
- Dynamic client registration (initial access token)
- Scope enforcement on REST and `/sync`
- OAuth state (clients, codes, tokens, login sessions) and the signing key are stored in the `smart_oauth` table of the configured database (SQLite or Postgres), outside the FHIR resource log. Codes, tokens and session ids are stored as SHA-256 hashes; codes and refresh tokens are consumed with a single atomic delete

## Configuration

```toml
[smart]
enabled = true

[smart.oidc]
issuer = "https://idp.example.com/realms/clinic"
client_id = "refhir"
# client secret: REFHIR_OIDC_CLIENT_SECRET
```

SMART will not start without a login: configure `[smart.oidc]`, or set `demo_login = true` for testing (accepts any username, verifies nothing). See [deploy/production.md](../deploy/production.md#smart-sign-in-openid-connect).

With `--memory`, OAuth state lives in the in-memory database and ends with the process.

## Validation

- **Automated smoke (CI)**: `.github/workflows/inferno-smoke.yml` — Docker profile in `deploy/inferno/` (discovery + standalone PKCE + interact).
- **Full Inferno UI**: [SMART App Launch Test Kit](https://github.com/inferno-framework/smart-app-launch-test-kit) — see [deploy/inferno/README.md](../deploy/inferno/README.md).

Document Inferno pass/fail in your release process.

## Not yet production-grade

- Demo login (`demo_login`) is unauthenticated and for testing only; production uses `[smart.oidc]`
- SAML is not supported; use an OpenID Connect provider (most SAML IdPs can be brokered through Keycloak)
- No group or role mapping: every user who may sign in (all of them, unless `allowed_claims` or `allowed_email_domains` narrows it) gets the scopes the client requests
- No multi-node OAuth store (file snapshot is single-process)
