//! Tenants served side by side by one host. Each tenant here is its own in-memory database, so
//! what these tests check is the host: that a request reaches the server of the tenant it names
//! and no other, and that what a tenant's server keeps (sockets, sessions, Subscriptions) stays
//! its own. That the database keeps tenants apart is checked by the Postgres tenancy tests.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

use super::*;
use crate::tests::{connect, next_json, subscription_parts};
use crate::{tenant_app, ServerConfig};

const ADMIN_TOKEN: &str = "admin-secret";

fn tenant(
    name: &str,
    parts: (
        Arc<refhir_database::Database>,
        Arc<std::sync::RwLock<refhir_fhir::Catalog>>,
        Arc<dyn refhir_fhir::ConformanceStore>,
    ),
) -> Arc<TenantApp> {
    let (db, catalog, store) = parts;
    let config = ServerConfig {
        tenant: Some(name.to_string()),
        public_url: Some(format!("https://fhir.example.com/t/{name}")),
        // Each tenant signs its health cards with a key of its own.
        health_cards: Some(Arc::new(crate::IssuerKey::generate().unwrap().0)),
        ..ServerConfig::default()
    };
    Arc::new(tenant_app(db, catalog, store, config))
}

/// A host with tenants `a` and `b`, by path and by host name.
fn two_tenants() -> Arc<TenantHost> {
    let host = TenantHost::new(HostConfig {
        routing: Routing {
            path_prefix: Some("/t".into()),
            domain: Some("fhir.example.com".into()),
            public_url: Some("https://fhir.example.com".into()),
        },
        admin_token: Some(ADMIN_TOKEN.into()),
        role: "primary".into(),
        ..HostConfig::default()
    });
    for name in ["a", "b"] {
        host.set(
            name,
            TenantState::Ready(tenant(name, subscription_parts())),
            TenantLimits::default(),
        );
    }
    host.set_ready();
    host
}

struct Reply {
    status: StatusCode,
    body: Value,
}

async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Reply {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/fhir+json");
    }
    let request = builder
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    Reply { status, body }
}

async fn put(router: &Router, tenant: &str, resource: Value) -> Reply {
    let uri = format!(
        "/t/{tenant}/{}/{}",
        resource["resourceType"].as_str().unwrap(),
        resource["id"].as_str().unwrap()
    );
    call(router, "PUT", &uri, &[], Some(resource)).await
}

fn ids(bundle: &Value, mode: &str) -> Vec<String> {
    let mut found: Vec<String> = bundle["entry"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|entry| mode.is_empty() || entry["search"]["mode"] == mode)
        .map(|entry| {
            format!(
                "{}/{}",
                entry["resource"]["resourceType"].as_str().unwrap(),
                entry["resource"]["id"].as_str().unwrap()
            )
        })
        .collect();
    found.sort();
    found
}

/// Both tenants hold a Patient `p1` with an Observation about it; `a`'s are Ann's, `b`'s Bee's.
async fn seed(router: &Router) {
    for (tenant, family, observation) in [("a", "Ann", "oa"), ("b", "Bee", "ob")] {
        let made = put(
            router,
            tenant,
            json!({"resourceType": "Patient", "id": "p1", "active": true, "name": [{"family": family}]}),
        )
        .await;
        assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
        let made = put(
            router,
            tenant,
            json!({"resourceType": "Observation", "id": observation, "status": "final",
                   "code": {"text": "x"}, "subject": {"reference": "Patient/p1"}}),
        )
        .await;
        assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
    }
}

#[tokio::test]
async fn two_tenants_in_one_process_never_see_each_others_resources() {
    let host = two_tenants();
    let router = host.router();
    seed(&router).await;

    // Reads: the same id is each tenant's own.
    let a = call(&router, "GET", "/t/a/Patient/p1", &[], None).await;
    let b = call(&router, "GET", "/t/b/Patient/p1", &[], None).await;
    assert_eq!(a.body["name"][0]["family"], "Ann");
    assert_eq!(b.body["name"][0]["family"], "Bee");
    assert_eq!(
        call(&router, "GET", "/t/a/Observation/ob", &[], None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // Searches, and what they include.
    let searched = call(
        &router,
        "GET",
        "/t/a/Observation?_include=Observation:subject",
        &[],
        None,
    )
    .await;
    assert_eq!(searched.status, StatusCode::OK, "{}", searched.body);
    assert_eq!(ids(&searched.body, "match"), ["Observation/oa"]);
    assert_eq!(ids(&searched.body, "include"), ["Patient/p1"]);
    let included = &searched.body["entry"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["search"]["mode"] == "include")
        .unwrap()["resource"];
    assert_eq!(included["name"][0]["family"], "Ann");
    let reverse = call(
        &router,
        "GET",
        "/t/b/Patient?_revinclude=Observation:subject",
        &[],
        None,
    )
    .await;
    assert_eq!(ids(&reverse.body, "include"), ["Observation/ob"]);

    // $everything gathers one tenant's compartment only.
    let everything = call(&router, "GET", "/t/a/Patient/p1/$everything", &[], None).await;
    assert_eq!(everything.status, StatusCode::OK, "{}", everything.body);
    let found = ids(&everything.body, "");
    assert!(found.contains(&"Observation/oa".to_string()), "{found:?}");
    assert!(!found.contains(&"Observation/ob".to_string()), "{found:?}");

    // History, of the system and of an instance.
    let history = call(&router, "GET", "/t/b/_history", &[], None).await;
    let found = ids(&history.body, "");
    assert!(found.contains(&"Observation/ob".to_string()), "{found:?}");
    assert!(!found.contains(&"Observation/oa".to_string()), "{found:?}");

    // A delete in one tenant leaves the other's resource of the same id.
    let deleted = call(&router, "DELETE", "/t/b/Patient/p1", &[], None).await;
    assert!(deleted.status.is_success(), "{}", deleted.body);
    assert_eq!(
        call(&router, "GET", "/t/a/Patient/p1", &[], None)
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn tenants_are_named_by_path_host_name_or_what_a_replica_forwards() {
    let host = two_tenants();
    let router = host.router();
    seed(&router).await;
    let family = |reply: Reply| reply.body["name"][0]["family"].clone();

    let by_host = call(
        &router,
        "GET",
        "/Patient/p1",
        &[("host", "b.fhir.example.com:443")],
        None,
    )
    .await;
    assert_eq!(family(by_host), "Bee");
    let forwarded = call(
        &router,
        "GET",
        "/Patient/p1",
        &[("x-refhir-forwarded", "1"), ("x-refhir-tenant", "a")],
        None,
    )
    .await;
    assert_eq!(family(forwarded), "Ann");

    // A tenant the process does not have, and paths that name none.
    let unknown = call(&router, "GET", "/t/zz/Patient/p1", &[], None).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let other_domain = call(
        &router,
        "GET",
        "/Patient/p1",
        &[("host", "a.elsewhere.org")],
        None,
    )
    .await;
    assert_eq!(
        other_domain.status,
        StatusCode::NOT_FOUND,
        "{}",
        other_domain.body
    );
    let deeper = call(
        &router,
        "GET",
        "/Patient/p1",
        &[("host", "x.a.fhir.example.com")],
        None,
    )
    .await;
    assert_eq!(deeper.status, StatusCode::NOT_FOUND);

    // The process's own probes, and each tenant's.
    let health = call(&router, "GET", "/healthz", &[], None).await;
    assert_eq!(health.status, StatusCode::OK, "{}", health.body);
    assert_eq!(health.body["tenants"]["a"]["status"], "ready");
    let tenant_health = call(&router, "GET", "/t/a/healthz", &[], None).await;
    assert_eq!(tenant_health.body["tenant"], "a", "{}", tenant_health.body);

    // Each tenant names its own address in SMART discovery, whichever Host the request came in on,
    // and publishes its own health card key.
    let mut keys = Vec::new();
    for tenant in ["a", "b"] {
        let discovery = call(
            &router,
            "GET",
            &format!("/t/{tenant}/.well-known/smart-configuration"),
            &[("host", "internal.cluster:8080")],
            None,
        )
        .await;
        assert_eq!(discovery.status, StatusCode::OK, "{}", discovery.body);
        let issuer = format!("https://fhir.example.com/t/{tenant}");
        assert_eq!(discovery.body["issuer"], issuer);
        assert_eq!(
            discovery.body["token_endpoint"],
            format!("{issuer}/oauth/token")
        );
        assert_eq!(
            discovery.body["jwks_uri"],
            format!("{issuer}/.well-known/jwks.json")
        );
        let jwks = call(
            &router,
            "GET",
            &format!("/t/{tenant}/health-cards/.well-known/jwks.json"),
            &[],
            None,
        )
        .await;
        assert_eq!(jwks.status, StatusCode::OK, "{}", jwks.body);
        keys.push(jwks.body["keys"][0]["kid"].clone());
    }
    assert_ne!(keys[0], keys[1], "two tenants publish one key");
}

#[tokio::test]
async fn sync_on_one_tenant_hears_only_its_own_writes() {
    let host = two_tenants();
    let router = host.router();
    seed(&router).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    {
        let router = router.clone();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    }
    let (mut socket, _) = connect(&format!("ws://{addr}/t/a/sync")).await;
    socket
        .send(Message::Text(
            json!({"type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1, "modifications": [
                {"type": "Add", "queryId": 1, "udfPath": "Patient/search", "args": {"active": "true"}}
            ]})
            .to_string(),
        ))
        .await
        .unwrap();
    let first = next_json(&mut socket).await;
    let entries = &first["modifications"][0]["value"]["entry"];
    assert_eq!(first["modifications"][0]["value"]["total"], 1, "{first}");
    assert_eq!(entries[0]["resource"]["name"][0]["family"], "Ann");

    // A write in `b` that would join the result in `a` is not `a`'s.
    let made = put(
        &router,
        "b",
        json!({"resourceType": "Patient", "id": "p2", "active": true}),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED);
    let quiet = tokio::time::timeout(Duration::from_millis(700), socket.next()).await;
    assert!(quiet.is_err(), "a heard b's write: {quiet:?}");

    // One in `a` is.
    put(
        &router,
        "a",
        json!({"resourceType": "Patient", "id": "p3", "active": true}),
    )
    .await;
    let joined = next_json(&mut socket).await;
    assert_eq!(joined["modifications"][0]["value"]["total"], 2, "{joined}");

    // A sync mutation runs in the tenant the socket is on.
    socket
        .send(Message::Text(
            json!({"type": "Mutation", "requestId": 1, "udfPath": "Patient/create",
                   "args": {"resource": {"resourceType": "Patient", "active": false}}})
            .to_string(),
        ))
        .await
        .unwrap();
    let _ = next_json(&mut socket).await;
    let a_count = call(&router, "GET", "/t/a/Patient", &[], None).await.body["total"].clone();
    let b_count = call(&router, "GET", "/t/b/Patient", &[], None).await.body["total"].clone();
    assert_eq!((a_count, b_count), (json!(3), json!(2)));
}

#[tokio::test]
async fn a_subscription_is_told_only_of_its_own_tenants_commits() {
    let host = two_tenants();
    let router = host.router();
    // The same Subscription id in both tenants.
    for tenant in ["a", "b"] {
        let made = put(
            &router,
            tenant,
            json!({"resourceType": "Subscription", "id": "s1", "status": "active",
                   "criteria": "Patient?active=true", "channel": {"type": "websocket"}}),
        )
        .await;
        assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    {
        let router = router.clone();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    }
    let (mut a, _) = connect(&format!("ws://{addr}/t/a/Subscription/s1/$websocket")).await;
    let (mut b, _) = connect(&format!("ws://{addr}/t/b/Subscription/s1/$websocket")).await;
    // Both sockets are registered once the upgrade has been answered.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let made = put(
        &router,
        "b",
        json!({"resourceType": "Patient", "id": "only-b", "active": true}),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED);
    let heard = tokio::time::timeout(Duration::from_secs(5), b.next())
        .await
        .expect("b's socket heard nothing")
        .unwrap()
        .unwrap();
    assert!(heard.to_text().unwrap().contains("only-b"), "{heard:?}");
    let quiet = tokio::time::timeout(Duration::from_millis(700), a.next()).await;
    assert!(quiet.is_err(), "a's socket heard b's commit: {quiet:?}");
}

#[tokio::test]
async fn over_sync_a_tenant_signs_its_cards_as_its_rest_does_with_its_issuer_and_newest_key() {
    use crate::tests::{assert_same_cards, card_header, card_parameters, checked_claims};
    // A tenant that names an issuer of its own, its key kept in its database as `--all-tenants`
    // keeps it, and a tenant with cards off.
    let keys = refhir_persistence::MemoryAuthStore::default();
    let key = Arc::new(crate::IssuerKey::for_tenant(&keys, true).unwrap());
    key.set_issuer(Some("https://health.clinic.example/"))
        .unwrap();
    let (db, catalog, store) = crate::tests::cards_parts();
    let clinic = tenant_app(
        db,
        catalog,
        store,
        ServerConfig {
            tenant: Some("clinic".into()),
            public_url: Some("https://fhir.example.com/t/clinic".into()),
            health_cards: Some(key.clone()),
            ..ServerConfig::default()
        },
    );
    let (db, catalog, store) = subscription_parts();
    let nocards = tenant_app(
        db,
        catalog,
        store,
        ServerConfig {
            tenant: Some("nocards".into()),
            public_url: Some("https://fhir.example.com/t/nocards".into()),
            ..ServerConfig::default()
        },
    );
    let host = TenantHost::new(HostConfig {
        routing: Routing {
            path_prefix: Some("/t".into()),
            domain: None,
            public_url: Some("https://fhir.example.com".into()),
        },
        admin_token: Some(ADMIN_TOKEN.into()),
        role: "primary".into(),
        ..HostConfig::default()
    });
    for (name, app) in [("clinic", clinic), ("nocards", nocards)] {
        host.set(
            name,
            TenantState::Ready(Arc::new(app)),
            TenantLimits::default(),
        );
    }
    host.set_ready();
    let router = host.router();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    {
        let router = router.clone();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    }

    // The socket is open before the key is replaced, as one would be when the admin rotates it.
    let (mut socket, _) = connect(&format!("ws://{addr}/t/clinic/sync")).await;
    let first = key.kid();
    let rotated = crate::IssuerKey::rotate_tenant(&keys).unwrap();
    assert!(key.refresh(&keys).unwrap());
    assert_eq!(rotated.replaced, first);

    let parameters = card_parameters(json!([]));
    socket
        .send(Message::Text(
            json!({"type": "Mutation", "requestId": 7, "udfPath": "Patient/$health-cards-issue",
                   "args": {"id": "p1", "resource": parameters}})
            .to_string(),
        ))
        .await
        .unwrap();
    let synced = next_json(&mut socket).await;
    assert_eq!(synced["type"], "MutationResponse", "{synced}");
    assert_eq!(synced["success"], true, "{synced}");
    let over_sync = &synced["result"];
    let jwks = call(
        &router,
        "GET",
        "/t/clinic/health-cards/.well-known/jwks.json",
        &[],
        None,
    )
    .await
    .body;
    let card = over_sync["parameter"][0]["valueString"].as_str().unwrap();
    // The tenant's issuer, and the key that replaced the first; the key set checks it.
    assert_eq!(card_header(card)["kid"], json!(rotated.kid));
    assert_eq!(
        checked_claims(card, &jwks)["iss"],
        "https://health.clinic.example"
    );
    // Its links are the tenant's own addresses.
    let link = over_sync["parameter"]
        .as_array()
        .unwrap()
        .iter()
        .find(|parameter| parameter["name"] == "resourceLink")
        .expect("a resourceLink");
    assert!(
        link["part"][2]["valueUri"]
            .as_str()
            .unwrap()
            .starts_with("https://fhir.example.com/t/clinic/"),
        "{link}"
    );
    let rest = call(
        &router,
        "POST",
        "/t/clinic/Patient/p1/$health-cards-issue",
        &[],
        Some(parameters.clone()),
    )
    .await;
    assert_eq!(rest.status, StatusCode::OK, "{}", rest.body);
    assert_same_cards(over_sync, &rest.body, &jwks);

    // A tenant with cards off answers what its REST answers, over the socket too.
    let rest = call(
        &router,
        "POST",
        "/t/nocards/Patient/p1/$health-cards-issue",
        &[],
        Some(parameters.clone()),
    )
    .await;
    assert_eq!(rest.status, StatusCode::NOT_IMPLEMENTED, "{}", rest.body);
    let (mut socket, _) = connect(&format!("ws://{addr}/t/nocards/sync")).await;
    socket
        .send(Message::Text(
            json!({"type": "Mutation", "requestId": 8, "udfPath": "Patient/$health-cards-issue",
                   "args": {"id": "p1", "resource": parameters}})
            .to_string(),
        ))
        .await
        .unwrap();
    let refused = next_json(&mut socket).await;
    assert_eq!(refused["success"], false, "{refused}");
    assert_eq!(refused["result"], rest.body);
}

#[tokio::test]
async fn a_stopped_tenant_answers_gone_and_closes_its_sockets() {
    let host = two_tenants();
    let router = host.router();
    seed(&router).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    {
        let router = router.clone();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    }
    let (mut socket, _) = connect(&format!("ws://{addr}/t/a/sync")).await;

    let TenantState::Ready(app) = host.set_state("a", TenantState::Deleted).unwrap() else {
        panic!("a was not ready");
    };
    app.shut_down().await;
    let gone = call(&router, "GET", "/t/a/Patient/p1", &[], None).await;
    assert_eq!(gone.status, StatusCode::GONE, "{}", gone.body);
    // The socket that was open ends rather than keep the tenant alive.
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the socket stayed open");
    // The other tenant is untouched.
    assert_eq!(
        call(&router, "GET", "/t/b/Patient/p1", &[], None)
            .await
            .status,
        StatusCode::OK
    );
    host.set_state("b", TenantState::Starting);
    let starting = call(&router, "GET", "/t/b/Patient/p1", &[], None).await;
    assert_eq!(starting.status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn a_tenant_over_its_rate_or_storage_is_refused_and_counted() {
    let host = two_tenants();
    let router = host.router();
    let ready = |name: &str| match host.state(name).unwrap() {
        TenantState::Ready(app) => TenantState::Ready(app),
        _ => unreachable!(),
    };
    host.set(
        "a",
        ready("a"),
        TenantLimits {
            requests_per_second: Some(0.001),
            burst: Some(2),
            ..TenantLimits::default()
        },
    );
    for _ in 0..2 {
        assert_eq!(
            call(&router, "GET", "/t/a/metadata", &[], None)
                .await
                .status,
            StatusCode::OK
        );
    }
    let refused = call(&router, "GET", "/t/a/metadata", &[], None).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.body["issue"][0]["code"], "throttled");
    // The other tenant has its own budget.
    assert_eq!(
        call(&router, "GET", "/t/b/metadata", &[], None)
            .await
            .status,
        StatusCode::OK
    );

    host.set(
        "b",
        ready("b"),
        TenantLimits {
            storage_bytes: Some(10),
            ..TenantLimits::default()
        },
    );
    host.set_storage("b", 11);
    let write = put(
        &router,
        "b",
        json!({"resourceType": "Patient", "id": "big"}),
    )
    .await;
    assert_eq!(
        write.status,
        StatusCode::INSUFFICIENT_STORAGE,
        "{}",
        write.body
    );
    // Reads and deletes still work, and so do the POSTs that store nothing.
    assert_eq!(
        call(&router, "GET", "/t/b/Patient", &[], None).await.status,
        StatusCode::OK
    );
    let search = call(
        &router,
        "POST",
        "/t/b/Patient/_search",
        &[],
        Some(json!({})),
    )
    .await;
    assert_ne!(
        search.status,
        StatusCode::INSUFFICIENT_STORAGE,
        "{}",
        search.body
    );
    let token = call(&router, "POST", "/t/b/oauth/token", &[], Some(json!({}))).await;
    assert_ne!(
        token.status,
        StatusCode::INSUFFICIENT_STORAGE,
        "{}",
        token.body
    );
    let batch = call(
        &router,
        "POST",
        "/t/b/",
        &[],
        Some(json!({"resourceType": "Bundle", "type": "batch"})),
    )
    .await;
    assert_eq!(batch.status, StatusCode::INSUFFICIENT_STORAGE);
    host.set_storage("b", 9);
    let write = put(
        &router,
        "b",
        json!({"resourceType": "Patient", "id": "big"}),
    )
    .await;
    assert_eq!(write.status, StatusCode::CREATED);

    let a = host.counters("a").unwrap().json();
    assert_eq!(a["throttled"], 1);
    assert_eq!(a["requests"], 2);
    let b = host.counters("b").unwrap().json();
    // The write that went over, and the batch.
    assert_eq!(b["refused_storage"], 2);
}

/// What the admin API was asked, for a test to look at.
#[derive(Default)]
struct Recorder {
    created: std::sync::Mutex<Vec<String>>,
    /// The Health Cards settings each create resolved to, and the changes `update` was given.
    cards: std::sync::Mutex<Vec<Value>>,
    rotated: std::sync::Mutex<Vec<String>>,
}

impl TenantAdmin for Recorder {
    fn create(&self, request: CreateTenant) -> Result<(Value, bool), AdminError> {
        self.created.lock().unwrap().push(request.name.clone());
        let cards = request
            .health_cards
            .resolve(&HealthCardsSettings::default(), true)
            .unwrap();
        self.cards.lock().unwrap().push(cards.report());
        Ok((json!({"name": request.name}), true))
    }
    fn list(&self) -> Result<Vec<Value>, AdminError> {
        Ok(vec![json!({"name": "a"}), json!({"name": "b"})])
    }
    fn get(&self, name: &str) -> Result<Value, AdminError> {
        if name == "a" {
            Ok(json!({"name": "a"}))
        } else {
            Err(AdminError::not_found(name))
        }
    }
    fn delete(&self, name: &str, request: DeleteTenant) -> Result<Value, AdminError> {
        Ok(
            json!({"name": name, "grace": request.grace.map(|grace| grace.as_secs()), "export": request.export}),
        )
    }
    fn restore(&self, name: &str) -> Result<Value, AdminError> {
        Ok(json!({"name": name}))
    }
    fn update(&self, name: &str, request: UpdateTenant) -> Result<Value, AdminError> {
        let change = request.health_cards.unwrap_or_default();
        self.cards.lock().unwrap().push(json!({
            "enabled": change.enabled,
            "issuer": change.issuer,
        }));
        Ok(json!({"name": name}))
    }
    fn rotate_health_cards(&self, name: &str) -> Result<Value, AdminError> {
        self.rotated.lock().unwrap().push(name.to_string());
        Ok(json!({"tenant": name, "kid": "new", "replaced_kid": "old", "keys": ["new", "old"]}))
    }
}

#[tokio::test]
async fn the_admin_api_refuses_requests_without_the_admin_token() {
    let host = two_tenants();
    let recorder = Arc::new(Recorder::default());
    host.set_admin(recorder.clone());
    let router = host.router();
    let bearer = format!("Bearer {ADMIN_TOKEN}");
    let create = json!({"name": "c"});

    for headers in [
        vec![],
        vec![("authorization", "Bearer wrong")],
        vec![("authorization", "Basic YWRtaW4tc2VjcmV0")],
        // A token for a tenant's FHIR API is not one for the admin API.
        vec![("authorization", "Bearer eyJhbGciOiJSUzI1NiJ9.e30.x")],
    ] {
        for (method, uri, body) in [
            ("GET", "/admin/tenants", None),
            ("POST", "/admin/tenants", Some(create.clone())),
            ("GET", "/admin/tenants/a", None),
            ("DELETE", "/admin/tenants/a", None),
            ("POST", "/admin/tenants/a/restore", None),
            (
                "PATCH",
                "/admin/tenants/a",
                Some(json!({"health_cards": true})),
            ),
            ("POST", "/admin/tenants/a/health-cards/rotate", None),
            ("GET", "/admin/metrics", None),
        ] {
            let reply = call(&router, method, uri, &headers, body).await;
            assert_eq!(
                reply.status,
                StatusCode::UNAUTHORIZED,
                "{method} {uri} {headers:?}"
            );
        }
    }
    // The admin API is not reachable through a tenant either.
    let through = call(
        &router,
        "GET",
        "/t/a/admin/tenants",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_ne!(through.status, StatusCode::OK);
    assert!(recorder.created.lock().unwrap().is_empty());

    let listed = call(
        &router,
        "GET",
        "/admin/tenants",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert_eq!(listed.body["tenants"][0]["served"]["status"], "ready");
    let created = call(
        &router,
        "POST",
        "/admin/tenants",
        &[("authorization", &bearer)],
        Some(create),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(*recorder.created.lock().unwrap(), ["c"]);
    let missing = call(
        &router,
        "GET",
        "/admin/tenants/zz",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let deleted = call(
        &router,
        "DELETE",
        "/admin/tenants/a?grace=36h&export=false",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(deleted.body["grace"], 36 * 3600, "{}", deleted.body);
    assert_eq!(deleted.body["export"], false);
    let bad = call(
        &router,
        "DELETE",
        "/admin/tenants/a?grace=soon",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let unknown_field = call(
        &router,
        "POST",
        "/admin/tenants",
        &[("authorization", &bearer)],
        Some(json!({"name": "d", "smrt": true})),
    )
    .await;
    assert_eq!(unknown_field.status, StatusCode::BAD_REQUEST);

    let metrics = call(
        &router,
        "GET",
        "/admin/metrics",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    let text = metrics.body.as_str().unwrap().to_string();
    assert!(text.contains("refhir_tenant_up{tenant=\"a\"} 1"), "{text}");
    assert!(
        text.contains("# TYPE refhir_tenant_responses_total counter"),
        "{text}"
    );

    // Without a token configured, the admin API is off.
    let off = TenantHost::new(HostConfig {
        routing: Routing {
            path_prefix: Some("/t".into()),
            ..Routing::default()
        },
        role: "primary".into(),
        ..HostConfig::default()
    });
    off.set_admin(recorder);
    let reply = call(
        &off.router(),
        "GET",
        "/admin/tenants",
        &[("authorization", "Bearer ")],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_tenants_health_card_settings_are_a_flag_or_an_object_and_a_bad_issuer_changes_nothing() {
    let host = two_tenants();
    let recorder = Arc::new(Recorder::default());
    host.set_admin(recorder.clone());
    let router = host.router();
    let auth = format!("Bearer {ADMIN_TOKEN}");
    let headers = [("authorization", auth.as_str())];

    // The flag, as before, and the object with an issuer (its trailing slash is trimmed).
    for (name, cards) in [
        ("flag", json!(true)),
        ("off", json!(false)),
        (
            "own",
            json!({"enabled": true, "issuer": "https://cards.example.com/"}),
        ),
        (
            "only-an-issuer",
            json!({"issuer": "http://localhost:8080/c"}),
        ),
        ("none", Value::Null),
    ] {
        let mut body = json!({"name": name});
        if !cards.is_null() {
            body["health_cards"] = cards;
        }
        let reply = call(&router, "POST", "/admin/tenants", &headers, Some(body)).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{name}: {}", reply.body);
    }
    assert_eq!(
        *recorder.cards.lock().unwrap(),
        [
            json!({"enabled": true, "issuer": null}),
            json!({"enabled": false, "issuer": null}),
            json!({"enabled": true, "issuer": "https://cards.example.com"}),
            json!({"enabled": true, "issuer": "http://localhost:8080/c"}),
            json!({"enabled": false, "issuer": null}),
        ]
    );

    // An issuer a verifier would not accept is refused at the API, and nothing is asked of the
    // tenant: not created, not changed.
    let before = recorder.cards.lock().unwrap().len();
    for issuer in [
        json!("http://cards.example.com"),
        json!("https://cards.example.com/?a=b"),
        json!("https://cards.example.com/#frag"),
        json!("https://user:pw@cards.example.com"),
        json!("https://user@cards.example.com"),
        json!(""),
        json!("/"),
        json!("cards.example.com"),
        json!("ftp://cards.example.com"),
        json!(7),
    ] {
        let created = call(
            &router,
            "POST",
            "/admin/tenants",
            &headers,
            Some(json!({"name": "bad", "health_cards": {"enabled": true, "issuer": issuer}})),
        )
        .await;
        assert_eq!(
            created.status,
            StatusCode::BAD_REQUEST,
            "{issuer}: {}",
            created.body
        );
        let patched = call(
            &router,
            "PATCH",
            "/admin/tenants/a",
            &headers,
            Some(json!({"health_cards": {"issuer": issuer}})),
        )
        .await;
        assert_eq!(
            patched.status,
            StatusCode::BAD_REQUEST,
            "{issuer}: {}",
            patched.body
        );
    }
    // An issuer for a tenant that is not to issue cards says nothing the tenant can do.
    for body in [
        json!({"name": "bad", "health_cards": {"enabled": false, "issuer": "https://x.example"}}),
        json!({"name": "bad", "health_cards": "yes"}),
        json!({"name": "bad", "health_cards": {"enabled": true, "colour": 1}}),
    ] {
        let created = call(
            &router,
            "POST",
            "/admin/tenants",
            &headers,
            Some(body.clone()),
        )
        .await;
        assert_eq!(
            created.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            created.body
        );
    }
    for body in [
        json!({"health_cards": {"enabled": false, "issuer": "https://x.example"}}),
        json!({"health_cards": "yes"}),
        json!({"other": 1}),
    ] {
        let patched = call(
            &router,
            "PATCH",
            "/admin/tenants/a",
            &headers,
            Some(body.clone()),
        )
        .await;
        assert_eq!(
            patched.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            patched.body
        );
    }
    assert_eq!(
        recorder.cards.lock().unwrap().len(),
        before,
        "nothing was asked"
    );
    assert_eq!(recorder.created.lock().unwrap().len(), 5);

    // A change says only what it changes; `null` takes the issuer away.
    for body in [
        json!({"health_cards": {"issuer": "https://cards.example.com"}}),
        json!({"health_cards": {"issuer": null}}),
        json!({"health_cards": false}),
    ] {
        let patched = call(&router, "PATCH", "/admin/tenants/a", &headers, Some(body)).await;
        assert_eq!(patched.status, StatusCode::OK, "{}", patched.body);
    }
    assert_eq!(
        recorder.cards.lock().unwrap()[before..],
        [
            json!({"enabled": null, "issuer": "https://cards.example.com"}),
            json!({"enabled": null, "issuer": null}),
            json!({"enabled": false, "issuer": null}),
        ]
    );

    let rotated = call(
        &router,
        "POST",
        "/admin/tenants/a/health-cards/rotate",
        &headers,
        None,
    )
    .await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.body);
    assert_eq!(rotated.body["kid"], "new");
    assert_eq!(*recorder.rotated.lock().unwrap(), ["a"]);
}

#[test]
fn a_change_to_health_cards_turns_them_on_only_for_an_issuer_and_off_with_its_issuer() {
    let issuer = |text: &str| HealthCardsChange {
        enabled: None,
        issuer: Some(Some(text.into())),
    };
    let none = HealthCardsSettings::default();
    // A new tenant given only an issuer issues cards; one given `null` (or nothing) does not.
    let on = issuer("https://cards.example.com/")
        .resolve(&none, true)
        .unwrap();
    assert!(on.enabled);
    assert_eq!(on.issuer.as_deref(), Some("https://cards.example.com"));
    let null = HealthCardsChange {
        enabled: None,
        issuer: Some(None),
    };
    assert_eq!(null.resolve(&none, true).unwrap(), none);
    assert_eq!(
        HealthCardsChange::default().resolve(&none, true).unwrap(),
        none
    );
    // Turning cards off takes the issuer they named; asking for both is refused.
    let off = HealthCardsChange {
        enabled: Some(false),
        issuer: None,
    };
    assert_eq!(off.resolve(&on, false).unwrap(), none);
    let both = HealthCardsChange {
        enabled: Some(false),
        issuer: Some(Some("https://cards.example.com".into())),
    };
    assert!(both.resolve(&on, false).is_err());
    // Leaving the issuer out of a change that keeps cards on keeps it.
    let keep = HealthCardsChange {
        enabled: Some(true),
        issuer: None,
    };
    assert_eq!(keep.resolve(&on, false).unwrap(), on);
}

#[test]
fn health_card_settings_are_stored_as_the_flag_when_there_is_no_issuer() {
    // What a release that only had the flag wrote reads back, and writes back the same.
    for (stored, settings) in [
        (json!(true), HealthCardsSettings::on()),
        (json!(false), HealthCardsSettings::default()),
        (
            json!({"enabled": true, "issuer": "https://cards.example.com"}),
            HealthCardsSettings {
                enabled: true,
                issuer: Some("https://cards.example.com".into()),
            },
        ),
    ] {
        let read: HealthCardsSettings = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(read, settings);
        assert_eq!(serde_json::to_value(&read).unwrap(), stored);
    }
    let old: TenantSettings = serde_json::from_value(json!({"health_cards": true})).unwrap();
    assert!(old.health_cards.enabled);
    let none: TenantSettings = serde_json::from_value(json!({})).unwrap();
    assert_eq!(none.health_cards, HealthCardsSettings::default());
    assert_eq!(
        serde_json::to_value(&none).unwrap()["health_cards"],
        json!(false)
    );
}

#[tokio::test]
async fn a_replica_or_standby_does_not_change_tenants_itself() {
    let host = TenantHost::new(HostConfig {
        routing: Routing {
            path_prefix: Some("/t".into()),
            ..Routing::default()
        },
        admin_token: Some(ADMIN_TOKEN.into()),
        role: "standby".into(),
        ..HostConfig::default()
    });
    let recorder = Arc::new(Recorder::default());
    host.set_admin(recorder.clone());
    let bearer = format!("Bearer {ADMIN_TOKEN}");
    let reply = call(
        &host.router(),
        "POST",
        "/admin/tenants",
        &[("authorization", &bearer)],
        Some(json!({"name": "c"})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    host.set_role("replica");
    let reply = call(
        &host.router(),
        "POST",
        "/admin/tenants",
        &[("authorization", &bearer)],
        Some(json!({"name": "c"})),
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "no primary to send it to"
    );
    assert!(recorder.created.lock().unwrap().is_empty());
    // Reading is answered where it is asked.
    let reply = call(
        &host.router(),
        "GET",
        "/admin/tenants",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[test]
fn only_requests_that_can_store_resources_count_against_the_storage_limit() {
    for (method, path, stores) in [
        (Method::PUT, "/Patient/p1", true),
        (Method::PATCH, "/Patient/p1", true),
        (Method::POST, "/", true),
        (Method::POST, "/Patient", true),
        (Method::POST, "/Patient/$extract", true),
        (Method::POST, "/Patient/p1/$apply", true),
        (Method::POST, "/Patient/_search", false),
        (Method::POST, "/Patient/$match", false),
        (Method::POST, "/Patient/$validate", false),
        (Method::POST, "/ValueSet/$expand", false),
        (Method::POST, "/oauth/token", false),
        (Method::POST, "/oauth/revoke", false),
        (Method::POST, "/graphql", false),
        (Method::DELETE, "/Patient/p1", false),
        (Method::GET, "/Patient", false),
    ] {
        assert_eq!(stores_more(&method, path), stores, "{method} {path}");
    }
}

#[test]
fn each_tenant_has_its_own_address_and_secrets() {
    let by_path = Routing {
        path_prefix: Some("/t".into()),
        domain: None,
        public_url: Some("https://fhir.example.com/".into()),
    };
    assert!(by_path.validate().is_ok());
    assert_eq!(
        by_path.public_url("acme").as_deref(),
        Some("https://fhir.example.com/t/acme")
    );
    let by_host = Routing {
        path_prefix: None,
        domain: Some("fhir.example.com".into()),
        public_url: Some("https://{tenant}.fhir.example.com".into()),
    };
    assert!(by_host.validate().is_ok());
    assert_eq!(
        by_host.public_url("acme").as_deref(),
        Some("https://acme.fhir.example.com")
    );
    for bad in [
        Routing::default(),
        Routing {
            path_prefix: Some("t".into()),
            ..Routing::default()
        },
        Routing {
            path_prefix: Some("/t/".into()),
            ..Routing::default()
        },
        Routing {
            path_prefix: Some("/admin".into()),
            ..Routing::default()
        },
        Routing {
            domain: Some(".x.org".into()),
            ..Routing::default()
        },
        // Told apart by host name, every tenant would get the same address.
        Routing {
            domain: Some("x.org".into()),
            public_url: Some("https://x.org".into()),
            ..Routing::default()
        },
        Routing {
            path_prefix: Some("/t".into()),
            public_url: Some("ftp://x.org".into()),
            ..Routing::default()
        },
    ] {
        assert!(bad.validate().is_err(), "{bad:?}");
    }

    let a = tenant_secret("process", "a", "smart-dcr-token");
    assert_eq!(a, tenant_secret("process", "a", "smart-dcr-token"));
    assert_ne!(a, tenant_secret("process", "b", "smart-dcr-token"));
    assert_ne!(a, tenant_secret("process", "a", "smart-confidential"));
    assert_ne!(a, tenant_secret("other", "a", "smart-dcr-token"));

    assert_eq!(admin::parse_duration("0").unwrap(), Duration::ZERO);
    assert_eq!(
        admin::parse_duration("15m").unwrap(),
        Duration::from_secs(900)
    );
    assert_eq!(
        admin::parse_duration("7d").unwrap(),
        Duration::from_secs(7 * 86_400)
    );
    for bad in ["", "d", "1w", "-1s", "1.5h"] {
        assert!(admin::parse_duration(bad).is_err(), "{bad}");
    }
    assert!(TenantLimits {
        requests_per_second: Some(0.0),
        ..TenantLimits::default()
    }
    .validate()
    .is_err());
    assert!(TenantLimits {
        connections: Some(0),
        ..TenantLimits::default()
    }
    .validate()
    .is_err());
}
