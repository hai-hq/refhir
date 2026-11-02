# SMART Health Cards

A [SMART Health Card](https://hl7.org/fhir/uv/smart-health-cards-and-links/cards-specification.html)
is a small, signed piece of someone's health record, such as their immunizations or a lab result.
It is a JSON Web Signature, which fits in a QR code or a file. Anyone who has it can check that
this server issued it and that it has not been changed, with no call to the server beyond fetching
its public key once.

ReFHIR issues cards through `Patient/[id]/$health-cards-issue`, once you give it a key to sign
with. A card holds the data itself, so it is for something small. For a whole
[patient summary](patient-summary.md), use a [health link](health-links.md), which holds an address
and a key instead.

## Turn it on

Make a signing key. It is a P-256 private key, and it is the one secret that makes cards yours:

```bash
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out health-cards.pem
chmod 600 health-cards.pem
```

Point the server at the file:

```toml
[server]
public_url = "https://refhir.example.org/fhir"

[health_cards]
key_file = "/etc/refhir/health-cards.pem"
```

or set `REFHIR_HEALTH_CARDS_KEY_FILE`. The file must be a PKCS#8 `PRIVATE KEY` and not encrypted.
A key in the older `EC PRIVATE KEY` form is refused with the command that converts it
(`openssl pkcs8 -topk8 -nocrypt`). A key that cannot be used stops the server from starting, and
says why.

A card says where its public key is, so the server has to know the address it is reached at. Set
`public_url`, which on a bind that is not loopback is required, and which a server behind a proxy
on the same machine needs too: without it a card names `http://127.0.0.1`, and the server says so
when it starts. Verifiers accept only an `https`
address.

With a key, the server:

- signs cards for `iss` = `[public_url]/health-cards`;
- publishes the public key at `[public_url]/health-cards/.well-known/jwks.json`, which needs no
  token and can be read from any origin, as a card verifier running in a browser needs;
- lists `health-cards` among the `capabilities` of `/.well-known/smart-configuration`, and
  `health-cards-issue` among the operations of `Patient` in the CapabilityStatement.

Without a key, it issues no cards, and `$health-cards-issue` answers `501`.

## Ask for a card

`POST [base]/Patient/[id]/$health-cards-issue`, with a `Parameters` body:

```bash
curl -X POST 'http://localhost:8080/Patient/p1/$health-cards-issue' \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/fhir+json' \
  -d '{"resourceType": "Parameters", "parameter": [
        {"name": "credentialType", "valueUri": "Immunization"}
      ]}'
```

The answer is a `Parameters` resource. Each card is a `verifiableCredential` parameter holding the
signed text, and each resource in it has a `resourceLink` that says where on this server it came
from:

```json
{"resourceType": "Parameters", "parameter": [
  {"name": "verifiableCredential", "valueString": "eyJ6aXAiOiJERUYi..."},
  {"name": "resourceLink", "part": [
    {"name": "vcIndex", "valueInteger": 0},
    {"name": "bundledResource", "valueUri": "resource:1"},
    {"name": "hostedResource", "valueUri": "https://refhir.example.org/fhir/Immunization/i1"}
  ]}
]}
```

When the patient has nothing to put in the card, the answer is an empty `Parameters`, not an error.

| Parameter | |
|---|---|
| `credentialType` | Required, repeatable. The kind of resource the card holds: one of `AllergyIntolerance`, `Condition`, `DiagnosticReport`, `Immunization`, `MedicationRequest`, `MedicationStatement`, `Observation` and `Procedure`. The older `https://smarthealth.cards#immunization` and `https://smarthealth.cards#laboratory` mean `Immunization` and `Observation`, and the second keeps only laboratory results. A type the server cannot make a card of is a `400`. Two or more types are "and": one card holds all of them, and if the patient has none of one of them there is no card. |
| `includeIdentityClaim` | Repeatable. The `Patient` elements that say who the card is about: `Patient.name`, `Patient.birthDate`, `Patient.gender`, `Patient.identifier`, `Patient.telecom` or `Patient.address`. Without it a card says the name (family and given) and the birth date. |
| `credentialValueSet` | Accepted and ignored, which the specification allows. |
| `_since` | Only what was written at or after this time goes in the card. It is a date (the start of that day, in UTC) or a date and time (a time with no zone is UTC), and is given once. A resource from before the server kept times has no time of its own and is left out. When nothing is new there is no card. See [When a resource was written](fhir-api.md#when-a-resource-was-written). |

Resources that are entered in error or refuted are not put in a card.

## What a card holds

A card is a compact JWS signed with ES256. Its header names the key (`kid`, the RFC 7638
thumbprint of the public key) and says the payload is compressed (`zip: DEF`). Its payload holds:

- `iss`, `nbf` (when it was issued) and no `exp`: a health card does not expire;
- `vc.type`: `https://smarthealth.cards#health-card`, and `#immunization` or `#laboratory` where
  the card holds them;
- `vc.credentialSubject.fhirVersion` and `fhirBundle`, a `collection` Bundle.

The Bundle holds the patient (`resource:0`) with the identity claims asked for and the resources
asked for. To keep it small it follows the card format: the resources have no `id`, no `meta`
except `meta.security`, and no narrative; a `CodeableConcept` that has a coding loses its `text`,
and a `Coding` its `display`; and a reference points at the entry it names (`resource:1`). A
reference to something the card does not hold keeps its `display` if it has one and is dropped
otherwise.

A JWS longer than 1195 characters cannot be one QR code. An immunization record fits; a long
laboratory history may not, and a patient summary does not. The chunked form that the
specification used for this is deprecated, so for larger data use a
[health link](health-links.md).

## Who is the issuer

A card's `iss` says who vouches for it, and a verifier decides whether to trust a card by deciding
whether to trust that address and the key published under it. ReFHIR signs with one key per
server, and one `iss` for it: `[public_url]/health-cards`. On a database that many
[tenants](clustering.md#tenants-on-one-database) share, each tenant is a process of its own with its
own `public_url` and its own key, so each tenant has its own issuer. When one tenant holds the
records of several organizations (clinics, hospitals), there are two ways to answer "who issued
this card", and they differ in what a verifier sees and in what you look after.

### A. The server or tenant is the issuer (what ReFHIR does)

Every card the server or tenant signs has `iss` = `[public_url]/health-cards`, whichever organization
the patient's record belongs to.

- **Needs no change.** It is what the server does now, and what the specification's examples show.
- **One key to keep.** One key file, one key set to publish, one thing to back up or replace.
  Replacing it ends every card it signed, as described below, but it is one event.
- **A verifier trusts the operator, not the organization.** A card says "this tenant issued it". It
  does not say which organization's records it came from. A verifier that wants to accept cards
  from one clinic and not another cannot tell them apart by `iss`, and little inside a card can say (a card
  drops `meta` and every `id`, so it carries no source of its own): it has to trust the operator for
  all of them. For a platform where the operator is the party the verifier already
  trusts, that is the natural reading.
- **A leak is one key, and everyone's cards.** If the key is stolen, every organization's cards
  are at risk and the key has to be replaced, which ends every card.
- **Every organization shares one trust decision.** A verifier cannot drop one organization from its
  trusted list, and an organization cannot leave without the operator's help.

### B. A key for each Organization, inside a tenant (not built)

Each organization that issues cards would have a key of its own, and cards would say
`iss` = `[public_url]/health-cards/[org]`, with the key set at
`[public_url]/health-cards/[org]/.well-known/jwks.json`.

- **A verifier can trust one organization and not another.** `iss` says whose records a card
  holds, so a verifier's list of trusted issuers can name a clinic, and a clinic that is removed
  from it stops being trusted without affecting the others.
- **An organization can take its key with it.** Replacing or retiring one organization's key ends
  only its cards, and the key can be kept by the organization when it does not want the operator to
  hold it.
- **A leak is bounded** to that organization's cards.
- **More to look after.** A key for each organization to make, store (the operator keeps a file or a
  secret for each, and the secrets must not be readable across organizations), back up, rotate and
  publish, and a key set for each at its own address. The number of keys grows with the number of
  organizations, and onboarding an organization means making a key.
- **A new decision for the server.** It has to know which organization a card is for, which means a
  rule for finding it from the patient (`Patient.managingOrganization`, which may be absent or point
  outside the tenant), what to do when there is none, and who may ask for a card on an
  organization's behalf. It also changes what a published `iss` means to verifiers already
  holding the old address, and each organization's address has to be reachable and `https`.
- **Verifiers see more trust surface.** A verifier that trusts the operator today would now have an
  issuer for each organization to trust, and a new organization is a new entry to add.

### Choosing

| | A. Server or tenant is the issuer | B. A key for each Organization |
|---|---|---|
| Change to ReFHIR | None | New key store, new routes, a rule for which organization a card is for |
| Keys to look after | One for the server or tenant | One for each organization |
| What a verifier can trust or drop | The server or tenant, as a whole | Each organization separately |
| If a key leaks | Every card of the server or tenant | One organization's cards |
| Fits when | The operator is the party the verifier trusts, or the tenant is one organization | Verifiers must tell organizations apart, or organizations must hold their own keys |

If each organization is a tenant of its own, A already gives each its own issuer and key, and B is
not needed: tenants are the unit that has a key. B is for several organizations inside one tenant
that must be told apart by their `iss`. The server does A today. B is not built.

## Who can ask

The caller needs to be able to read `Patient`, and the card holds only the kinds of resource their
scopes let them read. Under SMART a token that is tied to one patient asks for that patient's
card; another patient's is a `403`. See [SMART on FHIR](smart.md).

## Look after the key

The key set the server publishes holds the one key it signs with. If you replace the key, cards
signed with the old one stop verifying, because their `kid` is no longer in the set. Keep the key
safe, back it up, and treat a change of key as the end of every card it signed. A card cannot be
revoked.

See [Production deployment](../deploy/production.md#smart-health-cards-signing-key) for keeping the
key out of the image and out of the configuration file.
