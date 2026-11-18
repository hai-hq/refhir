//! `Binary` as FHIR R4 has it (http.html#binary): created and updated with the raw content as the
//! body, its `Content-Type` the Binary's `contentType` and `X-Security-Context` its
//! `securityContext`; read back as that content when the `Accept` names no FHIR format and no
//! `_format` asks for one. And a file of 10 MB, as JSON, through a tenant host over REST and
//! `/sync`, with the request limit just above where it is refused.

use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use refhir_database::Database;
use refhir_fhir::{install_package_dir, r4_dir, Catalog, ConformanceStore};
use refhir_sqlite::SqlitePersistence;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

use crate::tenancy::{HostConfig, Routing, TenantHost, TenantLimits, TenantState};
use crate::{tenant_app, ServerConfig, DEFAULT_MAX_REQUEST_BYTES};

type Parts = (
    Arc<Database>,
    Arc<RwLock<Catalog>>,
    Arc<dyn ConformanceStore>,
);

/// A server on the R4 package, with Patient `p1` for a Binary's `securityContext` to point at.
fn r4() -> Parts {
    let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
    let db = Arc::new(Database::open(store.clone()).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    install_package_dir(store.as_ref(), &db, &catalog, r4_dir()).unwrap();
    catalog.write().unwrap().set_update_create(true);
    let loaded = catalog.read().unwrap();
    refhir_fhir::run(
        &db,
        &loaded,
        "Patient/update",
        &json!({"id": "p1", "resource": {"resourceType": "Patient", "id": "p1"}}),
    )
    .unwrap();
    drop(loaded);
    (db, catalog, store)
}

fn server() -> axum::Router {
    let (db, catalog, store) = r4();
    crate::app(db, catalog, store)
}

/// A host serving tenant `a` at `/t/a`, with the limit every server has unless it is set.
fn host() -> axum::Router {
    let host = TenantHost::new(HostConfig {
        routing: Routing {
            path_prefix: Some("/t".into()),
            ..Routing::default()
        },
        role: "primary".into(),
        ..HostConfig::default()
    });
    let (db, catalog, store) = r4();
    let config = ServerConfig {
        tenant: Some("a".into()),
        ..ServerConfig::default()
    };
    host.set(
        "a",
        TenantState::Ready(Arc::new(tenant_app(db, catalog, store, config))),
        TenantLimits::default(),
    );
    host.set_ready();
    host.router()
}

struct Answer {
    status: StatusCode,
    content_type: String,
    security_context: Option<String>,
    bytes: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or_else(|_| {
            panic!(
                "{} is not JSON: {}",
                self.status,
                String::from_utf8_lossy(&self.bytes[..self.bytes.len().min(300)])
            )
        })
    }
}

async fn request(
    router: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> Answer {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().unwrap().to_string())
    };
    let (status, content_type, security_context) = (
        response.status(),
        header("content-type").unwrap_or_default(),
        header("x-security-context"),
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        content_type,
        security_context,
        bytes: bytes.to_vec(),
    }
}

/// Every byte value, so nothing survives that a text round trip would change.
fn pdf_like(len: usize) -> Vec<u8> {
    let mut bytes = b"%PDF-1.7\n".to_vec();
    bytes.extend((0..len).map(|n| (n * 7 % 256) as u8));
    bytes
}

fn decoded(binary: &Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(binary["data"].as_str().expect("data"))
        .unwrap()
}

#[tokio::test]
async fn raw_content_is_stored_as_sent_with_its_type_and_security_context() {
    let app = server();
    let content = pdf_like(4096);
    let made = request(
        &app,
        "POST",
        "/Binary",
        &[
            ("content-type", "application/pdf"),
            ("x-security-context", "Patient/p1"),
        ],
        content.clone(),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.json());
    let id = made.json()["id"].as_str().unwrap().to_string();

    let stored = request(
        &app,
        "GET",
        &format!("/Binary/{id}"),
        &[("accept", "application/fhir+json")],
        Vec::new(),
    )
    .await
    .json();
    assert_eq!(stored["contentType"], "application/pdf");
    assert_eq!(stored["securityContext"]["reference"], "Patient/p1");
    assert_eq!(decoded(&stored), content);

    // An update by content replaces it, and with no `X-Security-Context` there is none.
    let replaced = pdf_like(10);
    let updated = request(
        &app,
        "PUT",
        &format!("/Binary/{id}"),
        &[("content-type", "image/png")],
        replaced.clone(),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.json());
    let stored = request(&app, "GET", &format!("/Binary/{id}"), &[], Vec::new())
        .await
        .json();
    assert_eq!(stored["contentType"], "image/png");
    assert!(stored.get("securityContext").is_none(), "{stored}");
    assert_eq!(decoded(&stored), replaced);

    // A conditional update takes content too.
    let conditional = request(
        &app,
        "PUT",
        &format!("/Binary?_id={id}"),
        &[("content-type", "text/plain")],
        b"conditional".to_vec(),
    )
    .await;
    assert_eq!(conditional.status, StatusCode::OK, "{}", conditional.json());
    let stored = request(&app, "GET", &format!("/Binary/{id}"), &[], Vec::new())
        .await
        .json();
    assert_eq!(stored["contentType"], "text/plain");
    assert_eq!(decoded(&stored), b"conditional");

    // A body sent as FHIR JSON is still the resource, not content.
    let resource = json!({"resourceType": "Binary", "contentType": "text/plain", "data": "aGk="});
    let made = request(
        &app,
        "POST",
        "/Binary",
        &[("content-type", "application/fhir+json")],
        resource.to_string().into_bytes(),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.json());
    assert_eq!(made.json()["data"], "aGk=");
}

#[tokio::test]
async fn a_read_returns_the_content_unless_a_fhir_format_is_asked_for() {
    let app = server();
    let content = pdf_like(2048);
    let made = request(
        &app,
        "POST",
        "/Binary",
        &[
            ("content-type", "application/pdf"),
            ("x-security-context", "Patient/p1"),
        ],
        content.clone(),
    )
    .await;
    let id = made.json()["id"].as_str().unwrap().to_string();
    let uri = format!("/Binary/{id}");

    let raw = request(
        &app,
        "GET",
        &uri,
        &[("accept", "application/pdf")],
        Vec::new(),
    )
    .await;
    assert_eq!(raw.status, StatusCode::OK);
    assert_eq!(raw.content_type, "application/pdf");
    assert_eq!(raw.security_context.as_deref(), Some("Patient/p1"));
    assert_eq!(raw.bytes, content);

    let resource = request(
        &app,
        "GET",
        &uri,
        &[("accept", "application/fhir+json")],
        Vec::new(),
    )
    .await;
    assert_eq!(resource.content_type, "application/fhir+json");
    assert_eq!(resource.json()["resourceType"], "Binary");
    assert_eq!(decoded(&resource.json()), content);

    // `_format` wins over the `Accept`.
    let formatted = request(
        &app,
        "GET",
        &format!("{uri}?_format=json"),
        &[("accept", "application/pdf")],
        Vec::new(),
    )
    .await;
    assert_eq!(formatted.content_type, "application/fhir+json");
    assert_eq!(formatted.json()["id"], id.as_str());

    // Data the validator takes, unpadded and with a line break, is still returned as content.
    let unpadded = request(
        &app,
        "POST",
        "/Binary",
        &[("content-type", "application/fhir+json")],
        json!({"resourceType": "Binary", "contentType": "text/plain", "data": "QUJD\nRA"})
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(unpadded.status, StatusCode::CREATED, "{}", unpadded.json());
    let unpadded_id = unpadded.json()["id"].as_str().unwrap().to_string();
    let content_of = request(
        &app,
        "GET",
        &format!("/Binary/{unpadded_id}"),
        &[("accept", "text/plain")],
        Vec::new(),
    )
    .await;
    assert_eq!(content_of.status, StatusCode::OK);
    assert_eq!(content_of.bytes, b"ABCD");

    // No `Accept` at all is the resource, as for every other type.
    let plain = request(&app, "GET", &uri, &[], Vec::new()).await;
    assert_eq!(plain.content_type, "application/fhir+json");

    // A version is read the same way.
    let version = resource.json()["meta"]["versionId"]
        .as_str()
        .unwrap()
        .to_string();
    let old = request(
        &app,
        "GET",
        &format!("{uri}/_history/{version}"),
        &[("accept", "application/pdf")],
        Vec::new(),
    )
    .await;
    assert_eq!(old.bytes, content);
}

/// How many Binaries tenant `a` holds.
async fn binaries(router: &axum::Router) -> u64 {
    let found = request(
        router,
        "GET",
        "/t/a/Binary?_summary=count",
        &[("accept", "application/fhir+json")],
        Vec::new(),
    )
    .await
    .json();
    found["total"].as_u64().expect("total")
}

/// A Binary of `len` bytes as FHIR JSON.
fn binary_json(content: &[u8]) -> Value {
    json!({
        "resourceType": "Binary",
        "contentType": "application/pdf",
        "data": base64::engine::general_purpose::STANDARD.encode(content)
    })
}

/// A Binary whose JSON is `size` bytes long.
fn binary_json_of_size(size: usize) -> String {
    let empty =
        json!({"resourceType": "Binary", "contentType": "application/pdf", "data": ""}).to_string();
    let data = "A".repeat(size - empty.len());
    json!({"resourceType": "Binary", "contentType": "application/pdf", "data": data}).to_string()
}

const TEN_MB: usize = 10 * 1000 * 1000;

#[tokio::test]
async fn a_ten_megabyte_file_goes_through_a_tenant_host_over_rest() {
    let router = host();
    let content = pdf_like(TEN_MB);
    let body = binary_json(&content).to_string();
    assert!(body.len() > 13_000_000, "about 13.4 MB as JSON");
    let made = request(
        &router,
        "POST",
        "/t/a/Binary",
        &[("content-type", "application/fhir+json")],
        body.into_bytes(),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED);
    let id = made.json()["id"].as_str().unwrap().to_string();
    let read = request(
        &router,
        "GET",
        &format!("/t/a/Binary/{id}"),
        &[("accept", "application/pdf")],
        Vec::new(),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);
    assert!(read.bytes == content, "the content came back changed");

    // At the limit it is taken; a byte over, it is refused and nothing is written.
    let before = binaries(&router).await;
    let over = binary_json_of_size(DEFAULT_MAX_REQUEST_BYTES + 1);
    let refused = request(
        &router,
        "POST",
        "/t/a/Binary",
        &[("content-type", "application/fhir+json")],
        over.into_bytes(),
    )
    .await;
    assert_eq!(refused.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(refused.json()["issue"][0]["code"], "too-costly");
    // Said in `Content-Length`, it is refused before it is read.
    let declared = request(
        &router,
        "POST",
        "/t/a/Binary",
        &[
            ("content-type", "application/pdf"),
            (
                "content-length",
                &(DEFAULT_MAX_REQUEST_BYTES + 1).to_string(),
            ),
        ],
        Vec::new(),
    )
    .await;
    assert_eq!(declared.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(declared.json()["issue"][0]["code"], "too-costly");
    // Content that fits the limit raw but not as base64 JSON is refused: it could not be sent
    // back as the resource.
    let raw = request(
        &router,
        "POST",
        "/t/a/Binary",
        &[("content-type", "application/pdf")],
        vec![7u8; DEFAULT_MAX_REQUEST_BYTES / 4 * 3],
    )
    .await;
    assert_eq!(raw.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(raw.json()["issue"][0]["code"], "too-costly");
    assert_eq!(binaries(&router).await, before);
    // A GET that a route reads a body for is held to the limit too.
    let summary = request(
        &router,
        "GET",
        "/t/a/Patient/p1/$summary",
        &[],
        vec![b' '; DEFAULT_MAX_REQUEST_BYTES + 1],
    )
    .await;
    assert_eq!(summary.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(summary.json()["issue"][0]["code"], "too-costly");

    let at = binary_json_of_size(DEFAULT_MAX_REQUEST_BYTES);
    let taken = request(
        &router,
        "POST",
        "/t/a/Binary",
        &[("content-type", "application/fhir+json")],
        at.into_bytes(),
    )
    .await;
    assert_eq!(taken.status, StatusCode::CREATED);
    assert_eq!(binaries(&router).await, before + 1);
}

#[tokio::test]
async fn a_ten_megabyte_file_goes_through_a_tenant_host_over_sync() {
    let router = host();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    {
        let router = router.clone();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    }
    let (mut socket, _) = crate::tests::connect(&format!("ws://{addr}/t/a/sync")).await;

    let content = pdf_like(TEN_MB);
    let create = json!({"type": "Mutation", "requestId": 1, "udfPath": "Binary/create",
        "args": {"resource": binary_json(&content)}});
    socket
        .send(Message::Text(create.to_string()))
        .await
        .unwrap();
    let made = next_json(&mut socket).await;
    assert_eq!(made["success"], true, "{}", made["result"]);
    let id = made["result"]["id"].as_str().unwrap().to_string();

    let read = json!({"type": "Mutation", "requestId": 2, "udfPath": "Binary/read",
        "args": {"id": id}});
    socket.send(Message::Text(read.to_string())).await.unwrap();
    let read = next_json(&mut socket).await;
    assert_eq!(read["success"], true, "{}", read["result"]);
    assert!(
        decoded(&read["result"]) == content,
        "the content came back changed"
    );

    // A message over the limit is answered as REST answers it, and nothing is written.
    let before = binaries(&router).await;
    let over = format!(
        r#"{{"type":"Mutation","requestId":3,"udfPath":"Binary/create","args":{{"resource":{}}}}}"#,
        binary_json_of_size(DEFAULT_MAX_REQUEST_BYTES)
    );
    assert!(over.len() > DEFAULT_MAX_REQUEST_BYTES);
    socket.send(Message::Text(over)).await.unwrap();
    let refused = next_json(&mut socket).await;
    assert_eq!(refused["requestId"], 3);
    assert_eq!(refused["success"], false);
    assert_eq!(refused["result"]["issue"][0]["code"], "too-costly");
    assert_eq!(binaries(&router).await, before);

    // The socket goes on.
    let again = json!({"type": "Mutation", "requestId": 4, "udfPath": "Binary/read",
        "args": {"id": id}});
    socket.send(Message::Text(again.to_string())).await.unwrap();
    assert_eq!(next_json(&mut socket).await["success"], true);
}

async fn next_json(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(120), socket.next())
            .await
            .expect("an answer in time")
            .expect("the socket is open")
            .expect("a message");
        match message {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected {other:?}"),
        }
    }
}
