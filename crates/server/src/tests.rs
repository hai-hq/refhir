use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use refhir_database::Database;
use refhir_fhir::{install_package_dir, minir4_dir, Catalog, ConformanceStore, MAX_OCC_FAILURES};
use refhir_sqlite::SqlitePersistence;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

use crate::app;

fn memory() -> (
    Arc<Database>,
    Arc<RwLock<Catalog>>,
    Arc<dyn ConformanceStore>,
) {
    let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
    let db = Arc::new(Database::open(store.clone()).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    install_package_dir(store.as_ref(), &db, &catalog, minir4_dir()).unwrap();
    (db, catalog, store)
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
        .or_else(|| response.headers().get("content-location"))
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
    let (db, catalog, store) = memory();
    let app = app(db, catalog, store);
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
    let (db, catalog, store) = memory();
    let app = app(db.clone(), catalog, store);
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
    let (db, catalog, store) = memory();
    let app = app(db, catalog, store);
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
    let (db, catalog, store) = memory();
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
    let app = app(db, catalog, store);
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
    let (db, catalog, store) = memory();
    let app = app(db, catalog, store);
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

#[tokio::test]
async fn terminology_operations_are_http_routes() {
    let dir = std::env::temp_dir().join(format!(
        "refhir-http-terms-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"example.http.terms","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("CodeSystem-animals.json"),
        r#"{"resourceType":"CodeSystem","url":"http://example.org/animals","name":"Animals","content":"complete","concept":[{"code":"mammal","display":"Mammal","concept":[{"code":"dog","display":"Dog","definition":"A dog"}]}]}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("ValueSet-pets.json"),
        r#"{"resourceType":"ValueSet","url":"http://example.org/pets","compose":{"include":[{"system":"http://example.org/animals"}]}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("ConceptMap-pets.json"),
        r#"{"resourceType":"ConceptMap","url":"http://example.org/map","group":[{"source":"http://example.org/animals","target":"http://example.org/dst","element":[{"code":"dog","target":[{"code":"canine","equivalence":"equivalent"}]}]}]}"#,
    )
    .unwrap();
    let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
    let db = Arc::new(Database::open(store.clone()).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    install_package_dir(store.as_ref(), &db, &catalog, &dir).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let app = app(db, catalog, store);

    let expanded = send(
        &app,
        "POST",
        "/ValueSet/$expand",
        Some(json!({"url": "http://example.org/pets", "filter": "dog"})),
        None,
    )
    .await;
    assert_eq!(expanded.status, StatusCode::OK);
    assert_eq!(expanded.body["resourceType"], "ValueSet");
    assert_eq!(expanded.body["expansion"]["contains"][0]["code"], "dog");

    let checked = send(
        &app,
        "POST",
        "/ValueSet/$validate-code",
        Some(json!({"url": "http://example.org/pets", "code": "dog", "display": "Hound"})),
        None,
    )
    .await;
    assert_eq!(checked.status, StatusCode::OK);
    assert_eq!(checked.body["parameter"][0]["valueBoolean"], true);
    assert!(checked.body["parameter"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["name"] == "message"));

    let looked = send(
        &app,
        "POST",
        "/CodeSystem/$lookup",
        Some(json!({"system": "http://example.org/animals", "code": "dog"})),
        None,
    )
    .await;
    assert_eq!(looked.status, StatusCode::OK);
    assert_eq!(looked.body["resourceType"], "Parameters");

    let translated = send(
        &app,
        "POST",
        "/ConceptMap/$translate",
        Some(json!({"url": "http://example.org/map", "system": "http://example.org/animals", "code": "dog"})),
        None,
    )
    .await;
    assert_eq!(translated.status, StatusCode::OK);
    assert_eq!(translated.body["parameter"][0]["valueBoolean"], true);

    let wrong = send(
        &app,
        "POST",
        "/Patient/$expand",
        Some(json!({"url": "http://example.org/pets"})),
        None,
    )
    .await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);

    let subsumed = send(
        &app,
        "POST",
        "/CodeSystem/$subsumes",
        Some(json!({
            "system": "http://example.org/animals",
            "codeA": "mammal",
            "codeB": "dog"
        })),
        None,
    )
    .await;
    assert_eq!(subsumed.status, StatusCode::OK);
    assert_eq!(subsumed.body["parameter"][0]["valueCode"], "subsumes");
}

#[tokio::test]
async fn xml_graphql_smart_export_and_subscription() {
    let (db, catalog, store) = memory();
    let dir = std::env::temp_dir().join(format!(
        "refhir-export-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let app = crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: false,
            export_dir: dir.display().to_string(),
            terminology_urls: Vec::new(),
        },
    );
    let xml = r#"<Patient xmlns="http://hl7.org/fhir"><id value="x1"/><active value="true"/></Patient>"#;
    let created = send_text(
        &app,
        "POST",
        "/Patient",
        xml,
        &[("content-type", "application/fhir+xml")],
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);

    let read = send_text(
        &app,
        "GET",
        "/Patient/x1?_format=xml",
        "",
        &[("accept", "application/fhir+xml")],
    )
    .await;
    assert_eq!(read.0, StatusCode::OK);
    assert!(read.1.contains("<Patient"), "{}", read.1);

    let created_gql = send(
        &app,
        "POST",
        "/graphql",
        Some(json!({
            "query": "mutation { PatientCreate(resource: '{\"resourceType\":\"Patient\",\"id\":\"g1\",\"active\":true}') { id } }"
        })),
        None,
    )
    .await;
    assert_eq!(created_gql.status, StatusCode::OK, "{}", created_gql.body);
    assert_eq!(created_gql.body["data"]["PatientCreate"]["id"], "g1");
    let listed = send(
        &app,
        "POST",
        "/graphql",
        Some(json!({"query": "{ PatientList { id } }"})),
        None,
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert!(listed.body["data"]["PatientList"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == "g1"));

    let exported = send(&app, "POST", "/$export", Some(json!({})), None).await;
    assert_eq!(exported.status, StatusCode::ACCEPTED);
    let job = exported
        .location
        .unwrap_or_default()
        .trim_start_matches("/export/")
        .to_string();
    assert!(!job.is_empty(), "content-location missing");
    let mut status = String::new();
    for _ in 0..20 {
        let polled = send(&app, "GET", &format!("/export/{job}"), None, None).await;
        status = polled.body["status"].as_str().unwrap_or("").to_string();
        if status == "completed" || status == "error" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(status, "completed");
    let file = send_text(&app, "GET", &format!("/export/{job}/file"), "", &[]).await;
    assert_eq!(file.0, StatusCode::OK);
    assert!(file.1.contains("\"id\":\"x1\"") || file.1.contains("\"id\": \"x1\""), "{}", file.1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn smart_scopes_gate_http_and_sync() {
    let (db, catalog, store) = memory();
    let app = crate::app_with(
        db.clone(),
        catalog.clone(),
        store.clone(),
        crate::ServerConfig {
            auth: true,
            export_dir: "export".into(),
            terminology_urls: Vec::new(),
        },
    );
    let denied = send(&app, "GET", "/Patient", None, None).await;
    assert_eq!(denied.status, StatusCode::UNAUTHORIZED);
    let issued = send_text(
        &app,
        "POST",
        "/oauth/token",
        "scope=user/Patient.read&patient=p1",
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(issued.0, StatusCode::OK, "{}", issued.1);
    let token: Value = serde_json::from_str(&issued.1).unwrap();
    let access = token["access_token"].as_str().unwrap();
    let allowed = send_text(
        &app,
        "GET",
        "/Patient",
        "",
        &[("authorization", &format!("Bearer {access}"))],
    )
    .await;
    assert_eq!(allowed.0, StatusCode::OK, "{}", allowed.1);
    let forbidden = send_text(
        &app,
        "POST",
        "/Patient",
        &patient("p1").to_string(),
        &[
            ("authorization", &format!("Bearer {access}")),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    assert_eq!(forbidden.0, StatusCode::FORBIDDEN);

    let metadata = send(&app, "GET", "/metadata", None, None).await;
    assert!(metadata.body["rest"][0].get("security").is_some());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let url = format!("ws://{addr}/sync");
    let blocked = tokio_tungstenite::connect_async(&url).await;
    assert!(blocked.is_err());
}

#[tokio::test]
async fn include_search_invalidates_when_the_included_patient_changes() {
    let (db, catalog, store) = memory();
    {
        let loaded = catalog.read().expect("catalog");
        refhir_fhir::run(&db, &loaded, "Patient/create", &json!({"resource": patient("p1")})).unwrap();
        refhir_fhir::run(
            &db,
            &loaded,
            "Observation/create",
            &json!({"resource": {
                "resourceType": "Observation",
                "id": "o1",
                "subject": {"reference": "Patient/p1"},
                "code": {"coding": [{"code": "8480-6"}]}
            }}),
        )
        .unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = app(db, catalog, store);
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
                    "queryId": 7,
                    "udfPath": "Observation/search",
                    "args": {"_id": "o1", "_include": "Observation:subject"}
                }]
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let first = next_json(&mut subscriber).await;
    assert_eq!(first["modifications"][0]["value"]["total"], 1);
    let (mut writer, _) = connect(&url).await;
    writer
        .send(Message::Text(
            json!({
                "type": "Mutation",
                "requestId": 8,
                "udfPath": "Patient/update",
                "args": {
                    "id": "p1",
                    "resource": {
                        "resourceType": "Patient",
                        "id": "p1",
                        "active": true,
                        "name": [{"family": "Included"}]
                    }
                }
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let mutation = next_json(&mut writer).await;
    assert_eq!(mutation["success"], true, "{mutation}");
    let transition = next_json(&mut subscriber).await;
    let text = transition.to_string();
    assert!(text.contains("Included"), "{transition}");
}

#[tokio::test]
async fn subscription_rest_hook_runs_after_commit() {
    let package = std::env::temp_dir().join(format!(
        "refhir-sub-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{"name":"example.subs","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    std::fs::write(
        package.join("StructureDefinition-Subscription.json"),
        r#"{"resourceType":"StructureDefinition","url":"http://example.org/StructureDefinition/Subscription","name":"Subscription","status":"active","kind":"resource","abstract":false,"type":"Subscription","fhirVersion":"4.0.1","derivation":"specialization","snapshot":{"element":[{"path":"Subscription","min":0,"max":"*"},{"path":"Subscription.status","min":0,"max":"1","type":[{"code":"code"}]},{"path":"Subscription.criteria","min":0,"max":"1","type":[{"code":"string"}]},{"path":"Subscription.channel","min":0,"max":"1","type":[{"code":"BackboneElement"}]},{"path":"Subscription.channel.type","min":0,"max":"1","type":[{"code":"code"}]},{"path":"Subscription.channel.endpoint","min":0,"max":"1","type":[{"code":"url"}]}]}}"#,
    )
    .unwrap();
    let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
    let db = Arc::new(Database::open(store.clone()).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    install_package_dir(store.as_ref(), &db, &catalog, minir4_dir()).unwrap();
    install_package_dir(store.as_ref(), &db, &catalog, &package).unwrap();
    let _ = std::fs::remove_dir_all(&package);

    let hook = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = hook.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..4 {
            let Ok((mut stream, _)) = hook.accept() else {
                break;
            };
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .ok();
            let text = read_http_request(&mut stream);
            let _ = std::io::Write::write_all(
                &mut stream,
                b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            let _ = std::io::Write::flush(&mut stream);
            let _ = stream.shutdown(std::net::Shutdown::Both);
            if text.contains("p9") {
                let _ = tx.send(text);
                break;
            }
        }
    });

    let app = app(db, catalog, store);
    let created = send(
        &app,
        "POST",
        "/Subscription",
        Some(json!({
            "resourceType": "Subscription",
            "id": "sub1",
            "status": "active",
            "criteria": "Patient?active=true",
            "channel": {"type": "rest-hook", "endpoint": format!("http://127.0.0.1:{port}/hook")}
        })),
        None,
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let patient = send(&app, "POST", "/Patient", Some(patient("p9")), None).await;
    assert_eq!(patient.status, StatusCode::CREATED, "{}", patient.body);
    let body = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(8)))
        .await
        .unwrap()
        .expect("rest-hook");
    assert!(body.contains("p9"), "{body}");
}

async fn send_text(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::from(body.to_string())).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get("content-location")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();
    if let Some(location) = location {
        return (status, location);
    }
    (status, text)
}

#[tokio::test]
async fn patient_export_keeps_the_compartment() {
    let (db, catalog, store) = memory();
    let dir = std::env::temp_dir().join(format!(
        "refhir-compartment-export-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let app = crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: false,
            export_dir: dir.display().to_string(),
            terminology_urls: Vec::new(),
        },
    );
    send(&app, "POST", "/Patient", Some(patient("p1")), None).await;
    send(&app, "POST", "/Patient", Some(patient("p2")), None).await;
    send(
        &app,
        "POST",
        "/Observation",
        Some(json!({
            "resourceType": "Observation",
            "id": "o1",
            "subject": {"reference": "Patient/p1"},
            "code": {"coding": [{"code": "8480-6"}]}
        })),
        None,
    )
    .await;
    send(
        &app,
        "POST",
        "/Observation",
        Some(json!({
            "resourceType": "Observation",
            "id": "o2",
            "subject": {"reference": "Patient/p2"},
            "code": {"coding": [{"code": "8480-6"}]}
        })),
        None,
    )
    .await;
    let started = send(&app, "POST", "/Patient/p1/$export", Some(json!({})), None).await;
    assert_eq!(started.status, StatusCode::ACCEPTED);
    let job = started.location.unwrap().trim_start_matches("/export/").to_string();
    let mut status = String::new();
    for _ in 0..40 {
        let polled = send(&app, "GET", &format!("/export/{job}"), None, None).await;
        status = polled.body["status"].as_str().unwrap_or("").to_string();
        if status == "completed" || status == "error" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(status, "completed");
    let file = send_text(&app, "GET", &format!("/export/{job}/file"), "", &[]).await;
    assert!(file.1.contains("\"id\":\"p1\"") || file.1.contains("\"id\": \"p1\""), "{}", file.1);
    assert!(file.1.contains("\"id\":\"o1\"") || file.1.contains("\"id\": \"o1\""), "{}", file.1);
    assert!(!file.1.contains("\"id\":\"p2\"") && !file.1.contains("\"id\": \"p2\""), "{}", file.1);
    assert!(!file.1.contains("\"id\":\"o2\"") && !file.1.contains("\"id\": \"o2\""), "{}", file.1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn rest_hook_retries_after_a_failure() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut saw_failure = false;
        for _ in 0..8 {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .ok();
            let request = read_http_request(&mut stream);
            let status = if request.contains("retry1") && !saw_failure {
                saw_failure = true;
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else {
                "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            };
            let _ = std::io::Write::write_all(&mut stream, status.as_bytes());
            let _ = std::io::Write::flush(&mut stream);
            let _ = stream.shutdown(std::net::Shutdown::Both);
            if request.contains("retry1") && saw_failure && status.starts_with("HTTP/1.1 204") {
                let _ = tx.send(request);
                break;
            }
        }
    });

    let package = std::env::temp_dir().join(format!(
        "refhir-retry-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{"name":"example.retry","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    std::fs::write(
        package.join("StructureDefinition-Subscription.json"),
        r#"{"resourceType":"StructureDefinition","url":"http://example.org/StructureDefinition/Subscription","name":"Subscription","status":"active","kind":"resource","abstract":false,"type":"Subscription","fhirVersion":"4.0.1","derivation":"specialization","snapshot":{"element":[{"path":"Subscription","min":0,"max":"*"},{"path":"Subscription.status","min":0,"max":"1","type":[{"code":"code"}]},{"path":"Subscription.criteria","min":0,"max":"1","type":[{"code":"string"}]},{"path":"Subscription.channel","min":0,"max":"1","type":[{"code":"BackboneElement"}]},{"path":"Subscription.channel.type","min":0,"max":"1","type":[{"code":"code"}]},{"path":"Subscription.channel.endpoint","min":0,"max":"1","type":[{"code":"url"}]}]}}"#,
    )
    .unwrap();
    let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
    let db = Arc::new(Database::open(store.clone()).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    install_package_dir(store.as_ref(), &db, &catalog, minir4_dir()).unwrap();
    install_package_dir(store.as_ref(), &db, &catalog, &package).unwrap();
    let _ = std::fs::remove_dir_all(&package);
    let app = app(db, catalog, store);
    let created = send(
        &app,
        "POST",
        "/Subscription",
        Some(json!({
            "resourceType": "Subscription",
            "id": "sub-retry",
            "status": "active",
            "criteria": "Patient?active=true",
            "channel": {"type": "rest-hook", "endpoint": format!("http://127.0.0.1:{port}/hook")}
        })),
        None,
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let patient = send(&app, "POST", "/Patient", Some(patient("retry1")), None).await;
    assert_eq!(patient.status, StatusCode::CREATED, "{}", patient.body);
    let body = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(8)))
        .await
        .unwrap()
        .expect("retried hook");
    assert!(body.contains("retry1"), "{body}");
}

fn read_http_request(stream: &mut std::net::TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 2048];
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) && buffer.len() < 16_384 {
        match std::io::Read::read(stream, &mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&buffer[..end]);
                    let length = headers.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.eq_ignore_ascii_case("content-length") {
                            value.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    });
                    if buffer.len() >= end + 4 + length.unwrap_or(0) {
                        break;
                    }
                }
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                if !buffer.is_empty() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}
