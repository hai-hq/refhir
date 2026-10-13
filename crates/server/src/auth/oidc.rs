//! OpenID Connect relying party: sign users in through an external identity provider.
//!
//! The authorize step redirects the browser to the provider (authorization code + PKCE), the
//! callback exchanges the code server-side and validates the ID token against the provider's
//! published keys. The verified `sub` and `fhirUser` claim become the identity SMART tokens carry.

use std::collections::BTreeMap;
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
    /// Restrict who may sign in by ID token claim: every named claim must carry at least one of
    /// the listed values (a claim holding an array matches if any element is listed). Empty
    /// means anyone the provider authenticates is accepted.
    pub allowed_claims: BTreeMap<String, Vec<String>>,
    /// Restrict who may sign in by the domain of the `email` claim (exact match, case
    /// insensitive). An address the provider has not marked `email_verified: true` never matches. Empty means
    /// any address, or none, is accepted. Combined with `allowed_claims`, both must pass.
    pub allowed_email_domains: Vec<String>,
    /// Bind a person to a FHIR resource by search when the ID token has no usable
    /// `fhir_user_claim`. Off when unset.
    pub fhir_user_lookup: Option<FhirUserLookup>,
    /// When the signed-in person is bound to a `Patient`, make that patient the launch context
    /// instead of showing the patient picker. Off by default.
    pub patient_from_fhir_user: bool,
}

/// How to find the FHIR resource for a person the provider has authenticated: search
/// `[resource_type]?[search_param]=[system|]<value of claim>` and bind only on exactly one hit.
#[derive(Clone, Debug, PartialEq)]
pub struct FhirUserLookup {
    /// One of `Practitioner`, `Patient`, `RelatedPerson` or `Person`.
    pub resource_type: String,
    /// ID token claim holding the value to search for. `sub` (the default) or any claim name,
    /// e.g. `email`.
    pub claim: String,
    /// Search parameter to match the value against.
    pub search_param: String,
    /// Identifier system, sent as `system|value`. Without it only the value is sent.
    pub system: Option<String>,
}

impl FhirUserLookup {
    pub fn new(resource_type: impl Into<String>) -> Self {
        Self {
            resource_type: resource_type.into(),
            claim: "sub".into(),
            search_param: "identifier".into(),
            system: None,
        }
    }

    fn validate(&self) -> Result<(), String> {
        const TYPES: [&str; 4] = ["Practitioner", "Patient", "RelatedPerson", "Person"];
        if !TYPES.contains(&self.resource_type.as_str()) {
            return Err(format!(
                "oidc fhir_user_lookup resource_type must be one of {}",
                TYPES.join(", ")
            ));
        }
        if self.claim.is_empty() {
            return Err("oidc fhir_user_lookup claim must not be empty".into());
        }
        let param_ok = !self.search_param.is_empty()
            && self
                .search_param
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !param_ok {
            return Err("oidc fhir_user_lookup search_param is not a valid parameter name".into());
        }
        if let Some(system) = &self.system {
            if system.is_empty() || system.contains([',', '$', '\\', '|']) {
                return Err(
                    "oidc fhir_user_lookup system is empty or has a reserved character".into(),
                );
            }
            // `system|value` is token syntax; it means nothing to a string parameter.
            if self.search_param != "identifier" {
                return Err(
                    "oidc fhir_user_lookup system only applies to search_param identifier".into(),
                );
            }
        }
        Ok(())
    }
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
        for (claim, values) in &self.allowed_claims {
            if claim.is_empty() {
                return Err("oidc allowed_claims has an empty claim name".into());
            }
            // An empty list could never match, which would lock everyone out.
            if values.is_empty() || values.iter().any(String::is_empty) {
                return Err(format!(
                    "oidc allowed_claims.{claim} needs at least one non-empty value"
                ));
            }
        }
        if self
            .allowed_email_domains
            .iter()
            .any(|domain| normalise_domain(domain).is_empty())
        {
            return Err("oidc allowed_email_domains has an empty domain".into());
        }
        if let Some(lookup) = &self.fhir_user_lookup {
            lookup.validate()?;
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
            allowed_claims: BTreeMap::new(),
            allowed_email_domains: Vec::new(),
            fhir_user_lookup: None,
            patient_from_fhir_user: false,
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
    /// Value of the claim `fhir_user_lookup` searches on, when one is configured.
    #[serde(default)]
    pub lookup_key: Option<String>,
}

/// Why a sign-in did not complete.
#[derive(Debug)]
pub(crate) enum SignInError {
    /// The provider authenticated the user but the allowlist does not admit them. The text is
    /// for the log, not the user.
    NotPermitted(String),
    Failed(String),
}

impl From<String> for SignInError {
    fn from(reason: String) -> Self {
        Self::Failed(reason)
    }
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
    pub fn new(mut cfg: OidcConfig) -> Result<Self, String> {
        cfg.validate()?;
        for domain in &mut cfg.allowed_email_domains {
            *domain = normalise_domain(domain);
        }
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
    ) -> Result<Identity, SignInError> {
        let discovery = self.discovery()?;
        let id_token = self.exchange(&discovery, code, &flow.verifier, redirect_uri)?;
        let (identity, claims) = self.verify_id_token(&discovery, &id_token, &flow.nonce)?;
        // Before anything is stored or shown: someone the allowlist refuses never reaches consent.
        self.admit(&identity.sub, &claims)
            .map_err(SignInError::NotPermitted)?;
        Ok(identity)
    }

    /// Apply `allowed_claims` and `allowed_email_domains` to a verified ID token.
    fn admit(&self, sub: &str, claims: &Value) -> Result<(), String> {
        for (name, allowed) in &self.cfg.allowed_claims {
            if !claim_listed(claims.get(name), allowed) {
                return Err(format!(
                    "{sub}: claim {name} is not among the permitted values"
                ));
            }
        }
        if !self.cfg.allowed_email_domains.is_empty() {
            let domain = verified_email(claims).and_then(|email| {
                email
                    .rsplit_once('@')
                    .map(|(_, domain)| domain.to_lowercase())
            });
            let admitted = domain.as_deref().is_some_and(|domain| {
                self.cfg
                    .allowed_email_domains
                    .iter()
                    .any(|allowed| allowed == domain)
            });
            if !admitted {
                return Err(format!(
                    "{sub}: email is missing, unverified, or not in a permitted domain"
                ));
            }
        }
        Ok(())
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
    ) -> Result<(Identity, Value), String> {
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
        let lookup_key =
            self.cfg
                .fhir_user_lookup
                .as_ref()
                .and_then(|lookup| match lookup.claim.as_str() {
                    "sub" => Some(sub.clone()),
                    "email" => verified_email(&claims),
                    other => text(other),
                });
        let identity = Identity {
            fhir_user: text(&self.cfg.fhir_user_claim),
            display: text("preferred_username")
                .or_else(|| text("name"))
                .or_else(|| text("email")),
            lookup_key,
            sub,
        };
        Ok((identity, claims))
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

/// An allowed email domain as compared: no `@`, no padding, lower case.
fn normalise_domain(domain: &str) -> String {
    domain.trim().trim_start_matches('@').to_lowercase()
}

/// Whether a claim value is one of `allowed`. An array claim matches if any element does;
/// numbers and booleans compare by their JSON text.
fn claim_listed(value: Option<&Value>, allowed: &[String]) -> bool {
    let listed = |item: &Value| match item {
        Value::String(text) => allowed.contains(text),
        Value::Number(_) | Value::Bool(_) => allowed.contains(&item.to_string()),
        _ => false,
    };
    match value {
        Some(Value::Array(items)) => items.iter().any(listed),
        Some(item) => listed(item),
        None => false,
    }
}

/// The `email` claim, only when the provider says it has verified it. A token with no
/// `email_verified` claim counts as unverified.
fn verified_email(claims: &Value) -> Option<String> {
    let verified = match claims.get("email_verified") {
        Some(Value::Bool(verified)) => *verified,
        Some(Value::String(text)) => text.eq_ignore_ascii_case("true"),
        _ => false,
    };
    claims
        .get("email")
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty() && verified)
        .map(str::to_string)
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

    fn gated(claims: &[(&str, &[&str])], domains: &[&str]) -> Oidc {
        let mut cfg = OidcConfig::new("https://idp.example.com", "c");
        for (name, values) in claims {
            cfg.allowed_claims.insert(
                name.to_string(),
                values.iter().map(|v| v.to_string()).collect(),
            );
        }
        cfg.allowed_email_domains = domains.iter().map(|d| d.to_string()).collect();
        Oidc::new(cfg).unwrap()
    }

    #[test]
    fn no_allowlist_admits_everyone() {
        let oidc = gated(&[], &[]);
        assert!(oidc.admit("u", &serde_json::json!({})).is_ok());
    }

    #[test]
    fn allowed_claims_need_every_claim_to_match_one_listed_value() {
        let oidc = gated(
            &[("organization_id", &["org_a", "org_b"]), ("tier", &["1"])],
            &[],
        );
        let ok = serde_json::json!({"organization_id": "org_b", "tier": 1});
        assert!(oidc.admit("u", &ok).is_ok());
        let wrong_org = serde_json::json!({"organization_id": "org_c", "tier": 1});
        assert!(oidc.admit("u", &wrong_org).is_err());
        let missing = serde_json::json!({"organization_id": "org_a"});
        assert!(oidc.admit("u", &missing).is_err());
        // An array claim is admitted when any element is listed.
        let many = serde_json::json!({"organization_id": ["org_x", "org_a"], "tier": "1"});
        assert!(oidc.admit("u", &many).is_ok());
        let none = serde_json::json!({"organization_id": ["org_x"], "tier": "1"});
        assert!(oidc.admit("u", &none).is_err());
        let nested = serde_json::json!({"organization_id": {"id": "org_a"}, "tier": "1"});
        assert!(oidc.admit("u", &nested).is_err());
    }

    #[test]
    fn allowed_email_domains_match_the_whole_domain_of_a_verified_address() {
        let oidc = gated(&[], &["boolbyte.com", "@Example.org"]);
        let admit = |claims: Value| oidc.admit("u", &claims).is_ok();
        let verified = |email: &str| serde_json::json!({"email": email, "email_verified": true});
        assert!(admit(verified("a@boolbyte.com")));
        assert!(admit(verified("A@BoolByte.COM")));
        assert!(admit(verified("a@example.org")));
        // A token that does not say the address is verified does not count.
        assert!(!admit(serde_json::json!({"email": "a@boolbyte.com"})));
        assert!(!admit(serde_json::json!({"email": "a@evil-boolbyte.com"})));
        assert!(!admit(serde_json::json!({"email": "a@mail.boolbyte.com"})));
        assert!(!admit(
            serde_json::json!({"email": "boolbyte.com@evil.com"})
        ));
        assert!(!admit(
            serde_json::json!({"email": "a@boolbyte.com", "email_verified": false})
        ));
        assert!(!admit(
            serde_json::json!({"email": "a@boolbyte.com", "email_verified": "false"})
        ));
        assert!(!admit(serde_json::json!({"sub": "no-email"})));
    }

    #[test]
    fn claims_and_email_domains_must_both_pass() {
        let oidc = gated(&[("organization_id", &["org_a"])], &["boolbyte.com"]);
        assert!(oidc
            .admit(
                "u",
                &serde_json::json!({"organization_id": "org_a", "email": "a@boolbyte.com", "email_verified": true})
            )
            .is_ok());
        assert!(oidc
            .admit("u", &serde_json::json!({"organization_id": "org_a"}))
            .is_err());
    }

    #[test]
    fn an_allowlist_that_could_never_match_is_a_startup_error() {
        let mut cfg = OidcConfig::new("https://idp.example.com", "c");
        cfg.allowed_claims.insert("organization_id".into(), vec![]);
        assert!(cfg.validate().is_err());
        cfg.allowed_claims
            .insert("organization_id".into(), vec!["org_a".into()]);
        assert!(cfg.validate().is_ok());
        cfg.allowed_email_domains = vec!["@".into()];
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn fhir_user_lookup_is_validated() {
        let mut cfg = OidcConfig::new("https://idp.example.com", "c");
        cfg.fhir_user_lookup = Some(FhirUserLookup::new("Practitioner"));
        assert!(cfg.validate().is_ok());
        for bad in [
            FhirUserLookup::new("Observation"),
            FhirUserLookup {
                search_param: "a=b".into(),
                ..FhirUserLookup::new("Patient")
            },
            FhirUserLookup {
                claim: String::new(),
                ..FhirUserLookup::new("Patient")
            },
            FhirUserLookup {
                system: Some(String::new()),
                ..FhirUserLookup::new("Patient")
            },
            FhirUserLookup {
                system: Some("urn:a,b".into()),
                ..FhirUserLookup::new("Patient")
            },
            FhirUserLookup {
                system: Some("urn:x".into()),
                search_param: "email".into(),
                ..FhirUserLookup::new("Patient")
            },
        ] {
            cfg.fhir_user_lookup = Some(bad);
            assert!(cfg.validate().is_err());
        }
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
