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
    /// What `iss` says when it is not the server's own address, with no trailing slash.
    issuer: Option<String>,
    /// The key set that checks the cards, made once: this key, then any retired public keys.
    jwks: Value,
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
        Self::from_pem(&read_file(path.as_ref())?)
    }

    pub fn from_pem(pem: &str) -> Result<Self, String> {
        let der = pkcs8_der(pem)?;
        let rng = SystemRandom::new();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der, &rng).map_err(
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
        let jwks = json!({"keys": [{
            "kty": "EC",
            "kid": kid,
            "use": "sig",
            "alg": "ES256",
            "crv": "P-256",
            "x": x,
            "y": y
        }]});
        Ok(Self {
            pair,
            rng,
            kid,
            issuer: None,
            jwks,
        })
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// Cards name `issuer` as `iss`, not an address of this server's. It is checked by
    /// [`validate_issuer`] and kept with no trailing slash.
    pub fn with_issuer(mut self, issuer: &str) -> Result<Self, String> {
        self.issuer = Some(validate_issuer(issuer)?);
        Ok(self)
    }

    /// The `iss` the cards name when one was set.
    pub fn issuer(&self) -> Option<&str> {
        self.issuer.as_deref()
    }

    /// Also publish the public keys in the JWKS file at `path`, so a key that was replaced stays
    /// in the key set and the cards it signed still verify.
    pub fn with_retired_keys_file(self, path: impl AsRef<Path>) -> Result<Self, String> {
        self.with_retired_keys(&read_file(path.as_ref())?)
    }

    pub fn with_retired_keys(mut self, jwks: &str) -> Result<Self, String> {
        for key in public_keys(jwks)? {
            let published = self.jwks["keys"]
                .as_array_mut()
                .expect("the key set has keys");
            if !published.iter().any(|known| known["kid"] == key["kid"]) {
                published.push(key);
            }
        }
        Ok(self)
    }

    /// The key set that checks the cards: this key and any retired ones, never a private part.
    pub fn jwks(&self) -> Value {
        self.jwks.clone()
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

fn read_file(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|err| format!("cannot read {}: {err}", path.display()))
}

/// Whether `point` (0x04, x, y) is on the P-256 curve. `ring` has no call for it, but it refuses a
/// peer key that is not on the curve when it agrees a secret with one.
fn on_curve(point: &[u8]) -> bool {
    use ring::agreement::{agree_ephemeral, EphemeralPrivateKey, UnparsedPublicKey, ECDH_P256};
    let Ok(private) = EphemeralPrivateKey::generate(&ECDH_P256, &SystemRandom::new()) else {
        return false;
    };
    agree_ephemeral(private, &UnparsedPublicKey::new(&ECDH_P256, point), |_| ()).is_ok()
}

/// An address a card can name as `iss`: `https` (or `http` on a loopback host, for tests), a host,
/// a valid port if it has one, no user, query, fragment or empty or dot path segment. A trailing slash is dropped, because the specification has the
/// key set at `[iss]/.well-known/jwks.json` and `iss` with none.
pub fn validate_issuer(issuer: &str) -> Result<String, String> {
    let trimmed = issuer.trim();
    let url = trimmed.trim_end_matches('/');
    if url.is_empty() {
        return Err("the issuer is empty".into());
    }
    if url.contains(char::is_whitespace) {
        return Err(format!("the issuer {issuer:?} has a space in it"));
    }
    if url.contains('?') || url.contains('#') {
        return Err(format!(
            "the issuer {url} has a query or fragment: it must be a plain address"
        ));
    }
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("the issuer {url} is not an https address"))?;
    let mut segments = rest.split('/');
    let authority = segments.next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return Err(format!(
            "the issuer {url} needs a host, and no user name or password"
        ));
    }
    if segments.any(|segment| matches!(segment, "" | "." | "..")) {
        return Err(format!(
            "the issuer {url} has an empty or `.` or `..` part in its path"
        ));
    }
    // The host and the port, if any; an IPv6 address is bracketed.
    let (host, port) = match authority.strip_prefix('[') {
        Some(inner) => {
            let (host, after) = inner
                .split_once(']')
                .ok_or_else(|| format!("the issuer {url} has a `[` that is not closed"))?;
            match after {
                "" => (host, None),
                _ => (
                    host,
                    Some(
                        after
                            .strip_prefix(':')
                            .ok_or_else(|| format!("the issuer {url} has text after its host"))?,
                    ),
                ),
            }
        }
        None => match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    if let Some(port) = port {
        let digits = !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit());
        if !digits || port.parse::<u16>().map_or(true, |port| port == 0) {
            return Err(format!(
                "the issuer {url} has a port that is not 1 to 65535"
            ));
        }
    }
    let loopback = matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1"
    );
    match scheme {
        "https" => {}
        "http" if loopback => {}
        "http" => {
            return Err(format!(
                "the issuer {url} is an http address: a verifier accepts only https \
                 (http is allowed for localhost only)"
            ))
        }
        _ => return Err(format!("the issuer {url} is not an https address")),
    }
    if host.is_empty() {
        return Err(format!("the issuer {url} needs a host"));
    }
    Ok(url.to_string())
}

/// The keys in a JWKS, each as the public key the key set publishes it as, with the `kid` the
/// card header uses (its thumbprint). A key with a private member, or one that is not P-256, is an
/// error, and so is a `kid` that is not the key's thumbprint: it would not be found by a verifier.
fn public_keys(jwks: &str) -> Result<Vec<Value>, String> {
    let parsed: Value =
        serde_json::from_str(jwks).map_err(|err| format!("not a JSON key set: {err}"))?;
    let keys = parsed
        .get("keys")
        .and_then(Value::as_array)
        .ok_or("not a JWKS: no `keys` array")?;
    let mut out: Vec<Value> = Vec::new();
    for (index, key) in keys.iter().enumerate() {
        let at = |what: &str| format!("key {index}: {what}");
        let object = key.as_object().ok_or_else(|| at("not an object"))?;
        // `d` is the private scalar; the other members are RSA's private parts.
        for private in ["d", "p", "q", "dp", "dq", "qi", "oth", "k"] {
            if object.contains_key(private) {
                return Err(at(&format!(
                    "has a private member `{private}`: the file may hold public keys only"
                )));
            }
        }
        if object.get("kty").and_then(Value::as_str) != Some("EC")
            || object.get("crv").and_then(Value::as_str) != Some("P-256")
        {
            return Err(at("not an EC key on P-256"));
        }
        if let Some(usage) = object.get("use").and_then(Value::as_str) {
            if usage != "sig" {
                return Err(at(
                    "`use` is not `sig`: the key is not for checking signatures",
                ));
            }
        }
        if let Some(alg) = object.get("alg").and_then(Value::as_str) {
            if alg != "ES256" {
                return Err(at("`alg` is not ES256"));
            }
        }
        let coordinate = |name: &str| -> Result<String, String> {
            let value = object
                .get(name)
                .and_then(Value::as_str)
                .ok_or_else(|| at(&format!("no `{name}`")))?;
            match URL_SAFE_NO_PAD.decode(value) {
                Ok(bytes) if bytes.len() == 32 => Ok(value.to_string()),
                _ => Err(at(&format!("`{name}` is not 32 bytes of base64url"))),
            }
        };
        let (x, y) = (coordinate("x")?, coordinate("y")?);
        let mut point = vec![4u8];
        for part in [&x, &y] {
            point.extend(URL_SAFE_NO_PAD.decode(part).unwrap_or_default());
        }
        if !on_curve(&point) {
            return Err(at("the point (x, y) is not on the P-256 curve"));
        }
        let kid = thumbprint(&x, &y);
        if let Some(given) = object.get("kid").and_then(Value::as_str) {
            if given != kid {
                return Err(at(&format!(
                    "`kid` {given} is not the key's thumbprint {kid}"
                )));
            }
        }
        if out.iter().any(|known| known["kid"] == kid.as_str()) {
            continue;
        }
        out.push(json!({
            "kty": "EC", "kid": kid, "use": "sig", "alg": "ES256", "crv": "P-256", "x": x, "y": y
        }));
    }
    Ok(out)
}

/// The RFC 7638 thumbprint of an EC key, base64url: the SHA-256 of its required members in the
/// order the RFC fixes, with no spaces.
fn thumbprint(x: &str, y: &str) -> String {
    let canonical = format!("{{\"crv\":\"P-256\",\"kty\":\"EC\",\"x\":\"{x}\",\"y\":\"{y}\"}}");
    URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
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

    fn decode(part: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(part).unwrap()
    }

    /// Whether the key `jwk` checks `signature` of `signing_input`.
    fn checks(jwk: &Value, signing_input: &[u8], signature: &[u8]) -> bool {
        let mut point = vec![4u8];
        point.extend(decode(jwk["x"].as_str().unwrap()));
        point.extend(decode(jwk["y"].as_str().unwrap()));
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point)
            .verify(signing_input, signature)
            .is_ok()
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
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        assert!(
            checks(jwk, signing_input.as_bytes(), &signature),
            "the card verifies against the published key"
        );
        let mut tampered = signing_input.into_bytes();
        tampered[10] ^= 1;
        assert!(!checks(jwk, &tampered, &signature));
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

    #[test]
    fn an_issuer_is_an_https_address_without_a_query_a_fragment_or_a_trailing_slash() {
        let ok = |issuer: &str| validate_issuer(issuer).unwrap();
        assert_eq!(
            ok("https://health.clinic.example"),
            "https://health.clinic.example"
        );
        assert_eq!(
            ok("https://health.clinic.example/"),
            "https://health.clinic.example"
        );
        assert_eq!(ok(" https://x.example/cards// "), "https://x.example/cards");
        assert_eq!(
            ok("https://x.example:8443/a/b"),
            "https://x.example:8443/a/b"
        );
        assert_eq!(ok("https://x.example:443"), "https://x.example:443");
        for loopback in [
            "http://LOCALHOST:8080",
            "http://localhost:8080",
            "http://127.0.0.1:8080/cards",
            "http://[::1]:8080",
        ] {
            assert_eq!(ok(loopback), loopback);
        }

        let refused = |issuer: &str| validate_issuer(issuer).unwrap_err();
        assert!(refused("").contains("empty"));
        assert!(refused("/").contains("empty"));
        assert!(refused("http://health.clinic.example").contains("https"));
        assert!(refused("http://localhost.evil.example").contains("https"));
        assert!(refused("http://127.0.0.1.evil.example").contains("https"));
        assert!(refused("ftp://health.clinic.example").contains("https"));
        assert!(refused("health.clinic.example").contains("https"));
        assert!(refused("https://x.example?a=1").contains("query"));
        assert!(refused("https://x.example/cards#top").contains("fragment"));
        assert!(validate_issuer("https://").is_err());
        assert!(refused("https:///cards").contains("host"));
        assert!(refused("https://user:pw@x.example").contains("user"));
        for port in [
            "x.example:",
            "x.example:abc",
            "x.example:0",
            "x.example:99999999",
            "x.example:+80",
        ] {
            assert!(
                refused(&format!("https://{port}")).contains("port"),
                "{port}"
            );
        }
        assert!(refused("https://[::1]x").contains("text after"));
        assert!(refused("https://[::1").contains("not closed"));
        for path in ["x.example//a", "x.example/a/../b", "x.example/./a"] {
            assert!(
                refused(&format!("https://{path}")).contains("path"),
                "{path}"
            );
        }
        assert!(refused("https://x .example").contains("space"));
    }

    #[test]
    fn a_configured_issuer_is_what_a_card_says_and_is_kept_without_the_slash() {
        let key = IssuerKey::from_pem(&generated_pem()).unwrap();
        assert_eq!(key.issuer(), None);
        let key = key.with_issuer("https://health.clinic.example/").unwrap();
        assert_eq!(key.issuer(), Some("https://health.clinic.example"));
        let bad = IssuerKey::from_pem(&generated_pem()).unwrap();
        assert!(bad.with_issuer("http://health.clinic.example").is_err());
    }

    fn public_jwk(key: &IssuerKey) -> Value {
        key.jwks()["keys"][0].clone()
    }

    #[test]
    fn retired_keys_are_published_beside_the_signing_key_each_under_its_thumbprint() {
        let old_one = IssuerKey::from_pem(&generated_pem()).unwrap();
        let old_two = IssuerKey::from_pem(&generated_pem()).unwrap();
        // A `kid` the file gives is checked against the thumbprint; one it leaves out is made.
        let mut without_kid = public_jwk(&old_two);
        without_kid.as_object_mut().unwrap().remove("kid");
        without_kid.as_object_mut().unwrap().remove("use");
        let file = json!({"keys": [public_jwk(&old_one), without_kid]}).to_string();

        let key = IssuerKey::from_pem(&generated_pem())
            .unwrap()
            .with_retired_keys(&file)
            .unwrap();
        let jwks = key.jwks();
        let keys = jwks["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0]["kid"], key.kid(), "the signing key comes first");
        for (published, expected) in keys[1..].iter().zip([&old_one, &old_two]) {
            assert_eq!(published["kid"], expected.kid());
            assert_eq!(published, &public_jwk(expected));
            assert!(published.get("d").is_none());
            assert_eq!(
                published["kid"],
                thumbprint(
                    published["x"].as_str().unwrap(),
                    published["y"].as_str().unwrap()
                )
            );
        }

        // The signing key, or a key listed twice, is not published twice.
        let again = json!({"keys": [public_jwk(&old_one), public_jwk(&old_one), public_jwk(&key)]})
            .to_string();
        let key = key.with_retired_keys(&again).unwrap();
        assert_eq!(key.jwks()["keys"].as_array().unwrap().len(), 3);

        // A file with no keys is fine: a server that has retired nothing yet.
        let none = IssuerKey::from_pem(&generated_pem())
            .unwrap()
            .with_retired_keys(r#"{"keys": []}"#)
            .unwrap();
        assert_eq!(none.jwks()["keys"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_key_file_that_is_not_all_public_keys_on_p256_is_refused() {
        let good = public_jwk(&IssuerKey::from_pem(&generated_pem()).unwrap());
        let with = |member: &str, value: Value| {
            let mut jwk = good.clone();
            jwk[member] = value;
            json!({"keys": [jwk]}).to_string()
        };
        let refused = |text: &str| {
            IssuerKey::from_pem(&generated_pem())
                .unwrap()
                .with_retired_keys(text)
                .unwrap_err()
        };
        // A private member, even next to good keys, stops the file being read at all.
        let private = refused(&with("d", json!("c2VjcmV0")));
        assert!(private.contains("private member `d`"), "{private}");
        let mixed = json!({"keys": [good.clone(), {"kty": "EC", "d": "x"}]}).to_string();
        assert!(refused(&mixed).contains("private member"));
        assert!(refused(&with("crv", json!("P-384"))).contains("P-256"));
        assert!(refused(&with("kty", json!("RSA"))).contains("P-256"));
        assert!(refused(&with("alg", json!("RS256"))).contains("ES256"));
        assert!(refused(&with("use", json!("enc"))).contains("`use`"));
        // 32 bytes each, but (0, 0) is not a point on the curve.
        let zeros = "A".repeat(43);
        let off_curve = json!({"keys": [{"kty": "EC", "crv": "P-256", "x": zeros, "y": zeros}]});
        assert!(refused(&off_curve.to_string()).contains("not on the P-256 curve"));
        assert!(refused(&with("x", json!("AAAA"))).contains("32 bytes"));
        assert!(refused(&with("kid", json!("not-the-thumbprint"))).contains("thumbprint"));
        assert!(refused("not json").contains("not a JSON key set"));
        assert!(refused(r#"{"nope": []}"#).contains("keys"));
        assert!(refused(r#"{"keys": [1]}"#).contains("not an object"));

        let key = IssuerKey::from_pem(&generated_pem()).unwrap();
        assert!(key
            .with_retired_keys_file("/nonexistent/keys.json")
            .unwrap_err()
            .contains("cannot read"));
    }
}
