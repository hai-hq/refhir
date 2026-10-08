//! WebSocket subscribe / mutate / transition.
//!
//! Convex `crates/sync/src/worker.rs` handles `ModifyQuerySet`, `Mutation`,
//! and `Transition`. `udfPath` is a FHIR operation name such as `Patient/read`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use refhir_database::{Database, SubToken};
use refhir_fhir::{Catalog, ConformanceStore, RunResult};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::SyncSession;
use crate::http::{self, execute};

const ENGINE: &str = "refhir_engine";

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
                        if handle_text(&mut sink, &db, &catalog, &store, &inv_tx, &mut queries, &mut version, session.as_ref(), &text).await.is_err() {
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
                if push_updates(&mut sink, &db, &catalog, &store, &inv_tx, &mut queries, &mut version, session.as_ref(), ids).await.is_err() {
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
                        if let Some(old) = queries.remove(&query_id) {
                            db.unsubscribe(old.token);
                        }
                        let args = match authorize(session, db, catalog, &udf_path, &args, true) {
                            Ok(args) => args,
                            Err(err) => {
                                transition.push(json!({
                                    "type": "QueryFailed",
                                    "queryId": query_id,
                                    "error": err,
                                    "logLines": []
                                }));
                                continue;
                            }
                        };
                        match add_query(db, catalog, store, inv_tx, query_id, &udf_path, args).await
                        {
                            Ok((value, token, args)) => {
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
                            Err(err) => transition.push(json!({
                                "type": "QueryFailed",
                                "queryId": query_id,
                                "error": err,
                                "logLines": []
                            })),
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
            let args = match authorize(session, db, catalog, &udf_path, &args, false) {
                Ok(args) => args,
                Err(message) => {
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
                                    "diagnostics": message
                                }]
                            }),
                            "ts": Value::Null
                        }),
                    )
                    .await;
                }
            };
            let response = match execute(db, catalog, store, &udf_path, args).await {
                Ok(result) => json!({
                    "type": "MutationResponse",
                    "requestId": request_id,
                    "success": true,
                    "result": result.value,
                    "ts": result.ts.to_string()
                }),
                Err(err) => json!({
                    "type": "MutationResponse",
                    "requestId": request_id,
                    "success": false,
                    "result": http::outcome_value(&err),
                    "ts": Value::Null
                }),
            };
            send_json(sink, &response).await
        }
    }
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
