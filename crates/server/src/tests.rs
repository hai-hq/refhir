use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use refhir_database::Database;
use refhir_fhir::{install_package_dir, minir4_dir, Catalog, MAX_OCC_FAILURES};
use refhir_sqlite::SqlitePersistence;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

use crate::app;

fn memory() -> (Arc<Database>, Arc<RwLock<Catalog>>) {
    let store = SqlitePersistence::open_in_memory().unwrap();
    let db = Arc::new(Database::open(Arc::new(store.clone())).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    install_package_dir(&store, &db, &catalog, minir4_dir()).unwrap();
    (db, catalog)
}

struct Reply {
    status: StatusCode,
    body: Value,
    location: Option<String>,
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    if_match: Option<&str>,
) -> Reply {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(tag) = if_match {
        builder = builder.header("if-match", tag);
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/fhir+json");
    }
    let payload = body.map(|value| value.to_string()).unwrap_or_default();
    let request = builder.body(Body::from(payload)).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    Reply {
        status,
        body,
        location,
    }
}

fn patient(id: &str) -> Value {
    json!({
        "resourceType": "Patient",
        "id": id,
        "active": true,
        "name": [{"family": "Ann"}]
    })
}

#[tokio::test]
async fn http_crud_search_history_and_bundle() {
    let (db, catalog) = memory();
    let app = app(db, catalog);
    let metadata = send(&app, "GET", "/metadata", None, None).await;
    assert_eq!(metadata.status, StatusCode::OK);
    assert_eq!(metadata.body["resourceType"], "CapabilityStatement");
    let types: Vec<_> = metadata.body["rest"][0]["resource"]
        .as_array()
        .unwrap()
        .iter()
        .map(|resource| resource["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["Observation", "Patient"]);

    let created = send(&app, "POST", "/Patient", Some(patient("p1")), None).await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.body["meta"]["versionId"], "1");
    assert_eq!(created.location.as_deref(), Some("Patient/p1/_history/1"));

    let read = send(&app, "GET", "/Patient/p1", None, None).await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(read.body["id"], "p1");

    let mut replacement = patient("p1");
    replacement["birthDate"] = json!("1990-01-01");
    let updated = send(
        &app,
        "PUT",
        "/Patient/p1",
        Some(replacement),
        Some("W/\"1\""),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK);
    assert_eq!(updated.body["meta"]["versionId"], "2");

    let wrong = send(
        &app,
        "PUT",
        "/Patient/p1",
        Some(patient("p1")),
        Some("W/\"1\""),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::CONFLICT);

    let search = send(&app, "GET", "/Patient?name=ann", None, None).await;
    assert_eq!(search.status, StatusCode::OK);
    assert_eq!(search.body["total"], 1);

    let history = send(&app, "GET", "/Patient/p1/_history", None, None).await;
    assert_eq!(history.status, StatusCode::OK);
    assert_eq!(history.body["type"], "history");

    let vread = send(&app, "GET", "/Patient/p1/_history/1", None, None).await;
    assert_eq!(vread.status, StatusCode::OK);
    assert!(vread.body.get("birthDate").is_none());

    let missing = send(&app, "GET", "/Patient/missing", None, None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.body["resourceType"], "OperationOutcome");

    let deleted = send(&app, "DELETE", "/Patient/p1", None, None).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let gone = send(&app, "GET", "/Patient/p1/_history/3", None, None).await;
    assert_eq!(gone.status, StatusCode::GONE);

    let bundle = send(
        &app,
        "POST",
        "/",
        Some(json!({
            "resourceType": "Bundle",
            "type": "transaction",
            "entry": [
                {
                    "request": {"method": "POST", "url": "Patient"},
                    "resource": patient("p2")
                },
                {
                    "request": {"method": "POST", "url": "Observation"},
                    "resource": {
                        "resourceType": "Observation",
                        "id": "o1",
                        "subject": {"reference": "Patient/p2"},
                        "code": {"coding": [{"code": "8480-6"}]},
                        "effectiveDateTime": "2020-01-02"
                    }
                }
            ]
        })),
        None,
    )
    .await;
    assert_eq!(bundle.status, StatusCode::OK);
    assert_eq!(bundle.body["type"], "transaction-response");
    let found = send(&app, "GET", "/Observation?subject=Patient/p2", None, None).await;
    assert_eq!(found.body["total"], 1);
}

#[tokio::test]
async fn occ_retries_then_conflict() {
    let (db, catalog) = memory();
    let app = app(db.clone(), catalog);
    let created = send(&app, "POST", "/Patient", Some(patient("p1")), None).await;
    assert_eq!(created.status, StatusCode::CREATED);

    db.testing_fail_next_commits(1);
    let retried = send(&app, "PUT", "/Patient/p1", Some(patient("p1")), None).await;
    assert_eq!(retried.status, StatusCode::OK);

    db.testing_fail_next_commits(MAX_OCC_FAILURES + 1);
    let exhausted = send(&app, "PUT", "/Patient/p1", Some(patient("p1")), None).await;
    assert_eq!(exhausted.status, StatusCode::CONFLICT);
    assert_eq!(exhausted.body["issue"][0]["code"], "conflict");
}

#[tokio::test]
async fn concurrent_puts_both_commit() {
    let (db, catalog) = memory();
    let app = app(db, catalog);
    send(&app, "POST", "/Patient", Some(patient("p1")), None).await;
    let first = patient("p1");
    let mut second = patient("p1");
    second["birthDate"] = json!("2001-01-01");
    let app_a = app.clone();
    let app_b = app.clone();
    let (left, right) = tokio::join!(
        send(&app_a, "PUT", "/Patient/p1", Some(first), None),
        send(&app_b, "PUT", "/Patient/p1", Some(second), None),
    );
    assert_eq!(left.status, StatusCode::OK);
    assert_eq!(right.status, StatusCode::OK);
    assert_ne!(
        left.body["meta"]["versionId"],
        right.body["meta"]["versionId"]
    );
}

#[tokio::test]
async fn sync_subscription_transitions_after_mutation() {
    let (db, catalog) = memory();
    {
        let loaded = catalog.read().expect("catalog");
        refhir_fhir::run(
            &db,
            &loaded,
            "Patient/create",
            &json!({"resource": patient("p1")}),
        )
        .unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = app(db, catalog);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let url = format!("ws://{addr}/sync");
    let (mut subscriber, _) = connect(&url).await;
    subscriber
        .send(Message::Text(
            json!({
                "type": "ModifyQuerySet",
                "baseVersion": 0,
                "newVersion": 1,
                "modifications": [{
                    "type": "Add",
                    "queryId": 1,
                    "udfPath": "Patient/search",
                    "args": {"active": "true"}
                }]
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let first = next_json(&mut subscriber).await;
    assert_eq!(first["type"], "Transition");
    assert_eq!(first["modifications"][0]["value"]["total"], 1);

    let (mut writer, _) = connect(&url).await;
    writer
        .send(Message::Text(
            json!({
                "type": "Mutation",
                "requestId": 4,
                "udfPath": "Patient/update",
                "args": {
                    "id": "p1",
                    "resource": {
                        "resourceType": "Patient",
                        "id": "p1",
                        "active": true,
                        "name": [{"family": "Bee"}]
                    }
                }
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let mutation = next_json(&mut writer).await;
    assert_eq!(mutation["type"], "MutationResponse");
    assert_eq!(mutation["success"], true);

    let transition = next_json(&mut subscriber).await;
    assert_eq!(transition["type"], "Transition");
    let family =
        &transition["modifications"][0]["value"]["entry"][0]["resource"]["name"][0]["family"];
    assert_eq!(family, "Bee");
}

async fn connect(
    url: &str,
) -> (
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
) {
    let mut last = None;
    for _ in 0..20 {
        match tokio_tungstenite::connect_async(url).await {
            Ok(pair) => return pair,
            Err(err) => {
                last = Some(err);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    panic!("connect {url}: {last:?}");
}

async fn next_json(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    let message = timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("timed out waiting for websocket message")
        .unwrap()
        .unwrap();
    match message {
        Message::Text(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("unexpected websocket message {other:?}"),
    }
}

#[tokio::test]
async fn invalid_patient_create_returns_outcome_and_validate_does_not_write() {
    let (db, catalog) = memory();
    let app = app(db, catalog);
    let mut body = patient("p1");
    body["birthDate"] = json!("yesterday");
    let rejected = send(&app, "POST", "/Patient", Some(body.clone()), None).await;
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
    assert_eq!(rejected.body["resourceType"], "OperationOutcome");
    let validated = send(&app, "POST", "/Patient/$validate", Some(body), None).await;
    assert_eq!(validated.status, StatusCode::OK);
    assert_eq!(validated.body["resourceType"], "OperationOutcome");
    let missing = send(&app, "GET", "/Patient/p1", None, None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}
