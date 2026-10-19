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
`public_url`, which on a bind that is not loopback is required. Verifiers accept only an `https`
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
| `credentialType` | Required, repeatable. The kind of resource the card holds: a resource type of the patient's record, such as `Immunization`, `Observation` or `Condition`. The older `https://smarthealth.cards#immunization` and `https://smarthealth.cards#laboratory` mean `Immunization` and `Observation`, and the second keeps only laboratory results. A type the server cannot make a card of is a `400`. Two or more types are "and": one card holds all of them, and if the patient has none of one of them there is no card. |
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
