//! `/_audit`, `/_usage` and `/_requests`: what the server recorded, for whoever runs it.
//!
//! They sit under `_` like `/_history`, which is also system-level, and are authorised as
//! reads of `AuditEvent` (see `fhir_type_from_path`): a token needs `system/AuditEvent.read`, or
//! a scope that covers every resource, and a patient-scoped token never gets here. With SMART
//! off they are open, like the rest of the server.
//!
//! Each flushes what is waiting first, so that what a caller did a moment ago is in what they
//! read.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use refhir_persistence::{
    from_hex, to_hex, verify_audit_chain, AuditFilter, GovernanceStore, PersistError,
    RequestLogFilter, UsageFilter, MS_PER_DAY,
};
use serde_json::{json, Value};

use crate::http::{fhir_json_public, AppState};

use super::{audit, civil};

type Params = Vec<(String, String)>;

const DEFAULT_COUNT: u32 = 100;
const MAX_COUNT: u32 = 1_000;

fn refuse(status: StatusCode, code: &str, message: &str) -> Response {
    fhir_json_public(
        status,
        json!({
            "resourceType": "OperationOutcome",
            "issue": [{"severity": "error", "code": code, "diagnostics": message}]
        }),
    )
}

fn invalid(message: impl AsRef<str>) -> Response {
    refuse(StatusCode::BAD_REQUEST, "invalid", message.as_ref())
}

/// What the server recorded about its callers is for whoever runs it: with SMART on, the token
/// needs a `system/` scope that covers `AuditEvent`. `fhir_type_from_path` has already required
/// a scope to read `AuditEvent`, which a user's `user/*.read` satisfies; this is the stricter
/// part.
fn require_system_scope(app: &AppState, headers: &HeaderMap, uri: &Uri) -> Option<Response> {
    if !app.config.auth {
        return None;
    }
    let scopes = crate::auth::token_scopes(app, headers, uri.query()).unwrap_or_default();
    if crate::auth::can_read_audit(&scopes) {
        return None;
    }
    Some(refuse(
        StatusCode::FORBIDDEN,
        "forbidden",
        "reading what the server recorded needs a system/ scope that covers AuditEvent, \
         such as system/AuditEvent.read",
    ))
}

/// Reject a parameter this endpoint does not know. Dropping it quietly would hand someone who
/// is reading an audit trail a wider answer than the one they asked for.
fn only(params: &Params, known: &[&str]) -> Result<(), Response> {
    for (name, _) in params {
        if name != "access_token" && !known.contains(&name.as_str()) {
            return Err(invalid(format!("unsupported parameter {name}")));
        }
    }
    Ok(())
}

fn one<'a>(params: &'a Params, name: &str) -> Result<Option<&'a str>, Response> {
    let mut found = params.iter().filter(|(key, _)| key == name);
    let first = found.next();
    if found.next().is_some() {
        return Err(invalid(format!("{name} may be given once")));
    }
    Ok(first.map(|(_, value)| value.as_str()))
}

fn count(params: &Params) -> Result<u32, Response> {
    match one(params, "_count")? {
        None => Ok(DEFAULT_COUNT),
        Some(text) => match text.parse::<u32>() {
            Ok(value) if value >= 1 => Ok(value.min(MAX_COUNT)),
            _ => Err(invalid("_count must be a positive integer")),
        },
    }
}

fn cursor(params: &Params) -> Result<Option<u64>, Response> {
    one(params, "before")?
        .map(|text| {
            text.parse::<u64>()
                .map_err(|_| invalid("before must be an id from an earlier page"))
        })
        .transpose()
}

/// A caller as `client=` names them: a client id, or `anonymous`.
fn principal(params: &Params) -> Result<Option<String>, Response> {
    Ok(one(params, "client")?.map(|client| {
        if client == "anonymous" {
            client.to_string()
        } else {
            format!("client:{client}")
        }
    }))
}

/// `date=ge2026-10-01&date=lt2026-10-08`, as `[since, until)` in Unix milliseconds.
fn date_bounds(params: &Params) -> Result<(Option<u64>, Option<u64>), Response> {
    let mut since: Option<u64> = None;
    let mut until: Option<u64> = None;
    let raise = |current: Option<u64>, value: u64| Some(current.map_or(value, |c| c.max(value)));
    let lower = |current: Option<u64>, value: u64| Some(current.map_or(value, |c| c.min(value)));
    for (name, value) in params {
        if name != "date" {
            continue;
        }
        let split = if value.len() > 2
            && value.is_char_boundary(2)
            && value[..2].chars().all(char::is_alphabetic)
        {
            value.split_at(2)
        } else {
            ("eq", value.as_str())
        };
        let (prefix, text) = split;
        let ms = civil::parse_instant(text)
            .ok_or_else(|| invalid(format!("date {value} is not a date or a date and time")))?;
        // A bare date names the whole day: `le` runs to the end of it and `gt` starts after it.
        let past = if text.contains('T') { 1 } else { MS_PER_DAY };
        match prefix {
            "ge" => since = raise(since, ms),
            "gt" => since = raise(since, ms.saturating_add(past)),
            "le" => until = lower(until, ms.saturating_add(past)),
            "lt" => until = lower(until, ms),
            "eq" => {
                let width = if text.contains('T') {
                    1_000
                } else {
                    MS_PER_DAY
                };
                since = raise(since, ms);
                until = lower(until, ms.saturating_add(width));
            }
            other => return Err(invalid(format!("unsupported date prefix {other}"))),
        }
    }
    Ok((since, until))
}

fn audit_filter(params: &Params) -> Result<AuditFilter, Response> {
    only(
        params,
        &[
            "patient", "entity", "action", "outcome", "client", "user", "request", "date",
            "_count", "before",
        ],
    )?;
    let (since_ms, until_ms) = date_bounds(params)?;
    let (resource_type, resource_id) = match one(params, "entity")? {
        None => (None, None),
        Some(entity) => match entity.split_once('/') {
            Some((kind, id)) if !kind.is_empty() && !id.is_empty() => {
                (Some(kind.to_string()), Some(id.to_string()))
            }
            None if !entity.is_empty() => (Some(entity.to_string()), None),
            _ => return Err(invalid("entity is Type or Type/id")),
        },
    };
    let action = one(params, "action")?
        .map(|action| {
            if matches!(action, "C" | "R" | "U" | "D" | "E") {
                Ok(action.to_string())
            } else {
                Err(invalid("action is one of C, R, U, D, E"))
            }
        })
        .transpose()?;
    let failed = one(params, "outcome")?
        .map(|outcome| match outcome {
            "0" => Ok(false),
            "4" | "8" | "12" => Ok(true),
            _ => Err(invalid("outcome is 0 (success) or 4, 8, 12 (a failure)")),
        })
        .transpose()?;
    Ok(AuditFilter {
        resource_type,
        resource_id,
        patient: one(params, "patient")?.map(|patient| {
            patient
                .strip_prefix("Patient/")
                .unwrap_or(patient)
                .to_string()
        }),
        principal: principal(params)?,
        user: one(params, "user")?.map(str::to_string),
        action,
        failed,
        request_id: one(params, "request")?.map(str::to_string),
        since_ms,
        until_ms,
        before_id: cursor(params)?,
        limit: count(params)?,
    })
}

/// Run a store call off the async worker threads.
async fn from_store<T: Send + 'static>(
    store: std::sync::Arc<dyn GovernanceStore>,
    work: impl FnOnce(&dyn GovernanceStore) -> Result<T, PersistError> + Send + 'static,
) -> Result<T, Response> {
    match tokio::task::spawn_blocking(move || work(store.as_ref())).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => {
            tracing::error!(error = %err, "could not read the governance tables");
            Err(refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                "exception",
                "the record could not be read",
            ))
        }
        Err(_) => Err(refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            "exception",
            "the read stopped",
        )),
    }
}

/// A URL-encoded query component.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A link to this listing: the query as it came, with the cursor moved to `before` where there
/// is one. The token the caller authenticated with is never copied into a link.
fn link(app: &AppState, path: &str, params: &Params, before: Option<u64>) -> String {
    let mut parts: Vec<String> = params
        .iter()
        .filter(|(name, _)| name != "access_token" && !(before.is_some() && name == "before"))
        .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
        .collect();
    if let Some(before) = before {
        parts.push(format!("before={before}"));
    }
    let base = app
        .config
        .public_url
        .as_deref()
        .map(|url| url.trim_end_matches('/'))
        .unwrap_or("");
    if parts.is_empty() {
        format!("{base}{path}")
    } else {
        format!("{base}{path}?{}", parts.join("&"))
    }
}

/// `GET /_audit`: the audit trail as a search-set `Bundle` of `AuditEvent`, newest first.
pub(crate) async fn audit_events(
    State(app): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Query(params): Query<Params>,
) -> Response {
    if let Some(response) = require_system_scope(&app, &headers, &uri) {
        return response;
    }
    let filter = match audit_filter(&params) {
        Ok(filter) => filter,
        Err(response) => return response,
    };
    let governance = app.governance.clone();
    governance.flush().await;
    let limit = filter.limit as usize;
    let rows = match from_store(governance.store().clone(), move |store| {
        // One more than asked for says whether there is another page.
        store.audit_query(&AuditFilter {
            limit: filter.limit + 1,
            ..filter
        })
    })
    .await
    {
        Ok(rows) => rows,
        Err(response) => return response,
    };
    let more = rows.len() > limit;
    let rows = &rows[..rows.len().min(limit)];
    let mut links = vec![json!({"relation": "self", "url": link(&app, "/_audit", &params, None)})];
    if more {
        if let Some(last) = rows.last() {
            links.push(
                json!({"relation": "next", "url": link(&app, "/_audit", &params, Some(last.id))}),
            );
        }
    }
    fhir_json_public(
        StatusCode::OK,
        json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "link": links,
            "entry": rows.iter().map(|row| json!({
                "fullUrl": format!("AuditEvent/{}", row.id),
                "resource": audit::audit_event(row)
            })).collect::<Vec<_>>()
        }),
    )
}

/// `GET /_audit/verify`: walk the audit chain and say whether it is whole.
///
/// `checkpoint=<id>:<hash>` (a line the server logged, as the hash in hex) also says whether the
/// trail still holds that row, which is the only way to see that its newest rows were not
/// deleted. Reads the whole trail, so one verification runs at a time.
pub(crate) async fn audit_verify(
    State(app): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Query(params): Query<Params>,
) -> Response {
    if let Some(response) = require_system_scope(&app, &headers, &uri) {
        return response;
    }
    if let Err(response) = only(&params, &["checkpoint"]) {
        return response;
    }
    let checkpoint = match one(&params, "checkpoint") {
        Err(response) => return response,
        Ok(None) => None,
        Ok(Some(text)) => {
            match text
                .split_once(':')
                .and_then(|(id, hash)| Some((id.parse::<u64>().ok()?, from_hex(hash)?)))
            {
                Some(checkpoint) => Some(checkpoint),
                None => {
                    return invalid("checkpoint is <id>:<hash in hex>, as the server logged it")
                }
            }
        }
    };
    let governance = app.governance.clone();
    let Some(_running) = governance.begin_verify() else {
        return refuse(
            StatusCode::TOO_MANY_REQUESTS,
            "throttled",
            "an audit verification is already running; it reads the whole trail, so one runs at a time",
        );
    };
    governance.flush().await;
    let key = governance.config.audit.chain_key.clone();
    let report = match from_store(governance.store().clone(), move |store| {
        verify_audit_chain(
            store,
            key.as_ref(),
            checkpoint.as_ref().map(|(id, hash)| (*id, hash.as_slice())),
        )
    })
    .await
    {
        Ok(report) => report,
        Err(response) => return response,
    };
    let keyed = governance.config.audit.chain_key.is_some();
    Json(json!({
        "verified": report.intact(),
        "keyed": keyed,
        "checked": report.checked,
        "unchained": report.unchained,
        "firstId": report.first_id,
        "lastId": report.last_id,
        "head": report.head.as_ref().map(|(id, hash)| json!({"id": id, "hash": to_hex(hash)})),
        "breaks": report.breaks.iter().map(|found| json!({"id": found.id, "problem": found.problem.as_str()})).collect::<Vec<_>>(),
        "breaksTotal": report.breaks_total,
        "checkpoint": report.checkpoint.map(|result| result.as_str()),
        "note": if keyed {
            Value::Null
        } else {
            json!("the chain is not keyed (REFHIR_AUDIT_KEY is unset): it catches mistakes, and proves nothing against someone who can write the table")
        },
    }))
    .into_response()
}

/// `GET /_usage`: counters per day, caller and class.
pub(crate) async fn usage(
    State(app): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Query(params): Query<Params>,
) -> Response {
    if let Some(response) = require_system_scope(&app, &headers, &uri) {
        return response;
    }
    if let Err(response) = only(&params, &["client", "from", "to", "_count"]) {
        return response;
    }
    let day_of = |name: &str| -> Result<Option<u32>, Response> {
        one(&params, name)?
            .map(|text| {
                civil::parse_instant(text)
                    .map(civil::day_of)
                    .ok_or_else(|| invalid(format!("{name} is a date such as 2026-10-01")))
            })
            .transpose()
    };
    let (from_day, to_day, principal, limit) = match (
        day_of("from"),
        day_of("to"),
        principal(&params),
        count(&params),
    ) {
        (Ok(from), Ok(to), Ok(principal), Ok(limit)) => (from, to, principal, limit),
        (Err(response), ..)
        | (_, Err(response), ..)
        | (_, _, Err(response), _)
        | (.., Err(response)) => return response,
    };
    let governance = app.governance.clone();
    governance.flush().await;
    let rows = match from_store(governance.store().clone(), move |store| {
        // One more than asked for says whether the answer was cut short.
        store.usage_query(&UsageFilter {
            principal,
            from_day,
            to_day,
            limit: limit + 1,
        })
    })
    .await
    {
        Ok(rows) => rows,
        Err(response) => return response,
    };
    let truncated = rows.len() > limit as usize;
    let rows = &rows[..rows.len().min(limit as usize)];
    let (dropped_requests, dropped_audit) = governance.dropped();
    let (sampled_requests, sampled_audit) = governance.sampled();
    Json(json!({
        "usage": rows.iter().map(|row| json!({
            "date": civil::date_string(row.key.day),
            "principal": row.key.principal,
            "class": row.key.class,
            "requests": row.counters.requests,
            "clientErrors": row.counters.client_errors,
            "serverErrors": row.counters.server_errors,
            "throttled": row.counters.throttled,
            "bytesIn": row.counters.bytes_in,
            "bytesOut": row.counters.bytes_out,
            "durationMs": row.counters.duration_ms,
        })).collect::<Vec<Value>>(),
        // True when more rows matched than `_count` allowed, newest days first: narrow the
        // answer with `client`, `from` and `to`, or raise `_count`.
        "truncated": truncated,
        // Rows lost since the server started because the database was not taking writes.
        "dropped": {"requestLog": dropped_requests, "audit": dropped_audit},
        // Rows about refused traffic that were counted and not kept, for want of the
        // per-second budget, since the server started.
        "sampled": {"requestLog": sampled_requests, "audit": sampled_audit},
        // How many servers are dividing each rate limit between them, as this one counts.
        "servers": governance.share(),
    }))
    .into_response()
}

/// `GET /_requests`: the stored request log, newest first.
pub(crate) async fn requests(
    State(app): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Query(params): Query<Params>,
) -> Response {
    if let Some(response) = require_system_scope(&app, &headers, &uri) {
        return response;
    }
    if let Err(response) = only(
        &params,
        &["client", "status", "request", "date", "_count", "before"],
    ) {
        return response;
    }
    let build = || -> Result<RequestLogFilter, Response> {
        let (since_ms, until_ms) = date_bounds(&params)?;
        Ok(RequestLogFilter {
            principal: principal(&params)?,
            request_id: one(&params, "request")?.map(str::to_string),
            min_status: one(&params, "status")?
                .map(|text| {
                    text.parse::<u16>()
                        .map_err(|_| invalid("status is an HTTP status such as 400"))
                })
                .transpose()?,
            since_ms,
            until_ms,
            before_id: cursor(&params)?,
            limit: count(&params)?,
        })
    };
    let filter = match build() {
        Ok(filter) => filter,
        Err(response) => return response,
    };
    let governance = app.governance.clone();
    governance.flush().await;
    let limit = filter.limit as usize;
    let rows = match from_store(governance.store().clone(), move |store| {
        store.request_log_query(&RequestLogFilter {
            limit: filter.limit + 1,
            ..filter
        })
    })
    .await
    {
        Ok(rows) => rows,
        Err(response) => return response,
    };
    let more = rows.len() > limit;
    let rows = &rows[..rows.len().min(limit)];
    let next = if more {
        rows.last()
            .map(|last| link(&app, "/_requests", &params, Some(last.id)))
    } else {
        None
    };
    Json(json!({
        "requests": rows.iter().map(|row| json!({
            "id": row.id,
            "time": civil::instant_string(row.ts_ms),
            "requestId": row.request_id,
            "principal": row.principal,
            "remoteIp": row.remote_ip,
            "method": row.method,
            "path": row.path,
            "params": row.params,
            "class": row.class,
            "status": row.status,
            "durationMs": row.duration_ms,
            "bytesIn": row.bytes_in,
            "bytesOut": row.bytes_out,
        })).collect::<Vec<Value>>(),
        "next": next,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> Params {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    fn rejected(result: Result<AuditFilter, Response>) -> StatusCode {
        result.unwrap_err().status()
    }

    #[test]
    fn audit_parameters_become_a_filter() {
        let filter = audit_filter(&params(&[
            ("patient", "Patient/p1"),
            ("entity", "Observation/o1"),
            ("action", "U"),
            ("outcome", "4"),
            ("client", "app"),
            ("user", "u-1"),
            ("request", "req-1"),
            ("date", "ge2026-10-01"),
            ("date", "lt2026-10-08"),
            ("_count", "5000"),
            ("before", "77"),
            ("access_token", "ignored"),
        ]))
        .unwrap();
        assert_eq!(filter.patient.as_deref(), Some("p1"));
        assert_eq!(
            (
                filter.resource_type.as_deref(),
                filter.resource_id.as_deref()
            ),
            (Some("Observation"), Some("o1"))
        );
        assert_eq!(filter.action.as_deref(), Some("U"));
        assert_eq!(filter.failed, Some(true));
        assert_eq!(filter.principal.as_deref(), Some("client:app"));
        assert_eq!(filter.user.as_deref(), Some("u-1"));
        assert_eq!(filter.request_id.as_deref(), Some("req-1"));
        assert_eq!(filter.since_ms, civil::parse_instant("2026-10-01"));
        assert_eq!(filter.until_ms, civil::parse_instant("2026-10-08"));
        assert_eq!(filter.before_id, Some(77));
        assert_eq!(filter.limit, MAX_COUNT, "capped");
        let all = audit_filter(&params(&[])).unwrap();
        assert_eq!(all.limit, DEFAULT_COUNT);
        assert_eq!(all.failed, None);
        assert_eq!(
            audit_filter(&params(&[("outcome", "0")])).unwrap().failed,
            Some(false)
        );
        assert_eq!(
            audit_filter(&params(&[("entity", "Patient")]))
                .unwrap()
                .resource_type
                .as_deref(),
            Some("Patient")
        );
    }

    #[test]
    fn a_parameter_that_is_not_understood_is_refused_not_ignored() {
        for bad in [
            &[("patent", "p1")][..],
            &[("action", "X")],
            &[("outcome", "2")],
            &[("_count", "0")],
            &[("_count", "many")],
            &[("before", "x")],
            &[("date", "yesterday")],
            &[("date", "xx2026-10-01")],
            &[("entity", "/o1")],
            &[("client", "a"), ("client", "b")],
        ] {
            assert_eq!(
                rejected(audit_filter(&params(bad))),
                StatusCode::BAD_REQUEST,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn dates_bound_the_range_the_way_fhir_prefixes_say() {
        let day = |text: &str| civil::parse_instant(text).unwrap();
        let bounds = |pairs: &[(&str, &str)]| date_bounds(&params(pairs)).unwrap();
        assert_eq!(
            bounds(&[("date", "2026-10-12")]),
            (Some(day("2026-10-12")), Some(day("2026-10-13"))),
            "a bare date is that day"
        );
        assert_eq!(
            bounds(&[("date", "ge2026-10-01"), ("date", "le2026-10-02")]),
            (Some(day("2026-10-01")), Some(day("2026-10-03"))),
            "`le` a date takes in the whole of that day"
        );
        assert_eq!(
            bounds(&[("date", "gt2026-10-01")]),
            (Some(day("2026-10-02")), None),
            "`gt` a date starts after the whole of that day"
        );
        assert_eq!(
            bounds(&[("date", "le2026-10-01T10:00:00Z")]),
            (None, Some(day("2026-10-01T10:00:00Z") + 1)),
            "a time is an instant"
        );
        assert_eq!(
            bounds(&[("date", "gt2026-10-01T00:00:00Z")]),
            (Some(day("2026-10-01") + 1), None)
        );
        assert_eq!(
            bounds(&[("date", "lt2026-10-01")]),
            (None, Some(day("2026-10-01")))
        );
        // Two lower bounds: the later one holds.
        assert_eq!(
            bounds(&[("date", "ge2026-10-01"), ("date", "ge2026-10-05")]).0,
            Some(day("2026-10-05"))
        );
    }

    #[test]
    fn query_values_are_encoded_for_the_next_link() {
        assert_eq!(encode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(encode("Patient/p1"), "Patient%2Fp1");
        assert_eq!(
            encode("ge2026-10-01T00:00:00Z"),
            "ge2026-10-01T00%3A00%3A00Z"
        );
    }
}
