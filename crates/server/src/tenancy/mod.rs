//! Serving many tenants from one process.
//!
//! A [`TenantHost`] holds the server of every tenant a process serves, each built by
//! [`tenant_app`](crate::tenant_app) like a process that serves one: its own database handle,
//! catalog, SMART tables and keys, health card key, jobs, Subscriptions, `/sync` sessions and
//! audit trail. The host only chooses which of them answers a request, and so adds nothing a
//! tenant's server does not already keep to itself:
//!
//! - by **path**, `{prefix}/{tenant}/…` (the prefix is `/t` unless configured), which the tenant's
//!   server sees without the prefix;
//! - by **host name**, `{tenant}.{domain}`;
//! - for what a replica forwards to its primary (`x-refhir-forwarded` with `x-refhir-tenant`), by
//!   the tenant it names, since a replica forwards a path its tenant's server saw.
//!
//! Choosing a tenant grants nothing: each tenant's server checks its own SMART tokens, which name
//! it as their audience. What the host adds is a limit on each tenant's request rate and storage,
//! counters for each, and the admin API ([`admin`]) that creates and deletes tenants.

pub mod admin;
mod metrics;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::governance::limiter::Limiter;
use crate::http::TenantApp;
pub use admin::{
    AdminError, CreateTenant, DeleteTenant, HealthCardsChange, HealthCardsSettings, TenantAdmin,
    TenantSettings, UpdateTenant,
};
pub use metrics::TenantCounters;

/// The prefix of the path that names a tenant when none is configured: `/t/{tenant}/…`.
pub const DEFAULT_PATH_PREFIX: &str = "/t";

/// How requests name the tenant they are for, and the address each tenant is reached at.
#[derive(Clone, Debug, Default)]
pub struct Routing {
    /// `{prefix}/{tenant}/…`, the prefix without a trailing `/` (`/t`). `None` turns it off.
    pub path_prefix: Option<String>,
    /// `{tenant}.{domain}` in the `Host` header. `None` turns it off.
    pub domain: Option<String>,
    /// The address a tenant is reached at from outside, with `{tenant}` where its name goes:
    /// `https://fhir.example.com/t/{tenant}` or `https://{tenant}.fhir.example.com`. A URL
    /// without `{tenant}` is the base the path prefix is added to. SMART discovery, token
    /// audiences, health card issuers and the links a server writes are made from it, so each
    /// tenant gets its own. `None` leaves a tenant's server to work its address out as a process
    /// that serves one does.
    pub public_url: Option<String>,
}

impl Routing {
    /// The address tenant `name` is reached at, from [`Routing::public_url`].
    pub fn public_url(&self, name: &str) -> Option<String> {
        let template = self.public_url.as_deref()?.trim_end_matches('/');
        if template.contains("{tenant}") {
            return Some(template.replace("{tenant}", name));
        }
        let prefix = self
            .path_prefix
            .as_deref()
            .unwrap_or(DEFAULT_PATH_PREFIX)
            .trim_end_matches('/');
        Some(format!("{template}{prefix}/{name}"))
    }

    /// Checks the rule before anything is served by it.
    pub fn validate(&self) -> Result<(), String> {
        if self.path_prefix.is_none() && self.domain.is_none() {
            return Err(
                "tenants need a way to be told apart: a path prefix, a domain, or both".into(),
            );
        }
        if let Some(prefix) = &self.path_prefix {
            let ok = prefix.starts_with('/')
                && prefix.len() > 1
                && !prefix.ends_with('/')
                && prefix
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'));
            if !ok {
                return Err(format!(
                    "the tenant path prefix `{prefix}` must start with `/`, not end with one, and \
                     hold only letters, digits, `-`, `_`, `.` and `/`"
                ));
            }
            if prefix == "/admin" {
                return Err(
                    "the tenant path prefix cannot be /admin, where the admin API is".into(),
                );
            }
        }
        if let Some(domain) = &self.domain {
            if domain.is_empty() || domain.starts_with('.') || domain.contains('/') {
                return Err(format!(
                    "the tenant domain `{domain}` must be a host name such as fhir.example.com"
                ));
            }
        }
        if let Some(url) = &self.public_url {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(format!(
                    "the tenants' public URL `{url}` must be http or https"
                ));
            }
            if self.path_prefix.is_none() && !url.contains("{tenant}") {
                return Err(format!(
                    "tenants told apart by host name need `{{tenant}}` in their public URL \
                     (`https://{{tenant}}.example.com`), and `{url}` has none"
                ));
            }
        }
        Ok(())
    }

    /// The tenant a request names, and the path its server is to see.
    fn select(&self, headers: &HeaderMap, uri: &Uri) -> Option<(String, Option<Uri>)> {
        // What a replica forwards is for the tenant it names, at the path its server saw.
        if headers.contains_key(crate::cluster::FORWARDED) {
            if let Some(name) = headers
                .get(crate::cluster::TENANT)
                .and_then(|value| value.to_str().ok())
            {
                return Some((name.to_string(), None));
            }
        }
        if let Some(domain) = &self.domain {
            let host = headers
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
                .or_else(|| uri.host())
                .unwrap_or("");
            let host = host.rsplit_once(':').map_or(host, |(name, port)| {
                if port.chars().all(|c| c.is_ascii_digit()) {
                    name
                } else {
                    host
                }
            });
            if let Some(name) = host
                .to_ascii_lowercase()
                .strip_suffix(&format!(".{}", domain.to_ascii_lowercase()))
            {
                if !name.is_empty() && !name.contains('.') {
                    return Some((name.to_string(), None));
                }
            }
        }
        let prefix = self.path_prefix.as_deref()?;
        let rest = uri.path().strip_prefix(prefix)?.strip_prefix('/')?;
        let (name, tail) = rest.split_once('/').unwrap_or((rest, ""));
        if name.is_empty() {
            return None;
        }
        let path = format!("/{tail}");
        let rewritten = match uri.query() {
            Some(query) => format!("{path}?{query}"),
            None => path,
        };
        Some((name.to_string(), rewritten.parse().ok()))
    }
}

/// What a tenant may use of the process, beyond what its own governance settings say. `None` is
/// no limit (for the connection share, the process's default).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantLimits {
    /// Requests a second this process answers for the tenant, on average. Each process of a
    /// deployment counts its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_second: Option<f64>,
    /// How many requests may come at once above that rate. Defaults to one second's worth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u32>,
    /// Bytes the tenant may store. Past it, requests that would store more are refused (`507`)
    /// until it deletes enough, which is measured every few minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_bytes: Option<u64>,
    /// Database connections of the process's pool the tenant may hold at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<usize>,
}

impl TenantLimits {
    /// `self`, with what it leaves unset taken from `defaults`.
    pub fn or(&self, defaults: &TenantLimits) -> TenantLimits {
        TenantLimits {
            requests_per_second: self.requests_per_second.or(defaults.requests_per_second),
            burst: self.burst.or(defaults.burst),
            storage_bytes: self.storage_bytes.or(defaults.storage_bytes),
            connections: self.connections.or(defaults.connections),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(rate) = self.requests_per_second {
            if !(rate.is_finite() && rate > 0.0) {
                return Err("limits.requests_per_second must be a positive number".into());
            }
        }
        if self.burst == Some(0) {
            return Err("limits.burst must be at least 1".into());
        }
        if self.connections == Some(0) {
            return Err("limits.connections must be at least 1".into());
        }
        Ok(())
    }
}

/// A secret of tenant `tenant`, made from one of the process's own (`secret`) for `purpose`: the
/// SMART registration token and the built-in confidential client's secret. Every process that is
/// given the same secret makes the same one for a tenant, and no tenant's opens another's.
pub fn tenant_secret(secret: &str, tenant: &str, purpose: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC takes a key of any length");
    mac.update(b"refhir tenant secret\0");
    mac.update(purpose.as_bytes());
    mac.update(b"\0");
    mac.update(tenant.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Whether a tenant's server can answer.
#[derive(Clone)]
pub enum TenantState {
    /// Being opened: its packages installed, its catalog loaded, its lease taken.
    Starting,
    Ready(Arc<TenantApp>),
    /// It cannot be served by this process now, and why: it failed to open, or another process
    /// holds its writer lease. The process keeps trying where trying again can help.
    Unavailable(String),
    /// Deleted, and waiting out its grace period before its rows are removed.
    Deleted,
}

impl TenantState {
    pub fn name(&self) -> &'static str {
        match self {
            TenantState::Starting => "starting",
            TenantState::Ready(_) => "ready",
            TenantState::Unavailable(_) => "unavailable",
            TenantState::Deleted => "deleted",
        }
    }
}

struct Slot {
    state: TenantState,
    limits: TenantLimits,
    counters: Arc<TenantCounters>,
}

/// How a process that serves many tenants is set up.
#[derive(Clone, Debug, Default)]
pub struct HostConfig {
    pub routing: Routing,
    /// The bearer token the admin API takes. Without one the admin API answers nothing.
    pub admin_token: Option<String>,
    /// `primary`, `standby` or `replica`: what every tenant's server in this process is.
    pub role: String,
    /// Where a replica sends what changes tenants.
    pub primary_url: Option<String>,
    /// The limits of a tenant that sets none of its own.
    pub default_limits: TenantLimits,
}

/// The tenants a process serves, and how a request finds its tenant. See the module docs.
pub struct TenantHost {
    config: HostConfig,
    slots: RwLock<HashMap<String, Slot>>,
    limiter: Limiter,
    /// What the process measured each tenant to store, in bytes, and when.
    storage: RwLock<HashMap<String, (u64, Instant)>>,
    /// Whether the process has loaded the registry and opened what it serves.
    ready: std::sync::atomic::AtomicBool,
    admin: RwLock<Option<Arc<dyn TenantAdmin>>>,
    /// Per tenant connection counters, from the process's shared pool.
    pool_stats: RwLock<Option<PoolStatsFn>>,
    /// The rate limiter's clock starts here.
    epoch: Instant,
    /// What the process is now: a standby becomes a primary when it takes over a tenant.
    role: RwLock<String>,
}

type PoolStatsFn = Arc<dyn Fn(&str) -> Option<refhir_persistence::PoolStats> + Send + Sync>;

impl TenantHost {
    pub fn new(config: HostConfig) -> Arc<Self> {
        Arc::new(Self {
            role: RwLock::new(config.role.clone()),
            config,
            slots: RwLock::new(HashMap::new()),
            limiter: Limiter::new(100_000),
            storage: RwLock::new(HashMap::new()),
            ready: std::sync::atomic::AtomicBool::new(false),
            admin: RwLock::new(None),
            pool_stats: RwLock::new(None),
            epoch: Instant::now(),
        })
    }

    pub fn config(&self) -> &HostConfig {
        &self.config
    }

    /// `primary`, `standby` or `replica`.
    pub fn role(&self) -> String {
        self.role.read().expect("role").clone()
    }

    pub fn set_role(&self, role: &str) {
        *self.role.write().expect("role") = role.to_string();
    }

    /// What creates, lists and deletes tenants for the admin API.
    pub fn set_admin(&self, admin: Arc<dyn TenantAdmin>) {
        *self.admin.write().expect("admin") = Some(admin);
    }

    /// Where `/admin/metrics` reads each tenant's connection counters.
    pub fn set_pool_stats(
        &self,
        stats: impl Fn(&str) -> Option<refhir_persistence::PoolStats> + Send + Sync + 'static,
    ) {
        *self.pool_stats.write().expect("pool stats") = Some(Arc::new(stats));
    }

    /// Says the process has loaded the registry: `/readyz` and `/healthz` are 200 from then on.
    pub fn set_ready(&self) {
        self.ready.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Sets tenant `name`'s state and limits, adding it if it is new.
    pub fn set(&self, name: &str, state: TenantState, limits: TenantLimits) {
        let mut slots = self.slots.write().expect("slots");
        let slot = slots.entry(name.to_string()).or_insert_with(|| Slot {
            state: TenantState::Starting,
            limits: TenantLimits::default(),
            counters: Arc::default(),
        });
        slot.state = state;
        slot.limits = limits;
    }

    /// Changes only the state of tenant `name`, which must be there.
    pub fn set_state(&self, name: &str, state: TenantState) -> Option<TenantState> {
        let mut slots = self.slots.write().expect("slots");
        let slot = slots.get_mut(name)?;
        Some(std::mem::replace(&mut slot.state, state))
    }

    /// Stops routing to tenant `name` and forgets it, returning what it was.
    pub fn remove(&self, name: &str) -> Option<TenantState> {
        self.storage.write().expect("storage").remove(name);
        self.slots
            .write()
            .expect("slots")
            .remove(name)
            .map(|slot| slot.state)
    }

    pub fn state(&self, name: &str) -> Option<TenantState> {
        self.slots
            .read()
            .expect("slots")
            .get(name)
            .map(|slot| slot.state.clone())
    }

    /// The tenants this process knows, by name, with their states.
    pub fn states(&self) -> Vec<(String, TenantState)> {
        let mut states: Vec<(String, TenantState)> = self
            .slots
            .read()
            .expect("slots")
            .iter()
            .map(|(name, slot)| (name.clone(), slot.state.clone()))
            .collect();
        states.sort_by(|a, b| a.0.cmp(&b.0));
        states
    }

    pub fn counters(&self, name: &str) -> Option<Arc<TenantCounters>> {
        self.slots
            .read()
            .expect("slots")
            .get(name)
            .map(|slot| slot.counters.clone())
    }

    pub fn limits(&self, name: &str) -> Option<TenantLimits> {
        self.slots
            .read()
            .expect("slots")
            .get(name)
            .map(|slot| slot.limits.clone())
    }

    /// Records what tenant `name` was measured to store.
    pub fn set_storage(&self, name: &str, bytes: u64) {
        self.storage
            .write()
            .expect("storage")
            .insert(name.to_string(), (bytes, Instant::now()));
    }

    /// What tenant `name` was last measured to store, and how long ago.
    pub fn storage(&self, name: &str) -> Option<(u64, Duration)> {
        self.storage
            .read()
            .expect("storage")
            .get(name)
            .map(|(bytes, at)| (*bytes, at.elapsed()))
    }

    /// The router of the whole process: the probes, the admin API, and every tenant's server.
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new().fallback(dispatch).with_state(self.clone())
    }

    /// Only the admin API and the probes, for a listener of their own.
    pub fn admin_router(self: &Arc<Self>) -> Router {
        own_routes().with_state(self.clone())
    }
}

/// What the process itself answers, outside every tenant.
fn own_routes() -> Router<Arc<TenantHost>> {
    Router::new()
        .route("/livez", get(crate::health::livez))
        .route("/readyz", get(host_readyz))
        .route("/healthz", get(host_healthz))
        .nest("/admin", admin::routes())
}

async fn dispatch(State(host): State<Arc<TenantHost>>, request: Request<Body>) -> Response {
    let Some((name, path)) = host.config.routing.select(request.headers(), request.uri()) else {
        return match own_routes().with_state(host.clone()).oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        };
    };
    let (app, limits, counters) = {
        let slots = host.slots.read().expect("slots");
        let Some(slot) = slots.get(&name) else {
            return refusal(
                StatusCode::NOT_FOUND,
                "not-found",
                &format!("there is no tenant `{name}` here"),
                None,
            );
        };
        let app = match &slot.state {
            TenantState::Ready(app) => app.clone(),
            TenantState::Starting => {
                return refusal(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "transient",
                    &format!("tenant `{name}` is starting"),
                    Some(2),
                )
            }
            TenantState::Unavailable(reason) => {
                return refusal(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "transient",
                    &format!("tenant `{name}` cannot be served here now: {reason}"),
                    Some(5),
                )
            }
            TenantState::Deleted => {
                return refusal(
                    StatusCode::GONE,
                    "deleted",
                    &format!("tenant `{name}` has been deleted"),
                    None,
                )
            }
        };
        (app, slot.limits.clone(), slot.counters.clone())
    };
    let started = Instant::now();
    if let Some(rate) = limits.requests_per_second {
        let burst = limits.burst.unwrap_or_else(|| rate.ceil().max(1.0) as u32);
        let decision = host.limiter.take(
            &name,
            rate,
            burst,
            1,
            host.epoch.elapsed().as_nanos() as u64,
        );
        if !decision.allowed {
            counters.throttled();
            return refusal(
                StatusCode::TOO_MANY_REQUESTS,
                "throttled",
                &format!("tenant `{name}` is over its request rate"),
                Some(decision.retry_after_secs()),
            );
        }
    }
    if let Some(limit) = limits.storage_bytes {
        let seen_path = path
            .as_ref()
            .map_or_else(|| request.uri().path(), |uri| uri.path());
        if stores_more(request.method(), seen_path)
            && host.storage(&name).is_some_and(|(bytes, _)| bytes > limit)
        {
            counters.refused_storage();
            return refusal(
                StatusCode::INSUFFICIENT_STORAGE,
                "too-costly",
                &format!(
                    "tenant `{name}` stores more than its limit of {limit} bytes: delete \
                     something, or ask for a higher limit"
                ),
                None,
            );
        }
    }
    let mut request = request;
    if let Some(path) = path {
        *request.uri_mut() = path;
    }
    let _in_flight = counters.begin();
    let response = match app.router.clone().oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    counters.finish(response.status(), started.elapsed());
    response
}

/// Requests that can make a tenant store more resources: updates and patches, and the `POST`s that
/// create or install something (a create, a batch or transaction, `$install`, `$extract`, `$apply`,
/// `$submit`). Reads, deletes, searches by `POST`, the other operations and the SMART endpoints
/// are not: a tenant over its limit must still be able to sign in and delete its way back under it.
fn stores_more(method: &Method, path: &str) -> bool {
    match *method {
        Method::PUT | Method::PATCH => true,
        Method::POST => {
            let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
            match parts.as_slice() {
                [] => true,
                [first] => !first.starts_with('$') && !matches!(*first, "oauth" | "graphql"),
                [.., last] => matches!(*last, "$install" | "$extract" | "$apply" | "$submit"),
            }
        }
        _ => false,
    }
}

fn refusal(
    status: StatusCode,
    code: &str,
    diagnostics: &str,
    retry_after: Option<u64>,
) -> Response {
    let mut response = (
        status,
        [(header::CONTENT_TYPE, "application/fhir+json")],
        json!({
            "resourceType": "OperationOutcome",
            "issue": [{"severity": "error", "code": code, "diagnostics": diagnostics}]
        })
        .to_string(),
    )
        .into_response();
    if let Some(seconds) = retry_after {
        if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    response
}

/// `/readyz` of the process: 200 once it has loaded the registry and opened what it serves. A
/// tenant that cannot be served does not make the process unready; its own `/readyz` says so.
async fn host_readyz(State(host): State<Arc<TenantHost>>) -> Response {
    if host.is_ready() {
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json!({"status": "ready"}).to_string(),
        )
            .into_response()
    } else {
        crate::health::not_ready("starting", "loading the tenants")
    }
}

/// `/healthz` of the process: its role and each tenant's state.
async fn host_healthz(
    State(host): State<Arc<TenantHost>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let verbose = query.contains_key("verbose");
    let mut tenants = serde_json::Map::new();
    for (name, state) in host.states() {
        let mut entry = json!({"status": state.name()});
        if let TenantState::Unavailable(reason) = &state {
            entry["detail"] = json!(reason);
        }
        if verbose {
            if let Some(stats) = host
                .pool_stats
                .read()
                .expect("pool stats")
                .as_ref()
                .and_then(|stats| stats(&name))
            {
                entry["pool"] =
                    json!({"max": stats.max, "in_use": stats.in_use, "waits": stats.waits});
            }
        }
        tenants.insert(name, entry);
    }
    let ready = host.is_ready();
    let body = json!({
        "status": if ready { "ok" } else { "starting" },
        "role": host.role(),
        "tenants": Value::Object(tenants),
    });
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
