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

use crate::http::{self, execute};

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
                        if handle_text(&mut sink, &db, &catalog, &store, &inv_tx, &mut queries, &mut version, &text).await.is_err() {
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
                if push_updates(&mut sink, &db, &catalog, &store, &inv_tx, &mut queries, &mut version, ids).await.is_err() {
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
            send_transition(sink, start, *version, transition).await
        }
        ClientMessage::Mutation {
            request_id,
            udf_path,
            args,
        } => {
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
            Ok(token) => return Ok((result.value, token, args)),
            Err(_) => continue,
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
            Ok(()) => return Ok((value, token)),
            Err(refhir_database::SubscribeError::Stale) => continue,
            Err(refhir_database::SubscribeError::Gone) => {
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
