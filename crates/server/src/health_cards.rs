//! SMART Health Cards: the issuer's key, the card it signs, and the public key that checks it.
//!
//! `Patient/$health-cards-issue` has the engine choose and shrink what a card holds. This signs
//! it. A card is a compact JWS signed with ES256 whose payload is the credential's claims,
//! minified and compressed with raw DEFLATE. The JWS header names the key by its RFC 7638
//! thumbprint, and the public key is served at `{iss}/.well-known/jwks.json`.

use std::io::Write;
use std::path::Path;
use std::sync::RwLock;

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

/// What a tenant's key is kept as in its own database (beside its SMART signing key): the one
/// that signs, sealed when a passphrase is set, and the public halves of those it replaced.
const KEY_KIND: &str = "health_cards_key";
const ACTIVE: &str = "active";
const RETIRED_KIND: &str = "health_cards_retired";

/// The issuer's signing key: a P-256 key that signs every card this server issues.
///
/// What it signs with, the `iss` it names and the key set it publishes can change while the server
/// runs (a tenant's key is rotated, its issuer set), so they are held behind a lock that a card
/// being signed reads for as long as it takes to sign.
pub struct IssuerKey {
    rng: SystemRandom,
    live: RwLock<Live>,
}

struct Live {
    pair: EcdsaKeyPair,
    kid: String,
    /// The public half of `pair`, as the key set publishes it.
    current: Value,
    /// What `iss` says when it is not the server's own address, with no trailing slash.
    issuer: Option<String>,
    /// Public keys that signed cards before, never `current`.
    retired: Vec<Value>,
    /// The key set that checks the cards, made when anything above changes: `current`, then
    /// `retired`.
    jwks: Value,
}

impl Live {
    fn publish(&mut self) {
        let mut keys = vec![self.current.clone()];
        keys.extend(self.retired.iter().cloned());
        self.jwks = json!({ "keys": keys });
    }

    fn retire(&mut self, keys: Vec<Value>) {
        for key in keys {
            if key["kid"] != self.current["kid"]
                && !self.retired.iter().any(|known| known["kid"] == key["kid"])
            {
                self.retired.push(key);
            }
        }
        self.publish();
    }
}

impl std::fmt::Debug for IssuerKey {
    // The key itself is never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IssuerKey({})", self.kid())
    }
}

/// What [`IssuerKey::rotate_tenant`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rotated {
    /// The thumbprint of the key that signs from now on.
    pub kid: String,
    /// The thumbprint of the key that signed until now, which stays in the key set.
    pub replaced: String,
}

/// Why [`IssuerKey::rotate_tenant`] changed nothing (or nothing that matters).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RotateError {
    /// The tenant has no key yet: its primary makes one the first time it opens it.
    NoKey,
    /// Another process changed the key while this one rotated it. Nothing was lost; ask again.
    Raced,
    Failed(String),
}

impl std::fmt::Display for RotateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoKey => f.write_str("the tenant's health card key has not been made yet"),
            Self::Raced => f.write_str("the key was changed by another request meanwhile"),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

impl IssuerKey {
    /// The key in the PEM file at `path`: an unencrypted PKCS#8 `PRIVATE KEY` on P-256, as
    /// `openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256` writes it.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, String> {
        Self::from_pem(&read_file(path.as_ref())?)
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
        let current = json!({
            "kty": "EC",
            "kid": kid,
            "use": "sig",
            "alg": "ES256",
            "crv": "P-256",
            "x": x,
            "y": y
        });
        let mut live = Live {
            pair,
            kid,
            current,
            issuer: None,
            retired: Vec::new(),
            jwks: Value::Null,
        };
        live.publish();
        Ok(Self {
            rng,
            live: RwLock::new(live),
        })
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Live> {
        self.live
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Live> {
        self.live
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn kid(&self) -> String {
        self.read().kid.clone()
    }

    /// Cards name `issuer` as `iss`, not an address of this server's. It is checked by
    /// [`validate_issuer`] and kept with no trailing slash.
    pub fn with_issuer(self, issuer: &str) -> Result<Self, String> {
        self.set_issuer(Some(issuer))?;
        Ok(self)
    }

    /// Names `issuer` as `iss` from now on, or with `None` this server's own address again.
    pub fn set_issuer(&self, issuer: Option<&str>) -> Result<(), String> {
        let issuer = issuer.map(validate_issuer).transpose()?;
        self.write().issuer = issuer;
        Ok(())
    }

    /// The `iss` the cards name when one was set.
    pub fn issuer(&self) -> Option<String> {
        self.read().issuer.clone()
    }

    /// Also publish the public keys in the JWKS file at `path`, so a key that was replaced stays
    /// in the key set and the cards it signed still verify.
    pub fn with_retired_keys_file(self, path: impl AsRef<Path>) -> Result<Self, String> {
        self.with_retired_keys(&read_file(path.as_ref())?)
    }

    pub fn with_retired_keys(self, jwks: &str) -> Result<Self, String> {
        let keys = public_keys(jwks)?;
        self.write().retire(keys);
        Ok(self)
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
    /// makes it the first time (`create`); another process waits for it to be there. The key set
    /// also holds the public keys of the ones [`IssuerKey::rotate_tenant`] replaced.
    pub fn for_tenant(
        store: &dyn refhir_persistence::AuthStore,
        create: bool,
    ) -> Result<Self, String> {
        Self::for_tenant_sealed(store, create, Self::tenant_passphrase().as_deref())
    }

    fn for_tenant_sealed(
        store: &dyn refhir_persistence::AuthStore,
        create: bool,
        passphrase: Option<&str>,
    ) -> Result<Self, String> {
        let key = Self::tenant_signing_key(store, create, passphrase)?;
        let retired = tenant_retired_keys(store)?;
        key.write().retire(retired);
        Ok(key)
    }

    /// The passphrase that seals the tenant's keys: the one that seals its SMART signing key.
    fn tenant_passphrase() -> Option<String> {
        std::env::var("REFHIR_SIGNING_KEY_PASSPHRASE")
            .ok()
            .filter(|value| !value.is_empty())
    }

    /// A stored key (sealed or not) as a key.
    fn open_stored(pem: &str, passphrase: Option<&str>) -> Result<Self, String> {
        if pem.contains(ENCRYPTED_BEGIN) {
            let passphrase = passphrase
                .ok_or("the health card key is encrypted: set REFHIR_SIGNING_KEY_PASSPHRASE")?;
            return Self::from_der(&unseal(pem, passphrase)?);
        }
        Self::from_pem(pem)
    }

    /// The tenant's signing key alone, made if there is none and `create`.
    fn tenant_signing_key(
        store: &dyn refhir_persistence::AuthStore,
        create: bool,
        passphrase: Option<&str>,
    ) -> Result<Self, String> {
        let stored = |store: &dyn refhir_persistence::AuthStore| -> Result<Option<Self>, String> {
            match store.auth_get(KEY_KIND, ACTIVE, 0) {
                Ok(Some(json)) => {
                    let pem: String = serde_json::from_str(&json)
                        .map_err(|err| format!("the stored health card key: {err}"))?;
                    let key = Self::open_stored(&pem, passphrase)?;
                    if let (true, false, Some(passphrase)) =
                        (create, pem.contains(ENCRYPTED_BEGIN), passphrase)
                    {
                        // A key stored before a passphrase was set is sealed now.
                        match seal(&pem, passphrase).and_then(|sealed| {
                            serde_json::to_string(&sealed).map_err(|err| err.to_string())
                        }) {
                            Ok(sealed) => {
                                if let Err(err) =
                                    store.auth_put(KEY_KIND, ACTIVE, &sealed, u64::MAX)
                                {
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
        let json = sealed_json(&pem, passphrase)?;
        let inserted = store
            .auth_insert(KEY_KIND, ACTIVE, &json, u64::MAX, 0)
            .map_err(|err| format!("cannot store the health card key: {err}"))?;
        if inserted {
            return Ok(key);
        }
        // Another process made one first: use that one.
        stored(store)?.ok_or_else(|| "the health card key vanished".to_string())
    }

    /// Reads the tenant's key and the keys it replaced again, and uses them if another process
    /// rotated the key. Whether anything changed. The issuer is not touched.
    pub fn refresh(&self, store: &dyn refhir_persistence::AuthStore) -> Result<bool, String> {
        let passphrase = Self::tenant_passphrase();
        let fresh = Self::tenant_signing_key(store, false, passphrase.as_deref())?;
        let retired = tenant_retired_keys(store)?;
        let mut fresh = fresh.live.into_inner().unwrap_or_else(|p| p.into_inner());
        fresh.retire(retired);
        let mut live = self.write();
        if live.kid == fresh.kid && live.retired == fresh.retired {
            return Ok(false);
        }
        fresh.issuer = live.issuer.take();
        *live = fresh;
        Ok(true)
    }

    /// Replaces the tenant's signing key with a new one. The key it replaces keeps its place in
    /// the key set, as a public key, for as long as the tenant has one: a card never expires, and
    /// the key that signed it must stay there to check it. Nothing is read or written but the
    /// tenant's own rows, and a process that serves the tenant uses the new key after it
    /// [refreshes](IssuerKey::refresh).
    pub fn rotate_tenant(
        store: &dyn refhir_persistence::AuthStore,
    ) -> Result<Rotated, RotateError> {
        Self::rotate_tenant_sealed(store, Self::tenant_passphrase().as_deref())
    }

    fn rotate_tenant_sealed(
        store: &dyn refhir_persistence::AuthStore,
        passphrase: Option<&str>,
    ) -> Result<Rotated, RotateError> {
        let failed =
            |what: &str, err: &dyn std::fmt::Display| RotateError::Failed(format!("{what}: {err}"));
        let stored = store
            .auth_get(KEY_KIND, ACTIVE, 0)
            .map_err(|err| failed("cannot read the health card key", &err))?
            .ok_or(RotateError::NoKey)?;
        let pem: String = serde_json::from_str(&stored)
            .map_err(|err| failed("the stored health card key", &err))?;
        let old = Self::open_stored(&pem, passphrase).map_err(RotateError::Failed)?;
        let (new, new_pem) = Self::generate().map_err(RotateError::Failed)?;
        let replacement = sealed_json(&new_pem, passphrase).map_err(RotateError::Failed)?;
        let (old_kid, old_public) = {
            let live = old.read();
            (live.kid.clone(), live.current.clone())
        };
        // The old key is published before it stops signing, so there is never a moment when a card
        // it signed has no key to check it.
        let row = json!({"retired_at_ms": crate::governance::now_ms(), "jwk": old_public});
        store
            .auth_put(RETIRED_KIND, &old_kid, &row.to_string(), u64::MAX)
            .map_err(|err| failed("cannot keep the replaced key", &err))?;
        // Only if it is still the key that was read: of two rotations at once, one wins.
        match store.auth_delete_if(KEY_KIND, ACTIVE, &stored) {
            Ok(true) => {}
            Ok(false) => return Err(RotateError::Raced),
            Err(err) => return Err(failed("cannot replace the health card key", &err)),
        }
        match store.auth_insert(KEY_KIND, ACTIVE, &replacement, u64::MAX, 0) {
            Ok(true) => Ok(Rotated {
                kid: new.kid(),
                replaced: old_kid,
            }),
            Ok(false) => Err(RotateError::Raced),
            Err(err) => {
                // Put the old key back, so the tenant is not left with none.
                let _ = store.auth_insert(KEY_KIND, ACTIVE, &stored, u64::MAX, 0);
                Err(failed("cannot store the new health card key", &err))
            }
        }
    }

    /// The key set that checks the cards: this key and any retired ones, never a private part.
    pub fn jwks(&self) -> Value {
        self.read().jwks.clone()
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
        let live = self.read();
        let header = json!({"zip": "DEF", "alg": "ES256", "kid": live.kid});
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).map_err(|err| err.to_string())?),
            URL_SAFE_NO_PAD.encode(compressed)
        );
        // `FIXED` is the JWS form of an ECDSA signature: r then s, 32 bytes each.
        let signature = live
            .pair
            .sign(&self.rng, signing_input.as_bytes())
            .map_err(|_| "the key could not sign".to_string())?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        ))
    }
}

/// The JSON a tenant's key is stored as: its PEM, sealed when there is a passphrase.
fn sealed_json(pem: &str, passphrase: Option<&str>) -> Result<String, String> {
    let pem = match passphrase {
        Some(passphrase) => seal(pem, passphrase)?,
        None => {
            tracing::warn!(
                "a health card signing key is stored unencrypted in the database; set \
                 REFHIR_SIGNING_KEY_PASSPHRASE"
            );
            pem.to_string()
        }
    };
    Ok(serde_json::to_string(&pem).expect("pem json"))
}

/// The public keys of the keys the tenant replaced, the earliest replaced first.
fn tenant_retired_keys(store: &dyn refhir_persistence::AuthStore) -> Result<Vec<Value>, String> {
    let rows = store
        .auth_list(RETIRED_KIND, 0)
        .map_err(|err| format!("cannot read the replaced health card keys: {err}"))?;
    let mut jwks: Vec<Value> = Vec::with_capacity(rows.len());
    let mut retired_at: std::collections::HashMap<String, u64> = Default::default();
    for row in rows {
        let row: Value = serde_json::from_str(&row)
            .map_err(|err| format!("a stored replaced health card key: {err}"))?;
        if let Some(kid) = row["jwk"]["kid"].as_str() {
            retired_at.insert(kid.to_string(), row["retired_at_ms"].as_u64().unwrap_or(0));
        }
        jwks.push(row["jwk"].clone());
    }
    // The same checks as a key set a person wrote: public, P-256, and a thumbprint `kid`.
    let mut keys: Vec<(u64, Value)> = public_keys_of(&jwks)?
        .into_iter()
        .map(|key| {
            let at = key["kid"]
                .as_str()
                .and_then(|kid| retired_at.get(kid))
                .copied()
                .unwrap_or(0);
            (at, key)
        })
        .collect();
    keys.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1["kid"].as_str().cmp(&b.1["kid"].as_str()))
    });
    Ok(keys.into_iter().map(|(_, key)| key).collect())
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
    public_keys_of(keys)
}

/// [`public_keys`] for the members of a `keys` array that are already parsed.
fn public_keys_of(keys: &[Value]) -> Result<Vec<Value>, String> {
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
    use refhir_persistence::AuthStore;
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
        assert_eq!(
            key.issuer().as_deref(),
            Some("https://health.clinic.example")
        );
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

    /// Whether the card's own `kid` names a key of `jwks` that checks its signature.
    fn verifies(jwks: &Value, card: &str) -> bool {
        let parts: Vec<&str> = card.split('.').collect();
        let header: Value = serde_json::from_slice(&decode(parts[0])).unwrap();
        let Some(jwk) = jwks["keys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|key| key["kid"] == header["kid"])
        else {
            return false;
        };
        checks(
            jwk,
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &decode(parts[2]),
        )
    }

    fn card(key: &IssuerKey) -> String {
        let bundle = json!({"resourceType": "Bundle", "type": "collection", "entry": []});
        key.sign(
            "https://tenant.example/health-cards",
            1,
            &[],
            "4.0.1",
            &bundle,
        )
        .unwrap()
    }

    fn kids(key: &IssuerKey) -> Vec<String> {
        key.jwks()["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|key| key["kid"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_rotated_tenant_key_keeps_the_replaced_key_public_so_the_old_cards_still_verify() {
        let store = refhir_persistence::MemoryAuthStore::default();
        let key = IssuerKey::for_tenant(&store, true).unwrap();
        let first = key.kid();
        let before = card(&key);
        assert_eq!(kids(&key), [first.clone()]);

        let rotated = IssuerKey::rotate_tenant(&store).unwrap();
        assert_eq!(rotated.replaced, first);
        assert_ne!(rotated.kid, first);
        // This process signs with the new key once it reads what the rotation wrote.
        assert_eq!(key.kid(), first);
        assert!(key.refresh(&store).unwrap());
        assert!(!key.refresh(&store).unwrap(), "nothing more to read");
        assert_eq!(key.kid(), rotated.kid);
        assert_eq!(kids(&key), [rotated.kid.clone(), first.clone()]);

        let after = card(&key);
        let jwks = key.jwks();
        assert!(verifies(&jwks, &before), "a card the replaced key signed");
        assert!(verifies(&jwks, &after), "a card the new key signs");
        let header: Value =
            serde_json::from_slice(&decode(after.split('.').next().unwrap())).unwrap();
        assert_eq!(header["kid"], rotated.kid);

        // Each key is a public key under its thumbprint, and no private member is published.
        for published in jwks["keys"].as_array().unwrap() {
            assert!(published.get("d").is_none(), "{published}");
            assert_eq!(
                published["kid"],
                thumbprint(
                    published["x"].as_str().unwrap(),
                    published["y"].as_str().unwrap()
                )
            );
        }
        assert!(!jwks.to_string().contains("PRIVATE"));

        // Twice: all three keys, the one that signs first.
        let again = IssuerKey::rotate_tenant(&store).unwrap();
        assert_eq!(again.replaced, rotated.kid);
        key.refresh(&store).unwrap();
        let all = kids(&key);
        assert_eq!(all[0], again.kid);
        let mut replaced = all[1..].to_vec();
        replaced.sort();
        let mut expected = vec![rotated.kid.clone(), first.clone()];
        expected.sort();
        assert_eq!(replaced, expected);
        let newest = card(&key);
        for old in [&before, &after, &newest] {
            assert!(verifies(&key.jwks(), old));
        }

        // A process that opens the tenant afterwards (or a replica) has the same key set.
        let reopened = IssuerKey::for_tenant(&store, false).unwrap();
        assert_eq!(kids(&reopened), kids(&key));
        assert!(verifies(&reopened.jwks(), &before));
    }

    #[test]
    fn a_process_that_did_not_rotate_the_key_signs_with_the_new_one_after_it_refreshes() {
        let store = refhir_persistence::MemoryAuthStore::default();
        let primary = IssuerKey::for_tenant(&store, true).unwrap();
        let replica = IssuerKey::for_tenant(&store, false).unwrap();
        replica
            .set_issuer(Some("https://cards.example.com"))
            .unwrap();
        let before = card(&replica);
        let rotated = IssuerKey::rotate_tenant(&store).unwrap();
        primary.refresh(&store).unwrap();
        assert_eq!(replica.kid(), rotated.replaced, "until it reads the change");
        replica.refresh(&store).unwrap();
        assert_eq!(replica.kid(), rotated.kid);
        assert_eq!(replica.jwks(), primary.jwks());
        assert!(verifies(&replica.jwks(), &before));
        // The issuer is a setting, not part of the key: a refresh leaves it.
        assert_eq!(
            replica.issuer().as_deref(),
            Some("https://cards.example.com")
        );
    }

    #[test]
    fn rotating_a_tenant_without_a_key_or_a_key_changed_meanwhile_changes_nothing() {
        let store = refhir_persistence::MemoryAuthStore::default();
        assert_eq!(IssuerKey::rotate_tenant(&store), Err(RotateError::NoKey));
        assert!(store.auth_list(RETIRED_KIND, 0).unwrap().is_empty());

        let key = IssuerKey::for_tenant(&store, true).unwrap();
        // The key stored is replaced by another between the read and the swap.
        struct Races<'a>(&'a refhir_persistence::MemoryAuthStore);
        impl refhir_persistence::AuthStore for Races<'_> {
            fn auth_put(
                &self,
                kind: &str,
                id: &str,
                json: &str,
                exp: u64,
            ) -> Result<(), refhir_persistence::PersistError> {
                self.0.auth_put(kind, id, json, exp)
            }
            fn auth_insert(
                &self,
                kind: &str,
                id: &str,
                json: &str,
                exp: u64,
                now: u64,
            ) -> Result<bool, refhir_persistence::PersistError> {
                self.0.auth_insert(kind, id, json, exp, now)
            }
            fn auth_get(
                &self,
                kind: &str,
                id: &str,
                now: u64,
            ) -> Result<Option<String>, refhir_persistence::PersistError> {
                self.0.auth_get(kind, id, now)
            }
            fn auth_take(
                &self,
                kind: &str,
                id: &str,
                now: u64,
            ) -> Result<Option<String>, refhir_persistence::PersistError> {
                self.0.auth_take(kind, id, now)
            }
            fn auth_delete_if(
                &self,
                kind: &str,
                id: &str,
                json: &str,
            ) -> Result<bool, refhir_persistence::PersistError> {
                // Another rotation lands first.
                IssuerKey::rotate_tenant(self.0).unwrap();
                self.0.auth_delete_if(kind, id, json)
            }
            fn auth_list(
                &self,
                kind: &str,
                now: u64,
            ) -> Result<Vec<String>, refhir_persistence::PersistError> {
                self.0.auth_list(kind, now)
            }
            fn auth_prune(&self, now: u64) -> Result<(), refhir_persistence::PersistError> {
                self.0.auth_prune(now)
            }
        }
        assert_eq!(
            IssuerKey::rotate_tenant(&Races(&store)),
            Err(RotateError::Raced)
        );
        // The other rotation stands, and no key that signed anything was lost: the original is
        // public, and the tenant has a key that signs.
        key.refresh(&store).unwrap();
        assert_eq!(kids(&key).len(), 2);
        assert!(verifies(&key.jwks(), &card(&key)));
    }

    #[test]
    fn a_sealed_tenant_key_stays_sealed_when_it_is_rotated_and_the_replaced_key_is_public_only() {
        let store = refhir_persistence::MemoryAuthStore::default();
        let passphrase = Some("a long enough passphrase");
        let key = IssuerKey::for_tenant_sealed(&store, true, passphrase).unwrap();
        let first = key.kid();
        IssuerKey::rotate_tenant_sealed(&store, passphrase).unwrap();
        let stored = store.auth_get(KEY_KIND, ACTIVE, 0).unwrap().unwrap();
        assert!(stored.contains("ENCRYPTED PRIVATE KEY"), "{stored}");
        let retired = store.auth_list(RETIRED_KIND, 0).unwrap().join("");
        assert!(
            !retired.contains("PRIVATE") && !retired.contains("\"d\""),
            "{retired}"
        );
        let reopened = IssuerKey::for_tenant_sealed(&store, false, passphrase).unwrap();
        assert!(kids(&reopened).contains(&first));
        assert_ne!(reopened.kid(), first);
        // Without the passphrase it does not open.
        assert!(IssuerKey::for_tenant_sealed(&store, false, None).is_err());
    }
}
