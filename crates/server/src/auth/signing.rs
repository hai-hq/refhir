//! Stable RSA signing material for SMART `id_token` and JWKS, kept in the auth table.
//!
//! With `REFHIR_SIGNING_KEY_PASSPHRASE` set the private key is stored as encrypted PKCS#8, so a
//! copy of the database does not contain a usable key. Without it the key is stored in clear
//! and a warning says so.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use jsonwebtoken::EncodingKey;
use refhir_persistence::AuthStore;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256};

use super::store::SIGNING_KEY;

/// Row id of the active key. Every server sharing the database signs with it.
const ACTIVE: &str = "rs256";

pub struct ServerKeys {
    pub kid: String,
    pub encoding: EncodingKey,
    pub n: String,
    pub e: String,
}

const ENCRYPTED_HEADER: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----";

/// Load the stored signing key, creating it on first use.
///
/// A stored key that does not parse is kept aside under another id and replaced. A database
/// that cannot be read stops the server rather than silently signing with a throwaway key,
/// and so does an encrypted key whose passphrase is missing or wrong: replacing it would
/// invalidate every token already issued. These cases are returned as errors, not panics, so
/// startup can report them and a request that needs the key can answer with a 500.
pub(crate) fn load_or_create(store: &dyn AuthStore) -> Result<ServerKeys, String> {
    load(store, true)
}

/// Load the stored signing key and never write one. A process that only reads (a replica)
/// uses this: it must not create a key the writer would then have to throw away, nor
/// re-encrypt or replace one that is there.
pub(crate) fn load_existing(store: &dyn AuthStore) -> Result<ServerKeys, String> {
    load(store, false)
}

fn load(store: &dyn AuthStore, create: bool) -> Result<ServerKeys, String> {
    let passphrase = std::env::var("REFHIR_SIGNING_KEY_PASSPHRASE")
        .ok()
        .filter(|value| !value.is_empty());
    load_with(store, passphrase.as_deref(), create)
}

const NO_KEY_YET: &str = "the smart signing key has not been created yet; start the primary first";

fn seal(key: &RsaPrivateKey, passphrase: Option<&str>) -> String {
    let pem = match passphrase {
        Some(passphrase) => key
            .to_pkcs8_encrypted_pem(&mut rand::thread_rng(), passphrase, LineEnding::LF)
            .expect("encrypt signing key")
            .to_string(),
        None => {
            tracing::warn!(
                "the smart signing key is stored unencrypted in the database; set REFHIR_SIGNING_KEY_PASSPHRASE"
            );
            key.to_pkcs8_pem(LineEnding::LF).expect("pem").to_string()
        }
    };
    serde_json::to_string(&pem).expect("pem json")
}

#[cfg(test)]
fn load_or_create_with(
    store: &dyn AuthStore,
    passphrase: Option<&str>,
) -> Result<ServerKeys, String> {
    load_with(store, passphrase, true)
}

fn load_with(
    store: &dyn AuthStore,
    passphrase: Option<&str>,
    create: bool,
) -> Result<ServerKeys, String> {
    loop {
        match store.auth_get(SIGNING_KEY, ACTIVE, 0) {
            Ok(Some(json)) => {
                let pem: String = serde_json::from_str(&json).unwrap_or_default();
                if pem.starts_with(ENCRYPTED_HEADER) {
                    let Some(passphrase) = passphrase else {
                        return Err(
                            "the smart signing key is encrypted: set REFHIR_SIGNING_KEY_PASSPHRASE"
                                .into(),
                        );
                    };
                    let key = RsaPrivateKey::from_pkcs8_encrypted_pem(&pem, passphrase).map_err(
                        |_| "REFHIR_SIGNING_KEY_PASSPHRASE does not open the smart signing key",
                    )?;
                    return Ok(keys_from_rsa(&key));
                }
                match RsaPrivateKey::from_pkcs8_pem(&pem) {
                    Ok(key) => {
                        if passphrase.is_some() && create {
                            // Encrypt a key that was stored before a passphrase was set.
                            if let Err(err) = store.auth_put(
                                SIGNING_KEY,
                                ACTIVE,
                                &seal(&key, passphrase),
                                u64::MAX,
                            ) {
                                tracing::warn!("smart signing key not encrypted: {err}");
                            }
                        }
                        return Ok(keys_from_rsa(&key));
                    }
                    Err(_) if !create => {
                        return Err(
                            "the stored smart signing key is invalid; the primary replaces it"
                                .into(),
                        );
                    }
                    Err(err) => {
                        let backup = format!("{ACTIVE}.corrupt-{}", super::oauth::now());
                        tracing::error!(
                            "smart signing key is invalid ({err}); generating a new one, old key kept as {backup}"
                        );
                        // Remove the bad row only if it is still the bad row: another server
                        // may have replaced it with a good key since this one read it.
                        store
                            .auth_put(SIGNING_KEY, &backup, &json, u64::MAX)
                            .and_then(|_| {
                                store.auth_delete_if(SIGNING_KEY, ACTIVE, &json).map(drop)
                            })
                            .map_err(|err| {
                                format!(
                                    "the invalid smart signing key could not be set aside: {err}"
                                )
                            })?;
                    }
                }
            }
            Ok(None) if !create => return Err(NO_KEY_YET.into()),
            Ok(None) => {}
            Err(err) => return Err(format!("smart signing key could not be read: {err}")),
        }
        let key = RsaPrivateKey::new(&mut rand::thread_rng(), 2048).expect("rsa key");
        let json = seal(&key, passphrase);
        // Another server may have created a key meanwhile; then loop and use that one.
        match store.auth_insert(SIGNING_KEY, ACTIVE, &json, u64::MAX, 0) {
            Ok(true) => return Ok(keys_from_rsa(&key)),
            Ok(false) => continue,
            Err(err) => return Err(format!("smart signing key could not be stored: {err}")),
        }
    }
}

fn keys_from_rsa(key: &RsaPrivateKey) -> ServerKeys {
    let pem = key.to_pkcs8_pem(LineEnding::LF).expect("pem");
    let encoding = EncodingKey::from_rsa_pem(pem.as_bytes()).expect("encoding key");
    let public = RsaPublicKey::from(key);
    let n = URL_SAFE_NO_PAD.encode(public.n().to_bytes_be());
    let e = URL_SAFE_NO_PAD.encode(public.e().to_bytes_be());
    // RFC 7638 thumbprint, so a new key always gets a new kid.
    let thumbprint = format!(r#"{{"e":"{e}","kty":"RSA","n":"{n}"}}"#);
    ServerKeys {
        kid: URL_SAFE_NO_PAD.encode(Sha256::digest(thumbprint.as_bytes())),
        encoding,
        n,
        e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use refhir_persistence::MemoryAuthStore;

    #[test]
    fn the_key_is_created_once_and_reused() {
        let store = MemoryAuthStore::default();
        let first = load_or_create(&store).unwrap();
        let second = load_or_create(&store).unwrap();
        assert_eq!(first.kid, second.kid);
        assert_eq!(first.n, second.n);
    }

    #[test]
    fn a_passphrase_encrypts_the_stored_key() {
        let store = MemoryAuthStore::default();
        let first = load_or_create_with(&store, Some("correct horse")).unwrap();
        let row = store.auth_get(SIGNING_KEY, ACTIVE, 0).unwrap().unwrap();
        assert!(row.contains("ENCRYPTED PRIVATE KEY"), "{row}");
        let again = load_or_create_with(&store, Some("correct horse")).unwrap();
        assert_eq!(first.kid, again.kid);
    }

    #[test]
    fn a_key_stored_in_clear_is_encrypted_once_a_passphrase_is_set() {
        let store = MemoryAuthStore::default();
        let clear = load_or_create_with(&store, None).unwrap();
        let upgraded = load_or_create_with(&store, Some("pass")).unwrap();
        assert_eq!(clear.kid, upgraded.kid);
        let row = store.auth_get(SIGNING_KEY, ACTIVE, 0).unwrap().unwrap();
        assert!(row.contains("ENCRYPTED PRIVATE KEY"));
    }

    #[test]
    fn a_wrong_or_missing_passphrase_is_an_error_and_never_replaces_the_key() {
        let store = MemoryAuthStore::default();
        let right = load_or_create_with(&store, Some("right")).unwrap();
        let wrong = load_or_create_with(&store, Some("wrong")).err().unwrap();
        assert!(wrong.contains("does not open"), "{wrong}");
        let missing = load_or_create_with(&store, None).err().unwrap();
        assert!(missing.contains("is encrypted"), "{missing}");
        assert_eq!(
            load_or_create_with(&store, Some("right")).unwrap().kid,
            right.kid
        );
    }

    #[test]
    fn an_invalid_key_is_kept_aside_and_replaced() {
        let store = MemoryAuthStore::default();
        store
            .auth_put(SIGNING_KEY, ACTIVE, "\"not a pem\"", u64::MAX)
            .unwrap();
        let keys = load_or_create(&store).unwrap();
        assert!(!keys.kid.is_empty());
        let rows = store.auth_list(SIGNING_KEY, 0).unwrap();
        assert!(rows.contains(&"\"not a pem\"".to_string()), "{rows:?}");
        assert_eq!(rows.len(), 2);
    }
}
