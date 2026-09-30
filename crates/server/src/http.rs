//! HTTP FHIR mapped onto the operation registry.
//! The router does not implement a second engine.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use haifhir_database::{CommitError, Database};
use haifhir_fhir::{outcome_code, FhirError, RunError, RunResult};
use serde_json::{json, Value};

use crate::sync;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Database>,
}

pub fn app(db: Arc<Database>) -> Router {
    Router::new()
        .route("/metadata", get(metadata))
        .route("/sync", get(sync_ws))
        .route("/", post(bundle_post))
        .route("/:resource_type", get(search).post(create))
        .route("/:resource_type/:id/_history/:vid", get(vread))
        .route("/:resource_type/:id/_history", get(instance_history))
        .route("/:resource_type/:id", get(read).put(update).delete(remove))
        .with_state(AppState { db })
}

pub async fn execute(db: &Arc<Database>, path: &str, args: Value) -> Result<RunResult, RunError> {
    let db = db.clone();
    let path = path.to_string();
    match tokio::task::spawn_blocking(move || haifhir_fhir::run(&db, &path, &args)).await {
        Ok(result) => result,
        Err(_) => Err(RunError::Commit(CommitError::Invalid(
            "operation worker stopped".into(),
        ))),
    }
}

async fn metadata() -> Response {
    fhir_json(StatusCode::OK, haifhir_fhir::capability_statement())
}

async fn sync_ws(ws: WebSocketUpgrade, State(app): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| sync::handle_socket(socket, app.db))
}

async fn bundle_post(State(app): State<AppState>, body: Bytes) -> Response {
    let resource = match parse_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    match execute(&app.db, "Bundle/transaction", json!({"resource": resource})).await {
        Ok(result) => fhir_json(StatusCode::OK, result.value),
        Err(err) => error_response(err),
    }
}

async fn create(
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
        &format!("{resource_type}/create"),
        json!({"resource": resource}),
    )
    .await
    {
        Ok(result) => {
            let id = result.value["id"].as_str().unwrap_or("");
            let location = format!("{resource_type}/{id}/_history/{}", result.ts);
            resource_response(StatusCode::CREATED, result.value, Some(location))
        }
        Err(err) => error_response(err),
    }
}

async fn read(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
) -> Response {
    match execute(&app.db, &format!("{resource_type}/read"), json!({"id": id})).await {
        Ok(result) => resource_response(StatusCode::OK, result.value, None),
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
    match execute(&app.db, &format!("{resource_type}/update"), args).await {
        Ok(result) => {
            let location = format!("{resource_type}/{id}/_history/{}", result.ts);
            resource_response(StatusCode::OK, result.value, Some(location))
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
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let mut args = serde_json::Map::new();
    for (key, value) in params {
        args.insert(key, Value::String(value));
    }
    match execute(
        &app.db,
        &format!("{resource_type}/search"),
        Value::Object(args),
    )
    .await
    {
        Ok(result) => fhir_json(StatusCode::OK, result.value),
        Err(err) => error_response(err),
    }
}

async fn instance_history(
    State(app): State<AppState>,
    Path((resource_type, id)): Path<(String, String)>,
) -> Response {
    match execute(
        &app.db,
        &format!("{resource_type}/history"),
        json!({"id": id}),
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
        &format!("{resource_type}/vread"),
        json!({"id": id, "versionId": vid}),
    )
    .await
    {
        Ok(result) => resource_response(StatusCode::OK, result.value, None),
        Err(err) => error_response(err),
    }
}

fn parse_body(bytes: &[u8]) -> Result<Value, Response> {
    if bytes.is_empty() {
        return Err(outcome(
            StatusCode::BAD_REQUEST,
            "invalid",
            "request body is required",
        ));
    }
    serde_json::from_slice(bytes).map_err(|err| {
        outcome(
            StatusCode::BAD_REQUEST,
            "invalid",
            &format!("invalid JSON: {err}"),
        )
    })
}

fn resource_response(status: StatusCode, value: Value, location: Option<String>) -> Response {
    let etag = value
        .pointer("/meta/versionId")
        .and_then(|item| item.as_str())
        .map(|version| format!("W/\"{version}\""));
    let mut response = fhir_json(status, value);
    let headers = response.headers_mut();
    if let Some(location) = location.and_then(|value| value.parse().ok()) {
        headers.insert(header::LOCATION, location);
    }
    if let Some(etag) = etag.and_then(|value| value.parse().ok()) {
        headers.insert(header::ETAG, etag);
    }
    response
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
    outcome(status, code, &diagnostics)
}

pub(crate) fn outcome_value(err: &RunError) -> Value {
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

fn fhir_status(err: &FhirError) -> StatusCode {
    match err {
        FhirError::NotFound(_) | FhirError::UnknownResource(_) => StatusCode::NOT_FOUND,
        FhirError::Gone(_) => StatusCode::GONE,
        FhirError::Conflict(_) => StatusCode::CONFLICT,
        FhirError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        FhirError::Invalid(_) | FhirError::Unsupported(_) => StatusCode::BAD_REQUEST,
    }
}
