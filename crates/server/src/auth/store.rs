//! SMART clients, codes, launches, login sessions, and tokens, kept in the database.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use refhir_persistence::{AuthStore, MemoryAuthStore, PersistError};
use sha2::{Digest, Sha256};

use super::Token;

const CODE_TTL: u64 = 300;
const LAUNCH_TTL: u64 = 600;
/// How long a token or the client origin set read from the database is reused. A revoked
/// token stays usable for at most this long on other processes (never on this one).
const CACHE_TTL: Duration = Duration::from_secs(2);
const TOKEN_CACHE_MAX: usize = 4096;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Client {
    pub id: String,
    pub secret: Option<String>,
    pub public: bool,
    pub redirect_uris: Vec<String>,
    pub jwks: Option<serde_json::Value>,
    pub grants: Vec<String>,
    pub allowed_scopes: Option<Vec<String>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct AuthCode {
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    pub challenge: String,
    pub patient: Option<String>,
    pub encounter: Option<String>,
    pub fhir_user: Option<String>,
    pub sub: String,
    pub nonce: Option<String>,
    pub exp: u64,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingSession {
    pub client_id: String,
    pub redirect_uri: String,
    pub state: String,
    pub scopes: Vec<String>,
    pub challenge: String,
    pub patient: Option<String>,
    pub encounter: Option<String>,
    pub fhir_user: Option<String>,
    pub needs_patient_pick: bool,
    pub needs_encounter_pick: bool,
    pub nonce: Option<String>,
    /// Set once the identity provider has authenticated the user.
    #[serde(default)]
    pub identity: Option<super::oidc::Identity>,
    /// Set while the browser is away at the identity provider.
    #[serde(default)]
    pub oidc: Option<super::oidc::OidcFlow>,
    /// Binder cookie of the browser that signed in; the consent POST must present it.
    #[serde(default)]
    pub browser: Option<String>,
    pub exp: u64,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Launch {
    pub client_id: String,
    pub patient: Option<String>,
    pub encounter: Option<String>,
    pub fhir_user: Option<String>,
    pub exp: u64,
}

pub struct SmartStore {
    backend: Arc<dyn AuthStore>,
    /// Loaded from `backend` on first use; see [`SmartStore::signing_keys`].
    keys: std::sync::OnceLock<super::signing::ServerKeys>,
    /// Token lookups happen several times per request; keep them off the database.
    token_cache: Mutex<HashMap<String, (Instant, Token)>>,
    origin_cache: Mutex<Option<(Instant, HashSet<String>)>>,
}

impl std::fmt::Debug for SmartStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SmartStore")
    }
}

/// Row kinds in the auth table.
const CLIENT: &str = "client";
const CODE: &str = "code";
const LAUNCH: &str = "launch";
const PENDING: &str = "pending";
const TOKEN: &str = "token";
const REFRESH: &str = "refresh";
const JTI: &str = "jti";
pub(crate) const SIGNING_KEY: &str = "signing_key";

/// Bearer values (codes, tokens, session ids) are stored only as their SHA-256, so a copy
/// of the database does not hand out working credentials.
fn hashed(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

const SECRET_PREFIX: &str = "sha256:";

/// What is stored for a client secret. Secrets are random, so a plain SHA-256 is enough.
pub(crate) fn hash_secret(secret: &str) -> String {
    format!("{SECRET_PREFIX}{}", hashed(secret))
}

impl Client {
    /// Check a presented secret. Rows saved before secrets were hashed are compared as is.
    pub(crate) fn secret_matches(&self, given: &str) -> bool {
        match &self.secret {
            Some(stored) if stored.starts_with(SECRET_PREFIX) => {
                super::oidc::constant_time_eq(stored, &hash_secret(given))
            }
            Some(stored) => super::oidc::constant_time_eq(stored, given),
            None => false,
        }
    }
}

impl SmartStore {
    /// Tables in process memory. For tests and servers that do not need to survive a restart.
    pub fn with_defaults() -> Self {
        Self::with_confidential_secret("secret")
    }

    pub fn with_confidential_secret(secret: impl Into<String>) -> Self {
        Self::open(Arc::new(MemoryAuthStore::default()), secret)
    }

    /// Tables in `backend`. The built-in clients are (re)written, so the configured secret
    /// always wins over the one saved last run.
    pub fn open(backend: Arc<dyn AuthStore>, confidential_secret: impl Into<String>) -> Self {
        let store = Self {
            backend,
            keys: std::sync::OnceLock::new(),
            token_cache: Mutex::new(HashMap::new()),
            origin_cache: Mutex::new(None),
        };
        let redirects: Vec<String> = vec![
            "http://127.0.0.1/cb".into(),
            "http://localhost/cb".into(),
            "http://127.0.0.1/app".into(),
            "http://localhost/app".into(),
        ];
        store
            .put_client(Client {
                id: "public".into(),
                secret: None,
                public: true,
                redirect_uris: redirects.clone(),
                jwks: None,
                grants: vec!["authorization_code".into(), "refresh_token".into()],
                allowed_scopes: None,
            })
            .expect("store the built-in smart clients");
        store
            .put_client(Client {
                id: "confidential".into(),
                secret: Some(hash_secret(&confidential_secret.into())),
                public: false,
                redirect_uris: redirects,
                jwks: None,
                grants: vec!["authorization_code".into(), "refresh_token".into()],
                allowed_scopes: None,
            })
            .expect("store the built-in smart clients");
        store
    }

    /// Load the signing key now instead of on the first token request, so a missing or wrong
    /// passphrase is a startup error. Call at startup.
    pub fn init_signing_key(&self) -> Result<(), String> {
        self.signing_keys().map(|_| ())
    }

    /// The key `id_token`s are signed with, loaded from (or created in) the database. A failed
    /// load is not remembered, so a later request can try again.
    pub(crate) fn signing_keys(&self) -> Result<&super::signing::ServerKeys, String> {
        if let Some(keys) = self.keys.get() {
            return Ok(keys);
        }
        let loaded = super::signing::load_or_create(self.backend.as_ref())?;
        Ok(self.keys.get_or_init(|| loaded))
    }

    /// A failed write is reported, so callers never hand out a credential that was not stored.
    fn put<T: serde::Serialize>(
        &self,
        kind: &str,
        id: &str,
        value: &T,
        exp: u64,
    ) -> Result<(), PersistError> {
        let json = serde_json::to_string(value)
            .map_err(|err| PersistError::Definite(format!("smart {kind} json: {err}")))?;
        self.backend.auth_put(kind, id, &json, exp).map_err(|err| {
            tracing::error!("smart {kind} not stored: {err}");
            err
        })
    }

    /// Storage errors read as "not found", so a failing database denies access.
    fn decode<T: serde::de::DeserializeOwned>(
        kind: &str,
        row: Result<Option<String>, PersistError>,
    ) -> Option<T> {
        match row {
            Ok(json) => json.and_then(|json| match serde_json::from_str(&json) {
                Ok(value) => Some(value),
                Err(err) => {
                    tracing::error!("smart {kind} row is unreadable: {err}");
                    None
                }
            }),
            Err(err) => {
                tracing::error!("smart {kind} lookup failed: {err}");
                None
            }
        }
    }

    fn get<T: serde::de::DeserializeOwned>(&self, kind: &str, id: &str) -> Option<T> {
        Self::decode(kind, self.backend.auth_get(kind, id, now()))
    }

    fn take<T: serde::de::DeserializeOwned>(&self, kind: &str, id: &str) -> Option<T> {
        Self::decode(kind, self.backend.auth_take(kind, id, now()))
    }

    /// Delete expired rows. Reads already ignore them, so this only reclaims space; run it
    /// from a background task, not per request.
    pub fn prune(&self) {
        if let Err(err) = self.backend.auth_prune(now()) {
            tracing::warn!("smart prune failed: {err}");
        }
    }

    pub(crate) fn client(&self, id: &str) -> Option<Client> {
        self.get(CLIENT, id)
    }

    pub(crate) fn put_client(&self, client: Client) -> Result<(), PersistError> {
        self.put(CLIENT, &client.id.clone(), &client, u64::MAX)?;
        *self.origin_cache.lock().expect("origins") = None;
        Ok(())
    }

    pub(crate) fn put_code(&self, id: String, code: AuthCode) -> Result<(), PersistError> {
        self.put(CODE, &hashed(&id), &code, code.exp)
    }

    pub(crate) fn take_code(&self, id: &str) -> Option<AuthCode> {
        self.take(CODE, &hashed(id))
    }

    pub(crate) fn put_launch(&self, id: String, launch: Launch) -> Result<(), PersistError> {
        self.put(LAUNCH, &hashed(&id), &launch, launch.exp)
    }

    pub(crate) fn launch(&self, id: &str) -> Option<Launch> {
        self.get(LAUNCH, &hashed(id))
    }

    pub(crate) fn put_pending(
        &self,
        id: String,
        session: PendingSession,
    ) -> Result<(), PersistError> {
        self.put(PENDING, &hashed(&id), &session, session.exp)
    }

    pub(crate) fn take_pending(&self, id: &str) -> Option<PendingSession> {
        self.take(PENDING, &hashed(id))
    }

    pub(crate) fn pending(&self, id: &str) -> Option<PendingSession> {
        self.get(PENDING, &hashed(id))
    }

    pub(crate) fn token(&self, id: &str) -> Option<Token> {
        let key = hashed(id);
        if let Some((at, token)) = self.token_cache.lock().expect("tokens").get(&key) {
            if at.elapsed() < CACHE_TTL {
                return Some(token.clone());
            }
        }
        let token: Token = self.get(TOKEN, &key)?;
        let mut cache = self.token_cache.lock().expect("tokens");
        if cache.len() >= TOKEN_CACHE_MAX {
            cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
            if cache.len() >= TOKEN_CACHE_MAX {
                cache.clear();
            }
        }
        cache.insert(key, (Instant::now(), token.clone()));
        Some(token)
    }

    /// Whether `token()` would answer from memory, without touching the database.
    pub(crate) fn token_is_cached(&self, id: &str) -> bool {
        self.token_cache
            .lock()
            .expect("tokens")
            .get(&hashed(id))
            .is_some_and(|(at, _)| at.elapsed() < CACHE_TTL)
    }

    pub(crate) fn put_token(&self, id: String, token: Token) -> Result<(), PersistError> {
        self.put(TOKEN, &hashed(&id), &token, token.exp)
    }

    pub(crate) fn remove_token(&self, id: &str) -> Option<Token> {
        let key = hashed(id);
        self.token_cache.lock().expect("tokens").remove(&key);
        self.take(TOKEN, &key)
    }

    pub(crate) fn refresh(&self, id: &str) -> Option<Token> {
        self.get(REFRESH, &hashed(id))
    }

    pub(crate) fn take_refresh(&self, id: &str) -> Option<Token> {
        self.take(REFRESH, &hashed(id))
    }

    pub(crate) fn put_refresh(&self, id: String, token: Token) -> Result<(), PersistError> {
        self.put(REFRESH, &hashed(&id), &token, token.exp)
    }

    /// Record a client assertion id. False if it was already used and has not expired.
    pub(crate) fn remember_jti(&self, jti: &str, exp: u64) -> bool {
        match self.backend.auth_insert(JTI, jti, "{}", exp, now()) {
            Ok(inserted) => inserted,
            Err(err) => {
                tracing::error!("smart jti not recorded: {err}");
                false
            }
        }
    }

    pub(crate) fn origins(&self) -> HashSet<String> {
        if let Some((at, origins)) = &*self.origin_cache.lock().expect("origins") {
            if at.elapsed() < CACHE_TTL {
                return origins.clone();
            }
        }
        let rows = match self.backend.auth_list(CLIENT, now()) {
            Ok(rows) => rows,
            Err(err) => {
                tracing::error!("smart clients not listed: {err}");
                return HashSet::new();
            }
        };
        let origins: HashSet<String> = rows
            .iter()
            .filter_map(|json| serde_json::from_str::<Client>(json).ok())
            .flat_map(|client| client.redirect_uris)
            .filter_map(|uri| origin_of(&uri))
            .collect();
        *self.origin_cache.lock().expect("origins") = Some((Instant::now(), origins.clone()));
        origins
    }

    #[cfg(test)]
    pub fn install_token(&self, token: &str, scopes: &[&str], patient: Option<&str>) {
        self.install_token_exp(token, scopes, patient, u64::MAX);
    }

    #[cfg(test)]
    pub fn install_token_exp(&self, token: &str, scopes: &[&str], patient: Option<&str>, exp: u64) {
        let _ = self.put_token(
            token.to_string(),
            Token {
                scopes: scopes.iter().map(|scope| (*scope).to_string()).collect(),
                patient: patient.map(|id| normalize_patient_id(id)),
                encounter: None,
                fhir_user: None,
                client_id: "test".into(),
                exp,
                sub: None,
                iss: None,
                need_patient_banner: None,
                smart_style_url: None,
            },
        );
    }
}

pub(crate) fn redirect_allowed(client: &Client, redirect_uri: &str) -> bool {
    client
        .redirect_uris
        .iter()
        .any(|registered| uri_matches(registered, redirect_uri))
}

pub(crate) fn origin_allowed(store: &SmartStore, origin: &str) -> bool {
    let Some(got) = parse_http_uri(origin) else {
        return false;
    };
    store.origins().iter().any(|allowed| {
        let Some(registered) = parse_http_uri(allowed) else {
            return false;
        };
        if registered.scheme != got.scheme || registered.host != got.host {
            return false;
        }
        if is_loopback_host(&registered.host) && is_loopback_host(&got.host) {
            return registered.host == got.host;
        }
        registered.port == got.port
    })
}

pub(crate) fn normalize_patient_id(raw: &str) -> String {
    raw.trim().trim_start_matches("Patient/").trim().to_string()
}

pub(crate) const fn code_ttl() -> u64 {
    CODE_TTL
}

pub(crate) const fn launch_ttl() -> u64 {
    LAUNCH_TTL
}

pub(crate) struct HttpUri {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    path: String,
    query: String,
}

pub(crate) fn redirect_uri_valid(uri: &str) -> bool {
    let Some(parsed) = parse_http_uri(uri) else {
        return false;
    };
    if parsed.scheme == "https" {
        return true;
    }
    parsed.scheme == "http" && is_loopback_host(&parsed.host)
}

fn uri_matches(registered: &str, actual: &str) -> bool {
    let Some(want) = parse_http_uri(registered) else {
        return false;
    };
    let Some(got) = parse_http_uri(actual) else {
        return false;
    };
    if want.scheme != got.scheme || want.path != got.path || want.query != got.query {
        return false;
    }
    if want.host != got.host {
        return false;
    }
    if want.port == got.port {
        return true;
    }
    is_loopback_host(&want.host) && is_loopback_host(&got.host)
}

pub(crate) fn parse_http_uri(uri: &str) -> Option<HttpUri> {
    if uri.contains('#') {
        return None;
    }
    let (scheme, rest) = uri.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let (authority, path_query) = match rest.split_once('/') {
        Some((authority, rest)) => (authority, format!("/{rest}")),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = split_host_port(authority)?;
    let (path, query) = match path_query.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (path_query, String::new()),
    };
    Some(HttpUri {
        scheme: scheme.to_string(),
        host: host.to_ascii_lowercase(),
        port,
        path,
        query,
    })
}

fn split_host_port(authority: &str) -> Option<(String, Option<u16>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        if tail.is_empty() {
            return Some((host.to_string(), None));
        }
        let port = tail.strip_prefix(':')?.parse().ok()?;
        return Some((host.to_string(), Some(port)));
    }
    if let Some((host, port)) = authority.rsplit_once(':') {
        if host.parse::<std::net::Ipv4Addr>().is_ok() || !host.contains(':') {
            return Some((host.to_string(), Some(port.parse().ok()?)));
        }
    }
    Some((authority.to_string(), None))
}

pub(crate) fn is_loopback_host(host: &str) -> bool {
    host == "127.0.0.1" || host == "localhost" || host == "::1"
}

fn origin_of(uri: &str) -> Option<String> {
    let parsed = parse_http_uri(uri)?;
    match parsed.port {
        Some(port) => Some(format!("{}://{}:{port}", parsed.scheme, parsed.host)),
        None => Some(format!("{}://{}", parsed.scheme, parsed.host)),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite_store(secret: &str) -> (SmartStore, Arc<refhir_sqlite::SqlitePersistence>) {
        let db = Arc::new(refhir_sqlite::SqlitePersistence::open_in_memory().unwrap());
        (SmartStore::open(db.clone(), secret), db)
    }

    #[test]
    fn configured_secret_replaces_stored_secret() {
        let (_, db) = sqlite_store("first");
        let reopened = SmartStore::open(db, "second");
        let client = reopened.client("confidential").unwrap();
        assert!(client.secret_matches("second"));
        assert!(!client.secret_matches("first"));
        assert_ne!(client.secret.as_deref(), Some("second"), "stored hashed");
    }

    #[test]
    fn state_survives_reopening_the_database() {
        let (store, db) = sqlite_store("s");
        store.install_token("tok", &["user/*.rs"], None);
        let reopened = SmartStore::open(db, "s");
        assert!(reopened.token("tok").is_some());
    }

    #[test]
    fn tokens_and_signing_key_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("refhir-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("db.sqlite");
        let kid = {
            let db = Arc::new(refhir_sqlite::SqlitePersistence::open(&file).unwrap());
            let store = SmartStore::open(db, "s");
            store.install_token("tok", &["user/*.rs"], None);
            store.signing_keys().unwrap().kid.clone()
        };
        let db = Arc::new(refhir_sqlite::SqlitePersistence::open(&file).unwrap());
        let store = SmartStore::open(db, "s");
        assert!(store.token("tok").is_some());
        assert_eq!(store.signing_keys().unwrap().kid, kid);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_code_is_redeemed_once() {
        let (store, db) = sqlite_store("s");
        let code = AuthCode {
            client_id: "public".into(),
            redirect_uri: "http://127.0.0.1/cb".into(),
            scopes: vec![],
            challenge: String::new(),
            patient: None,
            encounter: None,
            fhir_user: None,
            sub: "u".into(),
            nonce: None,
            exp: u64::MAX,
        };
        store.put_code("c1".into(), code).unwrap();
        let other = SmartStore::open(db, "s");
        let first = std::thread::spawn(move || store.take_code("c1").is_some());
        let second = other.take_code("c1").is_some();
        assert!(
            first.join().unwrap() ^ second,
            "exactly one redemption wins"
        );
    }

    #[test]
    fn bearer_values_are_stored_hashed() {
        let (store, db) = sqlite_store("s");
        store.install_token("plain-token", &["user/*.rs"], None);
        assert!(db.auth_get(TOKEN, "plain-token", 0).unwrap().is_none());
        assert!(db
            .auth_get(TOKEN, &hashed("plain-token"), 0)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_client_assertion_id_is_accepted_once() {
        let store = SmartStore::with_defaults();
        assert!(store.remember_jti("j1", u64::MAX));
        assert!(!store.remember_jti("j1", u64::MAX));
    }

    #[test]
    fn loopback_is_an_exact_host() {
        let client = Client {
            id: "public".into(),
            secret: None,
            public: true,
            redirect_uris: vec!["http://127.0.0.1/cb".into()],
            jwks: None,
            grants: vec![],
            allowed_scopes: None,
        };
        assert!(redirect_allowed(&client, "http://127.0.0.1/cb"));
        assert!(redirect_allowed(&client, "http://127.0.0.1:9443/cb"));
        assert!(!redirect_allowed(
            &client,
            "http://127.0.0.1.evil.example/cb"
        ));
        assert!(!redirect_allowed(
            &client,
            "http://localhost.evil.example/cb"
        ));
        assert!(!redirect_allowed(&client, "https://evil.example/cb"));
        assert!(!redirect_allowed(&client, "http://127.0.0.1/other"));
        assert!(!redirect_allowed(
            &client,
            "https://127.0.0.1@evil.example/cb"
        ));
    }

    #[test]
    fn redirect_registration_rejects_userinfo_and_cleartext_remote() {
        assert!(redirect_uri_valid("http://127.0.0.1/cb"));
        assert!(redirect_uri_valid("http://localhost:9443/cb"));
        assert!(redirect_uri_valid("https://app.example/cb"));
        assert!(!redirect_uri_valid("http://evil.example/cb"));
        assert!(!redirect_uri_valid("https://app.example@evil.example/cb"));
        assert!(!redirect_uri_valid("javascript:alert(1)"));
        assert!(!redirect_uri_valid("https://app.example/cb#frag"));
        assert!(!uri_matches(
            "https://app.example@evil.example/cb",
            "https://app.example@evil.example/cb"
        ));
    }
}
