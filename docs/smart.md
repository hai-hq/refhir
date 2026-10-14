# SMART on FHIR

**Inferno:** SMART-enabled Docker profile and runbook — [deploy/inferno/README.md](../deploy/inferno/README.md). CI runs discovery + OAuth smoke via `.github/workflows/inferno-smoke.yml`.

`--smart` turns the server into a SMART App Launch 2.2 authorization server and resource server:

- Discovery at `/.well-known/smart-configuration` and JWKS at `/.well-known/jwks.json`
- Authorization code + PKCE (`S256`), EHR launch (`/oauth/ehr-launch`), standalone `launch/patient`
- Token endpoint: `authorization_code`, `refresh_token`, and `client_credentials` (asymmetric JWT)
- OpenID Connect `id_token`, token introspection, revocation, and dynamic client registration
- SMART v1 (`patient/*.read`) and v2 (`patient/*.rs`) scopes, plus patient-compartment enforcement

Default clients: `public` (no secret) and `confidential` (`--smart-confidential-secret`), with loopback redirect URIs `/cb` and `/app`. App-launch tokens cannot include `system/` scopes.

**Sign-in.** `--smart` needs a login and will not start without one. For real use, configure an OpenID Connect provider (`[smart.oidc]` or `--oidc-issuer`/`--oidc-client-id` plus `REFHIR_OIDC_CLIENT_SECRET`): the authorize step sends users to the provider and the verified `sub` and `fhirUser` claim go into the tokens. Optional settings under `[smart.oidc]` limit who may sign in (`allowed_claims`, `allowed_email_domains`), bind a person to a `Practitioner` or `Patient` when the provider has no `fhirUser` claim (`fhir_user_lookup`), and fix the launch patient to the signed-in patient (`patient_from_fhir_user`); all are off by default. A gateway in front of ReFHIR can ask `POST /oauth/introspect` (confidential or backend client credentials) for the `client_id`, `sub`, `scope` and `exp` of an access token. See [deploy/production.md](../deploy/production.md#smart-sign-in-openid-connect). `--smart-demo-login` skips authentication entirely (any username works) and exists only for local testing and conformance runs such as Inferno.

`POST /oauth/register` requires `--smart-dcr-token`. If either flag is omitted, a value is generated and printed once to stderr when the process has a terminal; secrets are never written to logs. Redirect URIs must be `https` or loopback `http`. Backend clients register with `private_key_jwt`, inline `jwks`, and `system/` scopes. EHR launch context comes from `--smart-patient` / `--smart-encounter`, not from query parameters. `--public-url` is required when `--smart` is combined with a non-loopback `--bind`.
