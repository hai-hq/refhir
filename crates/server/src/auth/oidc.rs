//! OpenID Connect relying party: sign users in through an external identity provider.
//!
//! The authorize step redirects the browser to the provider (authorization code + PKCE), the
//! callback exchanges the code server-side and validates the ID token against the provider's
//! published keys. The verified `sub` and `fhirUser` claim become the identity SMART tokens carry.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::store::{is_loopback_host, parse_http_uri};

const CACHE_TTL: Duration = Duration::from_secs(3600);
/// Minimum gap between key refetches triggered by an unknown `kid`.
const JWKS_REFETCH_GAP: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct OidcConfig {
    /// Provider issuer URL. Must be `https`, or `http` on a loopback host.
    pub issuer: String,
    pub client_id: String,
    /// Omit for a public client; PKCE is always used.
    pub client_secret: Option<String>,
    /// Scopes requested from the provider. `openid` is always included.
    pub scopes: Vec<String>,
    /// ID token claim holding the user's FHIR identity, e.g. `Practitioner/123`.
    pub fhir_user_claim: String,
    /// Redirect URI registered at the provider. Defaults to `<issuer>/oauth/oidc/callback`
    /// of this server.
    pub redirect_uri: Option<String>,
}

impl OidcConfig {
    /// Check the settings that can be checked without contacting the provider.
    pub fn validate(&self) -> Result<(), String> {
        if self.issuer.is_empty() {
            return Err("oidc issuer is required".into());
        }
        check_endpoint("oidc issuer", &self.issuer)?;
        if self.client_id.is_empty() {
            return Err("oidc client_id is required".into());
        }
        if let Some(uri) = &self.redirect_uri {
            check_endpoint("oidc redirect_uri", uri)?;
        }
        Ok(())
    }

    pub fn new(issuer: impl Into<String>, client_id: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            client_id: client_id.into(),
            client_secret: None,
            scopes: vec!["openid".into()],
            fhir_user_claim: "fhirUser".into(),
            redirect_uri: None,
        }
    }
}

/// Who the provider says the user is.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Identity {
    pub sub: String,
    /// Value of the configured FHIR identity claim, as sent (may be relative).
    pub fhir_user: Option<String>,
    /// Human-readable label for the consent page.
    pub display: Option<String>,
}

/// State kept between redirecting to the provider and the callback.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct OidcFlow {
    pub state: String,
    pub nonce: String,
    pub verifier: String,
    /// Also set as a cookie, so only the browser that started the login can finish it.
    pub binder: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

#[derive(Default)]
struct Cache {
    discovery: Option<(Instant, Discovery)>,
    jwks: Option<(Instant, JwkSet)>,
    jwks_fetched: Option<Instant>,
}

pub struct Oidc {
    cfg: OidcConfig,
    agent: ureq::Agent,
    cache: Mutex<Cache>,
}

impl std::fmt::Debug for Oidc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Oidc")
    }
}

impl Oidc {
    pub fn new(cfg: OidcConfig) -> Result<Self, String> {
        cfg.validate()?;
        let agent = ureq::AgentBuilder::new()
            .timeout(HTTP_TIMEOUT)
            .redirects(0)
            .build();
        Ok(Self {
            cfg,
            agent,
            cache: Mutex::new(Cache::default()),
        })
    }

    pub(crate) fn config(&self) -> &OidcConfig {
        &self.cfg
    }

    fn discovery(&self) -> Result<Discovery, String> {
        if let Some((at, found)) = &self.cache.lock().expect("oidc").discovery {
            if at.elapsed() < CACHE_TTL {
                return Ok(found.clone());
            }
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.cfg.issuer.trim_end_matches('/')
        );
        let found: Discovery = self.get_json(&url)?;
        if found.issuer.trim_end_matches('/') != self.cfg.issuer.trim_end_matches('/') {
            return Err(format!(
                "provider reports issuer {} but {} is configured",
                found.issuer, self.cfg.issuer
            ));
        }
        for (name, endpoint) in [
            ("authorization_endpoint", &found.authorization_endpoint),
            ("token_endpoint", &found.token_endpoint),
            ("jwks_uri", &found.jwks_uri),
        ] {
            check_endpoint(name, endpoint)?;
        }
        self.cache.lock().expect("oidc").discovery = Some((Instant::now(), found.clone()));
        Ok(found)
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, String> {
        let body = self
            .agent
            .get(url)
            .call()
            .map_err(|err| format!("GET {url}: {err}"))?
            .into_string()
            .map_err(|err| format!("GET {url}: {err}"))?;
        serde_json::from_str(&body).map_err(|err| format!("GET {url}: invalid JSON: {err}"))
    }

    /// Start a login: the flow to remember and the provider URL to send the browser to.
    pub(crate) fn begin(
        &self,
        session_id: &str,
        redirect_uri: &str,
    ) -> Result<(OidcFlow, String), String> {
        let discovery = self.discovery()?;
        let flow = OidcFlow {
            state: format!("{session_id}.{}", uuid::Uuid::new_v4().simple()),
            nonce: uuid::Uuid::new_v4().simple().to_string(),
            verifier: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            binder: uuid::Uuid::new_v4().simple().to_string(),
        };
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(flow.verifier.as_bytes()));
        let mut scopes = vec!["openid".to_string()];
        for scope in &self.cfg.scopes {
            if !scopes.contains(scope) {
                scopes.push(scope.clone());
            }
        }
        let query = [
            ("response_type", "code"),
            ("client_id", &self.cfg.client_id),
            ("redirect_uri", redirect_uri),
            ("scope", &scopes.join(" ")),
            ("state", &flow.state),
            ("nonce", &flow.nonce),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ]
        .iter()
        .map(|(key, value)| format!("{key}={}", super::oauth::query_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
        let separator = if discovery.authorization_endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        Ok((
            flow.clone(),
            format!("{}{separator}{query}", discovery.authorization_endpoint),
        ))
    }

    /// Redeem the authorization code and return the verified identity.
    pub(crate) fn complete(
        &self,
        code: &str,
        flow: &OidcFlow,
        redirect_uri: &str,
    ) -> Result<Identity, String> {
        let discovery = self.discovery()?;
        let id_token = self.exchange(&discovery, code, &flow.verifier, redirect_uri)?;
        self.verify_id_token(&discovery, &id_token, &flow.nonce)
    }

    fn exchange(
        &self,
        discovery: &Discovery,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<String, String> {
        let mut request = self.agent.post(&discovery.token_endpoint);
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ];
        match &self.cfg.client_secret {
            Some(secret) => {
                let credentials = format!(
                    "{}:{}",
                    super::oauth::query_encode(&self.cfg.client_id),
                    super::oauth::query_encode(secret)
                );
                request = request.set(
                    "Authorization",
                    &format!("Basic {}", STANDARD.encode(credentials)),
                );
            }
            None => form.push(("client_id", &self.cfg.client_id)),
        }
        let body = request
            .send_form(&form)
            .map_err(|err| match err {
                ureq::Error::Status(status, response) => {
                    let body = response.into_string().unwrap_or_default();
                    format!("token endpoint returned {status}: {body}")
                }
                other => format!("token endpoint: {other}"),
            })?
            .into_string()
            .map_err(|err| format!("token endpoint: {err}"))?;
        let response: Value = serde_json::from_str(&body)
            .map_err(|err| format!("token endpoint: invalid JSON: {err}"))?;
        response
            .get("id_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "token response has no id_token".to_string())
    }

    fn verify_id_token(
        &self,
        discovery: &Discovery,
        token: &str,
        nonce: &str,
    ) -> Result<Identity, String> {
        let header =
            jsonwebtoken::decode_header(token).map_err(|err| format!("bad id_token: {err}"))?;
        if !matches!(
            header.alg,
            Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::PS256
                | Algorithm::PS384
                | Algorithm::PS512
                | Algorithm::ES256
                | Algorithm::ES384
        ) {
            return Err(format!(
                "id_token algorithm {:?} is not accepted",
                header.alg
            ));
        }
        let key = self.signing_key(discovery, header.kid.as_deref())?;
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[discovery.issuer.as_str()]);
        validation.set_audience(&[self.cfg.client_id.as_str()]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.leeway = 60;
        let claims = jsonwebtoken::decode::<Value>(token, &key, &validation)
            .map_err(|err| format!("id_token rejected: {err}"))?
            .claims;
        if claims.get("nonce").and_then(Value::as_str) != Some(nonce) {
            return Err("id_token nonce does not match".into());
        }
        if let Some(azp) = claims.get("azp").and_then(Value::as_str) {
            if azp != self.cfg.client_id {
                return Err("id_token was issued to another client".into());
            }
        }
        let sub = claims
            .get("sub")
            .and_then(Value::as_str)
            .filter(|sub| !sub.is_empty())
            .ok_or("id_token has no sub")?
            .to_string();
        let text = |name: &str| {
            claims
                .get(name)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        Ok(Identity {
            fhir_user: text(&self.cfg.fhir_user_claim),
            display: text("preferred_username")
                .or_else(|| text("name"))
                .or_else(|| text("email")),
            sub,
        })
    }

    fn signing_key(&self, discovery: &Discovery, kid: Option<&str>) -> Result<DecodingKey, String> {
        let find = |set: &JwkSet| {
            let jwk = match kid {
                Some(kid) => set.find(kid),
                None if set.keys.len() == 1 => set.keys.first(),
                None => None,
            }?;
            DecodingKey::from_jwk(jwk).ok()
        };
        let cached = {
            let cache = self.cache.lock().expect("oidc");
            cache
                .jwks
                .as_ref()
                .filter(|(at, _)| at.elapsed() < CACHE_TTL)
                .map(|(_, set)| set.clone())
        };
        if let Some(key) = cached.as_ref().and_then(find) {
            return Ok(key);
        }
        // Unknown kid or stale cache: refetch, but not more often than the gap allows.
        {
            let cache = self.cache.lock().expect("oidc");
            if cached.is_some()
                && cache
                    .jwks_fetched
                    .is_some_and(|at| at.elapsed() < JWKS_REFETCH_GAP)
            {
                return Err("id_token signing key is not published by the provider".into());
            }
        }
        let set: JwkSet = self.get_json(&discovery.jwks_uri)?;
        let key = find(&set);
        {
            let mut cache = self.cache.lock().expect("oidc");
            cache.jwks = Some((Instant::now(), set));
            cache.jwks_fetched = Some(Instant::now());
        }
        key.ok_or_else(|| "id_token signing key is not published by the provider".into())
    }
}

fn check_endpoint(name: &str, url: &str) -> Result<(), String> {
    let parsed = parse_http_uri(url).ok_or_else(|| format!("{name} {url} is not a valid URL"))?;
    if parsed.scheme == "https" || is_loopback_host(&parsed.host) {
        Ok(())
    } else {
        Err(format!("{name} {url} must use https"))
    }
}

/// Value of cookie `name` in a `Cookie` request header.
pub(crate) fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issuer_must_be_https_or_loopback() {
        assert!(Oidc::new(OidcConfig::new("https://idp.example.com", "c")).is_ok());
        assert!(Oidc::new(OidcConfig::new("http://localhost:8081/realms/x", "c")).is_ok());
        assert!(Oidc::new(OidcConfig::new("http://idp.example.com", "c")).is_err());
        assert!(Oidc::new(OidcConfig::new("not a url", "c")).is_err());
        assert!(Oidc::new(OidcConfig::new("https://idp.example.com", "")).is_err());
    }

    #[test]
    fn cookie_lookup() {
        assert_eq!(
            cookie_value("a=1; refhir_oidc=xyz; b=2", "refhir_oidc").as_deref(),
            Some("xyz")
        );
        assert_eq!(cookie_value("a=1", "refhir_oidc"), None);
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }
}
