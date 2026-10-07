//! HTTP FHIR mapped onto the operation registry.
//! The router does not implement a second engine.

use std::sync::{Arc, RwLock};

use axum::body::{Body, Bytes};
use axum::extract::ws::Message;
use axum::extract::{Extension, Path, Query, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, Request, StatusCode};
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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            auth: false,
            export_dir: "export".into(),
            terminology_urls: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Database>,
    pub catalog: Arc<RwLock<Catalog>>,
    pub store: Arc<dyn ConformanceStore>,
    pub config: ServerConfig,
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
    let router = Router::new()
        .route("/metadata", get(metadata))
        .route("/.well-known/smart-configuration", get(smart_config))
        .route("/oauth/token", post(oauth_token))
        .route("/graphql", post(graphql_route))
        .route("/sync", get(sync_ws))
        .route("/_history", get(system_history))
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
        .route("/:resource_type/$expand", post(expand_op))
        .route("/:resource_type/$validate-code", post(validate_code_op))
        .route("/:resource_type/$lookup", post(lookup_op))
        .route("/:resource_type/$translate", post(translate_op))
        .route("/:resource_type/$subsumes", post(subsumes_op))
        .route("/:resource_type/$export", post(export_type))
        .route("/:resource_type/:id/_history/:vid", get(vread))
        .route("/:resource_type/:id/_history", get(instance_history))
        .route("/:resource_type/:id/$meta", get(meta_get).post(meta_post))
        .route("/:resource_type/:id/$validate", post(validate_instance))
        .route("/:resource_type/:id/$everything", get(everything))
        .route("/:resource_type/:id/$document", get(document_op))
        .route("/:resource_type/:id/$websocket", get(subscription_socket))
        .route("/:resource_type/:id/$export", post(export_instance))
        .route(
            "/:resource_type/:id",
            get(read).put(update).delete(remove).patch(patch_op),
        )
        .route("/:resource_type/:id/:compartment", get(compartment_search));
    let state = AppState {
        db: db.clone(),
        catalog: catalog.clone(),
        store: store.clone(),
        config: config.clone(),
    };
    let router = router
        .layer(middleware::from_fn(request_log))
        .layer(middleware::from_fn_with_state(state.clone(), smart_guard))
        .with_state(state);
    crate::jobs::attach(db, catalog, store, &config);
    router
}

pub async fn execute(
    db: &Arc<Database>,
    catalog: &Arc<RwLock<Catalog>>,
    store: &Arc<dyn ConformanceStore>,
    path: &str,
    args: Value,
) -> Result<RunResult, RunError> {
    let db = db.clone();
    let catalog = catalog.clone();
    let store = store.clone();
    let path = path.to_string();
    match tokio::task::spawn_blocking(move || {
        refhir_fhir::commit_operation(store.as_ref(), &db, &catalog, &path, &args)
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(RunError::Commit(CommitError::Invalid(
            "operation worker stopped".into(),
        ))),
    }
}

async fn metadata(State(app): State<AppState>) -> Response {
    let mut statement = app.catalog.read().expect("catalog").capability_statement();
    if app.config.auth {
        if let Some(rest) = statement
            .pointer_mut("/rest/0")
            .and_then(|value| value.as_object_mut())
        {
            rest.insert(
                "security".into(),
                json!({
                    "service": [{"coding": [{"system": "http://terminology.hl7.org/CodeSystem/restful-security-service", "code": "SMART-on-FHIR"}]}],
                    "extension": [{
                        "url": "http://fhir-registry.smarthealthit.org/StructureDefinition/oauth-uris",
                        "extension": [
                            {"url": "authorize", "valueUri": "/oauth/authorize"},
                            {"url": "token", "valueUri": "/oauth/token"}
                        ]
                    }]
                }),
            );
        }
    }
    fhir_json(StatusCode::OK, statement)
}

async fn request_log(request: Request<Body>, next: Next) -> Response {
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

fn redact_query(query: Option<&str>) -> Option<String> {
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

async fn smart_guard(
    State(app): State<AppState>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if app.config.auth {
        let path = request.uri().path().to_string();
        let open =
            path == "/metadata" || path.starts_with("/.well-known") || path.starts_with("/oauth/");
        if !open {
            let method = request.method().as_str().to_string();
            let query = request.uri().query().map(str::to_string);
            if path == "/sync" {
                match crate::auth::open_sync_session(&app, request.headers(), query.as_deref()) {
                    Ok(session) => {
                        request.extensions_mut().insert(session);
                    }
                    Err(response) => return response,
                }
            } else {
                let resource = fhir_type_from_path(&path);
                if let Some(response) = crate::auth::check(
                    &app,
                    request.headers(),
                    query.as_deref(),
                    &method,
                    &resource,
                    None,
                ) {
                    return response;
                }
                let patient =
                    crate::auth::patient_compartment(&app, request.headers(), query.as_deref());
                if let Some(patient) = patient {
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
    let Some((operation, args)) = operation else {
        return None;
    };
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
    session: Option<Extension<crate::auth::SyncSession>>,
) -> Response {
    if app.config.auth && session.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let session = session.map(|Extension(session)| session);
    ws.on_upgrade(move |socket| {
        sync::handle_socket(socket, app.db, app.catalog, app.store, session)
    })
    .into_response()
}

async fn bundle_post(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let resource = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let path = if resource.get("type").and_then(|value| value.as_str()) == Some("batch") {
        "Bundle/batch"
    } else {
        "Bundle/transaction"
    };
    match execute(
        &app.db,
        &app.catalog,
        &app.store,
        path,
        json!({"resource": resource}),
    )
    .await
    {
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
    let (path, args) = if let Some(query) = if_none_exist {
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

async fn expand_op(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    body: Bytes,
) -> Response {
    terminology_op(app, resource_type, "$expand", body).await
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

async fn terminology_op(app: AppState, resource_type: String, op: &str, body: Bytes) -> Response {
    let args = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
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
            resource_response(
                StatusCode::OK,
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
    if let Some(response) = authorize(&app, &headers, uri.query(), "GET", &resource_type, None) {
        return response;
    }
    let xml = prefer_xml(&headers, &params);
    let mut args = query_object(params);
    args.remove("access_token");
    if let Some(compartment) = crate::auth::patient_compartment(&app, &headers, uri.query()) {
        args.entry("_compartment".to_string())
            .or_insert(Value::String(compartment));
    }
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
        Ok(result) => render(StatusCode::OK, result.value, xml),
        Err(err) => error_response(err),
    }
}

async fn instance_history(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
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

async fn type_history(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
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
    let query = query_object(params)
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
    let query = query_object(params)
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
    Path((_resource_type, id)): Path<(String, String)>,
) -> impl IntoResponse {
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
}

async fn compartment_search(
    State(app): State<AppState>,
    Path((compartment, id, resource_type)): Path<(String, String, String)>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    let mut args = query_object(params);
    args.insert(
        "_compartment".into(),
        Value::String(format!("{compartment}/{id}")),
    );
    run_read(
        &app,
        &format!("{resource_type}/search"),
        Value::Object(args),
    )
    .await
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
) -> Response {
    run_read(
        &app,
        &format!("{resource_type}/$everything"),
        json!({"id": id}),
    )
    .await
}

async fn document_op(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
) -> Response {
    run_read(
        &app,
        &format!("{resource_type}/$document"),
        json!({"id": id}),
    )
    .await
}

async fn subsumes_op(
    State(app): State<AppState>,
    Path(resource_type): Path<String>,
    body: Bytes,
) -> Response {
    terminology_op(app, resource_type, "$subsumes", body).await
}

async fn smart_config(State(app): State<AppState>) -> Response {
    crate::auth::configuration(&app)
}

async fn oauth_token(State(app): State<AppState>, body: Bytes) -> Response {
    crate::auth::issue(&app, &body)
}

async fn graphql_route(State(app): State<AppState>, body: Bytes) -> Response {
    crate::graphql::handle(&app, &body).await
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
    crate::jobs::status(&app, &job)
}

async fn export_file(State(app): State<AppState>, Path(job): Path<String>) -> Response {
    crate::jobs::file(&app, &job)
}

async fn run_read(app: &AppState, path: &str, args: Value) -> Response {
    match execute(&app.db, &app.catalog, &app.store, path, args).await {
        Ok(result) => fhir_json(StatusCode::OK, result.value),
        Err(err) => error_response(err),
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
    crate::auth::check(app, headers, query, method, resource_type, patient)
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
    let (status, code, diagnostics) = match &err {
        RunError::Fhir(fhir) => (fhir_status(fhir), outcome_code(fhir), fhir.to_string()),
        RunError::Commit(CommitError::Occ(_)) => (
            StatusCode::CONFLICT,
            "conflict",
            "OCC retries exhausted".into(),
        ),
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

pub(crate) fn outcome_value(err: &RunError) -> Value {
    if let RunError::Fhir(FhirError::Outcome(value)) = err {
        return value.clone();
    }
    let (code, diagnostics) = match err {
        RunError::Fhir(fhir) => (outcome_code(fhir), fhir.to_string()),
        RunError::Commit(CommitError::Occ(_)) => ("conflict", "OCC retries exhausted".into()),
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

fn fhir_status(err: &FhirError) -> StatusCode {
    match err {
        FhirError::NotFound(_) | FhirError::UnknownResource(_) => StatusCode::NOT_FOUND,
        FhirError::Gone(_) => StatusCode::GONE,
        FhirError::Conflict(_) => StatusCode::CONFLICT,
        FhirError::Precondition(_) => StatusCode::PRECONDITION_FAILED,
        FhirError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        FhirError::Invalid(_) | FhirError::Outcome(_) | FhirError::Unsupported(_) => {
            StatusCode::BAD_REQUEST
        }
    }
}
