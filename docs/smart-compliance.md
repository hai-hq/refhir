# SMART App Launch support

ReFHIR implements SMART App Launch 2.2 as authorization server + FHIR resource server.

## Implemented

- Discovery (`/.well-known/smart-configuration`), JWKS, PKCE (S256), authorization code + refresh + client_credentials
- Interactive authorize (`/oauth/interact`): sign-in through an OpenID Connect provider (code + PKCE, nonce, ID token signature check), patient picker, consent
- EHR launch with per-launch patient (public clients use configured `smart_patient` only)
- Dynamic client registration (initial access token)
- Scope enforcement on REST and `/sync`
- OAuth state (clients, codes, tokens, login sessions) and the signing key are stored in the configured database, outside the FHIR resources. Codes, tokens and session ids are stored as hashes

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

## Sign-in notes

- Demo login (`demo_login`) is unauthenticated and for testing only; production uses `[smart.oidc]`.
- SAML is not supported. Use an OpenID Connect provider; most SAML identity providers can be brokered through Keycloak.
- There is no group or role mapping: every user who may sign in gets the scopes the client requests. Narrow who may sign in with `allowed_claims` or `allowed_email_domains`.
