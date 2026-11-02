//! The admin API: creating, listing and deleting the tenants of a process that serves many.
//!
//! It is authenticated apart from FHIR: by a bearer token of its own (`[tenants] admin_token`),
//! which no SMART token can stand in for, and it can be given a listener of its own so that only
//! the network that manages tenants reaches it (put a proxy that checks client certificates in
//! front of that listener for mutual TLS). Without a token it refuses everything.
//!
//! What it does to a tenant it asks [`TenantAdmin`] to do: the process knows how to open a tenant
//! and where the registry is, and this module only speaks HTTP. On a replica, a request that
//! changes a tenant is sent on to the primary, as a FHIR write is.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{TenantHost, TenantLimits, TenantState};

/// What a tenant is created with. Kept in the registry, so every process that serves the tenant
/// serves it the same way, and a restart serves it as it was.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantSettings {
    /// FHIR packages installed for the tenant beyond the release's own, as `--package` takes
    /// them: `name@version` from the registry, or a directory or `.tgz` the process can read.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Lets `PUT [type]/[id]` create a resource under an id the client names.
    #[serde(default)]
    pub update_as_create: bool,
    /// SMART App Launch on the tenant. Needs the process to have a login configured.
    #[serde(default)]
    pub smart: bool,
    /// The tenant issues SMART Health Cards, signed with a key of its own.
    #[serde(default)]
    pub health_cards: bool,
    #[serde(default)]
    pub limits: TenantLimits,
}

/// The body of `POST /admin/tenants`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTenant {
    pub name: String,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub update_as_create: bool,
    /// `None` takes the process's own setting (`--smart`).
    #[serde(default)]
    pub smart: Option<bool>,
    #[serde(default)]
    pub health_cards: bool,
    #[serde(default)]
    pub limits: TenantLimits,
}

/// How `DELETE /admin/tenants/{name}` was asked to delete.
#[derive(Clone, Debug, Default)]
pub struct DeleteTenant {
    /// How long the tenant's rows are kept, and it can be restored. `None` is the process's own
    /// default.
    pub grace: Option<Duration>,
    /// Whether every resource is written out before the tenant stops being served. `None` is the
    /// process's own default.
    pub export: Option<bool>,
}

/// Why the admin API refused, with the status to say it with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminError {
    pub status: StatusCode,
    pub message: String,
}

impl AdminError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn not_found(name: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            format!("there is no tenant `{name}`"),
        )
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

/// What the process does for the admin API. Each method may block: it is called off the runtime.
/// A tenant is described as a JSON object; the host adds what it knows (how it is served here,
/// its counters, its last measured size) under `served`.
pub trait TenantAdmin: Send + Sync {
    /// Creates tenant `request.name` and opens it, or finds it: `(tenant, true)` when it is new,
    /// `(tenant, false)` when one with the same settings was there. One with other settings is a
    /// conflict (`409`).
    fn create(&self, request: CreateTenant) -> Result<(Value, bool), AdminError>;
    fn list(&self) -> Result<Vec<Value>, AdminError>;
    fn get(&self, name: &str) -> Result<Value, AdminError>;
    fn delete(&self, name: &str, request: DeleteTenant) -> Result<Value, AdminError>;
    fn restore(&self, name: &str) -> Result<Value, AdminError>;
}

pub(super) fn routes() -> Router<Arc<TenantHost>> {
    Router::new()
        .route("/tenants", get(list).post(create))
        .route("/tenants/:name", get(read).delete(remove))
        .route("/tenants/:name/restore", post(restore))
        .route("/metrics", get(metrics))
}

/// The admin token, compared in constant time; `None` when the request may go on.
fn refuse_unauthenticated(host: &TenantHost, headers: &HeaderMap) -> Option<Response> {
    let Some(expected) = host
        .config
        .admin_token
        .as_deref()
        .filter(|token| !token.is_empty())
    else {
        return Some(error(&AdminError::new(
            StatusCode::FORBIDDEN,
            "the admin API is off: give the process an admin token ([tenants] admin_token or \
             REFHIR_ADMIN_TOKEN)",
        )));
    };
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
        });
    let matches = given.is_some_and(|given| {
        let (left, right) = (
            Sha256::digest(given.as_bytes()),
            Sha256::digest(expected.as_bytes()),
        );
        left.iter()
            .zip(right.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    });
    if matches {
        return None;
    }
    let mut response = error(&AdminError::new(
        StatusCode::UNAUTHORIZED,
        "the admin API needs the admin token as a bearer token",
    ));
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    Some(response)
}

fn admin(host: &TenantHost) -> Result<Arc<dyn TenantAdmin>, Response> {
    host.admin.read().expect("admin").clone().ok_or_else(|| {
        error(&AdminError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "the process is starting",
        ))
    })
}

/// A request that changes tenants, on a process that does not: sent to the primary, or refused.
async fn not_here(
    host: &TenantHost,
    method: Method,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Bytes,
) -> Option<Response> {
    match host.role().as_str() {
        "primary" => None,
        "replica" => {
            let Some(base) = host.config.primary_url.clone() else {
                return Some(error(&AdminError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "this replica has no --primary-url to send tenant changes to",
                )));
            };
            let target = match query {
                Some(query) => format!("{}{path}?{query}", base.trim_end_matches('/')),
                None => format!("{}{path}", base.trim_end_matches('/')),
            };
            let authorization = headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let sent = tokio::task::spawn_blocking(move || {
                let mut request = ureq::AgentBuilder::new()
                    .redirects(0)
                    .timeout(Duration::from_secs(600))
                    .build()
                    .request(method.as_str(), &target)
                    .set("content-type", "application/json");
                if let Some(authorization) = &authorization {
                    request = request.set("authorization", authorization);
                }
                let result = if body.is_empty() {
                    request.call()
                } else {
                    request.send_bytes(&body)
                };
                match result {
                    Ok(response) | Err(ureq::Error::Status(_, response)) => {
                        let status = response.status();
                        let text = response.into_string().unwrap_or_default();
                        Ok((status, text))
                    }
                    Err(err) => Err(err.to_string()),
                }
            })
            .await;
            Some(match sent {
                Ok(Ok((status, text))) => (
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                    [(header::CONTENT_TYPE, "application/json")],
                    text,
                )
                    .into_response(),
                Ok(Err(message)) => error(&AdminError::new(
                    StatusCode::BAD_GATEWAY,
                    format!("could not reach the primary: {message}"),
                )),
                Err(_) => error(&AdminError::internal("forwarding task stopped")),
            })
        }
        _ => Some(error(&AdminError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "this process is a standby: send tenant changes to the primary",
        ))),
    }
}

async fn list(State(host): State<Arc<TenantHost>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = refuse_unauthenticated(&host, &headers) {
        return refusal;
    }
    let admin = match admin(&host) {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    match blocking(move || admin.list()).await {
        Ok(tenants) => {
            let tenants: Vec<Value> = tenants
                .into_iter()
                .map(|tenant| with_served(&host, tenant))
                .collect();
            reply(StatusCode::OK, json!({"tenants": tenants}))
        }
        Err(err) => error(&err),
    }
}

async fn create(
    State(host): State<Arc<TenantHost>>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(refusal) = refuse_unauthenticated(&host, &headers) {
        return refusal;
    }
    if let Some(elsewhere) = not_here(
        &host,
        method,
        "/admin/tenants",
        None,
        &headers,
        body.clone(),
    )
    .await
    {
        return elsewhere;
    }
    let request: CreateTenant = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return error(&AdminError::new(
                StatusCode::BAD_REQUEST,
                format!("the body: {err}"),
            ))
        }
    };
    if let Err(message) = request.limits.validate() {
        return error(&AdminError::invalid(message));
    }
    let admin = match admin(&host) {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let name = request.name.clone();
    match blocking(move || admin.create(request)).await {
        Ok((tenant, created)) => {
            let mut response = reply(
                if created {
                    StatusCode::CREATED
                } else {
                    StatusCode::OK
                },
                with_served(&host, tenant),
            );
            if let Ok(location) = HeaderValue::from_str(&format!("/admin/tenants/{name}")) {
                response.headers_mut().insert(header::LOCATION, location);
            }
            response
        }
        Err(err) => error(&err),
    }
}

async fn read(
    State(host): State<Arc<TenantHost>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = refuse_unauthenticated(&host, &headers) {
        return refusal;
    }
    let admin = match admin(&host) {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    match blocking(move || admin.get(&name)).await {
        Ok(tenant) => reply(StatusCode::OK, with_served(&host, tenant)),
        Err(err) => error(&err),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteQuery {
    grace: Option<String>,
    export: Option<bool>,
}

async fn remove(
    State(host): State<Arc<TenantHost>>,
    Path(name): Path<String>,
    method: Method,
    query: Result<Query<DeleteQuery>, axum::extract::rejection::QueryRejection>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = refuse_unauthenticated(&host, &headers) {
        return refusal;
    }
    if let Some(elsewhere) = not_here(
        &host,
        method,
        &format!("/admin/tenants/{name}"),
        uri.query(),
        &headers,
        Bytes::new(),
    )
    .await
    {
        return elsewhere;
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(err) => return error(&AdminError::new(StatusCode::BAD_REQUEST, err.body_text())),
    };
    let grace = match query.grace.as_deref().map(parse_duration).transpose() {
        Ok(grace) => grace,
        Err(message) => return error(&AdminError::new(StatusCode::BAD_REQUEST, message)),
    };
    let admin = match admin(&host) {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    let request = DeleteTenant {
        grace,
        export: query.export,
    };
    match blocking(move || admin.delete(&name, request)).await {
        Ok(tenant) => reply(StatusCode::OK, with_served(&host, tenant)),
        Err(err) => error(&err),
    }
}

async fn restore(
    State(host): State<Arc<TenantHost>>,
    Path(name): Path<String>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = refuse_unauthenticated(&host, &headers) {
        return refusal;
    }
    if let Some(elsewhere) = not_here(
        &host,
        method,
        &format!("/admin/tenants/{name}/restore"),
        None,
        &headers,
        Bytes::new(),
    )
    .await
    {
        return elsewhere;
    }
    let admin = match admin(&host) {
        Ok(admin) => admin,
        Err(response) => return response,
    };
    match blocking(move || admin.restore(&name)).await {
        Ok(tenant) => reply(StatusCode::OK, with_served(&host, tenant)),
        Err(err) => error(&err),
    }
}

/// `GET /admin/metrics`: every tenant's counters, in the Prometheus text format.
async fn metrics(State(host): State<Arc<TenantHost>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = refuse_unauthenticated(&host, &headers) {
        return refusal;
    }
    let states = host.states();
    let mut families: Vec<(&str, &str, &str, Vec<String>)> = Vec::new();
    let mut add =
        |family: &'static str, kind: &'static str, help: &'static str, line: String| match families
            .iter_mut()
            .find(|entry| entry.0 == family)
        {
            Some(entry) => entry.3.push(line),
            None => families.push((family, kind, help, vec![line])),
        };
    let pool_stats = host.pool_stats.read().expect("pool stats").clone();
    for (name, state) in &states {
        add(
            "refhir_tenant_up",
            "gauge",
            "1 when the tenant is served by this process",
            format!(
                "refhir_tenant_up{{tenant=\"{name}\"}} {}",
                u8::from(matches!(state, TenantState::Ready(_)))
            ),
        );
        if let Some(counters) = host.counters(name) {
            for (family, kind, help, labels, value) in counters.samples() {
                add(
                    family,
                    kind,
                    help,
                    format!("{family}{{tenant=\"{name}\"{labels}}} {value}"),
                );
            }
        }
        if let Some((bytes, _)) = host.storage(name) {
            add(
                "refhir_tenant_storage_bytes",
                "gauge",
                "What the tenant stores, as last measured",
                format!("refhir_tenant_storage_bytes{{tenant=\"{name}\"}} {bytes}"),
            );
        }
        if let Some(stats) = pool_stats.as_ref().and_then(|stats| stats(name)) {
            add(
                "refhir_tenant_connections_in_use",
                "gauge",
                "Database connections the tenant holds now",
                format!(
                    "refhir_tenant_connections_in_use{{tenant=\"{name}\"}} {}",
                    stats.in_use
                ),
            );
            add(
                "refhir_tenant_connections_max",
                "gauge",
                "Database connections the tenant may hold at once (its share of the pool)",
                format!(
                    "refhir_tenant_connections_max{{tenant=\"{name}\"}} {}",
                    stats.max
                ),
            );
            add(
                "refhir_tenant_connection_waits_total",
                "counter",
                "Times the tenant waited for a database connection",
                format!(
                    "refhir_tenant_connection_waits_total{{tenant=\"{name}\"}} {}",
                    stats.waits
                ),
            );
        }
    }
    let mut out = String::new();
    for (family, kind, help, lines) in families {
        out.push_str(&format!("# HELP {family} {help}\n# TYPE {family} {kind}\n"));
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        out,
    )
        .into_response()
}

/// `tenant` with what this process knows of it under `served`.
fn with_served(host: &TenantHost, mut tenant: Value) -> Value {
    let Some(name) = tenant
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return tenant;
    };
    let mut served = json!({
        "status": host.state(&name).map_or("not served", |state| state.name()),
    });
    if let Some(TenantState::Unavailable(reason)) = host.state(&name) {
        served["detail"] = json!(reason);
    }
    if let Some(counters) = host.counters(&name) {
        served["metrics"] = counters.json();
    }
    if let Some((bytes, age)) = host.storage(&name) {
        served["storage_bytes"] = json!(bytes);
        served["storage_measured_seconds_ago"] = json!(age.as_secs());
    }
    if let Some(limits) = host.limits(&name) {
        served["limits"] = serde_json::to_value(limits).unwrap_or(Value::Null);
    }
    tenant["served"] = served;
    tenant
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, AdminError> + Send + 'static,
) -> Result<T, AdminError> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|_| Err(AdminError::internal("the admin task stopped")))
}

fn reply(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn error(err: &AdminError) -> Response {
    reply(err.status, json!({"error": err.message}))
}

/// `30s`, `15m`, `24h`, `7d`, or a bare number of seconds.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let (number, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => text.split_at(at),
        None => (text, "s"),
    };
    let value: u64 = number
        .parse()
        .map_err(|_| format!("`{text}` is not a duration: use 30s, 15m, 24h or 7d"))?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => {
            return Err(format!(
                "`{text}` is not a duration: use 30s, 15m, 24h or 7d"
            ))
        }
    };
    Ok(Duration::from_secs(value.saturating_mul(seconds)))
}
