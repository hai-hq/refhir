//! SMART Health Cards: the issuer's key, the card it signs, and the public key that checks it.
//!
//! `Patient/$health-cards-issue` has the engine choose and shrink what a card holds. This signs
//! it. A card is a compact JWS signed with ES256 whose payload is the credential's claims,
//! minified and compressed with raw DEFLATE. The JWS header names the key by its RFC 7638
//! thumbprint, and the public key is served at `{iss}/.well-known/jwks.json`.

use std::io::Write;
use std::path::Path;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use flate2::write::DeflateEncoder;
use flate2::Compression;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The credential type every health card carries.
pub const HEALTH_CARD_TYPE: &str = "https://smarthealth.cards#health-card";

/// The issuer's signing key: a P-256 key that signs every card this server issues.
pub struct IssuerKey {
    pair: EcdsaKeyPair,
    rng: SystemRandom,
    kid: String,
    x: String,
    y: String,
}

impl std::fmt::Debug for IssuerKey {
    // The key itself is never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IssuerKey({})", self.kid)
    }
}

impl IssuerKey {
    /// The key in the PEM file at `path`: an unencrypted PKCS#8 `PRIVATE KEY` on P-256, as
    /// `openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256` writes it.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let pem = std::fs::read_to_string(path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        Self::from_pem(&pem)
    }

    pub fn from_pem(pem: &str) -> Result<Self, String> {
        Self::from_der(&pkcs8_der(pem)?)
    }

    fn from_der(der: &[u8]) -> Result<Self, String> {
        let rng = SystemRandom::new();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, der, &rng).map_err(
            |_| {
                "not a P-256 private key in PKCS#8 form: generate one with `openssl genpkey \
                 -algorithm EC -pkeyopt ec_paramgen_curve:P-256`"
                    .to_string()
            },
        )?;
        // The public key is the uncompressed point: 0x04, then x and y, 32 bytes each.
        let point = pair.public_key().as_ref();
        if point.len() != 65 || point[0] != 4 {
            return Err("the public key is not an uncompressed P-256 point".into());
        }
        let x = URL_SAFE_NO_PAD.encode(&point[1..33]);
        let y = URL_SAFE_NO_PAD.encode(&point[33..65]);
        let kid = thumbprint(&x, &y);
        Ok(Self {
            pair,
            rng,
            kid,
            x,
            y,
        })
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// A new P-256 key, and its PEM to keep.
    pub fn generate() -> Result<(Self, String), String> {
        let rng = SystemRandom::new();
        let document = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .map_err(|_| "cannot generate a P-256 key".to_string())?;
        let body = STANDARD.encode(document.as_ref());
        let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
        for line in body.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(line).expect("base64 is ascii"));
            pem.push('\n');
        }
        pem.push_str("-----END PRIVATE KEY-----\n");
        Ok((Self::from_pem(&pem)?, pem))
    }

    /// The key a tenant of a process that serves many signs its cards with: its own, kept in its
    /// own database (beside its SMART signing key), so every process that serves the tenant signs
    /// with the same one and no tenant's cards verify against another's. The tenant's primary
    /// makes it the first time (`create`); another process waits for it to be there.
    pub fn for_tenant(
        store: &dyn refhir_persistence::AuthStore,
        create: bool,
    ) -> Result<Self, String> {
        const KIND: &str = "health_cards_key";
        const ACTIVE: &str = "active";
        // The same passphrase seals this key as seals the SMART signing key.
        let passphrase = std::env::var("REFHIR_SIGNING_KEY_PASSPHRASE")
            .ok()
            .filter(|value| !value.is_empty());
        let stored = |store: &dyn refhir_persistence::AuthStore| -> Result<Option<Self>, String> {
            match store.auth_get(KIND, ACTIVE, 0) {
                Ok(Some(json)) => {
                    let pem: String = serde_json::from_str(&json)
                        .map_err(|err| format!("the stored health card key: {err}"))?;
                    if pem.contains(ENCRYPTED_BEGIN) {
                        let passphrase = passphrase.as_deref().ok_or(
                            "the health card key is encrypted: set REFHIR_SIGNING_KEY_PASSPHRASE",
                        )?;
                        return Self::from_der(&unseal(&pem, passphrase)?).map(Some);
                    }
                    let key = Self::from_pem(&pem)?;
                    if let (true, Some(passphrase)) = (create, passphrase.as_deref()) {
                        // A key stored before a passphrase was set is sealed now.
                        match seal(&pem, passphrase).and_then(|sealed| {
                            serde_json::to_string(&sealed).map_err(|err| err.to_string())
                        }) {
                            Ok(sealed) => {
                                if let Err(err) = store.auth_put(KIND, ACTIVE, &sealed, u64::MAX) {
                                    tracing::warn!("health card key not encrypted: {err}");
                                }
                            }
                            Err(err) => tracing::warn!("health card key not encrypted: {err}"),
                        }
                    }
                    Ok(Some(key))
                }
                Ok(None) => Ok(None),
                Err(err) => Err(format!("cannot read the health card key: {err}")),
            }
        };
        if let Some(key) = stored(store)? {
            return Ok(key);
        }
        if !create {
            return Err("the tenant's health card key has not been made yet".into());
        }
        let (key, pem) = Self::generate()?;
        let pem = match passphrase.as_deref() {
            Some(passphrase) => seal(&pem, passphrase)?,
            None => {
                tracing::warn!(
                    "a health card signing key is stored unencrypted in the database; set \
                     REFHIR_SIGNING_KEY_PASSPHRASE"
                );
                pem
            }
        };
        let json = serde_json::to_string(&pem).expect("pem json");
        let inserted = store
            .auth_insert(KIND, ACTIVE, &json, u64::MAX, 0)
            .map_err(|err| format!("cannot store the health card key: {err}"))?;
        if inserted {
            return Ok(key);
        }
        // Another process made one first: use that one.
        stored(store)?.ok_or_else(|| "the health card key vanished".to_string())
    }

    /// The key set that checks the cards: this one public key, never the private part.
    pub fn jwks(&self) -> Value {
        json!({"keys": [{
            "kty": "EC",
            "kid": self.kid,
            "use": "sig",
            "alg": "ES256",
            "crv": "P-256",
            "x": self.x,
            "y": self.y
        }]})
    }

    /// A health card: the claims of a credential, compressed and signed. `types` are the
    /// credential types beyond `health-card`, which is always there.
    pub fn sign(
        &self,
        iss: &str,
        nbf: u64,
        types: &[String],
        fhir_version: &str,
        bundle: &Value,
    ) -> Result<String, String> {
        let mut all_types = vec![HEALTH_CARD_TYPE.to_string()];
        for kind in types {
            if !all_types.contains(kind) {
                all_types.push(kind.clone());
            }
        }
        let payload = json!({
            "iss": iss,
            "nbf": nbf,
            "vc": {
                "type": all_types,
                "credentialSubject": {"fhirVersion": fhir_version, "fhirBundle": bundle}
            }
        });
        let minified = serde_json::to_vec(&payload).map_err(|err| err.to_string())?;
        let mut compressor = DeflateEncoder::new(Vec::new(), Compression::default());
        compressor
            .write_all(&minified)
            .map_err(|err| err.to_string())?;
        let compressed = compressor.finish().map_err(|err| err.to_string())?;
        let header = json!({"zip": "DEF", "alg": "ES256", "kid": self.kid});
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).map_err(|err| err.to_string())?),
            URL_SAFE_NO_PAD.encode(compressed)
        );
        // `FIXED` is the JWS form of an ECDSA signature: r then s, 32 bytes each.
        let signature = self
            .pair
            .sign(&self.rng, signing_input.as_bytes())
            .map_err(|_| "the key could not sign".to_string())?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        ))
    }
}

/// The RFC 7638 thumbprint of an EC key, base64url: the SHA-256 of its required members in the
/// order the RFC fixes, with no spaces.
fn thumbprint(x: &str, y: &str) -> String {
    let canonical = format!("{{\"crv\":\"P-256\",\"kty\":\"EC\",\"x\":\"{x}\",\"y\":\"{y}\"}}");
    URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
}

const ENCRYPTED_BEGIN: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----";
const ENCRYPTED_END: &str = "-----END ENCRYPTED PRIVATE KEY-----";

/// A `PRIVATE KEY` PEM as an `ENCRYPTED PRIVATE KEY` PEM under `passphrase`.
fn seal(pem: &str, passphrase: &str) -> Result<String, String> {
    use rsa::pkcs8::PrivateKeyInfo;
    let der = pkcs8_der(pem)?;
    let info = PrivateKeyInfo::try_from(der.as_slice())
        .map_err(|err| format!("the health card key: {err}"))?;
    let sealed = info
        .encrypt(&mut rand::thread_rng(), passphrase)
        .map_err(|err| format!("cannot encrypt the health card key: {err}"))?;
    let body = STANDARD.encode(sealed.as_bytes());
    let mut out = format!("{ENCRYPTED_BEGIN}\n");
    for line in body.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).expect("base64 is ascii"));
        out.push('\n');
    }
    out.push_str(ENCRYPTED_END);
    out.push('\n');
    Ok(out)
}

/// The PKCS#8 DER inside an `ENCRYPTED PRIVATE KEY` PEM, opened with `passphrase`.
fn unseal(pem: &str, passphrase: &str) -> Result<Vec<u8>, String> {
    use rsa::pkcs8::EncryptedPrivateKeyInfo;
    let start = pem.find(ENCRYPTED_BEGIN).ok_or("not an encrypted key")? + ENCRYPTED_BEGIN.len();
    let end = pem[start..]
        .find(ENCRYPTED_END)
        .ok_or("the encrypted key is not closed")?
        + start;
    let body: String = pem[start..end]
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let der = STANDARD
        .decode(body)
        .map_err(|_| "the encrypted health card key is not valid PEM".to_string())?;
    let info = EncryptedPrivateKeyInfo::try_from(der.as_slice())
        .map_err(|err| format!("the encrypted health card key: {err}"))?;
    let opened = info.decrypt(passphrase).map_err(|_| {
        "REFHIR_SIGNING_KEY_PASSPHRASE does not open the health card key".to_string()
    })?;
    Ok(opened.as_bytes().to_vec())
}

/// The DER inside a PKCS#8 `PRIVATE KEY` PEM.
fn pkcs8_der(pem: &str) -> Result<Vec<u8>, String> {
    const BEGIN: &str = "-----BEGIN PRIVATE KEY-----";
    const END: &str = "-----END PRIVATE KEY-----";
    if pem.contains("-----BEGIN EC PRIVATE KEY-----") {
        return Err(
            "this is a SEC1 `EC PRIVATE KEY`: convert it with `openssl pkcs8 -topk8 -nocrypt`"
                .into(),
        );
    }
    if pem.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----") {
        return Err("the key is encrypted: the server needs it unencrypted".into());
    }
    let start = pem
        .find(BEGIN)
        .ok_or("no `-----BEGIN PRIVATE KEY-----` block in the key file")?
        + BEGIN.len();
    let end = pem[start..]
        .find(END)
        .ok_or("the `PRIVATE KEY` block is not closed")?
        + start;
    let body: String = pem[start..end]
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    STANDARD
        .decode(body)
        .map_err(|_| "the key file is not valid PEM".to_string())
}

/// A fresh key in the PEM form the server reads, for tests.
#[cfg(test)]
pub(crate) fn generated_pem() -> String {
    let der = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
        .unwrap();
    let body = STANDARD.encode(der.as_ref());
    let lines: Vec<String> = body
        .as_bytes()
        .chunks(64)
        .map(|line| String::from_utf8(line.to_vec()).unwrap())
        .collect();
    format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::DeflateDecoder;
    use ring::signature::{UnparsedPublicKey, ECDSA_P256_SHA256_FIXED};
    use std::io::Read;

    #[test]
    fn a_sealed_health_card_key_opens_with_its_passphrase_and_no_other() {
        let (key, pem) = IssuerKey::generate().unwrap();
        let sealed = seal(&pem, "correct horse").unwrap();
        assert!(sealed.contains(ENCRYPTED_BEGIN) && !sealed.contains("-----BEGIN PRIVATE KEY"));
        let opened = IssuerKey::from_der(&unseal(&sealed, "correct horse").unwrap()).unwrap();
        assert_eq!(opened.kid(), key.kid());
        // A wrong passphrase can, rarely, decrypt to padding that looks valid, but never to a key.
        assert!(unseal(&sealed, "wrong")
            .and_then(|der| IssuerKey::from_der(&der))
            .is_err());
    }

    fn decode(part: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(part).unwrap()
    }

    #[test]
    fn a_card_is_a_compact_jws_that_the_published_key_checks() {
        let key = IssuerKey::from_pem(&generated_pem()).unwrap();
        let bundle = json!({"resourceType": "Bundle", "type": "collection", "entry": []});
        let jws = key
            .sign(
                "https://example.org/fhir/health-cards",
                1_700_000_000,
                &["https://smarthealth.cards#immunization".to_string()],
                "4.0.1",
                &bundle,
            )
            .unwrap();
        let parts: Vec<&str> = jws.split('.').collect();
        assert_eq!(parts.len(), 3, "a compact JWS has three parts");

        let header: Value = serde_json::from_slice(&decode(parts[0])).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["zip"], "DEF");
        assert_eq!(header["kid"], key.kid());

        // The payload is raw DEFLATE, with no zlib header, of minified JSON.
        let mut payload = String::new();
        DeflateDecoder::new(&decode(parts[1])[..])
            .read_to_string(&mut payload)
            .unwrap();
        assert!(
            !payload.contains('\n') && !payload.contains(": "),
            "minified: {payload}"
        );
        let claims: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(claims["iss"], "https://example.org/fhir/health-cards");
        assert_eq!(claims["nbf"], 1_700_000_000);
        assert!(claims.get("exp").is_none(), "health cards do not expire");
        assert_eq!(
            claims["vc"]["type"],
            json!([HEALTH_CARD_TYPE, "https://smarthealth.cards#immunization"])
        );
        assert_eq!(claims["vc"]["credentialSubject"]["fhirVersion"], "4.0.1");
        assert_eq!(claims["vc"]["credentialSubject"]["fhirBundle"], bundle);

        // The signature is the 64 bytes of r and s, and the key in the key set checks it.
        let signature = decode(parts[2]);
        assert_eq!(signature.len(), 64);
        let jwk = &key.jwks()["keys"][0];
        let mut point = vec![4u8];
        point.extend(decode(jwk["x"].as_str().unwrap()));
        point.extend(decode(jwk["y"].as_str().unwrap()));
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point)
            .verify(signing_input.as_bytes(), &signature)
            .expect("the card verifies against the published key");
        let mut tampered = signing_input.into_bytes();
        tampered[10] ^= 1;
        let mut point = vec![4u8];
        point.extend(decode(jwk["x"].as_str().unwrap()));
        point.extend(decode(jwk["y"].as_str().unwrap()));
        assert!(UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point)
            .verify(&tampered, &signature)
            .is_err());
    }

    #[test]
    fn the_key_set_holds_the_public_key_only_under_the_thumbprint_kid() {
        let key = IssuerKey::from_pem(&generated_pem()).unwrap();
        let jwks = key.jwks();
        let jwk = &jwks["keys"][0];
        assert_eq!(jwks["keys"].as_array().unwrap().len(), 1);
        assert_eq!(jwk["kty"], "EC");
        assert_eq!(jwk["use"], "sig");
        assert_eq!(jwk["alg"], "ES256");
        assert_eq!(jwk["crv"], "P-256");
        assert!(
            jwk.get("d").is_none(),
            "the private part is never published"
        );
        assert_eq!(
            jwk["kid"],
            thumbprint(jwk["x"].as_str().unwrap(), jwk["y"].as_str().unwrap())
        );
        assert_eq!(key.kid().len(), 43, "a base64url SHA-256");
    }

    #[test]
    fn the_thumbprint_is_the_hash_of_the_required_members_in_order_with_no_spaces() {
        // The value is the SHA-256 of `{"crv":"P-256","kty":"EC","x":"A","y":"B"}`, base64url,
        // worked out outside this code.
        assert_eq!(
            thumbprint("A", "B"),
            "jDSLYI4g8wUifTgjc41AfQnCzAqRk_ll2Z2csXBhYu4"
        );
    }

    #[test]
    fn a_key_that_cannot_be_used_is_refused_and_the_reason_says_how_to_make_one() {
        let refused = |pem: &str| IssuerKey::from_pem(pem).unwrap_err();
        assert!(refused("").contains("no `-----BEGIN PRIVATE KEY-----`"));
        assert!(
            refused("-----BEGIN EC PRIVATE KEY-----\nAAAA\n-----END EC PRIVATE KEY-----")
                .contains("pkcs8 -topk8")
        );
        assert!(refused(
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----"
        )
        .contains("encrypted"));
        assert!(
            refused("-----BEGIN PRIVATE KEY-----\nnot base64!\n-----END PRIVATE KEY-----")
                .contains("not valid PEM")
        );
        assert!(
            refused("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----")
                .contains("P-256")
        );
        assert!(IssuerKey::from_file("/nonexistent/key.pem")
            .unwrap_err()
            .contains("cannot read"));
    }
}
