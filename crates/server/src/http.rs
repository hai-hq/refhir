//! HTTP FHIR mapped onto the operation registry.
//! The router does not implement a second engine.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::body::{Body, Bytes};
use axum::extract::ws::Message;
use axum::extract::{Extension, OriginalUri, Path, Query, RawQuery, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, Request, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use refhir_database::{CommitError, Database};
use refhir_fhir::{outcome_code, Catalog, ConformanceStore, FhirError, RunError, RunResult};
use serde_json::{json, Value};

use crate::sync;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub auth: bool,
    pub export_dir: String,
    pub terminology_urls: Vec<String>,
    /// Form Archiver: QuestionnaireResponse may be created, not read.
    pub sdc_archiver: bool,
    /// Absolute FHIR base URL used in SMART discovery (issuer, endpoints).
    pub public_url: Option<String>,
    /// Patient id established for `launch/patient` when none is supplied.
    pub smart_patient: Option<String>,
    /// Encounter id established for `launch/encounter` when none is supplied.
    pub smart_encounter: Option<String>,
    /// Relative FHIR identity used as the `fhirUser` claim, e.g. `Patient/smart`.
    pub smart_fhir_user: Option<String>,
    /// RFC 7591 initial access token required for `POST /oauth/register`.
    pub smart_dcr_token: Option<String>,
    /// Secret for the built-in `confidential` client. Generated when unset.
    pub smart_confidential_secret: Option<String>,
    /// Sign users in through an external OpenID Connect provider. This is the login for real
    /// deployments.
    pub oidc: Option<crate::auth::OidcConfig>,
    /// Accept any username with no credential check. For testing only; ignored when `oidc` is set.
    pub smart_demo_login: bool,
    /// The key that signs SMART Health Cards. Without one the server issues none, and says so.
    pub health_cards: Option<std::sync::Arc<crate::health_cards::IssuerKey>>,
    /// Injected SMART tables for tests. A fresh store is created when this is `None`.
    pub smart: Option<std::sync::Arc<crate::auth::SmartStore>>,
    /// Where a replica sends what it cannot do itself: writes, and reads that need a newer
    /// snapshot than it has. Unused by a primary.
    pub primary_url: Option<String>,
    /// A replica that has not refreshed within this long reports itself unhealthy.
    pub max_staleness: std::time::Duration,
    /// How long `/readyz` waits for its database query before it says `unavailable`.
    pub ready_timeout: std::time::Duration,
    /// How long `/readyz` reuses its last database result. The endpoint is open, so this keeps
    /// the query count at one per interval however many callers probe it.
    pub ready_cache: std::time::Duration,
    /// How long a replica waits for `X-Refhir-Min-Ts` before sending the request to the primary.
    pub min_ts_wait: std::time::Duration,
    /// How often an open `/sync` socket checks that its token is still valid. It is also checked
    /// when the socket receives a message and when the token's own expiry passes, so a revoked
    /// token ends its sockets within this long and an expired one at once.
    pub sync_token_recheck: std::time::Duration,
    /// Table of background jobs such as `$export`. `refhir` passes the database's own, so jobs
    /// outlive a restart. Without one they live in memory and end with the process.
    pub jobs: Option<crate::jobs::SharedJobStore>,
    /// Rate limits, metering, request logs, quotas and the audit trail. All off by default.
    pub governance: crate::governance::GovernanceConfig,
    /// Where usage, the request log and the audit trail are written. `refhir` passes the
    /// database's own, so they outlive a restart. Without one they live in memory and end with
    /// the process.
    pub governance_store: Option<SharedGovernanceStore>,
    /// How many matches a search `GET` returns on one page when it names no `_count`; the rest
    /// follow behind the `next` link. `0` returns every match in one response, as a search used to.
    /// Only the searches a client sends are paged this way: a conditional update or delete, a
    /// `_has` or chained search, a live query and GraphQL read every match.
    pub default_page_size: usize,
    /// The tenant this process serves, when the database is shared by tenants. It is sent to the
    /// primary with everything a replica forwards, and a primary refuses what arrives from a
    /// process of another tenant, so a replica pointed at the wrong primary cannot write into a
    /// tenant that is not its own. `None` is a database with no tenants.
    pub tenant: Option<String>,
}

/// The page a search `GET` that names no `_count` gets. FHIR leaves the page size to the server.
pub const DEFAULT_PAGE_SIZE: usize = 100;

/// The database-backed governance tables, as passed in `ServerConfig::governance_store`.
#[derive(Clone)]
pub struct SharedGovernanceStore(pub std::sync::Arc<dyn refhir_persistence::GovernanceStore>);

impl SharedGovernanceStore {
    pub fn new(store: std::sync::Arc<dyn refhir_persistence::GovernanceStore>) -> Self {
        Self(store)
    }
}

impl std::fmt::Debug for SharedGovernanceStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedGovernanceStore")
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            auth: false,
            export_dir: "export".into(),
            terminology_urls: Vec::new(),
            sdc_archiver: false,
            health_cards: None,
            public_url: None,
            smart_patient: None,
            smart_encounter: None,
            smart_fhir_user: None,
            smart_dcr_token: None,
            smart_confidential_secret: None,
            oidc: None,
            smart_demo_login: false,
            smart: None,
            primary_url: None,
            max_staleness: std::time::Duration::from_secs(5),
            ready_timeout: std::time::Duration::from_secs(2),
            ready_cache: std::time::Duration::from_secs(1),
            min_ts_wait: std::time::Duration::from_millis(250),
            sync_token_recheck: std::time::Duration::from_secs(30),
            jobs: None,
            governance: crate::governance::GovernanceConfig::default(),
            governance_store: None,
            default_page_size: DEFAULT_PAGE_SIZE,
            tenant: None,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Database>,
    pub catalog: Arc<RwLock<Catalog>>,
    pub store: Arc<dyn ConformanceStore>,
    pub config: ServerConfig,
    pub smart: Arc<crate::auth::SmartStore>,
    pub oidc: Option<Arc<crate::auth::Oidc>>,
    pub jobs: Arc<crate::jobs::JobRunner>,
    pub governance: Arc<crate::governance::Governance>,
    pub(crate) ready_probe: Arc<crate::health::ReadyProbe>,
}

/// Delete expired OAuth rows once a minute. Reads ignore expired rows, so this only reclaims
/// space. It stops when the store is dropped, and does nothing outside a tokio runtime.
fn prune_in_background(smart: &Arc<crate::auth::SmartStore>) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let smart = Arc::downgrade(smart);
    runtime.spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            let Some(store) = smart.upgrade() else { break };
            let _ = tokio::task::spawn_blocking(move || store.prune()).await;
        }
    });
}

pub fn app(
    db: Arc<Database>,
    catalog: Arc<RwLock<Catalog>>,
    store: Arc<dyn ConformanceStore>,
) -> Router {
    app_with(db, catalog, store, ServerConfig::default())
}

pub fn app_with(
    db: Arc<Database>,
    catalog: Arc<RwLock<Catalog>>,
    store: Arc<dyn ConformanceStore>,
    config: ServerConfig,
) -> Router {
    app_parts(db, catalog, store, config).0
}

/// The router, and the governance state behind it, which a server flushes on shutdown.
pub fn app_parts(
    db: Arc<Database>,
    catalog: Arc<RwLock<Catalog>>,
    store: Arc<dyn ConformanceStore>,
    config: ServerConfig,
) -> (Router, Arc<crate::governance::Governance>) {
    catalog
        .write()
        .expect("catalog")
        .set_sdc_archiver(config.sdc_archiver);
    let router = Router::new()
        .route("/healthz", get(crate::cluster::healthz))
        .route("/metadata", get(metadata))
        .route("/.well-known/smart-configuration", get(smart_config))
        .route("/.well-known/jwks.json", get(jwks))
        .route(
            "/oauth/authorize",
            get(oauth_authorize).post(oauth_authorize_post),
        )
        .route("/oauth/token", post(oauth_token))
        .route("/oauth/introspect", post(oauth_introspect))
        .route("/oauth/revoke", post(oauth_revoke))
        .route("/oauth/register", post(oauth_register))
        .route("/oauth/manage", get(oauth_manage))
        .route("/oauth/ehr-launch", get(oauth_ehr_launch))
        .route(
            "/oauth/interact",
            get(oauth_interact).post(oauth_interact_post),
        )
        .route("/oauth/oidc/callback", get(oauth_oidc_callback))
        .route("/oauth/style.json", get(oauth_style))
        .route("/graphql", post(graphql_route))
        .route("/sync", get(sync_ws))
        .route("/_history", get(system_history))
        .route("/_audit", get(crate::governance::audit_events))
        .route("/_audit/verify", get(crate::governance::audit_verify))
        .route("/_usage", get(crate::governance::usage))
        .route("/_requests", get(crate::governance::requests))
        .route("/$export", post(export_system))
        .route("/export/:job", get(export_status))
        .route("/export/:job/file", get(export_file))
        .route("/", post(bundle_post))
        .route(
            "/:resource_type",
            get(search)
                .post(create)
                .put(conditional_update)
                .delete(conditional_delete),
        )
        .route("/:resource_type/_history", get(type_history))
        .route("/:resource_type/$validate", post(validate_op))
        .route("/:resource_type/$populate", post(populate_type))
        .route("/:resource_type/$assemble", post(assemble_type))
        .route("/:resource_type/$next-question", post(next_question_type))
        .route("/:resource_type/$behavior", post(behavior_type))
        .route("/:resource_type/$extract", post(extract_type))
        .route("/:resource_type/$install", post(install_op))
        .route(
            "/:resource_type/$expand",
            post(expand_op).get(terminology_get),
        )
        .route("/:resource_type/$expandability", post(expandability_op))
        .route(
            "/:resource_type/$validate-code",
            post(validate_code_op).get(terminology_get),
        )
        .route(
            "/:resource_type/$lookup",
            post(lookup_op).get(terminology_get),
        )
        .route(
            "/:resource_type/$translate",
            post(translate_op).get(terminology_get),
        )
        .route(
            "/:resource_type/$subsumes",
            post(subsumes_op).get(terminology_get),
        )
        .route("/:resource_type/$match", post(match_op))
        .route("/:resource_type/$submit", post(submit_op))
        .route("/:resource_type/$export", post(export_type))
        .route(
            "/:resource_type/$summary",
            get(summary_type).post(summary_type),
        )
        .route("/:resource_type/:id/_history/:vid", get(vread))
        .route("/:resource_type/:id/_history", get(instance_history))
        .route("/:resource_type/:id/$meta", get(meta_get).post(meta_post))
        .route("/:resource_type/:id/$validate", post(validate_instance))
        .route(
            "/:resource_type/:id/$expand",
            post(terminology_instance_post).get(terminology_get),
        )
        .route(
            "/:resource_type/:id/$validate-code",
            post(terminology_instance_post).get(terminology_get),
        )
        .route(
            "/:resource_type/:id/$subsumes",
            post(terminology_instance_post).get(terminology_get),
        )
        .route(
            "/:resource_type/:id/$translate",
            post(terminology_instance_post).get(terminology_get),
        )
        .route("/:resource_type/:id/$populate", post(populate_instance))
        .route("/:resource_type/:id/$assemble", post(assemble_instance))
        .route(
            "/:resource_type/:id/$next-question",
            post(next_question_instance),
        )
        .route("/:resource_type/:id/$behavior", post(behavior_instance))
        .route("/:resource_type/:id/$extract", post(extract_instance))
        .route("/:resource_type/:id/$apply", post(apply_op))
        .route("/:resource_type/:id/$everything", get(everything))
        .route("/:resource_type/:id/$document", get(document_op))
        .route(
            "/:resource_type/:id/$summary",
            get(summary_instance).post(summary_instance),
        )
        .route(
            "/:resource_type/:id/$health-cards-issue",
            post(health_cards_issue),
        )
        .route(
            "/health-cards/.well-known/jwks.json",
            get(health_cards_jwks),
        )
        .route("/:resource_type/:id/$websocket", get(subscription_socket))
        .route("/:resource_type/:id/$export", post(export_instance))
        .route(
            "/:resource_type/:id",
            get(read).put(update).delete(remove).patch(patch_op),
        )
        .route("/:resource_type/:id/:compartment", get(compartment_search));
    // Without a store from the caller, SMART state lives in memory and ends with the process.
    let smart = config.smart.clone().unwrap_or_else(|| {
        let secret = config
            .smart_confidential_secret
            .clone()
            .unwrap_or_else(|| format!("secret-{}", uuid::Uuid::new_v4()));
        Arc::new(crate::auth::SmartStore::with_confidential_secret(secret))
    });
    // Only the writer reclaims expired OAuth rows; a reader has nothing to delete with.
    if !db.is_read_only() {
        prune_in_background(&smart);
    }
    let jobs = crate::jobs::start(&db, &catalog, &config);
    let governance = crate::governance::Governance::new(
        config.governance.clone(),
        config.auth,
        config
            .governance_store
            .as_ref()
            .map(|shared| shared.0.clone()),
    );
    governance.start_background();
    let state = AppState {
        db: db.clone(),
        catalog: catalog.clone(),
        store: store.clone(),
        config: config.clone(),
        smart,
        oidc: config.oidc.clone().map(|cfg| {
            Arc::new(crate::auth::Oidc::new(cfg).expect("oidc config is validated at startup"))
        }),
        jobs,
        governance: governance.clone(),
        ready_probe: Arc::default(),
    };
    // Outermost first: cors, the legacy request line (silent once governance logs instead),
    // governance's scope, authentication, the caller's own limits, and then what a replica does
    // with a request it cannot serve. The limits come before that, so that a request is turned
    // away before a replica buffers its body to forward it or waits for a snapshot.
    let router = router
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::cluster::cluster_layer,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::governance::gate,
        ))
        .layer(middleware::from_fn_with_state(state.clone(), smart_guard))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::governance::govern,
        ))
        .layer(middleware::from_fn_with_state(state.clone(), request_log))
        .layer(middleware::from_fn_with_state(state.clone(), cors))
        // After every layer, on purpose: `layer` wraps only the routes added before it, so the
        // probes skip SMART, CORS, request logging and a replica's forwarding. Keep them here.
        .route("/livez", get(crate::health::livez))
        .route("/readyz", get(crate::health::readyz))
        .with_state(state);
    // Hooks, subscriptions and terminology fetches run where commits are made.
    if !db.is_read_only() {
        crate::jobs::attach(db, catalog, store, &config);
    }
    (router, governance)
}

pub async fn execute(
    db: &Arc<Database>,
    catalog: &Arc<RwLock<Catalog>>,
    store: &Arc<dyn ConformanceStore>,
    path: &str,
    args: Value,
) -> Result<RunResult, RunError> {
    // Whoever is running this is attributed in the audit trail by what the engine reports it
    // did, not by what the request said it would.
    let audit = crate::governance::begin_audit(path, &args);
    let db = db.clone();
    let catalog = catalog.clone();
    let store = store.clone();
    let path = path.to_string();
    let outcome = match tokio::task::spawn_blocking(move || {
        refhir_fhir::commit_operation(store.as_ref(), &db, &catalog, &path, &args)
    })
    .await
    {
        Ok(result) => {
            if let Ok(run) = &result {
                crate::cluster::report_ts(run.ts);
            }
            result
        }
        Err(_) => Err(RunError::Commit(CommitError::Invalid(
            "operation worker stopped".into(),
        ))),
    };
    if let Some(audit) = audit {
        audit.finish(&outcome);
    }
    outcome
}

async fn metadata(State(app): State<AppState>, headers: HeaderMap) -> Response {
    let mut statement = app.catalog.read().expect("catalog").capability_statement();
    if app.config.auth {
        if let Some(rest) = statement
            .pointer_mut("/rest/0")
            .and_then(|value| value.as_object_mut())
        {
            rest.insert(
                "security".into(),
                crate::auth::security_extension(&app, &headers),
            );
        }
    }
    if app.config.health_cards.is_some() {
        // Health cards are issued only where a signing key is set, so only then is it named.
        if let Some(patient) = statement
            .pointer_mut("/rest/0/resource")
            .and_then(|value| value.as_array_mut())
            .and_then(|resources| {
                resources.iter_mut().find(|resource| {
                    resource.get("type").and_then(|kind| kind.as_str()) == Some("Patient")
                })
            })
        {
            if let Some(operations) = patient
                .get_mut("operation")
                .and_then(|operations| operations.as_array_mut())
            {
                operations.push(json!({
                    "name": "health-cards-issue",
                    "definition": "http://hl7.org/fhir/uv/smart-health-cards-and-links/OperationDefinition/patient-i-health-cards-issue"
                }));
            }
        }
    }
    fhir_json(StatusCode::OK, statement)
}

async fn cors(State(app): State<AppState>, request: Request<Body>, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let origin_ok = match &origin {
        Some(origin) if !is_discovery(&path) => origin_registered(&app, origin).await,
        _ => false,
    };
    if request.method() == axum::http::Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        apply_cors(response.headers_mut(), &path, origin.as_deref(), origin_ok);
        return response;
    }
    let mut response = next.run(request).await;
    apply_cors(response.headers_mut(), &path, origin.as_deref(), origin_ok);
    response
}

fn is_discovery(path: &str) -> bool {
    path == "/metadata"
        || path.starts_with("/.well-known")
        // A health card is checked by an app in a browser, which reads the key set from here.
        || path == "/health-cards/.well-known/jwks.json"
}

/// Whether `origin` belongs to a registered SMART client. The list is cached in memory; when
/// the cache has expired the read goes to the database, which must not happen on an async
/// worker thread (the synchronous Postgres client refuses to run there).
async fn origin_registered(app: &AppState, origin: &str) -> bool {
    if app.smart.origins_are_cached() {
        return crate::auth::origin_allowed(&app.smart, origin);
    }
    let smart = app.smart.clone();
    let origin = origin.to_string();
    tokio::task::spawn_blocking(move || crate::auth::origin_allowed(&smart, &origin))
        .await
        .unwrap_or(false)
}

fn apply_cors(headers: &mut HeaderMap, path: &str, origin: Option<&str>, origin_ok: bool) {
    let allow = if is_discovery(path) {
        Some("*".to_string())
    } else if let (Some(origin), true) = (origin, origin_ok) {
        Some(origin.to_string())
    } else {
        None
    };
    if let Some(allow) = allow {
        if let Ok(value) = allow.parse() {
            headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
            if allow != "*" {
                headers.append(header::VARY, "Origin".parse().unwrap());
            }
        }
    }
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS"
            .parse()
            .unwrap(),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "Authorization, Content-Type, Accept, Prefer, If-Match, If-None-Match, X-Refhir-Min-Ts"
            .parse()
            .unwrap(),
    );
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        "Location, Content-Location, ETag, WWW-Authenticate, X-Refhir-Commit-Ts, Retry-After, \
         X-Request-Id, RateLimit-Limit, RateLimit-Remaining, RateLimit-Reset, Preference-Applied"
            .parse()
            .unwrap(),
    );
}

async fn request_log(State(app): State<AppState>, request: Request<Body>, next: Next) -> Response {
    // Governance logs every request itself, with a request id and the caller.
    if app.governance.config.request_log.enabled {
        return next.run(request).await;
    }
    let method = request.method().clone();
    let uri = request.uri().clone();
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    let status = response.status();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    log_http_request(&method, uri.path(), uri.query(), status, elapsed_ms);
    response
}

fn log_http_request(
    method: &axum::http::Method,
    path: &str,
    query: Option<&str>,
    status: StatusCode,
    elapsed_ms: f64,
) {
    let redacted = redact_query(query);
    let query = redacted.as_deref();
    if status.is_server_error() {
        tracing::error!(
            method = %method,
            path,
            query,
            status = status.as_u16(),
            elapsed_ms,
            "http request"
        );
    } else if status.is_client_error() {
        tracing::warn!(
            method = %method,
            path,
            query,
            status = status.as_u16(),
            elapsed_ms,
            "http request"
        );
    } else {
        tracing::info!(
            method = %method,
            path,
            query,
            status = status.as_u16(),
            elapsed_ms,
            "http request"
        );
    }
}

pub(crate) fn redact_query(query: Option<&str>) -> Option<String> {
    let query = query?;
    if !query.contains("access_token") {
        return Some(query.to_string());
    }
    Some(
        query
            .split('&')
            .map(|pair| {
                let key = pair.split_once('=').map(|(key, _)| key).unwrap_or(pair);
                if key == "access_token" {
                    "access_token=REDACTED"
                } else {
                    pair
                }
            })
            .collect::<Vec<_>>()
            .join("&"),
    )
}

/// Scope check for a REST request; the patient compartment, if the token is patient-scoped.
fn guard_request(
    app: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
    method: &str,
    resource: &str,
    path: &str,
) -> Result<Option<String>, Response> {
    if let Some(response) = crate::auth::check(app, headers, query, method, resource, path, None) {
        return Err(response);
    }
    Ok(crate::auth::patient_compartment(app, headers, query))
}

async fn smart_guard(
    State(app): State<AppState>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if app.config.auth {
        let path = request.uri().path().to_string();
        // `/healthz` stays open for a load balancer. Its `verbose` form shows commit counts,
        // timings and pool use, and takes a token like any other path.
        let verbose_health = path == "/healthz"
            && request.uri().query().is_some_and(|query| {
                query
                    .split('&')
                    .any(|pair| pair.split('=').next() == Some("verbose"))
            });
        let open = (path == "/healthz" && !verbose_health)
            || path == "/metadata"
            || path.starts_with("/.well-known")
            || path == "/health-cards/.well-known/jwks.json"
            || path.starts_with("/oauth/");
        if !open {
            let method = request.method().as_str().to_string();
            let query = request.uri().query().map(str::to_string);
            // A token that is not cached is read from the database: keep that off the async
            // worker threads. A cached one is a mutex and a clone, so it is checked in place.
            let cheap = crate::auth::lookup_is_cheap(&app, request.headers(), query.as_deref());
            if path == "/sync" {
                let opened = if cheap {
                    crate::auth::open_sync_session(&app, request.headers(), query.as_deref())
                } else {
                    let (guard_app, headers, query) =
                        (app.clone(), request.headers().clone(), query.clone());
                    match run_blocking_with(move || {
                        crate::auth::open_sync_session(&guard_app, &headers, query.as_deref())
                    })
                    .await
                    {
                        Ok(opened) => opened,
                        Err(response) => return response,
                    }
                };
                match opened {
                    Ok(session) => {
                        note_identity(&app, request.headers(), query.as_deref());
                        request.extensions_mut().insert(session);
                    }
                    Err(response) => return response,
                }
            } else {
                let resource = fhir_type_from_path(&path);
                let checked = if cheap {
                    guard_request(
                        &app,
                        request.headers(),
                        query.as_deref(),
                        &method,
                        &resource,
                        &path,
                    )
                } else {
                    let (guard_app, headers, query) =
                        (app.clone(), request.headers().clone(), query.clone());
                    let (guard_method, guard_path) = (method.clone(), path.clone());
                    match run_blocking_with(move || {
                        guard_request(
                            &guard_app,
                            &headers,
                            query.as_deref(),
                            &guard_method,
                            &resource,
                            &guard_path,
                        )
                    })
                    .await
                    {
                        Ok(checked) => checked,
                        Err(response) => return response,
                    }
                };
                // A caller refused for want of scope is still known, and the refusal is
                // attributed to them. One refused for want of a valid token is not.
                if checked.is_ok()
                    || matches!(&checked, Err(response) if response.status() == StatusCode::FORBIDDEN)
                {
                    note_identity(&app, request.headers(), query.as_deref());
                }
                let patient = match checked {
                    Ok(patient) => patient,
                    Err(response) => return response,
                };
                if let Some(patient) = patient {
                    // The body is read and parsed below, so the caller's limits come first: a
                    // refusal must not cost the work it is meant to spare.
                    if let Some(scope) = crate::governance::current() {
                        let class = crate::governance::classify(&method, &path);
                        if let Err(rejection) = app.governance.admit_once(&scope, class).await {
                            return rejection.into_response();
                        }
                    }
                    if let Some(response) =
                        reject_patient_http(&app, &patient, &method, &path, &mut request).await
                    {
                        return response;
                    }
                }
            }
        }
    }
    next.run(request).await
}

/// Tell the request's governance scope who this token belongs to.
fn note_identity(app: &AppState, headers: &HeaderMap, query: Option<&str>) {
    if let Some(scope) = crate::governance::current() {
        if let Some(identity) = crate::auth::identity(app, headers, query) {
            scope.set_identity(identity);
        }
    }
}

async fn reject_patient_http(
    app: &AppState,
    patient: &str,
    method: &str,
    path: &str,
    request: &mut Request<Body>,
) -> Option<Response> {
    let body = match buffered_body(request).await {
        Ok(body) => body,
        Err(response) => return Some(response),
    };
    let operation = match crate::auth::patient_http_operation(method, path, body.as_ref()) {
        Ok(operation) => operation,
        Err(message) => return Some(forbidden_response(&message)),
    };
    let (operation, args) = operation?;
    let catalog = app.catalog.read().expect("catalog");
    if let Err(message) =
        crate::auth::guard_patient_operation(&app.db, &catalog, patient, &operation, &args)
    {
        return Some(forbidden_response(&message));
    }
    None
}

async fn buffered_body(request: &mut Request<Body>) -> Result<Option<Value>, Response> {
    if request.method() == axum::http::Method::GET || request.method() == axum::http::Method::HEAD {
        return Ok(None);
    }
    let bytes = axum::body::to_bytes(
        std::mem::replace(request.body_mut(), Body::empty()),
        32 * 1024 * 1024,
    )
    .await
    .map_err(|_| {
        outcome(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too-costly",
            "request body is too large",
        )
    })?;
    *request.body_mut() = Body::from(bytes.clone());
    if bytes.is_empty() {
        return Ok(None);
    }
    parse_body(&bytes).map(Some)
}

fn forbidden_response(message: &str) -> Response {
    outcome(StatusCode::FORBIDDEN, "forbidden", message)
}

fn fhir_type_from_path(path: &str) -> String {
    let segment = path.trim_start_matches('/').split('/').next().unwrap_or("");
    // What the server recorded about its callers is read with the scope to read `AuditEvent`.
    if matches!(segment, "_audit" | "_usage" | "_requests") {
        return "AuditEvent".into();
    }
    if segment.is_empty()
        || segment.starts_with('$')
        || segment.starts_with('_')
        || matches!(segment, "graphql" | "sync" | "export")
    {
        "*".into()
    } else {
        segment.to_string()
    }
}

async fn sync_ws(
    ws: WebSocketUpgrade,
    State(app): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    session: Option<Extension<crate::auth::SyncSession>>,
) -> Response {
    if let Some(refusal) = crate::cluster::tenant_mismatch(&app, &headers) {
        return refusal;
    }
    if app.config.auth && session.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let session = session.map(|Extension(session)| session);
    // The upgrade is one request, but the connection that follows is many. It keeps the
    // scope of the request that opened it, so each message can be limited and attributed.
    let scope = crate::governance::current();
    let permit = match &scope {
        Some(scope) => match app.governance.open_sync_connection(scope) {
            Ok(permit) => Some(permit),
            Err(rejection) => return rejection.into_response(),
        },
        None => None,
    };
    // A replica answers queries itself and sends mutations to the primary as the same caller.
    let upstream = if app.db.is_read_only() {
        app.config
            .primary_url
            .as_deref()
            .map(|base| sync::Upstream {
                url: crate::cluster::sync_url(base),
                bearer: crate::auth::resolve_token(&headers, query.as_deref()),
                tenant: app.config.tenant.clone(),
            })
    } else {
        None
    };
    ws.on_upgrade(move |socket| async move {
        // The permit is held as long as the connection is.
        let _permit = permit;
        sync::handle_socket(
            socket,
            app.db,
            app.catalog,
            app.store,
            session,
            scope,
            upstream,
        )
        .await
    })
    .into_response()
}

async fn bundle_post(
    State(app): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let resource = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let path = if resource.get("type").and_then(|value| value.as_str()) == Some("batch") {
        "Bundle/batch"
    } else {
        "Bundle/transaction"
    };
    let mut args = json!({"resource": resource});
    stamp_sdc(&app, &headers, &mut args);
    if let Some(object) = args.as_object_mut() {
        limit_to_patient(&app, &headers, uri.query(), object);
    }
    match execute(&app.db, &app.catalog, &app.store, path, args).await {
        Ok(result) => render(StatusCode::OK, result.value, prefer_xml(&headers, &[])),
        Err(err) => error_response(err),
    }
}

async fn create(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let resource = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let if_none_exist = headers
        .get("if-none-exist")
        .and_then(|value| value.to_str().ok());
    let (path, mut args) = if let Some(query) = if_none_exist {
        (
            format!("{resource_type}/conditional-create"),
            json!({"resource": resource, "ifNoneExist": query}),
        )
    } else {
        (
            format!("{resource_type}/create"),
            json!({"resource": resource}),
        )
    };
    stamp_sdc(&app, &headers, &mut args);
    match execute(&app.db, &app.catalog, &app.store, &path, args).await {
        Ok(result) => {
            let id = result.value["id"].as_str().unwrap_or("");
            let location = format!("{resource_type}/{id}/_history/{}", result.ts);
            let status = if result.status == 200 {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            resource_response(
                status,
                result.value,
                Some(location),
                prefer_xml(&headers, &[]),
            )
        }
        Err(err) => error_response(err),
    }
}

async fn validate_op(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    body: Bytes,
) -> Response {
    let resource = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        &format!("{resource_type}/$validate"),
        json!({"resource": resource}),
    )
    .await
    {
        Ok(result) => fhir_json(StatusCode::OK, result.value),
        Err(err) => error_response(err),
    }
}

async fn populate_type(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    sdc_call(app, resource_type, None, "$populate", headers, body).await
}

async fn assemble_type(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    sdc_call(app, resource_type, None, "$assemble", headers, body).await
}

async fn next_question_type(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    sdc_call(app, resource_type, None, "$next-question", headers, body).await
}

async fn behavior_type(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    sdc_call(app, resource_type, None, "$behavior", headers, body).await
}

async fn extract_type(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    sdc_call(app, resource_type, None, "$extract", headers, body).await
}

async fn populate_instance(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    sdc_call(app, resource_type, Some(id), "$populate", headers, body).await
}

async fn assemble_instance(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    sdc_call(app, resource_type, Some(id), "$assemble", headers, body).await
}

async fn next_question_instance(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    sdc_call(
        app,
        resource_type,
        Some(id),
        "$next-question",
        headers,
        body,
    )
    .await
}

async fn behavior_instance(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    sdc_call(app, resource_type, Some(id), "$behavior", headers, body).await
}

async fn extract_instance(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    sdc_call(app, resource_type, Some(id), "$extract", headers, body).await
}

async fn sdc_call(
    app: AppState,
    resource_type: String,
    id: Option<String>,
    op: &str,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let resource = if body.is_empty() {
        json!({})
    } else {
        match parse_body(&body) {
            Ok(value) => value,
            Err(response) => return response,
        }
    };
    let mut args = json!({"resource": resource});
    if let Some(id) = id {
        args["id"] = json!(id);
    }
    stamp_sdc(&app, &headers, &mut args);
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        &format!("{resource_type}/{op}"),
        args,
    )
    .await
    {
        Ok(result) => fhir_json(StatusCode::OK, result.value),
        Err(err) => error_response(err),
    }
}

async fn install_op(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    body: Bytes,
) -> Response {
    if resource_type != "ImplementationGuide" {
        return outcome(
            StatusCode::BAD_REQUEST,
            "not-supported",
            "$install is an ImplementationGuide operation",
        );
    }
    if !app.config.auth {
        return crate::auth::unauthorized("ImplementationGuide/$install requires authentication");
    }
    terminology_op(app, resource_type, "$install", body).await
}

async fn expand_op(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    body: Bytes,
) -> Response {
    terminology_op(app, resource_type, "$expand", body).await
}

/// What a `GET` of a terminology operation puts in the query string, as the arguments a `POST`
/// body of plain JSON would hold: a parameter given twice counts once, as its first value.
fn query_args(params: Vec<(String, String)>) -> serde_json::Map<String, Value> {
    let mut args = serde_json::Map::new();
    for (name, value) in params {
        args.entry(name).or_insert(Value::String(value));
    }
    args
}

/// The operation a terminology route is for: the last segment of the path it matched.
fn route_op(uri: &Uri) -> &str {
    uri.path().rsplit('/').next().unwrap_or("")
}

/// The terminology operations have no side effects, so R4 allows each as a `GET` with its
/// parameters in the query string, at the type or, with an `[id]` in the path, at an instance.
/// (What cannot be a query parameter, a `coding` or a `codeableConcept`, is sent as a `POST`.)
async fn terminology_get(
    State(app): State<AppState>,
    Path(path): Path<HashMap<String, String>>,
    OriginalUri(uri): OriginalUri,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    let mut args = query_args(params);
    if let Some(id) = path.get("id") {
        args.insert(refhir_fhir::INSTANCE.into(), json!(id));
    }
    let resource_type = path.get("resource_type").cloned().unwrap_or_default();
    terminology_call(app, resource_type, route_op(&uri), Value::Object(args)).await
}

/// A terminology operation sent to an instance, `ValueSet/[id]/$expand`, as a `POST`. The body
/// may be empty: what is asked of is the instance, and nothing else is needed.
async fn terminology_instance_post(
    State(app): State<AppState>,
    Path(path): Path<HashMap<String, String>>,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response {
    let mut args = if body.is_empty() {
        json!({})
    } else {
        match parse_body(&body) {
            Ok(value) => value,
            Err(response) => return response,
        }
    };
    let Some(object) = args.as_object_mut() else {
        return outcome(
            StatusCode::BAD_REQUEST,
            "invalid",
            "the parameters of an operation are a JSON object",
        );
    };
    if let Some(id) = path.get("id") {
        object.insert(refhir_fhir::INSTANCE.into(), json!(id));
    }
    let resource_type = path.get("resource_type").cloned().unwrap_or_default();
    terminology_call(app, resource_type, route_op(&uri), args).await
}

async fn expandability_op(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    body: Bytes,
) -> Response {
    terminology_op(app, resource_type, "$expandability", body).await
}

async fn validate_code_op(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    body: Bytes,
) -> Response {
    terminology_op(app, resource_type, "$validate-code", body).await
}

async fn lookup_op(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    body: Bytes,
) -> Response {
    terminology_op(app, resource_type, "$lookup", body).await
}

async fn translate_op(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    body: Bytes,
) -> Response {
    terminology_op(app, resource_type, "$translate", body).await
}

/// `POST [base]/Claim/$submit` (and `CoverageEligibilityRequest/$submit`): the body is the
/// `Parameters` with a `resource`, or the Claim or Bundle itself, as R4 allows when that is the
/// only input. It writes the request, whatever it came with and the response, so the token needs
/// to be allowed to create each of them, not only the type in the path.
async fn submit_op(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    headers: HeaderMap,
    Query(params): Query<Vec<(String, String)>>,
    body: Bytes,
) -> Response {
    let resource = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let update_creates = app.catalog.read().expect("catalog").update_create();
    let writes = refhir_fhir::submission_writes(&resource_type, &resource, update_creates);
    if let Some(response) = crate::auth::require_scopes(&app, &headers, &writes) {
        return response;
    }
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        &format!("{resource_type}/$submit"),
        json!({"resource": resource}),
    )
    .await
    {
        Ok(result) => resource_response(
            StatusCode::OK,
            result.value,
            None,
            prefer_xml(&headers, &params),
        ),
        Err(err) => error_response(err),
    }
}

/// `POST [base]/ChargeItemDefinition/[id]/$apply`: the body is the operation's `Parameters`
/// (`chargeItem`, and `account` if there is one). It reads a ChargeItem and an Account the path does
/// not name, so the token needs to be able to read them, and it answers an Invoice that is not
/// stored. The body is JSON: the XML that `parse_body` reads does not make these Parameters.
async fn apply_op(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.first() == Some(&b'<') {
        return outcome(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "not-supported",
            "$apply takes its Parameters as JSON; XML is not supported",
        );
    }
    let resource = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Some(response) =
        crate::auth::require_scopes(&app, &headers, &refhir_fhir::apply_reads(&resource))
    {
        return response;
    }
    // The conditions of a definition follow references, and only what the token may read is theirs
    // to follow: the engine is told the token's scopes, as an SDC operation is.
    let mut args = json!({"id": id, "resource": resource});
    stamp_sdc(&app, &headers, &mut args);
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        &format!("{resource_type}/$apply"),
        args,
    )
    .await
    {
        Ok(result) => fhir_json(StatusCode::OK, result.value),
        Err(err) => error_response(err),
    }
}

/// `POST [base]/Patient/$match`: the body is the operation's `Parameters`, and what the engine makes
/// of them (and refuses in them) is its answer. It reads, so a replica answers it itself. The body
/// is JSON: the XML that `parse_body` can read does not make the Parameters this needs, and a
/// request it answered with "resource is required" would send its sender looking in the wrong place.
async fn match_op(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    body: Bytes,
) -> Response {
    if body.first() == Some(&b'<') {
        return outcome(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "not-supported",
            "$match takes its Parameters as JSON; XML is not supported",
        );
    }
    terminology_op(app, resource_type, "$match", body).await
}

async fn terminology_op(app: AppState, resource_type: String, op: &str, body: Bytes) -> Response {
    let args = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    terminology_call(app, resource_type, op, args).await
}

async fn terminology_call(app: AppState, resource_type: String, op: &str, args: Value) -> Response {
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        &format!("{resource_type}/{op}"),
        args,
    )
    .await
    {
        Ok(result) => fhir_json(StatusCode::OK, result.value),
        Err(err) => error_response(err),
    }
}

async fn read(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    if let Some(response) = archiver_read(&app, &resource_type) {
        return response;
    }
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        &format!("{resource_type}/read"),
        json!({"id": id}),
    )
    .await
    {
        Ok(result) => resource_response(
            StatusCode::OK,
            result.value,
            None,
            prefer_xml(&headers, &params),
        ),
        Err(err) => error_response(err),
    }
}

async fn update(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let resource = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let mut args = json!({"id": id, "resource": resource});
    if let Some(tag) = headers
        .get(header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
    {
        args["ifMatch"] = json!(tag);
    }
    stamp_sdc(&app, &headers, &mut args);
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        &format!("{resource_type}/update"),
        args,
    )
    .await
    {
        Ok(result) => {
            let location = format!("{resource_type}/{id}/_history/{}", result.ts);
            let status = if result.status == 201 {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            resource_response(
                status,
                result.value,
                Some(location),
                prefer_xml(&headers, &[]),
            )
        }
        Err(err) => error_response(err),
    }
}

async fn remove(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
) -> Response {
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        &format!("{resource_type}/delete"),
        json!({"id": id}),
    )
    .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => error_response(err),
    }
}

async fn search(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    if let Some(response) = archiver_read(&app, &resource_type) {
        return response;
    }
    if let Some(response) = authorize(&app, &headers, uri.query(), "GET", &resource_type, None) {
        return response;
    }
    let xml = prefer_xml(&headers, &params);
    let mut args = search_object(params);
    args.remove("access_token");
    limit_to_patient(&app, &headers, uri.query(), &mut args);
    let lenient = handle_unreadable_values(&headers, &mut args);
    ask_for_default_page(&app, &mut args);
    let args = Value::Object(args);
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        &format!("{resource_type}/search"),
        args,
    )
    .await
    {
        Ok(result) => note_handling(render(StatusCode::OK, result.value, xml), lenient),
        Err(err) => note_handling(error_response(err), lenient),
    }
}

async fn instance_history(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    if let Some(response) = archiver_read(&app, &resource_type) {
        return response;
    }
    let mut args = query_object(params);
    args.insert("id".into(), Value::String(id));
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        &format!("{resource_type}/history"),
        Value::Object(args),
    )
    .await
    {
        Ok(result) => fhir_json(StatusCode::OK, result.value),
        Err(err) => error_response(err),
    }
}

async fn vread(
    State(app): State<AppState>,
    Path((resource_type, id, vid)): Path<(String, String, String)>,
) -> Response {
    if let Some(response) = archiver_read(&app, &resource_type) {
        return response;
    }
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        &format!("{resource_type}/vread"),
        json!({"id": id, "versionId": vid}),
    )
    .await
    {
        Ok(result) => resource_response(StatusCode::OK, result.value, None, false),
        Err(err) => error_response(err),
    }
}

fn query_object(params: Vec<(String, String)>) -> serde_json::Map<String, Value> {
    let mut args = serde_json::Map::new();
    for (key, value) in params {
        if let Some(existing) = args.get(&key).and_then(|item| item.as_str()) {
            args.insert(key, Value::String(format!("{existing},{value}")));
        } else {
            args.insert(key, Value::String(value));
        }
    }
    args
}

/// The query of a search as arguments. A parameter given more than once is AND in a search
/// (`date=ge2020&date=lt2021`) and a comma inside one value is OR, so the occurrences are not
/// joined by a comma as `query_object` joins them.
fn search_object(params: Vec<(String, String)>) -> serde_json::Map<String, Value> {
    let mut folded = std::collections::BTreeMap::new();
    for (key, value) in params {
        refhir_fhir::add_param(&mut folded, key, value);
    }
    folded
        .into_iter()
        .map(|(key, value)| (key, Value::String(value)))
        .collect()
}

async fn type_history(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    if let Some(response) = archiver_read(&app, &resource_type) {
        return response;
    }
    run_read(
        &app,
        &format!("{resource_type}/history"),
        Value::Object(query_object(params)),
    )
    .await
}

async fn system_history(
    State(app): State<AppState>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    run_read(&app, "system/history", Value::Object(query_object(params))).await
}

async fn conditional_update(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    Query(params): Query<Vec<(String, String)>>,
    body: Bytes,
) -> Response {
    let resource = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let query = search_object(params)
        .iter()
        .map(|(key, value)| format!("{key}={}", value.as_str().unwrap_or("")))
        .collect::<Vec<_>>()
        .join("&");
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        &format!("{resource_type}/conditional-update"),
        json!({"resource": resource, "query": query}),
    )
    .await
    {
        Ok(result) => {
            let status = if result.status == 201 {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            resource_response(status, result.value, None, false)
        }
        Err(err) => error_response(err),
    }
}

async fn conditional_delete(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    let query = search_object(params)
        .iter()
        .map(|(key, value)| format!("{key}={}", value.as_str().unwrap_or("")))
        .collect::<Vec<_>>()
        .join("&");
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        &format!("{resource_type}/conditional-delete"),
        json!({"query": query}),
    )
    .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => error_response(err),
    }
}

async fn patch_op(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let patch = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let mut args = json!({"id": id, "patch": patch});
    if let Some(tag) = headers
        .get(header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
    {
        args["ifMatch"] = json!(tag);
    }
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        &format!("{resource_type}/patch"),
        args,
    )
    .await
    {
        Ok(result) => resource_response(
            StatusCode::OK,
            result.value,
            None,
            prefer_xml(&headers, &[]),
        ),
        Err(err) => error_response(err),
    }
}

async fn subscription_socket(
    ws: WebSocketUpgrade,
    State(app): State<AppState>,
    Path((_resource_type, id)): Path<(String, String)>,
) -> Response {
    // Notifications are delivered by the process that commits, so a socket opened on a replica
    // would never hear anything.
    if app.db.is_read_only() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "subscription sockets are served by the primary",
        )
            .into_response();
    }
    ws.on_upgrade(move |socket| async move {
        let mut rx = crate::jobs::register_socket(id);
        let (mut sink, mut stream) = socket.split();
        loop {
            tokio::select! {
                payload = rx.recv() => {
                    let Some(payload) = payload else { break };
                    if sink.send(Message::Text(payload)).await.is_err() {
                        break;
                    }
                }
                incoming = stream.next() => {
                    if incoming.is_none() {
                        break;
                    }
                }
            }
        }
    })
    .into_response()
}

async fn compartment_search(
    State(app): State<AppState>,
    Path((compartment, id, resource_type)): Path<(String, String, String)>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    let mut args = search_object(params);
    args.insert(
        "_compartment".into(),
        Value::String(format!("{compartment}/{id}")),
    );
    limit_to_patient(&app, &headers, uri.query(), &mut args);
    let lenient = handle_unreadable_values(&headers, &mut args);
    ask_for_default_page(&app, &mut args);
    let response = run_read(
        &app,
        &format!("{resource_type}/search"),
        Value::Object(args),
    )
    .await;
    note_handling(response, lenient)
}

async fn meta_get(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
) -> Response {
    run_read(&app, &format!("{resource_type}/$meta"), json!({"id": id})).await
}

async fn meta_post(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    Query(params): Query<Vec<(String, String)>>,
    body: Bytes,
) -> Response {
    let resource = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let delete = params.iter().any(|(key, _)| key == "_delete");
    let op = if delete { "$meta-delete" } else { "$meta-add" };
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        &format!("{resource_type}/{op}"),
        json!({"id": id, "resource": resource}),
    )
    .await
    {
        Ok(result) => fhir_json(StatusCode::OK, result.value),
        Err(err) => error_response(err),
    }
}

async fn validate_instance(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    Query(params): Query<Vec<(String, String)>>,
    body: Bytes,
) -> Response {
    let mut args = query_object(params);
    args.insert("id".into(), Value::String(id));
    if !body.is_empty() {
        match parse_body(&body) {
            Ok(value) => {
                args.insert("resource".into(), value);
            }
            Err(response) => return response,
        }
    }
    run_read(
        &app,
        &format!("{resource_type}/$validate"),
        Value::Object(args),
    )
    .await
}

async fn everything(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    // The caller's scopes ride with the request, so the answer holds only what they may read.
    let mut args = json!({"id": id});
    stamp_sdc(&app, &headers, &mut args);
    run_read(&app, &format!("{resource_type}/$everything"), args).await
}

async fn document_op(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let mut args = json!({"id": id});
    stamp_sdc(&app, &headers, &mut args);
    // A token that belongs to one patient is held to that patient's record by what the engine
    // follows from the Composition, as it is by what a search finds.
    if let Some(map) = args.as_object_mut() {
        limit_to_patient(&app, &headers, None, map);
    }
    run_read(&app, &format!("{resource_type}/$document"), args).await
}

async fn summary_instance(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(params): Query<Vec<(String, String)>>,
    body: Bytes,
) -> Response {
    summary_call(app, resource_type, Some(id), headers, params, body).await
}

async fn summary_type(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    headers: HeaderMap,
    Query(params): Query<Vec<(String, String)>>,
    body: Bytes,
) -> Response {
    summary_call(app, resource_type, None, headers, params, body).await
}

/// `$health-cards-issue`: the engine picks and shrinks what a card holds, and this signs it.
async fn health_cards_issue(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(key) = app.config.health_cards.clone() else {
        return outcome(
            StatusCode::NOT_IMPLEMENTED,
            "not-supported",
            "this server issues no health cards: no signing key is set",
        );
    };
    if resource_type != "Patient" {
        return outcome(
            StatusCode::BAD_REQUEST,
            "not-supported",
            "$health-cards-issue is a Patient operation",
        );
    }
    let parameters = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let mut args = json!({"id": id, "resource": parameters});
    stamp_sdc(&app, &headers, &mut args);
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        "Patient/$health-cards-issue",
        args,
    )
    .await
    {
        Ok(result) => {
            let base = crate::auth::issuer(&app, &headers);
            match issued_cards(&key, &base, &result.value) {
                Ok(parameters) => fhir_json(StatusCode::OK, parameters),
                Err(message) => outcome(StatusCode::INTERNAL_SERVER_ERROR, "exception", &message),
            }
        }
        Err(err) => error_response(err),
    }
}

/// The response of `$health-cards-issue`: each credential signed, and a `resourceLink` for each
/// resource in it that says where on this server it is. No credential is an empty Parameters.
fn issued_cards(
    key: &crate::health_cards::IssuerKey,
    base: &str,
    material: &Value,
) -> Result<Value, String> {
    let iss = format!("{base}/health-cards");
    let nbf = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let fhir_version = material
        .get("fhirVersion")
        .and_then(|version| version.as_str())
        .unwrap_or("4.0.1");
    let credentials = material
        .get("credentials")
        .and_then(|credentials| credentials.as_array())
        .cloned()
        .unwrap_or_default();
    let mut cards = Vec::new();
    let mut links = Vec::new();
    for (index, credential) in credentials.iter().enumerate() {
        let types: Vec<String> = credential
            .get("types")
            .and_then(|types| types.as_array())
            .map(|types| {
                types
                    .iter()
                    .filter_map(|kind| kind.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let bundle = credential.get("bundle").cloned().unwrap_or(Value::Null);
        cards.push(json!({
            "name": "verifiableCredential",
            "valueString": key.sign(&iss, nbf, &types, fhir_version, &bundle)?
        }));
        for link in credential
            .get("links")
            .and_then(|links| links.as_array())
            .into_iter()
            .flatten()
        {
            let bundled = link.get("bundledResource").and_then(|value| value.as_str());
            let hosted = link.get("hostedResource").and_then(|value| value.as_str());
            if let (Some(bundled), Some(hosted)) = (bundled, hosted) {
                links.push(json!({"name": "resourceLink", "part": [
                    {"name": "vcIndex", "valueInteger": index},
                    {"name": "bundledResource", "valueUri": bundled},
                    {"name": "hostedResource", "valueUri": format!("{base}/{hosted}")}
                ]}));
            }
        }
    }
    let mut parameter = cards;
    parameter.extend(links);
    if parameter.is_empty() {
        return Ok(json!({"resourceType": "Parameters"}));
    }
    Ok(json!({"resourceType": "Parameters", "parameter": parameter}))
}

/// The public key that checks the cards, where a card's `iss` says to look for it.
async fn health_cards_jwks(State(app): State<AppState>) -> Response {
    match &app.config.health_cards {
        Some(key) => axum::Json(key.jwks()).into_response(),
        None => outcome(
            StatusCode::NOT_FOUND,
            "not-found",
            "this server issues no health cards",
        ),
    }
}

/// `$summary`, by `GET` (the patient is the id in the path, or the `identifier` of the query) or
/// by `POST` (the same, or a Parameters body).
async fn summary_call(
    app: AppState,
    resource_type: String,
    id: Option<String>,
    headers: HeaderMap,
    params: Vec<(String, String)>,
    body: Bytes,
) -> Response {
    let mut args = json!({});
    // Only what the operation defines is taken from the query. The server's own controls ride
    // beside the caller's arguments, so a client does not get to name them.
    for (key, value) in params {
        if matches!(key.as_str(), "identifier" | "profile") {
            args[key] = json!(value);
        }
    }
    if !body.is_empty() {
        match parse_body(&body) {
            Ok(value) => args["resource"] = value,
            Err(response) => return response,
        }
    }
    if let Some(id) = id {
        args["id"] = json!(id);
    }
    stamp_sdc(&app, &headers, &mut args);
    run_read(&app, &format!("{resource_type}/$summary"), args).await
}

async fn subsumes_op(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    body: Bytes,
) -> Response {
    terminology_op(app, resource_type, "$subsumes", body).await
}

async fn smart_config(State(app): State<AppState>, headers: HeaderMap) -> Response {
    crate::auth::configuration(&app, &headers)
}

async fn jwks(State(app): State<AppState>) -> Response {
    run_blocking(move || crate::auth::jwks(&app)).await
}

async fn oauth_authorize(
    State(app): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let query = uri.query().map(str::to_string);
    run_blocking(move || crate::auth::authorize(&app, &headers, query.as_deref(), &[])).await
}

async fn oauth_authorize_post(
    State(app): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let query = uri.query().map(str::to_string);
    run_blocking(move || crate::auth::authorize(&app, &headers, query.as_deref(), &body)).await
}

async fn oauth_token(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    run_blocking(move || crate::auth::issue(&app, &headers, &body)).await
}

async fn oauth_introspect(
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    run_blocking(move || crate::auth::introspect(&app, &headers, &body)).await
}

async fn oauth_revoke(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    run_blocking(move || crate::auth::revoke(&app, &headers, &body)).await
}

async fn oauth_register(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    run_blocking(move || crate::auth::register(&app, &headers, &body)).await
}

async fn oauth_manage(State(app): State<AppState>, headers: HeaderMap) -> Response {
    run_blocking(move || crate::auth::manage(&app, &headers)).await
}

async fn oauth_interact(
    State(app): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let session = uri
        .query()
        .and_then(|query| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                if key == "session" {
                    Some(crate::auth::percent_decode(value))
                } else {
                    None
                }
            })
        })
        .unwrap_or_default();
    // Starting an OpenID Connect login fetches provider metadata over blocking HTTP.
    run_blocking(move || crate::auth::interact_get(&app, &headers, &session)).await
}

async fn oauth_oidc_callback(
    State(app): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let query = uri.query().unwrap_or("").to_string();
    run_blocking(move || crate::auth::oidc_callback(&app, &headers, &query)).await
}

/// Run blocking work (database or HTTP calls) off the async worker threads.
async fn run_blocking(work: impl FnOnce() -> Response + Send + 'static) -> Response {
    run_blocking_with(work)
        .await
        .unwrap_or_else(|response| response)
}

async fn run_blocking_with<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Response> {
    // A task-local does not cross into a blocking thread, so the scope goes along with the work.
    let scope = crate::governance::current();
    tokio::task::spawn_blocking(move || crate::governance::in_blocking(scope, work))
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "authorization handler failed",
            )
                .into_response()
        })
}

async fn oauth_interact_post(
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let form = crate::auth::parse_authorize_form(body.as_ref());
    run_blocking(move || crate::auth::interact_post(&app, &headers, &form)).await
}

async fn oauth_ehr_launch(
    State(app): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let query = uri.query().map(str::to_string);
    run_blocking(move || crate::auth::ehr_launch(&app, &headers, query.as_deref())).await
}

async fn oauth_style() -> Response {
    crate::auth::style()
}

async fn graphql_route(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    crate::graphql::handle(&app, &headers, &body).await
}

async fn export_system(State(app): State<AppState>, body: Bytes) -> Response {
    crate::jobs::accept_export(&app, None, None, &body).await
}

async fn export_type(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    body: Bytes,
) -> Response {
    crate::jobs::accept_export(&app, Some(resource_type), None, &body).await
}

async fn export_instance(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    crate::jobs::accept_export(&app, Some(resource_type), Some(id), &body).await
}

async fn export_status(State(app): State<AppState>, Path(job): Path<String>) -> Response {
    crate::jobs::status(&app, &job).await
}

async fn export_file(State(app): State<AppState>, Path(job): Path<String>) -> Response {
    crate::jobs::file(&app, &job).await
}

async fn run_read(app: &AppState, path: &str, args: Value) -> Response {
    match execute(&app.db, &app.catalog, &app.store, path, args).await {
        Ok(result) => fhir_json(StatusCode::OK, result.value),
        Err(err) => error_response(err),
    }
}

/// Say which compartment a request may touch, in the one place the engine reads it. A
/// patient-scoped token's compartment replaces whatever the request had under that name; any other
/// caller has none, and what a request put there is dropped. This is separate from `_compartment`,
/// which the client may set, because the two are combined: a client can narrow a search but never
/// widen it past its token.
fn limit_to_patient(
    app: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
    args: &mut serde_json::Map<String, Value>,
) {
    args.remove(refhir_fhir::PATIENT_COMPARTMENT);
    if let Some(compartment) = crate::auth::patient_compartment(app, headers, query) {
        args.insert(
            refhir_fhir::PATIENT_COMPARTMENT.into(),
            Value::String(compartment),
        );
    }
}

/// Say how a search is to treat a parameter value it cannot read, in the one place the engine
/// reads it. A search is strict (a `400` naming the parameter) unless the request has `Prefer:
/// handling=lenient`, which leaves such a parameter out and says so in the bundle. What the query
/// string had under `_handling` is dropped: the header is the only way to ask. Returns whether
/// the request asked for lenient.
fn handle_unreadable_values(
    headers: &HeaderMap,
    args: &mut serde_json::Map<String, Value>,
) -> bool {
    args.remove(refhir_fhir::HANDLING);
    let lenient = prefers_lenient(headers);
    if lenient {
        args.insert(refhir_fhir::HANDLING.into(), json!("lenient"));
    }
    lenient
}

/// Ask for the server's page size on a search that names no `_count`, so a client that does not
/// ask for a page is not sent every match in one response. The engine applies it, and the Bundle's
/// `self` link shows the `_count` that applied while the audit trail keeps the query as the client
/// sent it. A `_count` the client gave, `_count=0` included, is kept as it is. What the query
/// string had under `_defaultCount` is dropped: only the server's setting says. The engine's
/// `page_by_default` (in the fhir crate) is where the page is applied.
fn ask_for_default_page(app: &AppState, args: &mut serde_json::Map<String, Value>) {
    args.remove(refhir_fhir::DEFAULT_COUNT);
    if app.config.default_page_size > 0 {
        args.insert(
            refhir_fhir::DEFAULT_COUNT.into(),
            Value::String(app.config.default_page_size.to_string()),
        );
    }
}

/// What a search's answer says about `Prefer`, which it depends on: `Vary` for caches, so the
/// lenient answer to a URL is not served to a client that did not ask for it, and, when the
/// preference was honored, `Preference-Applied` (RFC 7240), so a client can tell a server that
/// understood it from one that ignored it.
fn note_handling(mut response: Response, lenient: bool) -> Response {
    response
        .headers_mut()
        .append(header::VARY, header::HeaderValue::from_static("Prefer"));
    if lenient && response.status() == StatusCode::OK {
        response.headers_mut().insert(
            "preference-applied",
            header::HeaderValue::from_static("handling=lenient"),
        );
    }
    response
}

/// Whether the request has `handling=lenient` among its `Prefer` preferences (RFC 7240: a comma
/// separated list of `token[=value][; parameter]`, the value possibly quoted). Anything else, an
/// unknown value included, leaves the search strict.
fn prefers_lenient(headers: &HeaderMap) -> bool {
    headers
        .get_all("prefer")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|preference| preference.split(';').next()?.split_once('='))
        .any(|(name, value)| {
            name.trim().eq_ignore_ascii_case("handling")
                && value
                    .trim()
                    .trim_matches('"')
                    .eq_ignore_ascii_case("lenient")
        })
}

pub(crate) fn stamp_sdc(app: &AppState, headers: &HeaderMap, args: &mut Value) {
    if app.config.sdc_archiver {
        args["_sdcArchiver"] = json!(true);
    }
    if app.config.auth {
        args["_readScopes"] =
            json!(crate::auth::token_scopes(app, headers, None).unwrap_or_default());
    }
}

fn archiver_read(app: &AppState, resource_type: &str) -> Option<Response> {
    if app.config.sdc_archiver && resource_type == "QuestionnaireResponse" {
        Some(outcome(
            StatusCode::FORBIDDEN,
            "forbidden",
            "form archiver does not read QuestionnaireResponse",
        ))
    } else {
        None
    }
}

fn authorize(
    app: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
    method: &str,
    resource_type: &str,
    patient: Option<&str>,
) -> Option<Response> {
    crate::auth::check(app, headers, query, method, resource_type, "", patient)
}

fn parse_body(bytes: &[u8]) -> Result<Value, Response> {
    if bytes.is_empty() {
        return Err(outcome(
            StatusCode::BAD_REQUEST,
            "invalid",
            "request body is required",
        ));
    }
    if bytes.first() == Some(&b'<') {
        let text = std::str::from_utf8(bytes).unwrap_or("");
        return crate::xml::to_json(text)
            .map_err(|err| outcome(StatusCode::BAD_REQUEST, "invalid", &err));
    }
    serde_json::from_slice(bytes).map_err(|err| {
        outcome(
            StatusCode::BAD_REQUEST,
            "invalid",
            &format!("invalid JSON: {err}"),
        )
    })
}

fn prefer_xml(headers: &HeaderMap, params: &[(String, String)]) -> bool {
    if params.iter().any(|(key, value)| {
        key == "_format"
            && (value == "xml" || value.contains("fhir+xml") || value == "application/xml")
    }) {
        return true;
    }
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| {
            let accept = accept.to_ascii_lowercase();
            (accept.contains("fhir+xml")
                || accept.contains("application/xml")
                || accept.contains("text/xml"))
                && !accept.contains("json")
        })
}

fn render(status: StatusCode, value: Value, xml: bool) -> Response {
    if xml {
        let text = crate::xml::from_json(&value);
        return (
            status,
            [(header::CONTENT_TYPE, "application/fhir+xml")],
            text,
        )
            .into_response();
    }
    fhir_json(status, value)
}

fn resource_response(
    status: StatusCode,
    value: Value,
    location: Option<String>,
    xml: bool,
) -> Response {
    let etag = value
        .pointer("/meta/versionId")
        .and_then(|item| item.as_str())
        .map(|version| format!("W/\"{version}\""));
    let mut response = render(status, value, xml);
    let headers = response.headers_mut();
    if let Some(location) = location.and_then(|value| value.parse().ok()) {
        headers.insert(header::LOCATION, location);
    }
    if let Some(etag) = etag.and_then(|value| value.parse().ok()) {
        headers.insert(header::ETAG, etag);
    }
    response
}

pub fn fhir_json_public(status: StatusCode, value: Value) -> Response {
    fhir_json(status, value)
}

fn fhir_json(status: StatusCode, value: Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/fhir+json")],
        Json(value),
    )
        .into_response()
}

pub(crate) fn error_response(err: RunError) -> Response {
    if let RunError::Fhir(FhirError::Outcome(value)) = &err {
        tracing::warn!(diagnostics = %value, "fhir operation failed");
        return fhir_json(StatusCode::BAD_REQUEST, value.clone());
    }
    if let RunError::Fhir(FhirError::Unprocessable(value)) = &err {
        tracing::warn!(diagnostics = %value, "fhir operation failed");
        return fhir_json(StatusCode::UNPROCESSABLE_ENTITY, value.clone());
    }
    if let RunError::Fhir(FhirError::MethodNotAllowed(message)) = &err {
        tracing::warn!(diagnostics = %message, "fhir operation failed");
        let mut response = outcome(StatusCode::METHOD_NOT_ALLOWED, "not-supported", message);
        // RFC 9110 requires an Allow header on a 405. These are the methods that act on an
        // existing instance; creating one with a chosen id is not among them.
        response.headers_mut().insert(
            header::ALLOW,
            axum::http::HeaderValue::from_static("GET, HEAD, PATCH, DELETE"),
        );
        return response;
    }
    let (status, code, diagnostics) = match &err {
        RunError::Fhir(fhir) => (fhir_status(fhir), outcome_code(fhir), fhir.to_string()),
        RunError::Commit(CommitError::Occ(_)) => (
            StatusCode::CONFLICT,
            "conflict",
            "OCC retries exhausted".into(),
        ),
        RunError::Commit(CommitError::ReadOnly) => {
            return crate::cluster::read_only_response(crate::cluster::READ_ONLY_MESSAGE)
        }
        RunError::Commit(other) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "exception",
            other.to_string(),
        ),
    };
    tracing::warn!(
        status = status.as_u16(),
        code,
        diagnostics = %diagnostics,
        "fhir operation failed"
    );
    outcome(status, code, &diagnostics)
}

/// The HTTP status `error_response` answers with.
pub(crate) fn run_error_status(err: &RunError) -> StatusCode {
    match err {
        RunError::Fhir(fhir) => fhir_status(fhir),
        RunError::Commit(CommitError::Occ(_)) => StatusCode::CONFLICT,
        RunError::Commit(CommitError::ReadOnly) => StatusCode::SERVICE_UNAVAILABLE,
        RunError::Commit(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn other_read_only() -> String {
    crate::cluster::READ_ONLY_MESSAGE.to_string()
}

pub(crate) fn outcome_value(err: &RunError) -> Value {
    if let RunError::Fhir(FhirError::Outcome(value) | FhirError::Unprocessable(value)) = err {
        return value.clone();
    }
    let (code, diagnostics) = match err {
        RunError::Fhir(fhir) => (outcome_code(fhir), fhir.to_string()),
        RunError::Commit(CommitError::Occ(_)) => ("conflict", "OCC retries exhausted".into()),
        RunError::Commit(CommitError::ReadOnly) => ("transient", other_read_only()),
        RunError::Commit(other) => ("exception", other.to_string()),
    };
    json!({
        "resourceType": "OperationOutcome",
        "issue": [{
            "severity": "error",
            "code": code,
            "diagnostics": diagnostics
        }]
    })
}

fn outcome(status: StatusCode, code: &str, diagnostics: &str) -> Response {
    fhir_json(
        status,
        json!({
            "resourceType": "OperationOutcome",
            "issue": [{
                "severity": "error",
                "code": code,
                "diagnostics": diagnostics
            }]
        }),
    )
}

fn fhir_status(err: &FhirError) -> StatusCode {
    match err {
        FhirError::NotFound(_) | FhirError::UnknownResource(_) => StatusCode::NOT_FOUND,
        FhirError::Gone(_) => StatusCode::GONE,
        FhirError::Conflict(_) => StatusCode::CONFLICT,
        FhirError::Precondition(_) => StatusCode::PRECONDITION_FAILED,
        FhirError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        FhirError::Forbidden(_) => StatusCode::FORBIDDEN,
        FhirError::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
        FhirError::MethodNotAllowed(_) => StatusCode::METHOD_NOT_ALLOWED,
        FhirError::Invalid(_) | FhirError::Outcome(_) | FhirError::Unsupported(_) => {
            StatusCode::BAD_REQUEST
        }
    }
}

#[cfg(test)]
mod query_redaction {
    use super::redact_query;

    #[test]
    fn access_token_is_not_logged() {
        assert_eq!(
            redact_query(Some("active=true&access_token=secret")).as_deref(),
            Some("active=true&access_token=REDACTED")
        );
        assert_eq!(
            redact_query(Some("active=true")).as_deref(),
            Some("active=true")
        );
        assert_eq!(redact_query(None), None);
    }
}
