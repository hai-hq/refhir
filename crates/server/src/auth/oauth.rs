//! SMART App Launch 2.2 authorization server.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::store::{
    code_ttl, is_loopback_host, launch_ttl, normalize_patient_id, parse_http_uri, redirect_allowed,
    redirect_uri_valid, AuthCode, Client, Launch,
};
use super::Token;
use crate::http::AppState;

const ACCESS_TTL: u64 = 3600;
const BACKEND_TTL: u64 = 300;
const REFRESH_TTL: u64 = 60 * 60 * 24 * 30;

struct ServerKeys {
    kid: String,
    encoding: EncodingKey,
    n: String,
    e: String,
}

fn keys() -> &'static ServerKeys {
    static KEYS: OnceLock<ServerKeys> = OnceLock::new();
    KEYS.get_or_init(|| {
        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, 2048).expect("rsa key");
        let pem = key.to_pkcs8_pem(LineEnding::LF).expect("pem");
        let encoding = EncodingKey::from_rsa_pem(pem.as_bytes()).expect("encoding key");
        let public = RsaPublicKey::from(&key);
        ServerKeys {
            kid: "refhir-1".into(),
            encoding,
            n: URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
            e: URL_SAFE_NO_PAD.encode(public.e().to_bytes_be()),
        }
    })
}

pub fn configuration(app: &AppState, headers: &HeaderMap) -> Response {
    let issuer = issuer(app, headers);
    json_response(
        StatusCode::OK,
        json!({
            "issuer": issuer,
            "jwks_uri": format!("{issuer}/.well-known/jwks.json"),
            "authorization_endpoint": format!("{issuer}/oauth/authorize"),
            "token_endpoint": format!("{issuer}/oauth/token"),
            "token_endpoint_auth_methods_supported": [
                "client_secret_basic",
                "client_secret_post",
                "private_key_jwt"
            ],
            "grant_types_supported": ["authorization_code", "client_credentials", "refresh_token"],
            "registration_endpoint": format!("{issuer}/oauth/register"),
            "scopes_supported": [
                "openid",
                "profile",
                "fhirUser",
                "launch",
                "launch/patient",
                "launch/encounter",
                "patient/*.read",
                "patient/*.write",
                "patient/*.rs",
                "patient/*.cruds",
                "user/*.read",
                "user/*.write",
                "user/*.rs",
                "user/*.cruds",
                "system/*.read",
                "system/*.rs",
                "system/*.cruds",
                "offline_access",
                "online_access"
            ],
            "response_types_supported": ["code"],
            "management_endpoint": format!("{issuer}/oauth/manage"),
            "introspection_endpoint": format!("{issuer}/oauth/introspect"),
            "revocation_endpoint": format!("{issuer}/oauth/revoke"),
            "code_challenge_methods_supported": ["S256"],
            "capabilities": [
                "launch-ehr",
                "launch-standalone",
                "authorize-post",
                "client-public",
                "client-confidential-symmetric",
                "client-confidential-asymmetric",
                "sso-openid-connect",
                "context-banner",
                "context-style",
                "context-ehr-patient",
                "context-ehr-encounter",
                "context-standalone-patient",
                "context-standalone-encounter",
                "permission-offline",
                "permission-online",
                "permission-patient",
                "permission-user",
                "permission-v1",
                "permission-v2"
            ]
        }),
    )
}

pub fn jwks() -> Response {
    let keys = keys();
    json_response(
        StatusCode::OK,
        json!({
            "keys": [{
                "kty": "RSA",
                "alg": "RS256",
                "use": "sig",
                "kid": keys.kid,
                "n": keys.n,
                "e": keys.e
            }]
        }),
    )
}

pub fn style() -> Response {
    json_response(
        StatusCode::OK,
        json!({
            "color_background": "#ffffff",
            "color_error": "#9e2d2d",
            "color_highlight": "#69b5ce",
            "color_modal_bg": "#00000080",
            "color_success": "#4caf50",
            "color_text": "#303030",
            "border_radius": "4px"
        }),
    )
}

pub fn manage(app: &AppState, headers: &HeaderMap) -> Response {
    if !app.config.auth {
        return oauth_error(
            StatusCode::NOT_FOUND,
            "invalid_request",
            "smart is not enabled",
        );
    }
    let Some(token) = super::resolve_token(headers, None) else {
        return oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_token",
            "a bearer token is required",
        );
    };
    let Some(record) = app.smart.token(&token).filter(|record| record.exp > now()) else {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_token", "unknown token");
    };
    json_response(
        StatusCode::OK,
        json!({
            "client_id": record.client_id,
            "scope": record.scopes.join(" "),
            "patient": record.patient,
            "exp": record.exp
        }),
    )
}

pub fn authorize(
    app: &AppState,
    headers: &HeaderMap,
    raw_query: Option<&str>,
    body: &[u8],
) -> Response {
    if !app.config.auth {
        return oauth_error(
            StatusCode::NOT_FOUND,
            "invalid_request",
            "smart is not enabled",
        );
    }
    app.smart.prune();
    let mut params = parse_form(raw_query.unwrap_or("").as_bytes());
    for (key, value) in parse_form(body) {
        params.insert(key, value);
    }
    let redirect_uri = params.get("redirect_uri").cloned().unwrap_or_default();
    let state = params.get("state").cloned().unwrap_or_default();
    let Some(client_id) = params.get("client_id").filter(|value| !value.is_empty()) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "client_id is required",
        );
    };
    let Some(client) = app.smart.client(client_id) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client",
            "unknown client_id",
        );
    };
    if !redirect_allowed(&client, &redirect_uri) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "redirect_uri is not registered",
        );
    }
    let fail = |code: &str, description: &str| {
        redirect_with(
            &redirect_uri,
            &[
                ("error", code),
                ("error_description", description),
                ("state", &state),
            ],
        )
    };
    if params.get("response_type").map(|value| value.as_str()) != Some("code") {
        return fail("unsupported_response_type", "response_type must be code");
    }
    let challenge = params.get("code_challenge").cloned().unwrap_or_default();
    if challenge.is_empty() {
        return fail("invalid_request", "code_challenge is required");
    }
    if params
        .get("code_challenge_method")
        .map(|value| value.as_str())
        != Some("S256")
    {
        return fail("invalid_request", "code_challenge_method must be S256");
    }
    let aud = params
        .get("aud")
        .or_else(|| params.get("resource"))
        .cloned()
        .unwrap_or_default();
    if !aud_matches(app, headers, &aud) {
        return fail("invalid_request", "aud must be this FHIR server");
    }
    let requested = params.get("scope").cloned().unwrap_or_default();
    let scopes = grant_scopes(&client, &requested, "authorization_code");
    if scopes.is_empty() {
        return fail("invalid_scope", "no supported scopes were requested");
    }
    let mut patient = app
        .config
        .smart_patient
        .clone()
        .map(|id| normalize_patient_id(&id));
    let mut encounter = app.config.smart_encounter.clone();
    let mut fhir_user = Some(format!(
        "{}/{}",
        issuer(app, headers).trim_end_matches('/'),
        app.config
            .smart_fhir_user
            .clone()
            .unwrap_or_else(|| "Patient/smart".into())
    ));
    if let Some(launch_id) = params.get("launch").filter(|value| !value.is_empty()) {
        if !scopes.iter().any(|scope| scope == "launch") {
            return fail("invalid_request", "EHR launch requires the launch scope");
        }
        let Some(launch) = app
            .smart
            .launch(launch_id)
            .filter(|launch| launch.exp > now())
        else {
            return fail("invalid_request", "unknown launch id");
        };
        if launch.client_id != client.id {
            return fail("invalid_request", "launch id belongs to another client");
        }
        if launch.patient.is_some() {
            patient = launch.patient.clone();
        }
        if launch.encounter.is_some() {
            encounter = launch.encounter.clone();
        }
        if launch.fhir_user.is_some() {
            fhir_user = launch.fhir_user.clone();
        }
    } else if scopes.iter().any(|scope| scope == "launch/patient") {
        patient = patient.or_else(|| Some("example".into()));
    }
    if scopes.iter().any(|scope| scope == "launch/encounter") {
        encounter = encounter.or_else(|| Some("example".into()));
    }
    let code = format!("code-{}", uuid::Uuid::new_v4());
    app.smart.put_code(
        code.clone(),
        AuthCode {
            client_id: client.id,
            redirect_uri: redirect_uri.clone(),
            scopes,
            challenge,
            patient,
            encounter,
            fhir_user,
            exp: now() + code_ttl(),
        },
    );
    redirect_with(&redirect_uri, &[("code", &code), ("state", &state)])
}

pub fn issue(app: &AppState, headers: &HeaderMap, body: &[u8]) -> Response {
    if !app.config.auth {
        return oauth_error(
            StatusCode::NOT_FOUND,
            "invalid_request",
            "smart is not enabled",
        );
    }
    app.smart.prune();
    let form = parse_form(body);
    let grant = form
        .get("grant_type")
        .map(|value| value.as_str())
        .unwrap_or("");
    match grant {
        "authorization_code" => token_authorization_code(app, headers, &form),
        "refresh_token" => token_refresh(app, headers, &form),
        "client_credentials" => token_client_credentials(app, headers, &form),
        _ => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "grant_type must be authorization_code, refresh_token, or client_credentials",
        ),
    }
}

pub fn introspect(app: &AppState, headers: &HeaderMap, body: &[u8]) -> Response {
    if !app.config.auth {
        return oauth_error(
            StatusCode::NOT_FOUND,
            "invalid_request",
            "smart is not enabled",
        );
    }
    let form = parse_form(body);
    if !introspect_authorized(app, headers, &form) {
        return oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "introspection requires a confidential client or bearer token",
        );
    }
    let Some(token) = form.get("token").filter(|value| !value.is_empty()) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "token is required",
        );
    };
    let Some(record) = app
        .smart
        .token(token)
        .or_else(|| app.smart.refresh(token))
        .filter(|record| record.exp > now())
    else {
        return json_response(StatusCode::OK, json!({"active": false}));
    };
    let mut body = json!({
        "active": true,
        "scope": record.scopes.join(" "),
        "client_id": record.client_id,
        "exp": record.exp,
        "token_type": "Bearer"
    });
    if let Some(patient) = &record.patient {
        body["patient"] = json!(patient);
    }
    if let Some(encounter) = &record.encounter {
        body["encounter"] = json!(encounter);
    }
    if let Some(fhir_user) = &record.fhir_user {
        body["fhirUser"] = json!(fhir_user);
    }
    if let Some(sub) = &record.sub {
        body["sub"] = json!(sub);
    }
    if let Some(iss) = &record.iss {
        body["iss"] = json!(iss);
    }
    json_response(StatusCode::OK, body)
}

pub fn revoke(app: &AppState, headers: &HeaderMap, body: &[u8]) -> Response {
    if !app.config.auth {
        return oauth_error(
            StatusCode::NOT_FOUND,
            "invalid_request",
            "smart is not enabled",
        );
    }
    let form = parse_form(body);
    let Some(token) = form.get("token").cloned() else {
        return json_response(StatusCode::OK, json!({"revoked": true}));
    };
    let client = match authenticate_client(app, headers, &form, None) {
        Ok(client) => client,
        Err(response) => return response,
    };
    if let Some(record) = app
        .smart
        .token(&token)
        .or_else(|| app.smart.refresh(&token))
    {
        if record.client_id != client.id {
            return oauth_error(
                StatusCode::UNAUTHORIZED,
                "invalid_client",
                "token does not belong to this client",
            );
        }
    }
    app.smart.remove_token(&token);
    app.smart.take_refresh(&token);
    json_response(StatusCode::OK, json!({"revoked": true}))
}

pub fn register(app: &AppState, headers: &HeaderMap, body: &[u8]) -> Response {
    if !app.config.auth {
        return oauth_error(
            StatusCode::NOT_FOUND,
            "invalid_request",
            "smart is not enabled",
        );
    }
    if !dcr_authorized(app, headers) {
        return oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_token",
            "dynamic client registration requires the initial access token",
        );
    }
    let requested: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_client_metadata",
                "JSON body is required",
            )
        }
    };
    if requested.get("jwks_uri").is_some() && requested.get("jwks").is_none() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "jwks_uri is not supported; send jwks",
        );
    }
    let method = requested
        .get("token_endpoint_auth_method")
        .and_then(|value| value.as_str())
        .unwrap_or("none");
    if !matches!(
        method,
        "none" | "client_secret_basic" | "client_secret_post" | "private_key_jwt"
    ) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "unsupported token_endpoint_auth_method",
        );
    }
    let public = method == "none";
    let grants = requested
        .get("grant_types")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| {
            if method == "private_key_jwt" {
                vec!["client_credentials".into()]
            } else {
                vec!["authorization_code".into(), "refresh_token".into()]
            }
        });
    let backend = grants.iter().any(|grant| grant == "client_credentials");
    let interactive = grants
        .iter()
        .any(|grant| grant == "authorization_code" || grant == "refresh_token");
    if backend && interactive {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "client_credentials cannot be mixed with authorization_code",
        );
    }
    if backend && method != "private_key_jwt" {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "client_credentials requires private_key_jwt",
        );
    }
    if backend && requested.get("jwks").is_none() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "private_key_jwt requires jwks",
        );
    }
    let redirects = requested
        .get("redirect_uris")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if interactive && redirects.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "authorization_code requires redirect_uris",
        );
    }
    if redirects.iter().any(|uri| !redirect_uri_valid(uri)) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "redirect_uris must be https or loopback http URLs without fragments or userinfo",
        );
    }
    let scopes = requested
        .get("scope")
        .and_then(|value| value.as_str())
        .map(split_scopes);
    if backend {
        match &scopes {
            None => {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_client_metadata",
                    "backend clients must register system scopes",
                )
            }
            Some(items)
                if items.is_empty() || items.iter().any(|scope| !is_system_scope(scope)) =>
            {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_client_metadata",
                    "backend clients may only register system scopes",
                )
            }
            Some(_) => {}
        }
    }
    if interactive {
        if let Some(items) = &scopes {
            if items
                .iter()
                .any(|scope| is_system_scope(scope) || !is_app_scope(scope))
            {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_client_metadata",
                    "app-launch clients may only register patient, user, and launch scopes",
                );
            }
        }
    }
    let secret = if public || method == "private_key_jwt" {
        None
    } else {
        Some(format!("secret-{}", uuid::Uuid::new_v4()))
    };
    let id = format!("client-{}", uuid::Uuid::new_v4());
    let client = Client {
        id: id.clone(),
        secret: secret.clone(),
        public,
        redirect_uris: redirects.clone(),
        jwks: requested.get("jwks").cloned(),
        grants: grants.clone(),
        allowed_scopes: scopes.clone(),
    };
    app.smart.put_client(client);
    let mut body = json!({
        "client_id": id,
        "token_endpoint_auth_method": method,
        "redirect_uris": redirects,
        "grant_types": grants,
        "response_types": ["code"]
    });
    if let Some(secret) = secret {
        body["client_secret"] = json!(secret);
    }
    if let Some(scopes) = scopes {
        body["scope"] = json!(scopes.join(" "));
    }
    json_response(StatusCode::CREATED, body)
}

pub fn ehr_launch(app: &AppState, headers: &HeaderMap, raw_query: Option<&str>) -> Response {
    if !app.config.auth {
        return oauth_error(
            StatusCode::NOT_FOUND,
            "invalid_request",
            "smart is not enabled",
        );
    }
    let params = parse_form(raw_query.unwrap_or("").as_bytes());
    let Some(client_id) = params.get("client_id").filter(|value| !value.is_empty()) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "client_id is required",
        );
    };
    let Some(client) = app.smart.client(client_id) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client",
            "unknown client_id",
        );
    };
    let Some(launch_uri) = params.get("launch_uri").cloned() else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "launch_uri is required",
        );
    };
    if !redirect_allowed(&client, &launch_uri) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "launch_uri is not registered",
        );
    }
    let iss = issuer(app, headers);
    let fhir_user = app.config.smart_fhir_user.as_ref().map(|id| {
        if parse_http_uri(id).is_some() {
            id.clone()
        } else {
            format!(
                "{}/{}",
                iss.trim_end_matches('/'),
                id.trim_start_matches('/')
            )
        }
    });
    let id = format!("launch-{}", uuid::Uuid::new_v4());
    app.smart.put_launch(
        id.clone(),
        Launch {
            client_id: client.id,
            patient: app
                .config
                .smart_patient
                .as_deref()
                .map(normalize_patient_id)
                .filter(|id| !id.is_empty()),
            encounter: app.config.smart_encounter.clone(),
            fhir_user,
            exp: now() + launch_ttl(),
        },
    );
    redirect_with(&launch_uri, &[("iss", &iss), ("launch", &id)])
}

fn token_authorization_code(
    app: &AppState,
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> Response {
    let client = match authenticate_client(app, headers, form, Some("authorization_code")) {
        Ok(client) => client,
        Err(response) => return response,
    };
    let Some(code) = form.get("code").filter(|value| !value.is_empty()) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "code is required",
        );
    };
    let Some(redirect_uri) = form.get("redirect_uri") else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "redirect_uri is required",
        );
    };
    let Some(verifier) = form.get("code_verifier") else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "code_verifier is required",
        );
    };
    let Some(record) = app.smart.take_code(code) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "authorization code is unknown",
        );
    };
    if record.exp <= now() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "authorization code expired",
        );
    }
    if record.client_id != client.id {
        app.smart.put_code(code.clone(), record);
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "authorization code belongs to another client",
        );
    }
    if record.redirect_uri != *redirect_uri {
        app.smart.put_code(code.clone(), record);
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "redirect_uri does not match",
        );
    }
    if !pkce_valid(verifier, &record.challenge) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "code_verifier does not match",
        );
    }
    issue_tokens(
        app,
        headers,
        &client,
        record.scopes,
        record.patient,
        record.encounter,
        record.fhir_user,
        ACCESS_TTL,
    )
}

fn token_refresh(app: &AppState, headers: &HeaderMap, form: &HashMap<String, String>) -> Response {
    let client = match authenticate_client(app, headers, form, Some("refresh_token")) {
        Ok(client) => client,
        Err(response) => return response,
    };
    let Some(refresh) = form.get("refresh_token") else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "refresh_token is required",
        );
    };
    let Some(record) = app.smart.take_refresh(refresh) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "refresh token is unknown",
        );
    };
    if record.exp <= now() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "refresh token expired",
        );
    }
    if record.client_id != client.id {
        app.smart.put_refresh(refresh.clone(), record);
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "refresh token belongs to another client",
        );
    }
    let mut scopes = record.scopes.clone();
    if let Some(requested) = form.get("scope") {
        let next = split_scopes(requested);
        if !next.iter().all(|scope| scopes.contains(scope)) {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_scope",
                "refresh cannot expand scopes",
            );
        }
        scopes = next;
    }
    issue_tokens(
        app,
        headers,
        &client,
        scopes,
        record.patient,
        record.encounter,
        record.fhir_user,
        ACCESS_TTL,
    )
}

fn token_client_credentials(
    app: &AppState,
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> Response {
    let client = match authenticate_client(app, headers, form, Some("client_credentials")) {
        Ok(client) => client,
        Err(response) => return response,
    };
    if client.public || client.jwks.is_none() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "unauthorized_client",
            "client_credentials requires private_key_jwt",
        );
    }
    let requested = form.get("scope").cloned().unwrap_or_default();
    if requested.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_scope",
            "scope is required",
        );
    }
    let scopes = grant_scopes(&client, &requested, "client_credentials");
    if scopes.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_scope",
            "no supported scopes were requested",
        );
    }
    issue_tokens(app, headers, &client, scopes, None, None, None, BACKEND_TTL)
}

fn issue_tokens(
    app: &AppState,
    headers: &HeaderMap,
    client: &Client,
    scopes: Vec<String>,
    patient: Option<String>,
    encounter: Option<String>,
    fhir_user: Option<String>,
    ttl: u64,
) -> Response {
    let issuer = issuer(app, headers);
    let now = now();
    let access = format!("tok-{}", uuid::Uuid::new_v4());
    let openid = scopes.iter().any(|scope| scope == "openid");
    let want_fhir_user = scopes.iter().any(|scope| scope == "fhirUser");
    let want_profile = scopes.iter().any(|scope| scope == "profile");
    let sub = Some("smart-user".to_string());
    let fhir_user = if want_fhir_user {
        fhir_user.or_else(|| {
            Some(format!(
                "{}/{}",
                issuer.trim_end_matches('/'),
                app.config
                    .smart_fhir_user
                    .clone()
                    .unwrap_or_else(|| "Patient/smart".into())
            ))
        })
    } else {
        None
    };
    let record = Token {
        scopes: scopes.clone(),
        patient: patient.clone().filter(|_| {
            scopes.iter().any(|scope| {
                scope.starts_with("patient/") || scope == "launch" || scope == "launch/patient"
            })
        }),
        encounter: encounter.clone(),
        fhir_user: fhir_user.clone(),
        client_id: client.id.clone(),
        exp: now + ttl,
        sub: sub.clone(),
        iss: Some(issuer.clone()),
        need_patient_banner: scopes.iter().any(|scope| scope == "launch").then_some(true),
        smart_style_url: scopes
            .iter()
            .any(|scope| scope == "launch")
            .then(|| format!("{issuer}/oauth/style.json")),
    };
    let id_token = if openid {
        match encode_id_token(
            &issuer,
            &client.id,
            sub.as_deref().unwrap_or("smart-user"),
            fhir_user.as_deref(),
            want_profile,
            now + ttl,
        ) {
            Ok(id_token) => Some(id_token),
            Err(_) => {
                return oauth_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    "could not sign id_token",
                )
            }
        }
    } else {
        None
    };
    app.smart.put_token(access.clone(), record.clone());
    let mut body = json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": ttl,
        "scope": scopes.join(" ")
    });
    if let Some(patient) = &record.patient {
        body["patient"] = json!(patient);
    }
    if let Some(encounter) = &record.encounter {
        body["encounter"] = json!(encounter);
    }
    if let Some(fhir_user) = &record.fhir_user {
        body["fhirUser"] = json!(fhir_user);
    }
    if let Some(banner) = record.need_patient_banner {
        body["need_patient_banner"] = json!(banner);
    }
    if let Some(style) = &record.smart_style_url {
        body["smart_style_url"] = json!(style);
    }
    if scopes
        .iter()
        .any(|scope| scope == "offline_access" || scope == "online_access")
    {
        let refresh = format!("rt-{}", uuid::Uuid::new_v4());
        let mut stored = record.clone();
        stored.exp = now + REFRESH_TTL;
        app.smart.put_refresh(refresh.clone(), stored);
        body["refresh_token"] = json!(refresh);
    }
    if let Some(id_token) = id_token {
        body["id_token"] = json!(id_token);
    }
    let mut response = json_response(StatusCode::OK, body);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
        .headers_mut()
        .insert(header::PRAGMA, header::HeaderValue::from_static("no-cache"));
    response
}

fn encode_id_token(
    issuer: &str,
    aud: &str,
    sub: &str,
    fhir_user: Option<&str>,
    profile: bool,
    exp: u64,
) -> Result<String, jsonwebtoken::errors::Error> {
    #[derive(Serialize)]
    struct Claims<'a> {
        iss: &'a str,
        sub: &'a str,
        aud: &'a str,
        exp: u64,
        iat: u64,
        #[serde(rename = "fhirUser", skip_serializing_if = "Option::is_none")]
        fhir_user: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<&'a str>,
    }
    let keys = keys();
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(keys.kid.clone());
    jsonwebtoken::encode(
        &header,
        &Claims {
            iss: issuer,
            sub,
            aud,
            exp,
            iat: now(),
            fhir_user,
            name: profile.then_some("SMART User"),
        },
        &keys.encoding,
    )
}

fn authenticate_client(
    app: &AppState,
    headers: &HeaderMap,
    form: &HashMap<String, String>,
    grant: Option<&str>,
) -> Result<Client, Response> {
    if form
        .get("client_assertion_type")
        .map(|value| value.as_str())
        == Some("urn:ietf:params:oauth:client-assertion-type:jwt-bearer")
    {
        return authenticate_jwt(app, headers, form, grant);
    }
    let (id, secret) = match basic_client(headers) {
        Some(pair) => pair,
        None => (
            form.get("client_id").cloned().unwrap_or_default(),
            form.get("client_secret").cloned(),
        ),
    };
    let Some(client) = app.smart.client(&id) else {
        return Err(oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "unknown client_id",
        ));
    };
    if let Some(grant) = grant {
        if !client.grants.iter().any(|item| item == grant) {
            return Err(oauth_error(
                StatusCode::BAD_REQUEST,
                "unauthorized_client",
                "client is not allowed this grant",
            ));
        }
    }
    if client.public {
        if secret.as_ref().is_some_and(|value| !value.is_empty()) {
            return Err(oauth_error(
                StatusCode::UNAUTHORIZED,
                "invalid_client",
                "public clients do not use a secret",
            ));
        }
        return Ok(client);
    }
    match (&client.secret, secret) {
        (Some(expected), Some(got)) if secrets_equal(expected, &got) => Ok(client),
        _ => Err(oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "client authentication failed",
        )),
    }
}

fn introspect_authorized(
    app: &AppState,
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> bool {
    if let Some(token) = super::resolve_token(headers, None) {
        if app
            .smart
            .token(&token)
            .is_some_and(|record| record.exp > now())
        {
            return true;
        }
    }
    matches!(authenticate_client(app, headers, form, None), Ok(client) if !client.public)
}

fn authenticate_jwt(
    app: &AppState,
    headers: &HeaderMap,
    form: &HashMap<String, String>,
    grant: Option<&str>,
) -> Result<Client, Response> {
    let Some(assertion) = form.get("client_assertion") else {
        return Err(oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "client_assertion is required",
        ));
    };
    let header = jsonwebtoken::decode_header(assertion).map_err(|_| {
        oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "client_assertion is not a JWT",
        )
    })?;
    if !matches!(
        header.alg,
        Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512
    ) {
        return Err(oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "client_assertion must be an RSA JWT",
        ));
    }
    let unverified = jwt_payload(assertion).ok_or_else(|| {
        oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "client_assertion is not a JWT",
        )
    })?;
    let iss = unverified
        .get("iss")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let sub = unverified
        .get("sub")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if iss.is_empty() || iss != sub {
        return Err(oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "client_assertion iss and sub must equal client_id",
        ));
    }
    let Some(client) = app.smart.client(iss) else {
        return Err(oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "unknown client_id",
        ));
    };
    if let Some(grant) = grant {
        if !client.grants.iter().any(|item| item == grant) {
            return Err(oauth_error(
                StatusCode::BAD_REQUEST,
                "unauthorized_client",
                "client is not allowed this grant",
            ));
        }
    }
    let Some(jwks) = client.jwks.as_ref() else {
        return Err(oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "client has no JWKS",
        ));
    };
    let decoding = decoding_key(jwks, header.kid.as_deref()).ok_or_else(|| {
        oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "no matching JWK",
        )
    })?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[iss]);
    validation.validate_aud = true;
    let token_endpoint = format!("{}/oauth/token", issuer(app, headers));
    validation.set_audience(&[token_endpoint]);
    let claims: Value = jsonwebtoken::decode(assertion, &decoding, &validation)
        .map_err(|_| {
            oauth_error(
                StatusCode::UNAUTHORIZED,
                "invalid_client",
                "client_assertion signature is invalid",
            )
        })?
        .claims;
    let exp = claims
        .get("exp")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    if exp == 0 || exp > now() + 300 {
        return Err(oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "client_assertion exp is missing or too far in the future",
        ));
    }
    let jti = claims
        .get("jti")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if jti.is_empty() {
        return Err(oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "client_assertion jti is required",
        ));
    }
    if !app.smart.remember_jti(jti, exp) {
        return Err(oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "client_assertion jti was already used",
        ));
    }
    Ok(client)
}

fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn decoding_key(jwks: &Value, kid: Option<&str>) -> Option<DecodingKey> {
    let keys = jwks.get("keys").and_then(|value| value.as_array())?;
    let key = match kid {
        Some(kid) => keys
            .iter()
            .find(|key| key.get("kid").and_then(|value| value.as_str()) == Some(kid))?,
        None if keys.len() == 1 => keys.first()?,
        None => return None,
    };
    let n = key.get("n")?.as_str()?;
    let e = key.get("e")?.as_str()?;
    DecodingKey::from_rsa_components(n, e).ok()
}

fn grant_scopes(client: &Client, requested: &str, grant: &str) -> Vec<String> {
    requested
        .split_whitespace()
        .filter(|item| !item.is_empty())
        .filter(|scope| scope_permitted(client, scope, grant))
        .map(|item| (*item).to_string())
        .collect()
}

fn scope_permitted(client: &Client, scope: &str, grant: &str) -> bool {
    if grant == "client_credentials" {
        if !is_system_scope(scope) {
            return false;
        }
    } else if is_system_scope(scope) {
        return false;
    } else if !is_app_scope(scope) {
        return false;
    }
    match &client.allowed_scopes {
        None => true,
        Some(allowed) => allowed
            .iter()
            .any(|item| item == scope || item == "*" || wildcard_scope(item, scope)),
    }
}

fn is_system_scope(scope: &str) -> bool {
    scope.starts_with("system/")
}

fn is_app_scope(scope: &str) -> bool {
    matches!(
        scope,
        "openid"
            | "profile"
            | "fhirUser"
            | "launch"
            | "launch/patient"
            | "launch/encounter"
            | "offline_access"
            | "online_access"
    ) || scope.starts_with("patient/")
        || scope.starts_with("user/")
}

fn wildcard_scope(allowed: &str, requested: &str) -> bool {
    if !allowed.ends_with("*.cruds") || allowed.len() <= 7 {
        return false;
    }
    let prefix = &allowed[..allowed.len() - 7];
    !prefix.is_empty() && requested.starts_with(prefix)
}

fn aud_matches(app: &AppState, headers: &HeaderMap, aud: &str) -> bool {
    if aud.is_empty() {
        return false;
    }
    let issuer = issuer(app, headers);
    aud.trim_end_matches('/') == issuer.trim_end_matches('/')
}

pub(crate) fn issuer(app: &AppState, headers: &HeaderMap) -> String {
    if let Some(url) = &app.config.public_url {
        return url.trim_end_matches('/').to_string();
    }
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("127.0.0.1");
    if let Some(parsed) = parse_http_uri(&format!("http://{host}")) {
        if is_loopback_host(&parsed.host) {
            return match parsed.port {
                Some(port) => format!("http://{}:{port}", parsed.host),
                None => format!("http://{}", parsed.host),
            };
        }
    }
    "http://127.0.0.1".into()
}

fn pkce_valid(verifier: &str, challenge: &str) -> bool {
    if verifier.len() < 43 || verifier.len() > 128 {
        return false;
    }
    if !verifier
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_' | '~'))
    {
        return false;
    }
    let digest = Sha256::digest(verifier.as_bytes());
    let computed = URL_SAFE_NO_PAD.encode(digest);
    secrets_equal(&computed, challenge)
}

fn secrets_equal(left: &str, right: &str) -> bool {
    let left = Sha256::digest(left.as_bytes());
    let right = Sha256::digest(right.as_bytes());
    left.iter()
        .zip(right.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn basic_client(headers: &HeaderMap) -> Option<(String, Option<String>)> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let encoded = value.strip_prefix("Basic ")?;
    let decoded = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
    let (id, secret) = decoded.split_once(':')?;
    Some((percent_decode(id), Some(percent_decode(secret))))
}

fn parse_form(body: &[u8]) -> HashMap<String, String> {
    let text = String::from_utf8_lossy(body);
    let mut out = HashMap::new();
    for pair in text.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(percent_decode(key), percent_decode(value));
    }
    out
}

fn percent_decode(input: &str) -> String {
    super::percent_decode(input)
}

fn split_scopes(scope: &str) -> Vec<String> {
    scope
        .split_whitespace()
        .filter(|item| !item.is_empty())
        .map(|item| item.to_string())
        .collect()
}

fn dcr_authorized(app: &AppState, headers: &HeaderMap) -> bool {
    let Some(expected) = app
        .config
        .smart_dcr_token
        .as_deref()
        .filter(|token| !token.is_empty())
    else {
        return false;
    };
    let Some(got) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
    else {
        return false;
    };
    secrets_equal(expected, got)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn redirect_with(base: &str, pairs: &[(&str, &str)]) -> Response {
    let mut url = base.to_string();
    let mut first = !base.contains('?');
    for (key, value) in pairs {
        if value.is_empty() && *key == "state" {
            continue;
        }
        url.push(if first { '?' } else { '&' });
        first = false;
        url.push_str(key);
        url.push('=');
        url.push_str(&query_encode(value));
    }
    (StatusCode::FOUND, [(header::LOCATION, url)]).into_response()
}

fn query_encode(input: &str) -> String {
    let mut out = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn json_response(status: StatusCode, value: Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        Json(value),
    )
        .into_response()
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    json_response(
        status,
        json!({
            "error": error,
            "error_description": description
        }),
    )
}

pub fn security_extension(app: &AppState, headers: &HeaderMap) -> Value {
    let issuer = issuer(app, headers);
    json!({
        "cors": true,
        "service": [{"coding": [{
            "system": "http://terminology.hl7.org/CodeSystem/restful-security-service",
            "code": "SMART-on-FHIR"
        }]}],
        "extension": [{
            "url": "http://fhir-registry.smarthealthit.org/StructureDefinition/oauth-uris",
            "extension": [
                {"url": "authorize", "valueUri": format!("{issuer}/oauth/authorize")},
                {"url": "token", "valueUri": format!("{issuer}/oauth/token")},
                {"url": "register", "valueUri": format!("{issuer}/oauth/register")},
                {"url": "manage", "valueUri": format!("{issuer}/oauth/manage")},
                {"url": "introspect", "valueUri": format!("{issuer}/oauth/introspect")},
                {"url": "revoke", "valueUri": format!("{issuer}/oauth/revoke")}
            ]
        }]
    })
}
