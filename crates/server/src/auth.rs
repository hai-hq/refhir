//! SMART scope checks in front of dispatch. Handlers do not see tokens.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::http::{fhir_json_public, AppState};

struct Token {
    scopes: Vec<String>,
    patient: Option<String>,
}

fn tokens() -> &'static Mutex<HashMap<String, Token>> {
    static TOKENS: std::sync::OnceLock<Mutex<HashMap<String, Token>>> = std::sync::OnceLock::new();
    TOKENS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn check(
    app: &AppState,
    headers: &HeaderMap,
    method: &str,
    resource_type: &str,
    _patient: Option<&str>,
) -> Option<Response> {
    if !app.config.auth {
        return None;
    }
    let Some(token) = bearer(headers) else {
        return Some(unauthorized("a bearer token is required"));
    };
    let table = tokens().lock().expect("tokens");
    let Some(record) = table.get(token) else {
        return Some(unauthorized("unknown token"));
    };
    let interaction = if method == "GET" || method == "HEAD" {
        "read"
    } else {
        "write"
    };
    if !record
        .scopes
        .iter()
        .any(|scope| scope_allows(scope, resource_type, interaction))
    {
        return Some(
            (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "resourceType": "OperationOutcome",
                    "issue": [{"severity": "error", "code": "forbidden", "diagnostics": "scope does not allow this request"}]
                })),
            )
                .into_response(),
        );
    }
    None
}

pub fn patient_compartment(app: &AppState, headers: &HeaderMap) -> Option<String> {
    if !app.config.auth {
        return None;
    }
    let token = bearer(headers)?;
    let table = tokens().lock().expect("tokens");
    let record = table.get(token)?;
    let patient_scope = record.scopes.iter().any(|scope| scope.starts_with("patient/"));
    if !patient_scope {
        return None;
    }
    record
        .patient
        .as_ref()
        .map(|id| format!("Patient/{id}"))
}

pub fn configuration(app: &AppState) -> Response {
    fhir_json_public(
        StatusCode::OK,
        json!({
            "authorization_endpoint": "/oauth/authorize",
            "token_endpoint": "/oauth/token",
            "capabilities": ["launch-standalone", "client-confidential-symmetric", "permission-patient", "permission-user"],
            "scopes_supported": ["patient/*.read", "user/*.read", "user/*.write", "system/*.read"],
            "enabled": app.config.auth
        }),
    )
}

pub fn issue(app: &AppState, body: &[u8]) -> Response {
    if !app.config.auth {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "smart is not enabled"})),
        )
            .into_response();
    }
    let text = String::from_utf8_lossy(body);
    let mut scope = String::new();
    let mut patient = None;
    for pair in text.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "scope" {
            scope = value.replace('+', " ");
        }
        if key == "patient" {
            patient = Some(value.to_string());
        }
    }
    let token = format!("tok-{}", tokens().lock().expect("tokens").len() + 1);
    let scopes = scope
        .split_whitespace()
        .filter(|item| !item.is_empty())
        .map(|item| item.to_string())
        .collect::<Vec<_>>();
    tokens().lock().expect("tokens").insert(
        token.clone(),
        Token {
            scopes: scopes.clone(),
            patient: patient.clone(),
        },
    );
    fhir_json_public(
        StatusCode::OK,
        json!({
            "access_token": token,
            "token_type": "bearer",
            "scope": scopes.join(" "),
            "patient": patient
        }),
    )
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

fn scope_allows(scope: &str, resource_type: &str, interaction: &str) -> bool {
    let mut parts = scope.split('/');
    let _context = parts.next().unwrap_or("");
    let Some(rest) = parts.next() else {
        return false;
    };
    let (type_name, permission) = rest.split_once('.').unwrap_or((rest, "*"));
    if type_name != "*" && type_name != resource_type {
        return false;
    }
    permission == "*" || permission == interaction || (permission == "read" && interaction == "read")
        || (permission == "*")
}

fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "resourceType": "OperationOutcome",
            "issue": [{"severity": "error", "code": "login", "diagnostics": message}]
        })),
    )
        .into_response()
}

pub fn seed_token(token: &str, scopes: &[&str], patient: Option<&str>) {
    tokens().lock().expect("tokens").insert(
        token.to_string(),
        Token {
            scopes: scopes.iter().map(|item| (*item).to_string()).collect(),
            patient: patient.map(|item| item.to_string()),
        },
    );
}

#[allow(dead_code)]
fn _value(value: Value) -> Value {
    value
}
