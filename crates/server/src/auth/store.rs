//! Per-server SMART client, code, and token tables.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::Token;

const CODE_TTL: u64 = 300;
const LAUNCH_TTL: u64 = 600;

#[derive(Clone)]
pub(crate) struct Client {
    pub id: String,
    pub secret: Option<String>,
    pub public: bool,
    pub redirect_uris: Vec<String>,
    pub jwks: Option<serde_json::Value>,
    pub grants: Vec<String>,
    pub allowed_scopes: Option<Vec<String>>,
}

#[derive(Clone)]
pub(crate) struct AuthCode {
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    pub challenge: String,
    pub patient: Option<String>,
    pub encounter: Option<String>,
    pub fhir_user: Option<String>,
    pub exp: u64,
}

#[derive(Clone)]
pub(crate) struct Launch {
    pub client_id: String,
    pub patient: Option<String>,
    pub encounter: Option<String>,
    pub fhir_user: Option<String>,
    pub exp: u64,
}

struct Inner {
    clients: HashMap<String, Client>,
    codes: HashMap<String, AuthCode>,
    launches: HashMap<String, Launch>,
    tokens: HashMap<String, Token>,
    refresh: HashMap<String, Token>,
    jti: HashMap<String, u64>,
}

pub struct SmartStore {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for SmartStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SmartStore")
    }
}

impl SmartStore {
    pub fn with_defaults() -> Self {
        Self::with_confidential_secret("secret")
    }

    pub fn with_confidential_secret(secret: impl Into<String>) -> Self {
        let mut clients = HashMap::new();
        let redirects = vec![
            "http://127.0.0.1/cb".into(),
            "http://localhost/cb".into(),
            "http://127.0.0.1/app".into(),
            "http://localhost/app".into(),
        ];
        clients.insert(
            "public".into(),
            Client {
                id: "public".into(),
                secret: None,
                public: true,
                redirect_uris: redirects.clone(),
                jwks: None,
                grants: vec!["authorization_code".into(), "refresh_token".into()],
                allowed_scopes: None,
            },
        );
        clients.insert(
            "confidential".into(),
            Client {
                id: "confidential".into(),
                secret: Some(secret.into()),
                public: false,
                redirect_uris: redirects,
                jwks: None,
                grants: vec!["authorization_code".into(), "refresh_token".into()],
                allowed_scopes: None,
            },
        );
        Self {
            inner: Mutex::new(Inner {
                clients,
                codes: HashMap::new(),
                launches: HashMap::new(),
                tokens: HashMap::new(),
                refresh: HashMap::new(),
                jti: HashMap::new(),
            }),
        }
    }

    pub fn prune(&self) {
        let now = now();
        let mut inner = self.inner.lock().expect("smart");
        inner.codes.retain(|_, code| code.exp > now);
        inner.launches.retain(|_, launch| launch.exp > now);
        inner.tokens.retain(|_, token| token.exp > now);
        inner.refresh.retain(|_, token| token.exp > now);
        inner.jti.retain(|_, exp| *exp > now);
    }

    pub(crate) fn client(&self, id: &str) -> Option<Client> {
        self.inner.lock().ok()?.clients.get(id).cloned()
    }

    pub(crate) fn put_client(&self, client: Client) {
        self.inner
            .lock()
            .expect("smart")
            .clients
            .insert(client.id.clone(), client);
    }

    pub(crate) fn put_code(&self, id: String, code: AuthCode) {
        self.inner.lock().expect("smart").codes.insert(id, code);
    }

    pub(crate) fn take_code(&self, id: &str) -> Option<AuthCode> {
        self.inner.lock().ok()?.codes.remove(id)
    }

    pub(crate) fn put_launch(&self, id: String, launch: Launch) {
        self.inner
            .lock()
            .expect("smart")
            .launches
            .insert(id, launch);
    }

    pub(crate) fn launch(&self, id: &str) -> Option<Launch> {
        self.inner.lock().ok()?.launches.get(id).cloned()
    }

    pub(crate) fn token(&self, id: &str) -> Option<Token> {
        self.inner.lock().ok()?.tokens.get(id).cloned()
    }

    pub(crate) fn put_token(&self, id: String, token: Token) {
        self.inner.lock().expect("smart").tokens.insert(id, token);
    }

    pub(crate) fn remove_token(&self, id: &str) -> Option<Token> {
        self.inner.lock().ok()?.tokens.remove(id)
    }

    pub(crate) fn refresh(&self, id: &str) -> Option<Token> {
        self.inner.lock().ok()?.refresh.get(id).cloned()
    }

    pub(crate) fn take_refresh(&self, id: &str) -> Option<Token> {
        self.inner.lock().ok()?.refresh.remove(id)
    }

    pub(crate) fn put_refresh(&self, id: String, token: Token) {
        self.inner.lock().expect("smart").refresh.insert(id, token);
    }

    pub(crate) fn remember_jti(&self, jti: &str, exp: u64) -> bool {
        let mut inner = self.inner.lock().expect("smart");
        if inner.jti.contains_key(jti) {
            return false;
        }
        inner.jti.insert(jti.to_string(), exp);
        true
    }

    pub(crate) fn origins(&self) -> HashSet<String> {
        let inner = self.inner.lock().expect("smart");
        inner
            .clients
            .values()
            .flat_map(|client| client.redirect_uris.iter())
            .filter_map(|uri| origin_of(uri))
            .collect()
    }

    #[cfg(test)]
    pub fn install_token(&self, token: &str, scopes: &[&str], patient: Option<&str>) {
        self.install_token_exp(token, scopes, patient, u64::MAX);
    }

    #[cfg(test)]
    pub fn install_token_exp(&self, token: &str, scopes: &[&str], patient: Option<&str>, exp: u64) {
        self.put_token(
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
