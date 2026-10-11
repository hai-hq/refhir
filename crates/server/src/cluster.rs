//! What a process needs to run next to others: `/healthz`, the commit-timestamp headers,
//! and forwarding from a replica to the primary.
//!
//! A replica cannot write, but a client should not have to know that. When a request would
//! write (the commit fails with `CommitError::ReadOnly`) or needs data the replica has not
//! caught up to (`X-Refhir-Min-Ts`), the replica sends the same request to the primary and
//! returns its answer, so the load balancer only has to spread traffic.

use std::cell::Cell;
use std::collections::HashMap;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use refhir_types::Timestamp;
use serde_json::{json, Value};

use crate::http::AppState;

/// Response header: the timestamp of the commit (for a write) or of the snapshot (for a read).
/// A client sends the largest value it has seen back as `MIN_TS`.
pub const COMMIT_TS: &str = "x-refhir-commit-ts";
/// Request header: do not answer from a snapshot older than this timestamp.
pub const MIN_TS: &str = "x-refhir-min-ts";
/// Marks a response that says "this process cannot do that, the primary can".
pub const READ_ONLY: &str = "x-refhir-read-only";
/// Response header: which kind of process answered. A forwarded answer says `primary`.
pub const ROLE: &str = "x-refhir-role";
/// Set on a request a replica forwarded, so a misconfigured pair cannot bounce it forever.
pub const FORWARDED: &str = "x-refhir-forwarded";

/// How much of a request body a replica buffers so it can replay it.
const MAX_BUFFERED_BODY: usize = 128 * 1024 * 1024;

tokio::task_local! {
    /// The highest timestamp an operation reported while serving this request.
    static REPORTED_TS: Cell<u64>;
}

/// Called by `execute` with the timestamp an operation ran at or committed at.
pub(crate) fn report_ts(ts: Timestamp) {
    let _ = REPORTED_TS.try_with(|cell| cell.set(cell.get().max(ts.as_u64())));
}

fn header_name(name: &'static str) -> HeaderName {
    HeaderName::from_static(name)
}

/// The response for a write this process cannot perform.
pub(crate) fn read_only_response(diagnostics: &str) -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "application/fhir+json")],
        json!({
            "resourceType": "OperationOutcome",
            "issue": [{
                "severity": "error",
                "code": "transient",
                "diagnostics": diagnostics
            }]
        })
        .to_string(),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header_name(READ_ONLY), HeaderValue::from_static("1"));
    response
}

/// A replica that has not reached the timestamp a caller asked for and cannot ask the primary.
fn not_caught_up_response(min: u64) -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "application/fhir+json")],
        json!({
            "resourceType": "OperationOutcome",
            "issue": [{
                "severity": "error",
                "code": "transient",
                "diagnostics": format!(
                    "this replica has not reached timestamp {min} yet (X-Refhir-Min-Ts) and has \
                     no --primary-url to ask; retry shortly"
                )
            }]
        })
        .to_string(),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

pub(crate) const READ_ONLY_MESSAGE: &str =
    "this server is a read-only replica; send writes to the primary";

/// `/healthz`. A primary is healthy while it runs. A replica is healthy while it keeps
/// refreshing; one that has not refreshed within `max_staleness` is out of rotation.
pub(crate) async fn healthz(
    State(app): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let verbose = query.contains_key("verbose");
    let replica = app.db.is_read_only();
    let mut healthy = true;
    let mut body = json!({
        "role": if replica { "replica" } else { "primary" },
        "latest_ts": app.db.latest_ts().to_string(),
    });
    if replica {
        let age = app.db.refreshed_ago();
        let stale = age.is_none_or(|age| age > app.config.max_staleness);
        healthy = !stale;
        body["refreshed_ms_ago"] = match age {
            Some(age) => json!(age.as_millis() as u64),
            None => Value::Null,
        };
        // How far behind the writer's watermark, in timestamps. Read in a blocking task: it
        // is a database query.
        let db = app.db.clone();
        let lag = tokio::task::spawn_blocking(move || db.published_lag()).await;
        if let Ok(Ok(Some(lag))) = lag {
            body["lag_ts"] = json!(lag);
        }
    }
    body["status"] = json!(if healthy { "ok" } else { "unhealthy" });
    if verbose {
        let stats = app.db.stats();
        body["commits"] = json!(stats.commits);
        body["occ_conflicts"] = json!(stats.occ_conflicts);
        body["refreshes"] = json!(stats.refreshes);
        body["time_ns"] = json!({
            "validate": stats.validate_ns,
            "persist": stats.persist_ns,
            "publish_wait": stats.publish_wait_ns,
            "watermark": stats.watermark_ns,
        });
        if let Some(pool) = stats.pool {
            body["pool"] = json!({
                "max": pool.max,
                "open": pool.open,
                "in_use": pool.in_use,
                "peak_in_use": pool.peak_in_use,
                "acquires": pool.acquires,
                "waits": pool.waits,
                "wait_ns": pool.wait_ns,
            });
        }
    }
    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// What a process serves while it waits to become the primary, or for a replica's schema and
/// catalog to appear: `/healthz` says 503 and nothing else is available.
pub fn bootstrap_router(role: &'static str, detail: &'static str) -> Router {
    let answer = move || async move {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::RETRY_AFTER, "2"),
            ],
            json!({"status": "starting", "role": role, "detail": detail}).to_string(),
        )
    };
    Router::new()
        .route("/healthz", get(answer))
        .fallback(answer)
}

/// A running bootstrap server. Stop it right before the real server takes over the port.
pub struct Bootstrap {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Bootstrap {
    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if tokio::time::timeout(Duration::from_secs(3), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
        }
    }
}

/// Serves `bootstrap_router` on `listener`, a clone of the socket the real server will use.
/// The kernel keeps the socket's backlog while this stops and the real server starts, so no
/// connection is refused in between.
pub fn serve_bootstrap(
    listener: std::net::TcpListener,
    role: &'static str,
    detail: &'static str,
) -> std::io::Result<Bootstrap> {
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let router = bootstrap_router(role, detail);
    let task = tokio::spawn(async move {
        let served = axum::serve(listener, router).with_graceful_shutdown(async move {
            let _ = stopped.await;
        });
        if let Err(err) = served.await {
            tracing::warn!("bootstrap server stopped: {err}");
        }
    });
    Ok(Bootstrap {
        stop: Some(stop),
        task,
    })
}

/// Innermost layer. Reports the commit timestamp, makes a replica wait for `X-Refhir-Min-Ts`,
/// and sends what a replica cannot do to the primary.
pub(crate) async fn cluster_layer(
    State(app): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if path == "/healthz" || path == "/sync" {
        return next.run(request).await;
    }
    let replica = app.db.is_read_only();
    let role = if replica { "replica" } else { "primary" };
    let min_ts = request
        .headers()
        .get(header_name(MIN_TS))
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.trim().parse::<u64>().ok());

    if !replica {
        return finish(
            REPORTED_TS
                .scope(Cell::new(0), run_reporting(request, next))
                .await,
            role,
        );
    }

    // Replica: buffer the request so it can be replayed on the primary.
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BUFFERED_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
    };
    let (method, uri, headers) = (
        parts.method.clone(),
        parts.uri.clone(),
        parts.headers.clone(),
    );
    let forwardable = app.config.primary_url.is_some() && !headers.contains_key(FORWARDED);

    if let Some(min) = min_ts {
        let reached = Timestamp::from_u64(min)
            .map(|min| app.db.latest_ts() >= min)
            .unwrap_or(true);
        if !reached {
            let db = app.db.clone();
            let wait = app.config.min_ts_wait;
            let caught_up = tokio::task::spawn_blocking(move || {
                Timestamp::from_u64(min)
                    .map(|min| db.wait_for_ts(min, wait))
                    .unwrap_or(true)
            })
            .await
            .unwrap_or(false);
            if !caught_up && forwardable {
                tracing::debug!(min_ts = min, "replica behind, forwarding to the primary");
                return forward(&app, method, uri, headers, bytes).await;
            }
            if !caught_up {
                // Nowhere to send it, and an older answer is exactly what the caller asked not
                // to get: say so, and let it retry.
                return not_caught_up_response(min);
            }
        }
    }

    let local = Request::from_parts(parts, Body::from(bytes.clone()));
    let response = finish(
        REPORTED_TS
            .scope(Cell::new(0), run_reporting(local, next))
            .await,
        role,
    );
    if response.headers().contains_key(READ_ONLY) && forwardable {
        return forward(&app, method, uri, headers, bytes).await;
    }
    response
}

/// Runs the inner service and attaches the timestamp it reported, if any.
async fn run_reporting(request: Request<Body>, next: Next) -> (Response, u64) {
    let response = next.run(request).await;
    let reported = REPORTED_TS.try_with(|cell| cell.get()).unwrap_or(0);
    (response, reported)
}

fn finish((mut response, reported): (Response, u64), role: &'static str) -> Response {
    response
        .headers_mut()
        .insert(header_name(ROLE), HeaderValue::from_static(role));
    if reported > 0 && !response.headers().contains_key(COMMIT_TS) {
        if let Ok(value) = HeaderValue::from_str(&reported.to_string()) {
            response.headers_mut().insert(header_name(COMMIT_TS), value);
        }
    }
    response
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    )
}

fn forward_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            // A redirect is the primary's answer, not something to chase for the client.
            .redirects(0)
            .timeout(Duration::from_secs(120))
            .build()
    })
}

/// Replays a request on the primary and returns what it says.
pub(crate) async fn forward(
    app: &AppState,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(base) = app.config.primary_url.clone() else {
        return read_only_response(READ_ONLY_MESSAGE);
    };
    let target = format!(
        "{}{}",
        base.trim_end_matches('/'),
        uri.path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/")
    );
    let outcome = tokio::task::spawn_blocking(move || {
        let mut request = forward_agent().request(method.as_str(), &target);
        for (name, value) in &headers {
            let name = name.as_str();
            if is_hop_by_hop(name) || name == MIN_TS || name == FORWARDED {
                continue;
            }
            if let Ok(value) = value.to_str() {
                request = request.set(name, value);
            }
        }
        request = request.set(FORWARDED, "1");
        let result = if body.is_empty() && matches!(method, Method::GET | Method::HEAD) {
            request.call()
        } else {
            request.send_bytes(&body)
        };
        // A 4xx or 5xx from the primary is still its answer.
        match result {
            Ok(response) | Err(ureq::Error::Status(_, response)) => Ok(response),
            Err(err) => Err(err.to_string()),
        }
    })
    .await;
    let response = match outcome {
        Ok(Ok(response)) => response,
        Ok(Err(message)) => return bad_gateway(&message),
        Err(_) => return bad_gateway("forwarding task stopped"),
    };
    let status = StatusCode::from_u16(response.status()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut headers = HeaderMap::new();
    for name in response.headers_names() {
        if is_hop_by_hop(&name.to_ascii_lowercase()) {
            continue;
        }
        let Ok(header) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        for value in response.all(&name) {
            if let Ok(value) = HeaderValue::from_str(value) {
                headers.append(header.clone(), value);
            }
        }
    }
    let mut bytes = Vec::new();
    {
        use std::io::Read;
        let mut reader = response.into_reader();
        if let Err(err) = reader.read_to_end(&mut bytes) {
            return bad_gateway(&format!("reading the primary's response: {err}"));
        }
    }
    let mut out = Response::new(Body::from(bytes));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

/// `http://host:port` becomes `ws://host:port/sync`.
pub(crate) fn sync_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    let socket = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{socket}/sync")
}

fn bad_gateway(message: &str) -> Response {
    tracing::warn!("could not reach the primary: {message}");
    (
        StatusCode::BAD_GATEWAY,
        [(header::CONTENT_TYPE, "application/fhir+json")],
        json!({
            "resourceType": "OperationOutcome",
            "issue": [{
                "severity": "error",
                "code": "transient",
                "diagnostics": format!("could not reach the primary: {message}")
            }]
        })
        .to_string(),
    )
        .into_response()
}
