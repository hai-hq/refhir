//! SMART scope checks in front of dispatch. Handlers do not see tokens.

mod interact;
mod oauth;
mod oidc;
mod scopes;
mod signing;
mod store;

use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use refhir_database::Database;
use refhir_fhir::{resource_in_compartment, Catalog, FhirError};

use crate::http::AppState;

use scopes::{interaction_for, scope_allows};
use store::normalize_patient_id;

pub use oidc::{FhirUserLookup, Oidc, OidcConfig};
pub use store::SmartStore;

#[cfg(test)]
pub(crate) use interact::patient_exists;
pub(crate) use interact::{interact_get, interact_post, oidc_callback};
pub(crate) use oauth::{
    authorize, configuration, ehr_launch, introspect, issue, jwks, manage, parse_authorize_form,
    register, revoke, security_extension, style,
};
pub(crate) use scopes::http_interaction;
pub(crate) use store::origin_allowed;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Token {
    pub scopes: Vec<String>,
    pub patient: Option<String>,
    pub encounter: Option<String>,
    pub fhir_user: Option<String>,
    pub client_id: String,
    pub exp: u64,
    pub sub: Option<String>,
    pub iss: Option<String>,
    pub need_patient_banner: Option<bool>,
    pub smart_style_url: Option<String>,
}

/// Identity for an authenticated `/sync` socket. Resource scopes are checked per message, and the
/// token itself is checked again while the socket is open (see [`SyncSession::is_live`]).
#[derive(Clone, Debug, Default)]
pub struct SyncSession {
    scopes: Vec<String>,
    /// `Patient/{id}` when the token is patient-scoped.
    patient: Option<String>,
    /// The server lets a `PUT` create the resource it names, so an update is also a create.
    update_create: bool,
    /// The token that opened the socket, to look at it again. `None` where SMART is off.
    credential: Option<Credential>,
    /// How often an open socket looks at its token again, besides when it receives a message.
    recheck: std::time::Duration,
}

/// The bearer token a socket was opened with, and where to ask whether it still stands.
#[derive(Clone)]
struct Credential {
    smart: std::sync::Arc<SmartStore>,
    token: String,
    /// When the token expires, in seconds since the epoch.
    expires_at: u64,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token is a secret: it is never printed.
        f.debug_struct("Credential")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn check(
    app: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
    method: &str,
    resource_type: &str,
    path: &str,
    _patient: Option<&str>,
) -> Option<Response> {
    if !app.config.auth {
        return None;
    }
    let Some(token) = resolve_token(headers, query) else {
        return Some(unauthorized("a bearer token is required"));
    };
    let Some(record) = app.smart.token(&token) else {
        return Some(unauthorized("unknown token"));
    };
    if record.exp <= now() {
        return Some(unauthorized("token expired"));
    }
    let interaction = http_interaction(method, path);
    // Where a PUT may create what it names, the scope that lets it update must let it create too:
    // the request cannot be told apart beforehand from one that only updates.
    let creates = method == "PUT" && app.catalog.read().expect("catalog").update_create();
    let allowed = |interaction: &str| {
        record
            .scopes
            .iter()
            .any(|scope| scope_allows(scope, resource_type, interaction))
    };
    if !allowed(interaction) || (creates && !allowed("create")) {
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

/// Refuse the request unless the token may do each of `needs` (a resource type and an
/// interaction), for an operation that reads or writes more than the type in its path. Nothing is
/// refused when authorization is off. The token itself was checked by [`check`] before the handler
/// ran, so one that is missing or unknown is refused here as well, and not allowed through.
pub(crate) fn require_scopes(
    app: &AppState,
    headers: &HeaderMap,
    needs: &[(String, &str)],
) -> Option<Response> {
    if !app.config.auth {
        return None;
    }
    let scopes = token_scopes(app, headers, None).unwrap_or_default();
    for (resource_type, interaction) in needs {
        if !scopes
            .iter()
            .any(|scope| scope_allows(scope, resource_type, interaction))
        {
            return Some(
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "resourceType": "OperationOutcome",
                        "issue": [{
                            "severity": "error", "code": "forbidden",
                            "diagnostics": format!(
                                "scope does not allow {interaction} of {resource_type}, which this request does"
                            )
                        }]
                    })),
                )
                    .into_response(),
            );
        }
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
    let record = app.smart.token(&token)?;
    if record.exp <= now() {
        return None;
    }
    let patient_scope = record
        .scopes
        .iter()
        .any(|scope| scope.starts_with("patient/"));
    if !patient_scope {
        return None;
    }
    record
        .patient
        .as_ref()
        .map(|id| format!("Patient/{}", normalize_patient_id(id)))
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
            ..SyncSession::default()
        });
    }
    let Some(token) = resolve_token(headers, query) else {
        return Err(unauthorized("a bearer token is required"));
    };
    let Some(record) = app.smart.token(&token) else {
        return Err(unauthorized("unknown token"));
    };
    if record.exp <= now() {
        return Err(unauthorized("token expired"));
    }
    if record.scopes.is_empty() {
        return Err(unauthorized("token has no scopes"));
    }
    Ok(SyncSession {
        update_create: app.catalog.read().expect("catalog").update_create(),
        credential: Some(Credential {
            smart: app.smart.clone(),
            token,
            expires_at: record.exp,
        }),
        recheck: app.config.sync_token_recheck,
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
            .map(|id| format!("Patient/{}", normalize_patient_id(id))),
    })
}

impl SyncSession {
    pub fn patient(&self) -> Option<&str> {
        self.patient.as_deref()
    }

    /// How often an open socket looks at its token again, and when the token runs out of its own
    /// accord. Both are `None` for a socket that has no token (SMART is off).
    pub fn token_checks(&self) -> Option<(std::time::Duration, std::time::Duration)> {
        let credential = self.credential.as_ref()?;
        let remaining = credential.expires_at.saturating_sub(now());
        Some((self.recheck, std::time::Duration::from_secs(remaining)))
    }

    /// Whether the token that opened this socket still stands: not expired, and not revoked. A
    /// socket is checked when it is opened and no other time otherwise, so without this a token
    /// that was revoked, or ran out, would keep its socket for as long as the client held it.
    /// Where there is no token to check it is `true`.
    pub async fn is_live(&self) -> bool {
        let Some(credential) = &self.credential else {
            return true;
        };
        if credential.expires_at <= now() {
            return false;
        }
        let smart = credential.smart.clone();
        let token = credential.token.clone();
        let cached = smart.token_is_cached(&token);
        let lookup = move || smart.token(&token).is_some_and(|record| record.exp > now());
        if cached {
            lookup()
        } else {
            // Not in the cache, so this reads the database: off the async worker threads. A
            // lookup that could not finish is a token that could not be vouched for.
            tokio::task::spawn_blocking(lookup).await.unwrap_or(false)
        }
    }

    /// Check scopes for one operation. Subscriptions must be reads.
    /// The args come back with the controls that say who is asking, among them the compartment a
    /// patient-scoped token is limited to; whatever the client sent under those names is gone.
    pub fn prepare(&self, path: &str, args: &Value, subscribe: bool) -> Result<Value, String> {
        if path == "Bundle/transaction" || path == "Bundle/batch" {
            if subscribe {
                return Err("bundle cannot be subscribed".into());
            }
            self.authorize_bundle(args)?;
            let mut args = args.clone();
            self.stamp_controls(&mut args);
            return Ok(args);
        }
        let (resource, op) = path
            .split_once('/')
            .ok_or_else(|| format!("bad operation {path}"))?;
        let interaction = interaction_for(op, subscribe)?;
        let resource_type = if resource == "system" { "*" } else { resource };
        if !self.allows(resource_type, interaction) {
            return Err(format!("scope does not allow {path}"));
        }
        if op == "update" && self.update_create && !self.allows(resource_type, "create") {
            return Err(format!(
                "scope does not allow {path}: an update may create here, which needs create"
            ));
        }
        if let Some(patient) = &self.patient {
            if resource == "Patient" && !patient_args_match(patient, args) {
                return Err("token is limited to a different patient".into());
            }
        }
        let mut args = args.clone();
        self.stamp_controls(&mut args);
        Ok(args)
    }

    fn stamp_controls(&self, args: &mut Value) {
        let Some(object) = args.as_object_mut() else {
            return;
        };
        // Whatever a client sent under these names is not the server's word about the caller.
        for name in refhir_fhir::CONTROL_ARGS {
            object.remove(name);
        }
        object.insert("_readScopes".into(), json!(self.scopes));
        // Every search, a search inside a Bundle included, learns from this which compartment it
        // is limited to. It is separate from `_compartment`, which the client may set.
        if let Some(patient) = &self.patient {
            object.insert(refhir_fhir::PATIENT_COMPARTMENT.into(), json!(patient));
        }
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
            let url = entry
                .pointer("/request/url")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let interaction = http_interaction(method, url);
            let creates = method == "PUT" && self.update_create;
            if !self.allows(resource_type, interaction)
                || (creates && !self.allows(resource_type, "create"))
            {
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
        // A request that finds its targets by query (a conditional create, update or delete) can
        // match what belongs to another patient, and the entry's own resource says nothing about
        // that. The direct routes refuse these for a patient-scoped token, and so does a Bundle.
        if entry.pointer("/request/ifNoneExist").is_some() {
            return Err(OUTSIDE.into());
        }
        let Some(url) = entry
            .pointer("/request/url")
            .and_then(|value| value.as_str())
        else {
            if entry.get("resource").is_none() {
                return Err("bundle entry is missing a resource type".into());
            }
            continue;
        };
        if method != "GET" && url.contains('?') {
            return Err(OUTSIDE.into());
        }
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

/// A live query is run again whenever a write overlaps it, so it has to be a read. With SMART on
/// that is part of the scope check; this is the same refusal for a socket with no token, which has
/// no scopes to check. It runs before the operation does: a write that was found out afterwards
/// would already have been committed. A name this server does not know is left to the operation,
/// which says so.
pub(crate) fn refuse_subscribed_write(path: &str) -> Result<(), String> {
    if path == "Bundle/transaction" || path == "Bundle/batch" {
        return Err("bundle cannot be subscribed".into());
    }
    let Some((_, op)) = path.split_once('/') else {
        return Ok(());
    };
    match interaction_for(op, true) {
        Err(reason) if reason.ends_with("cannot be subscribed") => Err(reason),
        _ => Ok(()),
    }
}

pub(crate) fn strip_sdc_controls(args: &mut Value) {
    if let Some(object) = args.as_object_mut() {
        for name in refhir_fhir::CONTROL_ARGS {
            object.remove(name);
        }
    }
}

pub fn resolve_token(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    if let Some(value) = bearer(headers) {
        return Some(value.to_string());
    }
    query_token(query)
}

/// True when checking this request needs no database read: there is no token to look up, or
/// it is cached.
pub fn lookup_is_cheap(app: &AppState, headers: &HeaderMap, query: Option<&str>) -> bool {
    resolve_token(headers, query).is_none_or(|token| app.smart.token_is_cached(&token))
}

/// Whether these scopes may read what the server recorded about its callers: a `system/` scope
/// that covers `AuditEvent`. A user's wildcard read (`user/*.read`) does not, although it covers
/// every resource type: it is the data a clinician may see, and the audit trail is the record of
/// every clinician's access to it.
pub(crate) fn can_read_audit(scopes: &[String]) -> bool {
    scopes
        .iter()
        .any(|scope| scope.starts_with("system/") && scope_allows(scope, "AuditEvent", "read"))
}

/// Who the token on this request belongs to, for attributing what it does. `None` where SMART
/// is off or there is no live token. Call it after the token has been checked: it is then in
/// the cache, and this is no database read.
pub(crate) fn identity(
    app: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Option<crate::governance::Identity> {
    if !app.config.auth {
        return None;
    }
    let record = app.smart.token(&resolve_token(headers, query)?)?;
    if record.exp <= now() {
        return None;
    }
    Some(crate::governance::Identity {
        client_id: record.client_id,
        user: record.sub,
        fhir_user: record.fhir_user,
    })
}

pub fn token_scopes(
    app: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Option<Vec<String>> {
    let token = resolve_token(headers, query)?;
    app.smart.token(&token).map(|record| record.scopes)
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

pub(crate) fn percent_decode(input: &str) -> String {
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

pub(crate) fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            "Bearer realm=\"refhir\", error=\"invalid_token\"",
        )],
        Json(json!({
            "resourceType": "OperationOutcome",
            "issue": [{"severity": "error", "code": "login", "diagnostics": message}]
        })),
    )
        .into_response()
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
    fn an_update_that_may_create_needs_the_create_scope_too() {
        let session = |scopes: &[&str], update_create: bool| SyncSession {
            update_create,
            scopes: scopes.iter().map(|scope| scope.to_string()).collect(),
            ..SyncSession::default()
        };
        let update = json!({"id": "p1"});
        let bundle = json!({"resource": {"resourceType": "Bundle", "type": "transaction", "entry": [
            {"request": {"method": "PUT", "url": "Patient/p1"}, "resource": {"resourceType": "Patient"}}
        ]}});
        // Where a PUT only updates, update permission is enough.
        let plain = session(&["user/Patient.u"], false);
        assert!(plain.prepare("Patient/update", &update, false).is_ok());
        assert!(plain.prepare("Bundle/transaction", &bundle, false).is_ok());
        // Where it may create, update permission alone is not.
        let update_only = session(&["user/Patient.u"], true);
        assert!(update_only
            .prepare("Patient/update", &update, false)
            .is_err());
        assert!(update_only
            .prepare("Bundle/transaction", &bundle, false)
            .is_err());
        // Create and update held separately together are enough.
        let both = session(&["user/Patient.u", "user/Patient.c"], true);
        assert!(both.prepare("Patient/update", &update, false).is_ok());
        assert!(both.prepare("Bundle/transaction", &bundle, false).is_ok());
    }

    /// Whatever a session prepares for a read has to be something the engine runs: the server's
    /// additions to the arguments ride beside the caller's, and a search used to refuse them.
    #[test]
    fn what_a_session_prepares_for_a_read_is_what_the_engine_runs() {
        let (db, catalog, _store) = crate::tests::memory();
        let catalog = catalog.read().unwrap();
        for id in ["p1", "p2"] {
            refhir_fhir::import_resource(
                &db,
                &catalog,
                &json!({"resourceType": "Patient", "id": id, "active": true}),
            )
            .unwrap();
        }
        let run = |session: &SyncSession, path: &str, args: Value| {
            let prepared = session.prepare(path, &args, true).unwrap();
            assert!(
                prepared["_readScopes"].is_array(),
                "the server's word is on it"
            );
            refhir_fhir::run(&db, &catalog, path, &prepared)
                .unwrap_or_else(|err| panic!("{path} with {prepared}: {err}"))
        };
        let user = SyncSession {
            update_create: false,
            scopes: vec!["user/*.read".into()],
            patient: None,
            ..SyncSession::default()
        };
        assert_eq!(
            run(&user, "Patient/search", json!({"active": "true"})).value["total"],
            2
        );
        assert_eq!(run(&user, "Patient/search", json!({})).value["total"], 2);
        for (path, args) in [
            ("Patient/read", json!({"id": "p1"})),
            ("Patient/history", json!({"id": "p1"})),
            ("Patient/history", json!({})),
            ("Patient/$meta", json!({"id": "p1"})),
            ("Patient/$everything", json!({"id": "p1"})),
            ("system/history", json!({})),
        ] {
            run(&user, path, args);
        }
        // A token confined to one patient has the compartment stamped beside the scopes, and
        // the search stays inside it.
        let confined = SyncSession {
            update_create: false,
            scopes: vec!["patient/Patient.read".into()],
            patient: Some("Patient/p1".into()),
            ..SyncSession::default()
        };
        let found = run(&confined, "Patient/search", json!({"active": "true"}));
        assert_eq!(found.value["total"], 1);
        assert_eq!(found.value["entry"][0]["resource"]["id"], "p1");
    }

    #[test]
    fn sync_session_checks_each_operation() {
        let session = SyncSession {
            update_create: false,
            scopes: vec!["user/Patient.read".into()],
            patient: None,
            ..SyncSession::default()
        };
        assert!(session.prepare("Patient/search", &json!({}), true).is_ok());
        // Leniency is the server's to give a `GET`, never a live query's: whatever a client sent
        // under that name is dropped.
        let prepared = session
            .prepare(
                "Patient/search",
                &json!({"active": "true", "_handling": "lenient"}),
                true,
            )
            .unwrap();
        assert!(prepared.get("_handling").is_none(), "{prepared}");
        assert_eq!(prepared["active"], "true");
        assert!(session
            .prepare("Patient/update", &json!({"id": "p1"}), false)
            .is_err());
        assert!(session
            .prepare("Organization/search", &json!({}), true)
            .is_err());
        assert!(session.prepare("system/history", &json!({}), true).is_err());

        let writer = SyncSession {
            update_create: false,
            scopes: vec!["user/Patient.write".into()],
            patient: None,
            ..SyncSession::default()
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
            update_create: false,
            scopes: vec!["patient/Patient.read".into()],
            patient: Some("Patient/p1".into()),
            ..SyncSession::default()
        };
        let search = patient
            .prepare("Patient/search", &json!({"active": "true"}), true)
            .unwrap();
        assert_eq!(search["_patientCompartment"], "Patient/p1");
        // What the client sends under the server's name is replaced, not kept, and its own
        // `_compartment` is left alone: the engine combines the two.
        let spoofed = patient
            .prepare(
                "Patient/search",
                &json!({"_patientCompartment": "Patient/p2", "_compartment": "Patient/p2"}),
                true,
            )
            .unwrap();
        assert_eq!(spoofed["_patientCompartment"], "Patient/p1");
        assert_eq!(spoofed["_compartment"], "Patient/p2");
        // A Bundle carries the limit too, so the searches inside it are limited.
        let bundle = patient
            .prepare(
                "Bundle/batch",
                &json!({"resource": {"resourceType": "Bundle", "type": "batch", "entry": []},
                        "_patientCompartment": "Patient/p2"}),
                false,
            )
            .unwrap();
        assert_eq!(bundle["_patientCompartment"], "Patient/p1");
        // A token that is not patient-scoped has no limit, and cannot be given one.
        let writer = SyncSession {
            update_create: false,
            scopes: vec!["user/Patient.read".into()],
            patient: None,
            ..SyncSession::default()
        };
        let open = writer
            .prepare(
                "Patient/search",
                &json!({"_patientCompartment": "Patient/p2"}),
                true,
            )
            .unwrap();
        assert!(open.get("_patientCompartment").is_none());
        assert!(patient
            .prepare("Patient/read", &json!({"id": "p2"}), true)
            .is_err());
        assert!(patient
            .prepare("Patient/read", &json!({"id": "p1"}), true)
            .is_ok());

        let reader = SyncSession {
            update_create: false,
            scopes: vec!["user/Questionnaire.read".into()],
            patient: None,
            ..SyncSession::default()
        };
        let prepared = reader
            .prepare(
                "Questionnaire/$populate",
                &json!({"_readScopes": ["user/*.*"], "_sdcArchiver": true, "subject": "Patient/p1"}),
                false,
            )
            .unwrap();
        assert_eq!(prepared["_readScopes"], json!(["user/Questionnaire.read"]));
        assert!(prepared.get("_sdcArchiver").is_none());
        assert_eq!(prepared["subject"], "Patient/p1");
        assert_eq!(http_interaction("POST", "/Questionnaire/$populate"), "read");
        assert_eq!(http_interaction("POST", "/Patient"), "create");
        assert_eq!(
            http_interaction("POST", "/ImplementationGuide/$install"),
            "install"
        );
    }
}
