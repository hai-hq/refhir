//! `/livez` and `/readyz`, the probes an orchestrator uses.
//!
//! `/livez` says the process is up and the HTTP server answers. `/readyz` says it can take
//! traffic: startup (package installs, search-index backfills) is over, the database answers a
//! one-row query, and a replica is keeping up with the primary. While startup runs, the bootstrap
//! server answers both (see `cluster::bootstrap_router`), so `/readyz` says `starting` then.
//!
//! The probes are open to anyone and must stay cheap, so `app_with` adds them after every layer:
//! SMART, CORS, request logging and a replica's forwarding never see them. Their bodies carry a
//! fixed reason and nothing about the deployment, and they read no resource.
//!
//! `/healthz` is a different, older endpoint (a load balancer's view, with `?verbose`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::http::AppState;

fn reply(status: StatusCode, body: Value) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body.to_string(),
    )
        .into_response()
}

/// `{"status":"ok"}`
pub(crate) fn ok() -> Response {
    reply(StatusCode::OK, json!({"status": "ok"}))
}

/// 503 with `{"status":"starting"|"unavailable","reason":...}`.
pub(crate) fn not_ready(status: &'static str, reason: &str) -> Response {
    reply(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({"status": status, "reason": reason}),
    )
}

pub(crate) async fn livez() -> Response {
    ok()
}

pub(crate) async fn readyz(State(app): State<AppState>) -> Response {
    if let Err(reason) = database_answers(&app).await {
        return not_ready("unavailable", reason);
    }
    if let Some(reason) = crate::cluster::not_following(&app) {
        return not_ready("unavailable", reason);
    }
    ok()
}

/// What `/readyz` remembers about its database query.
#[derive(Default)]
pub(crate) struct ReadyProbe {
    /// When the last check ended and what it found. The lock is held while a check runs, so a
    /// probe that arrives meanwhile waits for it and takes its result instead of starting a
    /// second query, and a probe within `ready_cache` of the last one takes that result.
    last: tokio::sync::Mutex<Option<(Instant, Result<(), &'static str>)>>,
    /// Set while a check's thread is running. That thread can outlive the wait for it.
    running: Arc<AtomicBool>,
}

/// Clears the in-flight flag when the check ends, however it ends (a panic included).
struct Done(Arc<AtomicBool>);

impl Drop for Done {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

async fn database_answers(app: &AppState) -> Result<(), &'static str> {
    let mut last = app.ready_probe.last.lock().await;
    if let Some((at, result)) = *last {
        if at.elapsed() < app.config.ready_cache {
            return result;
        }
    }
    let result = run_check(app).await;
    *last = Some((Instant::now(), result));
    result
}

/// Runs the cheap query off the async workers, waiting for it at most `ready_timeout`.
///
/// A pool hands out connections without a timeout, so a database that hangs would leave this
/// query waiting for good. Only one query runs at a time: when the last one is still stuck
/// (which takes longer than `ready_timeout`, as callers wait for each check), this says
/// `unavailable` at once instead of stranding another blocking thread.
async fn run_check(app: &AppState) -> Result<(), &'static str> {
    if app.ready_probe.running.swap(true, Ordering::AcqRel) {
        return Err("the previous database check has not returned");
    }
    let db = app.db.clone();
    let done = Done(app.ready_probe.running.clone());
    let check = tokio::task::spawn_blocking(move || {
        let _done = done;
        db.ping()
    });
    match tokio::time::timeout(app.config.ready_timeout, check).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(err))) => {
            // The body says only that the database failed; the cause is for the operator.
            tracing::debug!(%err, "readiness: the database check failed");
            Err("database unreachable")
        }
        Ok(Err(_)) => Err("the database check stopped"),
        Err(_) => Err("database did not answer in time"),
    }
}

#[cfg(test)]
mod tests;
