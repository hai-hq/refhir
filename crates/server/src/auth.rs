//! SMART scope checks in front of dispatch. Handlers do not see tokens.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use refhir_database::Database;
use refhir_fhir::{resource_in_compartment, Catalog, FhirError};

use crate::http::{fhir_json_public, AppState};

struct Token {
    scopes: Vec<String>,
    patient: Option<String>,
}

/// Identity for an authenticated `/sync` socket. Resource scopes are checked per message.
#[derive(Clone, Debug)]
pub struct SyncSession {
    scopes: Vec<String>,
    /// `Patient/{id}` when the token is patient-scoped.
    patient: Option<String>,
}

fn tokens() -> &'static Mutex<HashMap<String, Token>> {
    static TOKENS: std::sync::OnceLock<Mutex<HashMap<String, Token>>> = std::sync::OnceLock::new();
    TOKENS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn check(
    app: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
    method: &str,
    resource_type: &str,
    _patient: Option<&str>,
) -> Option<Response> {
    if !app.config.auth {
        return None;
    }
    let Some(token) = resolve_token(headers, query) else {
        return Some(unauthorized("a bearer token is required"));
    };
    let table = tokens().lock().expect("tokens");
    let Some(record) = table.get(&token) else {
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

pub fn patient_compartment(
    app: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Option<String> {
    if !app.config.auth {
        return None;
    }
    let token = resolve_token(headers, query)?;
    let table = tokens().lock().expect("tokens");
    let record = table.get(&token)?;
    let patient_scope = record
        .scopes
        .iter()
        .any(|scope| scope.starts_with("patient/"));
    if !patient_scope {
        return None;
    }
    record.patient.as_ref().map(|id| format!("Patient/{id}"))
}

/// A known token may open `/sync`. Each later operation is checked with [`SyncSession::prepare`].
pub fn open_sync_session(
    app: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<SyncSession, Response> {
    if !app.config.auth {
        return Ok(SyncSession {
            scopes: vec!["*/*.*".into()],
            patient: None,
        });
    }
    let Some(token) = resolve_token(headers, query) else {
        return Err(unauthorized("a bearer token is required"));
    };
    let table = tokens().lock().expect("tokens");
    let Some(record) = table.get(&token) else {
        return Err(unauthorized("unknown token"));
    };
    if record.scopes.is_empty() {
        return Err(unauthorized("token has no scopes"));
    }
    Ok(SyncSession {
        scopes: record.scopes.clone(),
        patient: record
            .patient
            .as_ref()
            .filter(|_| {
                record
                    .scopes
                    .iter()
                    .any(|scope| scope.starts_with("patient/"))
            })
            .map(|id| format!("Patient/{id}")),
    })
}

impl SyncSession {
    pub fn patient(&self) -> Option<&str> {
        self.patient.as_deref()
    }

    /// Check scopes for one operation. Subscriptions must be reads.
    /// Search args gain `_compartment` for patient-scoped tokens.
    pub fn prepare(&self, path: &str, args: &Value, subscribe: bool) -> Result<Value, String> {
        if path == "Bundle/transaction" || path == "Bundle/batch" {
            if subscribe {
                return Err("bundle cannot be subscribed".into());
            }
            self.authorize_bundle(args)?;
            return Ok(args.clone());
        }
        let (resource, op) = path
            .split_once('/')
            .ok_or_else(|| format!("bad operation {path}"))?;
        let interaction = interaction_for(op, subscribe)?;
        let resource_type = if resource == "system" { "*" } else { resource };
        if !self.allows(resource_type, interaction) {
            return Err(format!("scope does not allow {path}"));
        }
        if let Some(patient) = &self.patient {
            if resource == "Patient" && !patient_args_match(patient, args) {
                return Err("token is limited to a different patient".into());
            }
        }
        let mut args = args.clone();
        if op == "search" {
            if let Some(patient) = &self.patient {
                if let Some(object) = args.as_object_mut() {
                    object
                        .entry("_compartment")
                        .or_insert_with(|| Value::String(patient.clone()));
                }
            }
        }
        Ok(args)
    }

    fn allows(&self, resource_type: &str, interaction: &str) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope_allows(scope, resource_type, interaction))
    }

    fn authorize_bundle(&self, args: &Value) -> Result<(), String> {
        let bundle = args.get("resource").unwrap_or(args);
        let Some(entries) = bundle.get("entry").and_then(|value| value.as_array()) else {
            return Ok(());
        };
        for entry in entries {
            let resource_type = entry_resource_type(entry)
                .ok_or_else(|| "bundle entry is missing a resource type".to_string())?;
            let method = entry
                .pointer("/request/method")
                .and_then(|value| value.as_str())
                .unwrap_or("POST");
            let interaction = if method == "GET" || method == "HEAD" {
                "read"
            } else {
                "write"
            };
            if !self.allows(resource_type, interaction) {
                return Err(format!(
                    "scope does not allow {method} {resource_type} in bundle"
                ));
            }
        }
        Ok(())
    }
}

fn patient_args_match(patient: &str, args: &Value) -> bool {
    let expected = patient.trim_start_matches("Patient/");
    for id in [
        args.get("id").and_then(|value| value.as_str()),
        args.pointer("/resource/id")
            .and_then(|value| value.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        if !id.is_empty() && id != expected {
            return false;
        }
    }
    true
}

const OUTSIDE: &str = "token is limited to a different patient";

/// Map an HTTP request onto an operation for patient-compartment checks.
/// `Ok(None)` means search, which is limited by `_compartment` in the handler.
pub fn patient_http_operation(
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Option<(String, Value)>, String> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        if method == "POST" {
            let body = body.ok_or_else(|| "request body is required".to_string())?;
            let op = if body.get("type").and_then(|value| value.as_str()) == Some("batch") {
                "Bundle/batch"
            } else {
                "Bundle/transaction"
            };
            return Ok(Some((op.to_string(), json!({"resource": body}))));
        }
        return Err(OUTSIDE.into());
    }
    let mut parts = trimmed.split('/');
    let resource = parts.next().unwrap_or("");
    if resource.is_empty()
        || resource.starts_with('$')
        || resource.starts_with('_')
        || matches!(resource, "graphql" | "sync" | "export")
    {
        return Err(OUTSIDE.into());
    }
    let id = parts.next();
    let tail = parts.next();
    match (method, id, tail) {
        ("GET" | "HEAD", None, None) => Ok(None),
        ("POST", None, None) => {
            let body = body.ok_or_else(|| "request body is required".to_string())?;
            Ok(Some((
                format!("{resource}/create"),
                json!({"resource": body}),
            )))
        }
        ("PUT" | "DELETE" | "PATCH", None, None) => Err(OUTSIDE.into()),
        (_, Some("_history"), _) => Err(OUTSIDE.into()),
        (_, Some(id), None) if is_instance_id(id) => {
            Ok(Some(instance_operation(resource, method, id, body)))
        }
        (_, Some(id), Some("_history")) if is_instance_id(id) => {
            Ok(Some((format!("{resource}/history"), json!({"id": id}))))
        }
        (_, Some(id), Some(vid)) if is_instance_id(id) && tail_is_version(vid) => {
            Ok(Some((format!("{resource}/vread"), json!({"id": id}))))
        }
        (_, Some(id), _) if is_instance_id(id) => {
            Ok(Some(instance_operation(resource, method, id, body)))
        }
        _ => Err(OUTSIDE.into()),
    }
}

fn is_instance_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with('$') && !id.starts_with('_')
}

fn tail_is_version(tail: &str) -> bool {
    !tail.is_empty() && !tail.starts_with('$') && tail != "_history"
}

fn instance_operation(
    resource: &str,
    method: &str,
    id: &str,
    body: Option<&Value>,
) -> (String, Value) {
    let op = match method {
        "PUT" => "update",
        "DELETE" => "delete",
        "PATCH" => "patch",
        _ => "read",
    };
    let mut args = json!({"id": id});
    if let Some(body) = body {
        if body.get("resourceType").is_some() {
            args["resource"] = body.clone();
        }
    }
    (format!("{resource}/{op}"), args)
}

pub fn guard_patient_operation(
    db: &Database,
    catalog: &Catalog,
    patient: &str,
    path: &str,
    args: &Value,
) -> Result<(), String> {
    let patient_id = patient.trim_start_matches("Patient/");
    if path == "Bundle/transaction" || path == "Bundle/batch" {
        return guard_bundle(db, catalog, patient_id, args);
    }
    let (resource, op) = path
        .split_once('/')
        .ok_or_else(|| format!("bad operation {path}"))?;
    if op == "search" {
        return Ok(());
    }
    if resource == "Patient" && !patient_args_match(&format!("Patient/{patient_id}"), args) {
        return Err(OUTSIDE.into());
    }
    if resource == "system"
        || matches!(
            op,
            "conditional-create" | "conditional-update" | "conditional-delete" | "patch"
        )
    {
        return Err(OUTSIDE.into());
    }
    if op == "history"
        && args
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .is_empty()
    {
        return Err(OUTSIDE.into());
    }
    if let Some(id) = args.get("id").and_then(|value| value.as_str()) {
        if !id.is_empty() {
            match stored_in_patient(db, catalog, resource, id, patient_id)? {
                Some(true) => {}
                Some(false) => return Err(OUTSIDE.into()),
                None => {}
            }
        }
    }
    if let Some(resource_body) = args.get("resource") {
        if !body_in_patient(db, catalog, resource_body, patient_id)? {
            return Err(OUTSIDE.into());
        }
    }
    Ok(())
}

pub fn delivered_in_patient(
    db: &Database,
    catalog: &Catalog,
    patient: &str,
    value: &Value,
) -> Result<bool, String> {
    let Some(resource_type) = value.get("resourceType").and_then(|item| item.as_str()) else {
        return Ok(true);
    };
    if resource_type == "Bundle" {
        return Ok(true);
    }
    let patient_id = patient.trim_start_matches("Patient/");
    body_in_patient(db, catalog, value, patient_id)
}

fn guard_bundle(
    db: &Database,
    catalog: &Catalog,
    patient_id: &str,
    args: &Value,
) -> Result<(), String> {
    let bundle = args.get("resource").unwrap_or(args);
    let Some(entries) = bundle.get("entry").and_then(|value| value.as_array()) else {
        return Ok(());
    };
    for entry in entries {
        if let Some(resource) = entry.get("resource") {
            if !body_in_patient(db, catalog, resource, patient_id)? {
                return Err(OUTSIDE.into());
            }
        }
        let method = entry
            .pointer("/request/method")
            .and_then(|value| value.as_str())
            .unwrap_or("POST");
        let Some(url) = entry
            .pointer("/request/url")
            .and_then(|value| value.as_str())
        else {
            if entry.get("resource").is_none() {
                return Err("bundle entry is missing a resource type".into());
            }
            continue;
        };
        let (type_name, id) = split_request_url(url);
        if method == "POST" && id.is_none() && entry.get("resource").is_none() {
            return Err("bundle entry is missing a resource".into());
        }
        if let Some(id) = id {
            match stored_in_patient(db, catalog, type_name, id, patient_id)? {
                Some(true) => {}
                Some(false) => return Err(OUTSIDE.into()),
                None => {}
            }
        }
    }
    Ok(())
}

fn split_request_url(url: &str) -> (&str, Option<&str>) {
    let url = url.split('?').next().unwrap_or(url).trim_start_matches('/');
    let mut parts = url.split('/');
    let type_name = parts.next().unwrap_or("");
    let id = parts.next().filter(|id| is_instance_id(id));
    (type_name, id)
}

fn stored_in_patient(
    db: &Database,
    catalog: &Catalog,
    resource_type: &str,
    id: &str,
    patient_id: &str,
) -> Result<Option<bool>, String> {
    let mut tx = db.begin();
    let resource = match refhir_fhir::dispatch(
        &mut tx,
        catalog,
        &format!("{resource_type}/read"),
        &json!({"id": id}),
    ) {
        Ok(resource) => resource,
        Err(FhirError::NotFound(_)) => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    Ok(Some(resource_in_compartment(
        catalog, &mut tx, &resource, "Patient", patient_id,
    )))
}

fn body_in_patient(
    db: &Database,
    catalog: &Catalog,
    resource: &Value,
    patient_id: &str,
) -> Result<bool, String> {
    let mut tx = db.begin();
    Ok(resource_in_compartment(
        catalog, &mut tx, resource, "Patient", patient_id,
    ))
}

fn entry_resource_type(entry: &Value) -> Option<&str> {
    if let Some(resource_type) = entry
        .pointer("/resource/resourceType")
        .and_then(|value| value.as_str())
    {
        if !resource_type.is_empty() {
            return Some(resource_type);
        }
    }
    let url = entry
        .pointer("/request/url")
        .and_then(|value| value.as_str())?;
    let resource_type = url.split(['/', '?']).next().unwrap_or("");
    if resource_type.is_empty() {
        None
    } else {
        Some(resource_type)
    }
}

fn interaction_for(op: &str, subscribe: bool) -> Result<&'static str, String> {
    const READ_OPS: &[&str] = &[
        "read",
        "vread",
        "search",
        "history",
        "$validate",
        "$meta",
        "$everything",
        "$document",
        "$expand",
        "$lookup",
        "$validate-code",
        "$translate",
        "$subsumes",
    ];
    const WRITE_OPS: &[&str] = &[
        "create",
        "update",
        "delete",
        "patch",
        "conditional-create",
        "conditional-update",
        "conditional-delete",
        "$meta-add",
        "$meta-delete",
    ];
    let interaction = if READ_OPS.contains(&op) {
        "read"
    } else if WRITE_OPS.contains(&op) {
        "write"
    } else {
        return Err(format!("unsupported operation {op}"));
    };
    if subscribe && interaction != "read" {
        return Err(format!("{op} writes and cannot be subscribed"));
    }
    Ok(interaction)
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

pub fn resolve_token(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    if let Some(value) = bearer(headers) {
        return Some(value.to_string());
    }
    query_token(query)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

fn query_token(query: Option<&str>) -> Option<String> {
    let query = query?;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "access_token" && !value.is_empty() {
            return Some(percent_decode(value));
        }
    }
    None
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    out.push(value);
                    index += 3;
                } else {
                    out.push(b'%');
                    index += 1;
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
pub fn install_token(token: &str, scopes: &[&str], patient: Option<&str>) {
    tokens().lock().expect("tokens").insert(
        token.to_string(),
        Token {
            scopes: scopes.iter().map(|scope| (*scope).to_string()).collect(),
            patient: patient.map(str::to_string),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_routes_require_a_wildcard_resource_type() {
        assert!(!scope_allows("user/Patient.read", "*", "read"));
        assert!(!scope_allows("user/Patient.write", "*", "write"));
        assert!(scope_allows("user/*.read", "*", "read"));
        assert!(scope_allows("user/Patient.read", "Patient", "read"));
        assert!(!scope_allows("user/Patient.read", "Patient", "write"));
        assert!(!scope_allows("user/Patient.read", "Observation", "read"));
    }

    #[test]
    fn access_token_query_is_percent_decoded() {
        assert_eq!(
            query_token(Some("access_token=tok%2B1%2F2%3D")).as_deref(),
            Some("tok+1/2=")
        );
        assert_eq!(
            query_token(Some("access_token=plain")).as_deref(),
            Some("plain")
        );
    }

    #[test]
    fn sync_session_checks_each_operation() {
        let session = SyncSession {
            scopes: vec!["user/Patient.read".into()],
            patient: None,
        };
        assert!(session.prepare("Patient/search", &json!({}), true).is_ok());
        assert!(session
            .prepare("Patient/update", &json!({"id": "p1"}), false)
            .is_err());
        assert!(session
            .prepare("Organization/search", &json!({}), true)
            .is_err());
        assert!(session.prepare("system/history", &json!({}), true).is_err());

        let writer = SyncSession {
            scopes: vec!["user/Patient.write".into()],
            patient: None,
        };
        assert!(writer
            .prepare(
                "Bundle/transaction",
                &json!({"resource": {"entry": [{
                    "resource": {"resourceType": "Patient"},
                    "request": {"method": "POST"}
                }]}}),
                false,
            )
            .is_ok());
        assert!(writer
            .prepare(
                "Bundle/transaction",
                &json!({"resource": {"entry": [{
                    "resource": {"resourceType": "Organization"},
                    "request": {"method": "POST"}
                }]}}),
                false,
            )
            .is_err());

        let patient = SyncSession {
            scopes: vec!["patient/Patient.read".into()],
            patient: Some("Patient/p1".into()),
        };
        let search = patient
            .prepare("Patient/search", &json!({"active": "true"}), true)
            .unwrap();
        assert_eq!(search["_compartment"], "Patient/p1");
        assert!(patient
            .prepare("Patient/read", &json!({"id": "p2"}), true)
            .is_err());
        assert!(patient
            .prepare("Patient/read", &json!({"id": "p1"}), true)
            .is_ok());
    }
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
    permission == "*"
        || permission == interaction
        || (permission == "read" && interaction == "read")
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
