//! WebSocket subscribe / mutate / transition.
//!
//! Convex `crates/sync/src/worker.rs` handles `ModifyQuerySet`, `Mutation`,
//! and `Transition`. `udfPath` is a FHIR operation name such as `Patient/read`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use refhir_database::{CommitError, Database, SubToken};
use refhir_fhir::{Catalog, ConformanceStore, RunError, RunResult};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::SyncSession;
use crate::governance::{classify_operation, json_len, Class, RequestScope, SyncMessage};
use crate::http::{self, execute};

const ENGINE: &str = "refhir_engine";

/// Where a replica sends the mutations it cannot commit itself.
#[derive(Clone, Debug)]
pub struct Upstream {
    /// The primary's `/sync` URL, `ws://host:port/sync`.
    pub url: String,
    /// The caller's bearer token, so the primary applies the same scopes.
    pub bearer: Option<String>,
}

/// Runs one mutation on the primary over a connection of its own and returns its
/// `MutationResponse`. Per-mutation connections keep a replica's socket free of state to
/// reconnect, and a write is rare next to the reads a replica serves.
async fn forward_mutation(
    upstream: &Upstream,
    request_id: u64,
    udf_path: &str,
    args: &Value,
) -> Result<Value, String> {
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message as Wire;

    let mut request = upstream
        .url
        .as_str()
        .into_client_request()
        .map_err(|err| err.to_string())?;
    if let Some(token) = &upstream.bearer {
        let value = format!("Bearer {token}")
            .parse()
            .map_err(|_| "the bearer token is not a valid header value".to_string())?;
        request.headers_mut().insert("Authorization", value);
    }
    let (mut socket, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| "connecting to the primary timed out".to_string())?
    .map_err(|err| format!("connecting to the primary: {err}"))?;
    let message = json!({
        "type": "Mutation",
        "requestId": request_id,
        "udfPath": udf_path,
        "args": args,
    });
    socket
        .send(Wire::Text(message.to_string()))
        .await
        .map_err(|err| format!("sending to the primary: {err}"))?;
    loop {
        let next = tokio::time::timeout(Duration::from_secs(120), socket.next())
            .await
            .map_err(|_| "the primary did not answer".to_string())?;
        match next {
            Some(Ok(Wire::Text(text))) => {
                let value: Value = serde_json::from_str(&text).map_err(|err| err.to_string())?;
                if value["type"] == "MutationResponse" {
                    let _ = socket.close(None).await;
                    return Ok(value);
                }
                if value["type"] == "FatalError" {
                    return Err(value["message"]
                        .as_str()
                        .unwrap_or("fatal error")
                        .to_string());
                }
            }
            Some(Ok(_)) => {}
            Some(Err(err)) => return Err(format!("the primary's connection failed: {err}")),
            None => return Err("the primary closed the connection".to_string()),
        }
    }
}

/// The reply to a mutation a replica sent to the primary. Before answering, it waits for its own
/// snapshot to include the commit, so a live query on this socket reflects the write by the time
/// the caller hears it succeeded.
async fn forwarded_response(
    db: &Arc<Database>,
    upstream: &Upstream,
    request_id: u64,
    udf_path: &str,
    args: &Value,
) -> Value {
    match forward_mutation(upstream, request_id, udf_path, args).await {
        Ok(response) => {
            let committed = response["ts"]
                .as_str()
                .and_then(|text| text.parse::<u64>().ok())
                .and_then(|ts| refhir_types::Timestamp::from_u64(ts).ok());
            if let Some(ts) = committed {
                let db = db.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    db.wait_for_ts(ts, std::time::Duration::from_millis(500))
                })
                .await;
            }
            response
        }
        Err(message) => json!({
            "type": "MutationResponse",
            "requestId": request_id,
            "success": false,
            "result": {
                "resourceType": "OperationOutcome",
                "issue": [{
                    "severity": "error",
                    "code": "transient",
                    "diagnostics": format!("could not reach the primary: {message}")
                }]
            },
            "ts": Value::Null
        }),
    }
}

struct LiveQuery {
    token: SubToken,
    path: String,
    args: Value,
}

pub async fn handle_socket(
    socket: WebSocket,
    db: Arc<Database>,
    catalog: Arc<RwLock<Catalog>>,
    store: Arc<dyn ConformanceStore>,
    session: Option<SyncSession>,
    scope: Option<Arc<RequestScope>>,
    upstream: Option<Upstream>,
) {
    let (mut sink, mut stream) = socket.split();
    let (inv_tx, mut inv_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut queries: HashMap<u64, LiveQuery> = HashMap::new();
    let mut version: u64 = 0;

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    None => break,
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Ping(payload))) => {
                        if sink.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Text(text))) => {
                        if handle_text(&mut sink, &db, &catalog, &store, &inv_tx, &mut queries, &mut version, session.as_ref(), scope.as_ref(), upstream.as_ref(), &text).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
            Some(inv) = inv_rx.recv() => {
                let mut ids = HashSet::new();
                ids.insert(inv.query_id);
                while let Ok(more) = inv_rx.try_recv() {
                    ids.insert(more.query_id);
                }
                tracing::debug!(
                    target: ENGINE,
                    write_ts = inv.write_ts.as_u64(),
                    query_ids = ?ids,
                    "sync invalidation received, rerunning queries"
                );
                if push_updates(&mut sink, &db, &catalog, &store, &inv_tx, &mut queries, &mut version, session.as_ref(), scope.as_ref(), ids).await.is_err() {
                    break;
                }
            }
        }
    }

    for query in queries.values() {
        db.unsubscribe(query.token);
    }
}

async fn handle_text(
    sink: &mut (impl SinkExt<Message, Error = axum::Error> + Unpin),
    db: &Arc<Database>,
    catalog: &Arc<RwLock<Catalog>>,
    store: &Arc<dyn ConformanceStore>,
    inv_tx: &tokio::sync::mpsc::UnboundedSender<refhir_database::Invalidation>,
    queries: &mut HashMap<u64, LiveQuery>,
    version: &mut u64,
    session: Option<&SyncSession>,
    scope: Option<&Arc<RequestScope>>,
    upstream: Option<&Upstream>,
    text: &str,
) -> Result<(), ()> {
    let message: ClientMessage = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(err) => {
            send_json(
                sink,
                &json!({"type": "FatalError", "message": err.to_string()}),
            )
            .await?;
            return Err(());
        }
    };
    match message {
        ClientMessage::ModifyQuerySet {
            base_version,
            new_version,
            modifications,
        } => {
            tracing::debug!(
                target: ENGINE,
                base_version,
                new_version,
                modifications = modifications.len(),
                "sync ModifyQuerySet"
            );
            if base_version != *version {
                send_json(
                    sink,
                    &json!({
                        "type": "FatalError",
                        "message": format!("baseVersion {base_version} does not match {version}")
                    }),
                )
                .await?;
                return Err(());
            }
            let start = *version;
            *version = new_version;
            let mut transition = Vec::new();
            for modification in modifications {
                match modification {
                    QueryModification::Add {
                        query_id,
                        udf_path,
                        args,
                    } => {
                        // A live query is run again whenever a write overlaps it, so it is a
                        // cost that goes on after the message, and it is limited as one. The
                        // message is only the size of this one query, not of the whole frame.
                        let message = SyncMessage::begin(
                            scope,
                            Class::Sync,
                            &udf_path,
                            scope.map_or(0, |_| udf_path.len() + json_len(&args)),
                        );
                        if let Some(cap) =
                            scope.and_then(|scope| scope.governance().live_query_cap())
                        {
                            // Replacing a query is not holding another one.
                            let others =
                                queries.len() - usize::from(queries.contains_key(&query_id));
                            if others >= cap {
                                message.limit_reached();
                                transition.push(query_failed(
                                    query_id,
                                    format!("too many live queries: a connection may hold {cap}"),
                                ));
                                continue;
                            }
                        }
                        if let Err(rejection) = message.admit().await {
                            transition
                                .push(query_failed(query_id, rejection.message().to_string()));
                            continue;
                        }
                        // Only now, with the new query let in, is the one it replaces let go: a
                        // client refused for going too fast keeps the subscription it had.
                        if let Some(old) = queries.remove(&query_id) {
                            db.unsubscribe(old.token);
                        }
                        let args = match authorize(session, db, catalog, &udf_path, &args, true) {
                            Ok(args) => args,
                            Err(err) => {
                                message.deny(&args, &err);
                                transition.push(query_failed(query_id, err));
                                continue;
                            }
                        };
                        let added = message
                            .run(add_query(
                                db, catalog, store, inv_tx, query_id, &udf_path, args,
                            ))
                            .await;
                        match added {
                            Ok((value, token, args)) => {
                                message.done(200, || json_len(&value));
                                queries.insert(
                                    query_id,
                                    LiveQuery {
                                        token,
                                        path: udf_path,
                                        args,
                                    },
                                );
                                transition.push(json!({
                                    "type": "QueryUpdated",
                                    "queryId": query_id,
                                    "value": value,
                                    "logLines": []
                                }));
                            }
                            Err(err) => {
                                message.done(400, || 0);
                                transition.push(query_failed(query_id, err));
                            }
                        }
                    }
                    QueryModification::Remove { query_id } => {
                        if let Some(old) = queries.remove(&query_id) {
                            db.unsubscribe(old.token);
                        }
                        transition.push(json!({
                            "type": "QueryRemoved",
                            "queryId": query_id
                        }));
                    }
                }
            }
            tracing::debug!(
                target: ENGINE,
                start_version = start,
                end_version = *version,
                updates = transition.len(),
                "sync Transition (query set change)"
            );
            send_transition(sink, start, *version, transition).await
        }
        ClientMessage::Mutation {
            request_id,
            udf_path,
            args,
        } => {
            tracing::debug!(
                target: ENGINE,
                request_id,
                operation = udf_path.as_str(),
                "sync Mutation"
            );
            let message =
                SyncMessage::begin(scope, classify_operation(&udf_path), &udf_path, text.len());
            if let Err(rejection) = message.admit().await {
                return send_json(
                    sink,
                    &json!({
                        "type": "MutationResponse",
                        "requestId": request_id,
                        "success": false,
                        "result": throttled_outcome(rejection.message()),
                        "retryAfterSeconds": rejection.retry_after_secs(),
                        "ts": Value::Null
                    }),
                )
                .await;
            }
            let args = match authorize(session, db, catalog, &udf_path, &args, false) {
                Ok(args) => args,
                Err(reason) => {
                    message.deny(&args, &reason);
                    return send_json(
                        sink,
                        &json!({
                            "type": "MutationResponse",
                            "requestId": request_id,
                            "success": false,
                            "result": json!({
                                "resourceType": "OperationOutcome",
                                "issue": [{
                                    "severity": "error",
                                    "code": "forbidden",
                                    "diagnostics": reason
                                }]
                            }),
                            "ts": Value::Null
                        }),
                    )
                    .await;
                }
            };
            // Only a replica that can reach a primary may need the arguments again, to forward.
            let replay = upstream.map(|_| args.clone());
            let outcome = message
                .run(execute(db, catalog, store, &udf_path, args))
                .await;
            let (response, status) = match outcome {
                Ok(result) => (
                    json!({
                        "type": "MutationResponse",
                        "requestId": request_id,
                        "success": true,
                        "result": result.value,
                        "ts": result.ts.to_string()
                    }),
                    if result.status == 0 {
                        200
                    } else {
                        result.status
                    },
                ),
                // A replica cannot commit. The primary can, so ask it. The primary limits,
                // meters and audits the mutation as a message of its own, so it is not counted
                // here as well.
                Err(RunError::Commit(CommitError::ReadOnly)) if replay.is_some() => {
                    let (upstream, args) = (upstream.expect("checked"), replay.expect("checked"));
                    message.forwarded();
                    let response =
                        forwarded_response(db, upstream, request_id, &udf_path, &args).await;
                    let status = if response["success"] == true {
                        200
                    } else {
                        400
                    };
                    (response, status)
                }
                Err(err) => (
                    json!({
                        "type": "MutationResponse",
                        "requestId": request_id,
                        "success": false,
                        "result": http::outcome_value(&err),
                        "ts": Value::Null
                    }),
                    http::run_error_status(&err).as_u16(),
                ),
            };
            message.done(status, || json_len(&response));
            send_json(sink, &response).await
        }
    }
}

fn query_failed(query_id: u64, error: String) -> Value {
    json!({
        "type": "QueryFailed",
        "queryId": query_id,
        "error": error,
        "logLines": []
    })
}

fn throttled_outcome(message: &str) -> Value {
    json!({
        "resourceType": "OperationOutcome",
        "issue": [{"severity": "error", "code": "throttled", "diagnostics": message}]
    })
}

fn authorize(
    session: Option<&SyncSession>,
    db: &Database,
    catalog: &Arc<RwLock<Catalog>>,
    path: &str,
    args: &Value,
    subscribe: bool,
) -> Result<Value, String> {
    if path == "ImplementationGuide/$install" && session.is_none() {
        return Err("ImplementationGuide/$install requires authentication".into());
    }
    let Some(session) = session else {
        let mut args = args.clone();
        crate::auth::strip_sdc_controls(&mut args);
        return Ok(args);
    };
    let args = session.prepare(path, args, subscribe)?;
    if let Some(patient) = session.patient() {
        let catalog = catalog.read().expect("catalog");
        crate::auth::guard_patient_operation(db, &catalog, patient, path, &args)?;
    }
    Ok(args)
}

async fn add_query(
    db: &Arc<Database>,
    catalog: &Arc<RwLock<Catalog>>,
    store: &Arc<dyn ConformanceStore>,
    inv_tx: &tokio::sync::mpsc::UnboundedSender<refhir_database::Invalidation>,
    query_id: u64,
    path: &str,
    args: Value,
) -> Result<(Value, SubToken, Value), String> {
    for _ in 0..8 {
        let result = execute(db, catalog, store, path, args.clone())
            .await
            .map_err(|err| err.to_string())?;
        if result.wrote {
            return Err(format!("{path} writes and cannot be subscribed"));
        }
        match db.subscribe(result.reads, result.ts, query_id, inv_tx.clone()) {
            Ok(token) => {
                tracing::debug!(
                    target: ENGINE,
                    query_id,
                    operation = path,
                    snapshot_ts = result.ts.as_u64(),
                    sub_token = ?token,
                    "live query subscribed"
                );
                return Ok((result.value, token, args));
            }
            Err(_) => {
                tracing::debug!(
                    target: ENGINE,
                    query_id,
                    operation = path,
                    "live query subscribe stale, retrying operation"
                );
                continue;
            }
        }
    }
    Err("query stayed stale".into())
}

async fn push_updates(
    sink: &mut (impl SinkExt<Message, Error = axum::Error> + Unpin),
    db: &Arc<Database>,
    catalog: &Arc<RwLock<Catalog>>,
    store: &Arc<dyn ConformanceStore>,
    inv_tx: &tokio::sync::mpsc::UnboundedSender<refhir_database::Invalidation>,
    queries: &mut HashMap<u64, LiveQuery>,
    version: &mut u64,
    session: Option<&SyncSession>,
    scope: Option<&Arc<RequestScope>>,
    ids: HashSet<u64>,
) -> Result<(), ()> {
    let start = *version;
    *version += 1;
    let end = *version;
    let mut transition = Vec::new();
    for query_id in ids {
        let Some(query) = queries.get(&query_id) else {
            continue;
        };
        let path = query.path.clone();
        let args = query.args.clone();
        let token = query.token;
        match rerun(db, catalog, store, inv_tx, token, query_id, &path, &args).await {
            Ok((value, new_token)) => {
                if let Some(patient) = session.and_then(|session| session.patient()) {
                    let allowed = {
                        let loaded = catalog.read().expect("catalog");
                        crate::auth::delivered_in_patient(db, &loaded, patient, &value)
                    };
                    if allowed != Ok(true) {
                        if let Some(old) = queries.remove(&query_id) {
                            db.unsubscribe(old.token);
                        }
                        transition.push(json!({
                            "type": "QueryFailed",
                            "queryId": query_id,
                            "error": allowed.err().unwrap_or_else(|| "token is limited to a different patient".into()),
                            "logLines": []
                        }));
                        continue;
                    }
                }
                if let Some(query) = queries.get_mut(&query_id) {
                    query.token = new_token;
                }
                transition.push(json!({
                    "type": "QueryUpdated",
                    "queryId": query_id,
                    "value": value,
                    "logLines": []
                }));
            }
            Err(err) => transition.push(json!({
                "type": "QueryFailed",
                "queryId": query_id,
                "error": err,
                "logLines": []
            })),
        }
    }
    if transition.is_empty() {
        *version = start;
        return Ok(());
    }
    tracing::debug!(
        target: ENGINE,
        start_version = start,
        end_version = end,
        updates = transition.len(),
        "sync Transition (invalidation)"
    );
    if let Some(scope) = scope {
        scope.governance().record_push(scope, json_len(&transition));
    }
    send_transition(sink, start, end, transition).await
}

async fn rerun(
    db: &Arc<Database>,
    catalog: &Arc<RwLock<Catalog>>,
    store: &Arc<dyn ConformanceStore>,
    inv_tx: &tokio::sync::mpsc::UnboundedSender<refhir_database::Invalidation>,
    token: SubToken,
    query_id: u64,
    path: &str,
    args: &Value,
) -> Result<(Value, SubToken), String> {
    for _ in 0..8 {
        let RunResult {
            value, ts, reads, ..
        } = execute(db, catalog, store, path, args.clone())
            .await
            .map_err(|err| err.to_string())?;
        match db.refresh_subscription(token, reads.clone(), ts) {
            Ok(()) => {
                tracing::debug!(
                    target: ENGINE,
                    query_id,
                    operation = path,
                    commit_ts = ts.as_u64(),
                    sub_token = ?token,
                    "live query rerun complete (subscription refreshed)"
                );
                return Ok((value, token));
            }
            Err(refhir_database::SubscribeError::Stale) => {
                tracing::debug!(
                    target: ENGINE,
                    query_id,
                    operation = path,
                    "live query rerun stale, retrying"
                );
                continue;
            }
            Err(refhir_database::SubscribeError::Gone) => {
                tracing::debug!(
                    target: ENGINE,
                    query_id,
                    operation = path,
                    "subscription gone, re-subscribing"
                );
                match db.subscribe(reads, ts, query_id, inv_tx.clone()) {
                    Ok(new_token) => return Ok((value, new_token)),
                    Err(_) => continue,
                }
            }
        }
    }
    Err("query stayed stale".into())
}

async fn send_transition(
    sink: &mut (impl SinkExt<Message, Error = axum::Error> + Unpin),
    start: u64,
    end: u64,
    modifications: Vec<Value>,
) -> Result<(), ()> {
    send_json(
        sink,
        &json!({
            "type": "Transition",
            "startVersion": start,
            "endVersion": end,
            "modifications": modifications
        }),
    )
    .await
}

async fn send_json(
    sink: &mut (impl SinkExt<Message, Error = axum::Error> + Unpin),
    value: &Value,
) -> Result<(), ()> {
    sink.send(Message::Text(value.to_string()))
        .await
        .map_err(|_| ())
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ClientMessage {
    ModifyQuerySet {
        #[serde(rename = "baseVersion")]
        base_version: u64,
        #[serde(rename = "newVersion")]
        new_version: u64,
        modifications: Vec<QueryModification>,
    },
    Mutation {
        #[serde(rename = "requestId")]
        request_id: u64,
        #[serde(rename = "udfPath")]
        udf_path: String,
        args: Value,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum QueryModification {
    Add {
        #[serde(rename = "queryId")]
        query_id: u64,
        #[serde(rename = "udfPath")]
        udf_path: String,
        args: Value,
    },
    Remove {
        #[serde(rename = "queryId")]
        query_id: u64,
    },
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use refhir_database::Database;
    use refhir_fhir::Catalog;
    use refhir_sqlite::SqlitePersistence;
    use serde_json::json;

    use super::authorize;

    #[test]
    fn unauthenticated_sync_drops_client_controls() {
        let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
        let db = Database::open(store).unwrap();
        let catalog = Arc::new(RwLock::new(Catalog::empty()));
        let args = json!({"_readScopes": ["user/*.*"], "_sdcArchiver": true, "id": "qr1"});
        let got = authorize(
            None,
            &db,
            &catalog,
            "QuestionnaireResponse/read",
            &args,
            false,
        )
        .unwrap();
        assert!(got.get("_readScopes").is_none());
        assert!(got.get("_sdcArchiver").is_none());
        assert_eq!(got["id"], "qr1");
    }
}
