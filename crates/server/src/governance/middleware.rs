//! The two layers around authentication.
//!
//! `govern` is outermost. It works out the request id and the client's address, opens the
//! [`RequestScope`] that everything inside can read, and when the response comes back, counts
//! and logs the request. `gate` is innermost, after `smart_guard` has authenticated the caller,
//! so that the limits it applies are the caller's own.
//!
//! ```text
//! cors → govern ──┬→ smart_guard → gate → handler
//!                 └ turns away an address that keeps failing to authenticate,
//!                   before its credential is looked up
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, HttpBody};
use axum::extract::State;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use http_body_util::BodyExt;

use crate::http::AppState;

use super::scope::{current, within, RequestScope, Source};
use super::{classify, set_header, set_rate_headers, Finished};

pub(crate) async fn govern(
    State(app): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let governance = app.governance.clone();
    if !governance.active() || is_open_probe(request.uri().path(), request.uri().query()) {
        return next.run(request).await;
    }
    let started = Instant::now();
    let method = request.method().as_str().to_string();
    let path = request.uri().path().to_string();
    let query = request.uri().query().map(str::to_string);
    let class = classify(&method, &path);
    let remote_ip = governance.client_ip(&request);
    let scope = RequestScope::new(
        governance.clone(),
        governance.request_id(request.headers()),
        Source::Http,
        remote_ip,
    );
    let declared = declared_length(request.headers());
    // A chunked upload declares no length, so what the handler reads is counted as it is read.
    let read = Arc::new(AtomicU64::new(0));
    let request = count_body(request, read.clone());
    let attempts = tries_credential(&app, request.headers(), &path, query.as_deref());

    // An address that has failed to authenticate too often is not allowed to try another
    // credential until its failures have aged out, and the credential is not looked up
    // meanwhile: a guessed token costs a database read. A token the server already knows is
    // let through, so that one address misbehaving does not lock out the rest of a network.
    let refused = if attempts {
        governance.failures_exhausted(remote_ip)
    } else {
        None
    };
    let mut response = match refused {
        Some(rejection) => {
            scope.mark_throttled();
            rejection.into_response()
        }
        None => within(scope.clone(), next.run(request)).await,
    };
    if attempts && failed_attempt(&path, response.status()) && !scope.throttled() {
        governance.note_auth_failure(remote_ip);
    }

    let headers = response.headers_mut();
    set_header(headers, "x-request-id", &scope.request_id);
    if let Some(decision) = scope.decision() {
        set_rate_headers(headers, &decision);
    }
    governance.finish(
        &scope,
        &Finished {
            method: &method,
            path: &path,
            query: query.as_deref(),
            class,
            status: response.status().as_u16(),
            bytes_in: declared.max(read.load(Ordering::Relaxed)),
            bytes_out: response_length(&response),
            elapsed: started.elapsed(),
        },
    );
    response
}

/// Apply the caller's limits. It sits inside `smart_guard`, so a request that reaches it has
/// been authenticated and its identity is on the scope.
pub(crate) async fn gate(
    State(app): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let Some(scope) = current() else {
        return next.run(request).await;
    };
    let class = classify(request.method().as_str(), request.uri().path());
    match app.governance.admit_once(&scope, class).await {
        Ok(()) => next.run(request).await,
        Err(rejection) => rejection.into_response(),
    }
}

/// A load balancer's health probe: `/healthz` without `verbose`. It is not a caller. A probe that
/// the limiter refused would take a healthy server out of rotation, and one that was logged,
/// metered and audited would be most of every trail. `/livez` and `/readyz` are added after
/// every layer and never reach this one; this keeps `/healthz` as they are. Its `verbose` form
/// shows commit counts and takes a token, and is governed like any other request.
fn is_open_probe(path: &str, query: Option<&str>) -> bool {
    path == "/healthz"
        && !query.is_some_and(|query| {
            query
                .split('&')
                .any(|pair| pair.split('=').next() == Some("verbose"))
        })
}

/// Whether the request presents a credential at all.
fn carries_credentials(headers: &HeaderMap, query: Option<&str>) -> bool {
    headers.contains_key(header::AUTHORIZATION)
        || query.is_some_and(|query| {
            query
                .split('&')
                .any(|pair| pair.split('=').next() == Some("access_token"))
        })
}

/// The endpoints that take a client's credentials in the request and check them.
fn credential_endpoint(path: &str) -> bool {
    matches!(
        path,
        "/oauth/token" | "/oauth/introspect" | "/oauth/revoke" | "/oauth/register"
    )
}

/// Whether this request tries a credential that costs something to check: a token that is not
/// in memory, which is a database read, or a client secret at the OAuth endpoints, which is
/// that and a key derivation. A request that presents none, or one the server already knows,
/// is not a guess.
fn tries_credential(app: &AppState, headers: &HeaderMap, path: &str, query: Option<&str>) -> bool {
    if !app.config.auth {
        return false;
    }
    credential_endpoint(path)
        || (carries_credentials(headers, query)
            && !crate::auth::lookup_is_cheap(app, headers, query))
}

/// Whether the answer to an attempt says it failed. A 401 anywhere; at the OAuth endpoints also
/// the 400 and 403 that a wrong client secret or registration token is answered with.
fn failed_attempt(path: &str, status: StatusCode) -> bool {
    status == StatusCode::UNAUTHORIZED
        || (credential_endpoint(path)
            && matches!(status, StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN))
}

/// Count the bytes read from the request body into `counted`.
fn count_body(request: Request<Body>, counted: Arc<AtomicU64>) -> Request<Body> {
    let (parts, body) = request.into_parts();
    let body = Body::new(body.map_frame(move |frame| {
        if let Some(data) = frame.data_ref() {
            counted.fetch_add(data.len() as u64, Ordering::Relaxed);
        }
        frame
    }));
    Request::from_parts(parts, body)
}

fn declared_length(headers: &HeaderMap) -> u64 {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// The size of the answer, where it is known without reading it: hyper writes
/// `Content-Length` only after this layer has run, so it is the body's own size that is asked.
fn response_length(response: &Response) -> u64 {
    response
        .body()
        .size_hint()
        .exact()
        .or_else(|| {
            response
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_counts_as_a_failed_attempt_depends_on_where_it_was_made() {
        assert!(failed_attempt("/Patient", StatusCode::UNAUTHORIZED));
        assert!(
            !failed_attempt("/Patient", StatusCode::FORBIDDEN),
            "a known caller out of scope"
        );
        assert!(!failed_attempt("/Patient", StatusCode::BAD_REQUEST));
        assert!(failed_attempt("/oauth/token", StatusCode::BAD_REQUEST));
        assert!(failed_attempt("/oauth/register", StatusCode::FORBIDDEN));
        assert!(!failed_attempt("/oauth/token", StatusCode::OK));
        assert!(
            !failed_attempt("/oauth/authorize", StatusCode::BAD_REQUEST),
            "a browser page"
        );
    }

    #[test]
    fn a_health_probe_is_not_a_caller_but_its_verbose_form_is() {
        assert!(is_open_probe("/healthz", None));
        assert!(is_open_probe("/healthz", Some("x=1")));
        assert!(!is_open_probe("/healthz", Some("verbose")));
        assert!(!is_open_probe("/healthz", Some("a=b&verbose=1")));
        assert!(!is_open_probe("/Patient", None));
        assert!(!is_open_probe("/healthz/extra", None));
    }

    #[test]
    fn only_a_presented_credential_is_one_to_check() {
        let mut headers = HeaderMap::new();
        assert!(!carries_credentials(&headers, None));
        assert!(!carries_credentials(&headers, Some("name=ann")));
        assert!(carries_credentials(
            &headers,
            Some("name=ann&access_token=x")
        ));
        assert!(!carries_credentials(&headers, Some("my_access_token=x")));
        headers.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert!(carries_credentials(&headers, None));
    }
}
