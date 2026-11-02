use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use refhir_database::Database;
use refhir_fhir::{
    cards_dir, extra_dir, install_builtin_terminology, install_package_dir, minir4_dir, r4_dir,
    sdc_dir, Catalog, ConformanceStore, MAX_OCC_FAILURES,
};
use refhir_sqlite::SqlitePersistence;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

use crate::app;

/// A server over the minimal R4 package. Most tests need resources with ids they know in
/// advance, so update-as-create is on here (a PUT to a new id creates it); `strict()` is the
/// default configuration.
pub(crate) fn memory() -> (
    Arc<Database>,
    Arc<RwLock<Catalog>>,
    Arc<dyn ConformanceStore>,
) {
    let (db, catalog, store) = strict();
    catalog.write().unwrap().set_update_create(true);
    (db, catalog, store)
}

pub(crate) fn strict() -> (
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

pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) body: Value,
    pub(crate) location: Option<String>,
}

pub(crate) async fn send(
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

/// Create `resource` under the id it names: `PUT [type]/[id]`, which `memory()` allows.
async fn put(app: &axum::Router, resource: Value) -> Reply {
    let uri = format!(
        "/{}/{}",
        resource["resourceType"].as_str().unwrap(),
        resource["id"].as_str().unwrap()
    );
    send(app, "PUT", &uri, Some(resource), None).await
}

pub(crate) fn patient(id: &str) -> Value {
    json!({
        "resourceType": "Patient",
        "id": id,
        "active": true,
        "name": [{"family": "Ann"}]
    })
}

/// A window of time asked for in one zone finds what was written in another, over HTTP and the way
/// a billing client sends it: `date` twice, an offset written `%2B01:00`. The same moment is found
/// whatever zone it was stored in and whatever zone the window is written in, and a moment whose
/// clock reads inside the window but is outside it is not.
#[tokio::test]
async fn http_date_window_compares_moments_whatever_zone_they_are_written_in() {
    let (db, catalog, store) = memory();
    let app = app(db, catalog, store);
    let created = put(&app, patient("p1")).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    for (id, written) in [
        ("z", "2026-10-31T22:30:00Z"),
        ("plus01", "2026-10-31T23:30:00+01:00"),
        // 2026-11-01T03:30:00Z, though its clock reads 22:30 on the 31st.
        ("late", "2026-10-31T22:30:00-05:00"),
        // 2026-09-30T10:30:00Z, though its clock reads the 1st of October.
        ("early", "2026-10-01T00:30:00+14:00"),
    ] {
        let made = put(
            &app,
            json!({"resourceType": "Observation", "id": id, "status": "final",
                "code": {"text": "visit"}, "subject": {"reference": "Patient/p1"},
                "effectiveDateTime": written}),
        )
        .await;
        assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
    }
    let ids = |reply: &Reply| -> Vec<String> {
        let mut found: Vec<String> = reply.body["entry"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default();
        found.sort();
        found
    };
    // The window 2026-09-30T23:00:00Z up to 2026-10-31T23:00:00Z, in UTC and in other zones.
    for uri in [
        "/Observation?date=ge2026-09-30T23:00:00Z&date=lt2026-10-31T23:00:00Z",
        "/Observation?date=ge2026-10-01T00:00:00%2B01:00&date=lt2026-11-01T00:00:00%2B01:00",
        "/Observation?date=ge2026-09-30T18:00:00-05:00&date=lt2026-10-31T18:00:00-05:00",
        "/Observation?date=ge2026-10-01T04:30:00%2B05:30&date=lt2026-10-31T23:00:00Z",
    ] {
        let reply = send(&app, "GET", uri, None, None).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.body);
        assert_eq!(ids(&reply), ["plus01", "z"], "{uri}");
    }
    // The clinic's day, 2026-11-01 at +01:00, holds only the moment that is in it.
    let day = send(
        &app,
        "GET",
        "/Observation?date=ge2026-11-01T00:00:00%2B01:00&date=lt2026-11-02T00:00:00%2B01:00",
        None,
        None,
    )
    .await;
    assert_eq!(ids(&day), ["late"]);
    // A `+` that is not encoded reads as a space, and the answer says so instead of finding
    // nothing.
    let plain = send(
        &app,
        "GET",
        "/Observation?date=ge2026-10-01T00:00:00+01:00",
        None,
        None,
    )
    .await;
    assert_eq!(plain.status, StatusCode::BAD_REQUEST);
    assert!(plain.body.to_string().contains("%2B"), "{}", plain.body);
    // Newest first, by the moment and not by the clock in the text.
    let sorted = send(&app, "GET", "/Observation?_sort=-date", None, None).await;
    let order: Vec<&str> = sorted.body["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["resource"]["id"].as_str().unwrap())
        .collect();
    // The two that are the same moment are in the order of their ids.
    assert_eq!(order, ["late", "plus01", "z", "early"]);
}

/// The searches a booking client sends: a comma between values is OR, a parameter given twice is
/// AND, and `_include:iterate` is a control of the search, not an unknown parameter.
#[tokio::test]
async fn http_search_takes_comma_or_repeated_and_and_iterate_includes() {
    let (db, catalog, store) = memory();
    let app = app(db, catalog, store);
    for (id, family) in [("p1", "Ann"), ("p2", "Bee")] {
        let created = put(
            &app,
            json!({"resourceType": "Patient", "id": id, "name": [{"family": family}]}),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    }
    for (id, subject, code, date) in [
        ("o1", "p1", "8480-6", "2020-01-02"),
        ("o2", "p2", "8462-4", "2018-01-02"),
        ("o3", "p1", "8867-4", "2022-01-02"),
    ] {
        let created = put(
            &app,
            json!({
                "resourceType": "Observation", "id": id,
                "subject": {"reference": format!("Patient/{subject}")},
                "code": {"coding": [{"code": code}]},
                "effectiveDateTime": date
            }),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    }
    let ids = |reply: &Reply, mode: &str| -> Vec<String> {
        let mut found: Vec<String> = reply.body["entry"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["search"]["mode"] == mode)
            .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string())
            .collect();
        found.sort();
        found
    };
    let search = |uri: &'static str| {
        let app = app.clone();
        async move {
            let reply = send(&app, "GET", uri, None, None).await;
            assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.body);
            reply
        }
    };

    // Ids: a comma is OR.
    assert_eq!(search("/Patient?_id=p1").await.body["total"], 1);
    assert_eq!(search("/Patient?_id=p1,zzz").await.body["total"], 1);
    assert_eq!(search("/Patient?_id=p1,p2").await.body["total"], 2);
    assert_eq!(search("/Patient?_id=zzz,yyy").await.body["total"], 0);
    // A token: a comma is OR, and a page is a page of the union.
    assert_eq!(search("/Observation?code=8480-6").await.body["total"], 1);
    let either = search("/Observation?code=8480-6,8462-4&_count=1").await;
    assert_eq!(either.body["total"], 2);
    assert_eq!(either.body["entry"].as_array().unwrap().len(), 1);
    // Repeated, it is AND: no observation has both codes.
    assert_eq!(
        search("/Observation?code=8480-6&code=8462-4").await.body["total"],
        0
    );
    // A range written as two bounds, with and without a list in the same query.
    let range = search("/Observation?date=ge2019-01-01&date=lt2023-01-01").await;
    assert_eq!(ids(&range, "match"), ["o1", "o3"]);
    let narrowed = search("/Observation?code=8480-6,8867-4&date=ge2021-01-01").await;
    assert_eq!(ids(&narrowed, "match"), ["o3"]);

    // Includes: `:iterate` is accepted, alone or after a plain rule.
    let included = search("/Observation?_id=o1&_include:iterate=Observation:subject").await;
    assert_eq!(ids(&included, "match"), ["o1"]);
    assert_eq!(ids(&included, "include"), ["p1"]);
    let both = search(
        "/Observation?_id=o1&_include=Observation:subject&_include:iterate=Observation:subject",
    )
    .await;
    assert_eq!(ids(&both, "include"), ["p1"]);
    let reverse = search(
        "/Patient?_id=p1&_revinclude=Observation:subject&_revinclude:iterate=Observation:subject",
    )
    .await;
    assert_eq!(ids(&reverse, "include"), ["o1", "o3"]);
    // A reverse wildcard is refused by name, not answered with nothing.
    let refused = send(&app, "GET", "/Patient?_revinclude=*", None, None).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert!(
        refused.body.to_string().contains("not supported"),
        "{}",
        refused.body
    );

    // The CapabilityStatement says what is and is not supported.
    let metadata = send(&app, "GET", "/metadata", None, None).await;
    let notes = metadata.body["rest"][0]["documentation"].as_str().unwrap();
    assert!(
        notes.contains(":iterate") && notes.contains("`_revinclude=*`"),
        "{notes}"
    );
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

    let created = put(&app, patient("p1")).await;
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
                    "fullUrl": "urn:uuid:5b1d1b0e-0000-4000-8000-000000000001",
                    "request": {"method": "POST", "url": "Patient"},
                    "resource": patient("p2")
                },
                {
                    "request": {"method": "POST", "url": "Observation"},
                    "resource": {
                        "resourceType": "Observation",
                        "subject": {"reference": "urn:uuid:5b1d1b0e-0000-4000-8000-000000000001"},
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
    let new_id = bundle.body["entry"][0]["resource"]["id"].as_str().unwrap();
    assert_ne!(
        new_id, "p2",
        "the server assigns the id of a created resource"
    );
    let found = send(
        &app,
        "GET",
        &format!("/Observation?subject=Patient/{new_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(found.body["total"], 1);
}

fn observation_of(id: &str, patient_id: &str) -> Value {
    json!({
        "resourceType": "Observation",
        "id": id,
        "subject": {"reference": format!("Patient/{patient_id}")},
        "code": {"coding": [{"code": "8480-6"}]},
        "effectiveDateTime": "2020-01-02"
    })
}

#[tokio::test]
async fn a_bare_id_searches_like_type_slash_id_over_http_and_over_sync() {
    let (db, catalog, store) = memory();
    let app = app(db, catalog, store);
    for id in ["p1", "p2"] {
        assert_eq!(put(&app, patient(id)).await.status, StatusCode::CREATED);
    }
    for (id, patient_id) in [("o1", "p1"), ("o2", "p2")] {
        let created = put(&app, observation_of(id, patient_id)).await;
        assert_eq!(created.status, StatusCode::CREATED);
    }

    let get = |query: &'static str| {
        let app = app.clone();
        async move { send(&app, "GET", &format!("/Observation?{query}"), None, None).await }
    };
    let bare = get("subject=p1&code=8480-6").await;
    let typed = get("subject=Patient/p1&code=8480-6").await;
    assert_eq!(bare.status, StatusCode::OK);
    assert_eq!(bare.body["total"], 1);
    assert_eq!(bare.body["entry"][0]["resource"]["id"], "o1");
    assert_eq!(bare.body["entry"], typed.body["entry"]);
    assert_eq!(get("subject=nobody").await.body["total"], 0);

    // The same search, kept open, follows the writes that join its result.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let writes = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (mut socket, _) = connect(&format!("ws://{addr}/sync")).await;
    socket
        .send(Message::Text(
            json!({"type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1, "modifications": [
                {"type": "Add", "queryId": 1, "udfPath": "Observation/search", "args": {"subject": "p1"}}
            ]})
            .to_string(),
        ))
        .await
        .unwrap();
    let first = next_json(&mut socket).await;
    assert_eq!(first["modifications"][0]["type"], "QueryUpdated", "{first}");
    assert_eq!(first["modifications"][0]["value"]["total"], 1);
    assert_eq!(
        first["modifications"][0]["value"]["entry"][0]["resource"]["id"],
        "o1"
    );
    // A write about someone else leaves it alone; one about p1 joins it.
    let created = put(&writes, observation_of("o3", "p2")).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let created = put(&writes, observation_of("o4", "p1")).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let joined = next_json(&mut socket).await;
    assert_eq!(
        joined["modifications"][0]["type"], "QueryUpdated",
        "{joined}"
    );
    assert_eq!(joined["modifications"][0]["value"]["total"], 2);
}

#[tokio::test]
async fn a_search_value_that_cannot_be_read_is_a_400_unless_the_client_asks_for_lenient() {
    let (db, catalog, store) = memory();
    let app = app(db, catalog, store);
    assert_eq!(put(&app, patient("p1")).await.status, StatusCode::CREATED);
    let get = |uri: &'static str, prefer: Option<&'static str>| {
        let app = app.clone();
        async move {
            let headers: Vec<(&str, &str)> =
                prefer.map(|value| ("prefer", value)).into_iter().collect();
            let (status, text) = send_text(&app, "GET", uri, "", &headers).await;
            (status, serde_json::from_str::<Value>(&text).unwrap())
        }
    };

    // The default is to refuse, naming the parameter and the value, and not to answer with an empty
    // bundle: a date that is not one matches nothing, and so does a date nobody has.
    for uri in [
        "/Observation?date=notadate",
        "/Patient/p1/Observation?date=notadate",
        "/Patient?name:missing=maybe",
        // Each occurrence of a parameter given twice, and each alternative of a list, is read.
        "/Observation?date=ge2024-01-01&date=notadate",
        "/Observation?date=2024,notadate",
    ] {
        let (status, body) = get(uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
        assert_eq!(body["resourceType"], "OperationOutcome", "{uri}");
        assert_eq!(body["issue"][0]["code"], "invalid", "{uri}");
        let said = body["issue"][0]["diagnostics"].as_str().unwrap();
        assert!(said.starts_with("invalid value "), "{uri}: {said}");
    }
    // A range written as a parameter given twice is AND, and reads.
    let (status, body) = get("/Observation?date=ge2024-01-01&date=le2024-12-31", None).await;
    assert_eq!(
        (status, &body["total"]),
        (StatusCode::OK, &json!(0)),
        "{body}"
    );
    let (status, body) = get("/Observation?date=ge2024-01-01&date=notadate", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["issue"][0]["diagnostics"]
            .as_str()
            .unwrap()
            .contains("\"notadate\""),
        "{body}"
    );
    let (_, body) = get("/Observation?date=notadate", None).await;
    let said = body["issue"][0]["diagnostics"].as_str().unwrap();
    assert!(
        said.contains("\"notadate\"") && said.contains("search parameter date on Observation"),
        "{said}"
    );
    // A value that reads is an ordinary search, and so are the control parameters that already failed.
    let (status, body) = get("/Observation?date=ge2024-01-01", None).await;
    assert_eq!(
        (status, &body["total"]),
        (StatusCode::OK, &json!(0)),
        "{body}"
    );
    let (status, body) = get("/Patient?_count=abc", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // `Prefer: handling=lenient` leaves the parameter out and says so in the bundle.
    for prefer in [
        "handling=lenient",
        "return=minimal, handling=\"lenient\"; q=1",
        "Handling=Lenient",
    ] {
        let (status, body) = get("/Patient?name:missing=maybe&active=true", Some(prefer)).await;
        assert_eq!(status, StatusCode::OK, "{prefer}: {body}");
        assert_eq!(
            body["total"], 1,
            "{prefer}: the rest of the search ran: {body}"
        );
        let entries = body["entry"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "{prefer}: {body}");
        assert_eq!(entries[0]["resource"]["id"], "p1");
        assert_eq!(entries[1]["search"]["mode"], "outcome");
        assert_eq!(entries[1]["resource"]["resourceType"], "OperationOutcome");
        assert_eq!(entries[1]["resource"]["issue"][0]["severity"], "warning");
        // The parameter set aside is not in the link, and the page the server applied is.
        assert_eq!(
            body["link"][0]["url"], "Patient?_count=100&active=true",
            "{prefer}"
        );
    }
    let (status, body) = get(
        "/Patient/p1/Observation?date=notadate",
        Some("handling=lenient"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Of a parameter given twice, only the occurrence that cannot be read is left out.
    let (status, body) = get(
        "/Observation?date=ge2024-01-01&date=notadate",
        Some("handling=lenient"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["link"][0]["url"],
        "Observation?_count=100&date=ge2024-01-01"
    );

    // Only that header asks for it, and only for a value. A query string cannot, strict is a
    // request too, and a parameter that does not exist, or a control parameter that cannot be
    // read, is refused either way.
    for (uri, prefer) in [
        ("/Observation?date=notadate&_handling=lenient", None),
        ("/Observation?date=notadate", Some("handling=strict")),
        ("/Observation?date=notadate", Some("handling=lenient-ish")),
        ("/Observation?date=notadate", Some("return=minimal")),
        ("/Patient?bogus=1", Some("handling=lenient")),
        ("/Patient?_count=abc", Some("handling=lenient")),
    ] {
        let (status, body) = get(uri, prefer).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} {prefer:?}: {body}");
    }

    // A conditional delete that skipped a clause it could not read would match every patient and
    // delete the only one there is.
    let (status, _) = send_text(
        &app,
        "DELETE",
        "/Patient?name:missing=maybe",
        "",
        &[("prefer", "handling=lenient")],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        send(&app, "GET", "/Patient/p1", None, None).await.status,
        StatusCode::OK
    );

    // Nor does a conditional create or update, with the header or without, create the resource
    // because a condition it could not read matched nothing.
    let body = patient("p2").to_string();
    for prefer in [None, Some("handling=lenient")] {
        let mut headers = vec![("content-type", "application/fhir+json")];
        headers.extend(prefer.map(|value| ("prefer", value)));
        let mut create = headers.clone();
        create.push(("if-none-exist", "name:missing=maybe"));
        let (status, _) = send_text(&app, "POST", "/Patient", &body, &create).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "conditional create {prefer:?}"
        );
        let uri = "/Patient?name:missing=maybe";
        let (status, _) = send_text(&app, "PUT", uri, &body, &headers).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "conditional update {prefer:?}"
        );
    }
    let (_, body) = get("/Patient", None).await;
    assert_eq!(body["total"], 1, "nothing was created: {body}");
}

#[tokio::test]
async fn a_search_says_it_depends_on_prefer_and_when_it_was_lenient() {
    let (db, catalog, store) = memory();
    let app = app(db, catalog, store);
    assert_eq!(put(&app, patient("p1")).await.status, StatusCode::CREATED);
    let headers_of = |uri: &'static str, prefer: Option<&'static str>| {
        let app = app.clone();
        async move {
            let mut builder = Request::builder().method("GET").uri(uri);
            if let Some(value) = prefer {
                builder = builder.header("prefer", value);
            }
            let response = app
                .oneshot(builder.body(Body::empty()).unwrap())
                .await
                .unwrap();
            (response.status(), response.headers().clone())
        }
    };
    let varies_on_prefer = |headers: &axum::http::HeaderMap| {
        headers
            .get_all("vary")
            .iter()
            .any(|value| value.to_str().unwrap_or("").eq_ignore_ascii_case("prefer"))
    };

    // Every search varies on `Prefer`, whatever it answered, so a cache keeps the answers apart.
    // Only the one that honored lenient says so.
    for (uri, prefer, status, applied) in [
        ("/Observation?date=2024", None, StatusCode::OK, false),
        (
            "/Observation?date=notadate",
            None,
            StatusCode::BAD_REQUEST,
            false,
        ),
        (
            "/Observation?date=2024",
            Some("handling=lenient"),
            StatusCode::OK,
            true,
        ),
        (
            "/Observation?date=notadate",
            Some("handling=lenient"),
            StatusCode::OK,
            true,
        ),
        (
            "/Patient/p1/Observation?date=notadate",
            Some("handling=lenient"),
            StatusCode::OK,
            true,
        ),
        (
            "/Observation?date=notadate",
            Some("handling=strict"),
            StatusCode::BAD_REQUEST,
            false,
        ),
        (
            "/Patient?bogus=1",
            Some("handling=lenient"),
            StatusCode::BAD_REQUEST,
            false,
        ),
    ] {
        let (got, headers) = headers_of(uri, prefer).await;
        assert_eq!(got, status, "{uri} {prefer:?}");
        assert!(varies_on_prefer(&headers), "{uri} {prefer:?}: {headers:?}");
        let said = headers
            .get("preference-applied")
            .and_then(|value| value.to_str().ok());
        let want = applied.then_some("handling=lenient");
        assert_eq!(said, want, "{uri} {prefer:?}");
    }
    // A browser can read it too, and CORS does not replace the `Vary` the search set.
    let request = Request::builder()
        .method("GET")
        .uri("/Observation?date=2024")
        .header("prefer", "handling=lenient")
        .header("origin", "http://127.0.0.1:3000")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let varied: Vec<_> = response
        .headers()
        .get_all("vary")
        .iter()
        .map(|value| value.to_str().unwrap().to_string())
        .collect();
    assert!(varied.iter().any(|value| value == "Prefer"), "{varied:?}");
    assert!(varied.iter().any(|value| value == "Origin"), "{varied:?}");
    let exposed = response
        .headers()
        .get("access-control-expose-headers")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    assert!(exposed.contains("Preference-Applied"), "{exposed}");
}

#[tokio::test]
async fn a_graphql_argument_cannot_ask_for_lenient() {
    let (db, catalog, store) = memory();
    let app = app(db, catalog, store);
    let listed = send(
        &app,
        "POST",
        "/graphql",
        Some(json!({
            "query": "{ ObservationList(date: \"notadate\", _handling: \"lenient\") { id } }"
        })),
        None,
    )
    .await;
    assert_eq!(listed.status, StatusCode::BAD_REQUEST, "{}", listed.body);
    let said = listed.body["errors"][0]["message"].as_str().unwrap_or("");
    assert!(said.starts_with("invalid value "), "{said}");
    // And a value that reads is listed as ever.
    let listed = send(
        &app,
        "POST",
        "/graphql",
        Some(json!({"query": "{ ObservationList(date: \"2024\") { id } }"})),
        None,
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
}

#[tokio::test]
async fn the_server_assigns_ids_and_put_to_a_new_id_is_off_by_default() {
    let (db, catalog, store) = strict();
    let app = app(db, catalog, store);

    // POST ignores the id and the version a client names.
    let mut body = patient("chosen");
    body["meta"] = json!({"versionId": "99"});
    let created = send(&app, "POST", "/Patient", Some(body), None).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.body["id"].as_str().unwrap().to_string();
    assert_ne!(id, "chosen");
    assert_eq!(created.body["meta"]["versionId"], "1");
    assert_eq!(
        created.location.as_deref(),
        Some(format!("Patient/{id}/_history/1").as_str())
    );
    assert_eq!(
        send(&app, "GET", "/Patient/chosen", None, None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // PUT to an id that does not exist is a 405 that says why and what to do instead.
    let refused = send(
        &app,
        "PUT",
        "/Patient/chosen",
        Some(patient("chosen")),
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(refused.body["resourceType"], "OperationOutcome");
    assert_eq!(refused.body["issue"][0]["severity"], "error");
    let text = refused.body["issue"][0]["diagnostics"].as_str().unwrap();
    assert!(
        text.contains("update-as-create") && text.contains("POST to Patient"),
        "{text}"
    );
    // A 405 must say which methods the resource does take.
    let allow = {
        let request = Request::builder()
            .method("PUT")
            .uri("/Patient/chosen")
            .header("content-type", "application/fhir+json")
            .body(Body::from(patient("chosen").to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        response
            .headers()
            .get("allow")
            .map(|value| value.to_str().unwrap().to_string())
    };
    assert_eq!(allow.as_deref(), Some("GET, HEAD, PATCH, DELETE"));

    // An update to the id the server did assign still works.
    let updated = send(
        &app,
        "PUT",
        &format!("/Patient/{id}"),
        Some(patient(&id)),
        None,
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK);

    // The capability statement says all of this.
    let metadata = send(&app, "GET", "/metadata", None, None).await;
    for resource in metadata.body["rest"][0]["resource"].as_array().unwrap() {
        assert_eq!(resource["updateCreate"], false);
        assert_eq!(resource["conditionalCreate"], true);
    }
}

#[tokio::test]
async fn put_to_a_new_id_creates_it_when_the_deployment_allows() {
    let (db, catalog, store) = memory();
    let app = app(db, catalog, store);
    let metadata = send(&app, "GET", "/metadata", None, None).await;
    assert!(metadata.body["rest"][0]["resource"]
        .as_array()
        .unwrap()
        .iter()
        .all(|resource| resource["updateCreate"] == true));
    let created = put(&app, patient("mine")).await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.location.as_deref(), Some("Patient/mine/_history/1"));
    let again = put(&app, patient("mine")).await;
    assert_eq!(again.status, StatusCode::OK);
    // If-Match on an id that is not there is a failed precondition, not a create.
    let missing = send(
        &app,
        "PUT",
        "/Patient/other",
        Some(patient("other")),
        Some("W/\"1\""),
    )
    .await;
    assert_eq!(missing.status, StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn occ_retries_then_conflict() {
    let (db, catalog, store) = memory();
    let app = app(db.clone(), catalog, store);
    let created = put(&app, patient("p1")).await;
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
    put(&app, patient("p1")).await;
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
        refhir_fhir::import_resource(&db, &loaded, &patient("p1")).unwrap();
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
    assert!(first.get("startVersion").is_some());
    assert!(first.get("endVersion").is_some());
    assert_eq!(first["modifications"][0]["type"], "QueryUpdated");
    assert!(first["modifications"][0].get("queryId").is_some());
    assert!(first["modifications"][0].get("value").is_some());
    assert!(first["modifications"][0].get("logLines").is_some());
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
    assert!(mutation.get("requestId").is_some());
    assert!(mutation.get("result").is_some());
    assert!(mutation.get("ts").is_some());

    let transition = next_json(&mut subscriber).await;
    assert_eq!(transition["type"], "Transition");
    let family =
        &transition["modifications"][0]["value"]["entry"][0]["resource"]["name"][0]["family"];
    assert_eq!(family, "Bee");
}

pub(crate) async fn connect(
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

pub(crate) async fn next_json(
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
        r#"{"resourceType":"CodeSystem","url":"http://example.org/animals","name":"Animals","content":"complete","concept":[{"code":"mammal","display":"Mammal","concept":[{"code":"dog","display":"Dog","definition":"A dog"},{"code":"dodo","display":"Dodo","property":[{"code":"inactive","valueBoolean":true}]}]}]}"#,
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

    // R4 requires a timestamp on an expansion, and an entry says when its code is inactive.
    let timestamp = expanded.body["expansion"]["timestamp"].as_str().unwrap();
    assert!(
        timestamp.len() == 24 && timestamp.ends_with('Z') && timestamp.as_bytes()[10] == b'T',
        "{timestamp}"
    );
    assert!(
        timestamp[..4].parse::<u32>().is_ok_and(|year| year >= 2026),
        "{timestamp}"
    );
    let all_pets = send(
        &app,
        "GET",
        "/ValueSet/$expand?url=http://example.org/pets",
        None,
        None,
    )
    .await;
    let entries = all_pets.body["expansion"]["contains"].as_array().unwrap();
    let entry = |code: &str| entries.iter().find(|entry| entry["code"] == code).unwrap();
    assert_eq!(entry("dodo")["inactive"], true);
    assert!(entry("dog").get("inactive").is_none());
    assert!(entry("dog").get("abstract").is_none());

    // The other terminology operations answer a GET with the same answer as a POST.
    let both = |get: String, post_path: &'static str, body: Value| {
        let app = &app;
        async move {
            let got = send(app, "GET", &get, None, None).await;
            let posted = send(app, "POST", post_path, Some(body), None).await;
            assert_eq!(got.status, StatusCode::OK, "{get}: {}", got.body);
            assert_eq!(got.status, posted.status);
            assert_eq!(got.body, posted.body, "{get}");
            got.body
        }
    };
    let checked_by_get = both(
        "/ValueSet/$validate-code?url=http://example.org/pets&code=dog&display=Hound".to_string(),
        "/ValueSet/$validate-code",
        json!({"url": "http://example.org/pets", "code": "dog", "display": "Hound"}),
    )
    .await;
    assert_eq!(checked_by_get["parameter"][0]["valueBoolean"], true);
    let looked_by_get = both(
        "/CodeSystem/$lookup?system=http://example.org/animals&code=dog".to_string(),
        "/CodeSystem/$lookup",
        json!({"system": "http://example.org/animals", "code": "dog"}),
    )
    .await;
    assert_eq!(looked_by_get["resourceType"], "Parameters");
    let subsumed_by_get = both(
        "/CodeSystem/$subsumes?system=http://example.org/animals&codeA=mammal&codeB=dog"
            .to_string(),
        "/CodeSystem/$subsumes",
        json!({"system": "http://example.org/animals", "codeA": "mammal", "codeB": "dog"}),
    )
    .await;
    assert_eq!(subsumed_by_get["parameter"][0]["valueCode"], "subsumes");
    let translated_by_get = both(
        "/ConceptMap/$translate?url=http://example.org/map&system=http://example.org/animals&code=dog"
            .to_string(),
        "/ConceptMap/$translate",
        json!({"url": "http://example.org/map", "system": "http://example.org/animals", "code": "dog"}),
    )
    .await;
    assert_eq!(translated_by_get["parameter"][0]["valueBoolean"], true);
    // A code that is not there is the same answer too, and an operation is still only for its
    // own resource type.
    let missing = send(
        &app,
        "GET",
        "/CodeSystem/$validate-code?url=http://example.org/animals&code=unicorn",
        None,
        None,
    )
    .await;
    assert_eq!(missing.status, StatusCode::OK, "{}", missing.body);
    let wrong = send(&app, "GET", "/Patient/$lookup?system=x&code=y", None, None).await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);

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
async fn a_value_set_that_cannot_expand_is_a_422_that_says_why() {
    let dir = std::env::temp_dir().join(format!(
        "refhir-http-unexpandable-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"example.http.unexpandable","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("CodeSystem-sample.json"),
        r#"{"resourceType":"CodeSystem","url":"http://example.org/sample","content":"fragment","concept":[{"code":"a"}]}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("ValueSet-whole.json"),
        r#"{"resourceType":"ValueSet","url":"http://example.org/whole","compose":{"include":[{"system":"http://example.org/sample"}]}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("ValueSet-fine.json"),
        r#"{"resourceType":"ValueSet","url":"http://example.org/fine","compose":{"include":[{"system":"http://example.org/sample","concept":[{"code":"a"}]}]}}"#,
    )
    .unwrap();
    let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
    let db = Arc::new(Database::open(store.clone()).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    install_package_dir(store.as_ref(), &db, &catalog, &dir).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let app = app(db, catalog, store);

    let refused = send(
        &app,
        "POST",
        "/ValueSet/$expand",
        Some(json!({"url": "http://example.org/whole"})),
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(refused.body["resourceType"], "OperationOutcome");
    let issue = &refused.body["issue"][0];
    assert_eq!(issue["severity"], "error");
    assert_eq!(issue["expression"][0], "ValueSet.compose.include[0]");
    let text = issue["diagnostics"].as_str().unwrap();
    assert!(
        text.contains("http://example.org/sample") && text.contains("'fragment'"),
        "{text}"
    );

    // The same server still expands what it can.
    let fine = send(
        &app,
        "POST",
        "/ValueSet/$expand",
        Some(json!({"url": "http://example.org/fine"})),
        None,
    )
    .await;
    assert_eq!(fine.status, StatusCode::OK);

    // The preflight: named value sets, in a plain body and as Parameters, and the unnamed form.
    let named = send(
        &app,
        "POST",
        "/ValueSet/$expandability",
        Some(json!({"url": ["http://example.org/whole", "http://example.org/fine"]})),
        None,
    )
    .await;
    assert_eq!(named.status, StatusCode::OK);
    let report = named.body["parameter"].as_array().unwrap();
    assert_eq!(report.len(), 2);
    let flag = |entry: &Value| entry["part"][1]["valueBoolean"].as_bool().unwrap();
    assert!(!flag(&report[0]));
    assert!(flag(&report[1]));
    assert!(report[0]["part"]
        .as_array()
        .unwrap()
        .iter()
        .any(|part| { part["name"] == "reason" && part["valueCode"] == "code-system-incomplete" }));
    let as_parameters = send(
        &app,
        "POST",
        "/ValueSet/$expandability",
        Some(json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": "http://example.org/whole"}
        ]})),
        None,
    )
    .await;
    assert_eq!(as_parameters.body["parameter"].as_array().unwrap().len(), 1);
    let everything = send(
        &app,
        "POST",
        "/ValueSet/$expandability",
        Some(json!({})),
        None,
    )
    .await;
    let urls: Vec<&str> = everything.body["parameter"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["part"][0]["valueUri"].as_str().unwrap())
        .collect();
    assert_eq!(urls, ["http://example.org/whole"]);
}

/// The codes of an expansion, in the order they come.
fn expanded_codes(reply: &Reply) -> Vec<String> {
    reply.body["expansion"]["contains"]
        .as_array()
        .unwrap_or_else(|| panic!("{}", reply.body))
        .iter()
        .map(|entry| entry["code"].as_str().unwrap().to_string())
        .collect()
}

/// An expansion without its `timestamp` and `identifier`, which say when and which answer it was
/// and so differ between two requests for the same thing.
fn without_timestamp(mut body: Value) -> Value {
    if let Some(expansion) = body.get_mut("expansion").and_then(Value::as_object_mut) {
        expansion.remove("timestamp");
        expansion.remove("identifier");
    }
    body
}

#[tokio::test]
async fn terminology_operations_answer_at_the_instance_too() {
    let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
    let db = Arc::new(Database::open(store.clone()).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    install_package_dir(store.as_ref(), &db, &catalog, r4_dir()).unwrap();
    let app = app(db.clone(), catalog.clone(), store.clone());
    let system = "http://example.org/pets";

    // The server stores the terminology a client writes, and the instance is one of those.
    let made = |path: &'static str, body: Value| {
        let app = app.clone();
        async move {
            let created = send(&app, "POST", path, Some(body), None).await;
            assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
            created.body["id"].as_str().unwrap().to_string()
        }
    };
    let code_system = made(
        "/CodeSystem",
        json!({"resourceType": "CodeSystem", "url": system, "version": "2",
        "status": "active", "content": "complete",
        "concept": [{"code": "mammal", "display": "Mammal", "concept": [
            {"code": "dog", "display": "Dog"},
            {"code": "dodo", "display": "Dodo",
             "property": [{"code": "inactive", "valueBoolean": true}]}
        ]}]}),
    )
    .await;
    let value_set = made(
        "/ValueSet",
        json!({"resourceType": "ValueSet", "url": "http://example.org/pet-values",
               "status": "draft", "compose": {"include": [{"system": system}]}}),
    )
    .await;
    let concept_map = made(
        "/ConceptMap",
        json!({"resourceType": "ConceptMap", "url": "http://example.org/pet-map",
               "status": "active", "group": [{"source": system,
                   "target": "http://example.org/words",
                   "element": [{"code": "dog", "target": [
                       {"code": "canine", "equivalence": "equivalent"}]}]}]}),
    )
    .await;
    let instance = |kind: &str, id: &str, op: &str| format!("/{kind}/{id}/{op}");

    // `ValueSet/[id]/$expand` is the expansion of that value set: the one `url` gives, and it
    // says the status the value set has, and which expansion it is.
    let at_type = send(
        &app,
        "POST",
        "/ValueSet/$expand",
        Some(json!({"url": "http://example.org/pet-values"})),
        None,
    )
    .await;
    assert_eq!(at_type.status, StatusCode::OK, "{}", at_type.body);
    let got = send(
        &app,
        "GET",
        &instance("ValueSet", &value_set, "$expand"),
        None,
        None,
    )
    .await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    assert_eq!(
        without_timestamp(got.body.clone()),
        without_timestamp(at_type.body.clone())
    );
    assert_eq!(got.body["status"], "draft");
    assert!(got.body["expansion"]["identifier"]
        .as_str()
        .unwrap()
        .starts_with("urn:uuid:"));
    // The same expansion has the same identifier, however it was asked for.
    assert_eq!(
        got.body["expansion"]["identifier"],
        at_type.body["expansion"]["identifier"]
    );
    assert_eq!(expanded_codes(&got), ["dodo", "dog", "mammal"]);
    // A POST with no body at all is the same request, and so is one with a body of parameters.
    let posted = send(
        &app,
        "POST",
        &instance("ValueSet", &value_set, "$expand"),
        None,
        None,
    )
    .await;
    assert_eq!(
        without_timestamp(posted.body.clone()),
        without_timestamp(got.body.clone())
    );
    // `activeOnly` leaves out the inactive codes, and the expansion says it did.
    let active = send(
        &app,
        "GET",
        &format!(
            "{}?activeOnly=true&filter=o",
            instance("ValueSet", &value_set, "$expand")
        ),
        None,
        None,
    )
    .await;
    assert_eq!(active.status, StatusCode::OK, "{}", active.body);
    assert_eq!(expanded_codes(&active), ["dog"]);
    assert_eq!(active.body["expansion"]["total"], 1);
    let parameters = active.body["expansion"]["parameter"].as_array().unwrap();
    assert!(parameters.contains(&json!({"name": "activeOnly", "valueBoolean": true})));
    assert!(parameters.contains(&json!({"name": "filter", "valueString": "o"})));
    let everything = send(
        &app,
        "GET",
        &format!(
            "{}?activeOnly=false",
            instance("ValueSet", &value_set, "$expand")
        ),
        None,
        None,
    )
    .await;
    assert_eq!(expanded_codes(&everything), ["dodo", "dog", "mammal"]);
    // A Parameters resource has an id of its own, which is not an instance.
    let with_id = send(
        &app,
        "POST",
        "/ValueSet/$expand",
        Some(
            json!({"resourceType": "Parameters", "id": "nothing-to-do-with-it",
                    "parameter": [
                        {"name": "url", "valueUri": "http://example.org/pet-values"},
                        {"name": "activeOnly", "valueBoolean": true}]}),
        ),
        None,
    )
    .await;
    assert_eq!(with_id.status, StatusCode::OK, "{}", with_id.body);
    assert_eq!(expanded_codes(&with_id), ["dog", "mammal"]);

    // The others, as a GET and as a POST, with the instance as what they are asked of.
    let checked = send(
        &app,
        "GET",
        &format!(
            "{}?code=dog",
            instance("ValueSet", &value_set, "$validate-code")
        ),
        None,
        None,
    )
    .await;
    assert_eq!(checked.status, StatusCode::OK, "{}", checked.body);
    assert_eq!(checked.body["parameter"][0]["valueBoolean"], true);
    let unknown = send(
        &app,
        "POST",
        &instance("ValueSet", &value_set, "$validate-code"),
        Some(json!({"code": "unicorn"})),
        None,
    )
    .await;
    assert_eq!(unknown.body["parameter"][0]["valueBoolean"], false);
    let in_system = send(
        &app,
        "GET",
        &format!(
            "{}?code=dog",
            instance("CodeSystem", &code_system, "$validate-code")
        ),
        None,
        None,
    )
    .await;
    assert_eq!(in_system.status, StatusCode::OK, "{}", in_system.body);
    assert_eq!(in_system.body["parameter"][0]["valueBoolean"], true);
    // The system the instance is wins over one the caller names.
    let named = send(
        &app,
        "GET",
        &format!(
            "{}?code=dog&system=http://example.org/other",
            instance("CodeSystem", &code_system, "$validate-code")
        ),
        None,
        None,
    )
    .await;
    assert_eq!(named.body["parameter"][0]["valueBoolean"], true);
    let subsumed = send(
        &app,
        "GET",
        &format!(
            "{}?codeA=mammal&codeB=dog",
            instance("CodeSystem", &code_system, "$subsumes")
        ),
        None,
        None,
    )
    .await;
    assert_eq!(subsumed.status, StatusCode::OK, "{}", subsumed.body);
    assert_eq!(subsumed.body["parameter"][0]["valueCode"], "subsumes");
    let translated = send(
        &app,
        "GET",
        &format!(
            "{}?system={system}&code=dog",
            instance("ConceptMap", &concept_map, "$translate")
        ),
        None,
        None,
    )
    .await;
    assert_eq!(translated.status, StatusCode::OK, "{}", translated.body);
    assert_eq!(translated.body["parameter"][0]["valueBoolean"], true);

    // An instance whose url a package defines too cannot be told from that definition, which is
    // what the url gets, so it is refused (and asked for by url) rather than answered for the
    // package's. (A write that shadows an installed package's url is refused, so this is a package
    // installed after the instance was stored.)
    let shadowed = made(
        "/ValueSet",
        json!({"resourceType": "ValueSet", "url": "http://example.org/shadowed",
               "status": "draft", "compose": {"include": [{"system": system}]}}),
    )
    .await;
    let ok = send(
        &app,
        "GET",
        &instance("ValueSet", &shadowed, "$expand"),
        None,
        None,
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    let dir = std::env::temp_dir().join(format!(
        "refhir-shadow-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"example.shadow","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("ValueSet-shadowed.json"),
        r#"{"resourceType":"ValueSet","url":"http://example.org/shadowed","status":"active","compose":{"include":[{"system":"http://example.org/pets","concept":[{"code":"dog"}]}]}}"#,
    )
    .unwrap();
    install_package_dir(store.as_ref(), &db, &catalog, &dir).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    for method in ["GET", "POST"] {
        let refused = send(
            &app,
            method,
            &instance("ValueSet", &shadowed, "$expand"),
            None,
            None,
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::CONFLICT,
            "{method}: {}",
            refused.body
        );
    }
    let by_url = send(
        &app,
        "POST",
        "/ValueSet/$expand",
        Some(json!({"url": "http://example.org/shadowed"})),
        None,
    )
    .await;
    assert_eq!(by_url.status, StatusCode::OK, "{}", by_url.body);

    // An instance the server does not store is not found, and an id is of one type only.
    for path in [
        instance("ValueSet", "nothing-here", "$expand"),
        instance("CodeSystem", &value_set, "$validate-code"),
        instance("ValueSet", &code_system, "$expand"),
        // HL7's own value sets are installed from a package, not stored, so they have no id to
        // be asked for: they are expanded by `url`.
        instance("ValueSet", "organization-type", "$expand"),
    ] {
        let missing = send(&app, "GET", &path, None, None).await;
        assert_eq!(
            missing.status,
            StatusCode::NOT_FOUND,
            "{path}: {}",
            missing.body
        );
    }
    // R4 defines `$lookup` on the type only, and these operations only on terminology.
    let lookup = send(
        &app,
        "GET",
        &format!(
            "{}?code=dog",
            instance("CodeSystem", &code_system, "$lookup")
        ),
        None,
        None,
    )
    .await;
    assert!(lookup.status.is_client_error(), "{}", lookup.body);
    let wrong = send(&app, "GET", "/Patient/p1/$expand", None, None).await;
    assert!(wrong.status.is_client_error(), "{}", wrong.body);
}

#[tokio::test]
async fn expand_is_a_get_as_well_as_a_post() {
    let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
    let db = Arc::new(Database::open(store.clone()).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    install_package_dir(store.as_ref(), &db, &catalog, r4_dir()).unwrap();
    install_builtin_terminology(store.as_ref(), &db, &catalog).unwrap();
    let app = app(db, catalog, store);
    let types = "http://hl7.org/fhir/ValueSet/organization-type";

    // The query string says what the body of a POST does, and the answer is the same.
    let posted = send(
        &app,
        "POST",
        "/ValueSet/$expand",
        Some(json!({"url": types})),
        None,
    )
    .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.body);
    assert_eq!(expanded_codes(&posted).len(), 12);
    let got = send(
        &app,
        "GET",
        &format!("/ValueSet/$expand?url={types}"),
        None,
        None,
    )
    .await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    assert_eq!(
        without_timestamp(got.body.clone()),
        without_timestamp(posted.body.clone())
    );
    assert_eq!(got.body["expansion"]["total"], 12);

    // `filter`, `count` and `offset` work as they do in a body.
    let all = expanded_codes(&got);
    let filtered = send(
        &app,
        "GET",
        &format!("/ValueSet/$expand?url={types}&filter=hospital"),
        None,
        None,
    )
    .await;
    assert_eq!(filtered.status, StatusCode::OK, "{}", filtered.body);
    assert!(!expanded_codes(&filtered).is_empty());
    assert!(expanded_codes(&filtered).len() < all.len());
    assert_eq!(
        filtered.body["expansion"]["total"],
        expanded_codes(&filtered).len()
    );
    for entry in filtered.body["expansion"]["contains"].as_array().unwrap() {
        let text = format!("{} {}", entry["code"], entry["display"]).to_lowercase();
        assert!(text.contains("hospital"), "{text}");
    }
    let page = send(
        &app,
        "GET",
        &format!("/ValueSet/$expand?url={types}&count=3&offset=2"),
        None,
        None,
    )
    .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert_eq!(expanded_codes(&page), all[2..5]);
    assert_eq!(page.body["expansion"]["total"], 12);
    assert_eq!(page.body["expansion"]["offset"], 2);
    let paged_post = send(
        &app,
        "POST",
        "/ValueSet/$expand",
        Some(json!({"url": types, "count": 3, "offset": 2})),
        None,
    )
    .await;
    assert_eq!(
        without_timestamp(page.body.clone()),
        without_timestamp(paged_post.body.clone())
    );

    // The url may be percent-encoded, and a parameter given twice counts once.
    let encoded = send(
        &app,
        "GET",
        "/ValueSet/$expand?url=http%3A%2F%2Fhl7.org%2Ffhir%2FValueSet%2Forganization-type&url=http%3A%2F%2Fexample.org%2Fnone",
        None,
        None,
    )
    .await;
    assert_eq!(
        without_timestamp(encoded.body.clone()),
        without_timestamp(posted.body.clone())
    );

    // The value sets that cannot be expanded say why, as a POST does.
    let refused = send(
        &app,
        "GET",
        "/ValueSet/$expand?url=http://hl7.org/fhir/ValueSet/insuranceplan-type",
        None,
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body["issue"][0]["diagnostics"]
        .as_str()
        .unwrap()
        .contains("'fragment'"));
    let unsupported = send(
        &app,
        "GET",
        "/ValueSet/$expand?url=http://hl7.org/fhir/ValueSet/practitioner-role",
        None,
        None,
    )
    .await;
    assert_eq!(unsupported.status, StatusCode::UNPROCESSABLE_ENTITY);

    // No url is a bad request, and `$expand` is still only for value sets.
    let none = send(&app, "GET", "/ValueSet/$expand", None, None).await;
    assert_eq!(none.status, StatusCode::BAD_REQUEST);
    let wrong = send(
        &app,
        "GET",
        &format!("/Patient/$expand?url={types}"),
        None,
        None,
    )
    .await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);

    // A value set over a whole `example` code system lists its concepts and says so, in either form.
    let category = "http://hl7.org/fhir/ValueSet/service-category";
    let flagged = send(
        &app,
        "GET",
        &format!("/ValueSet/$expand?url={category}"),
        None,
        None,
    )
    .await;
    assert_eq!(flagged.status, StatusCode::OK, "{}", flagged.body);
    assert_eq!(expanded_codes(&flagged).len(), 38);
    assert_eq!(
        flagged.body["expansion"]["parameter"],
        json!([{
            "name": "http://terminology.hl7.org/CodeSystem/service-category|4.0.1#CodeSystem.content",
            "valueCode": "example"
        }])
    );
    let posted = send(
        &app,
        "POST",
        "/ValueSet/$expand",
        Some(json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": category}
        ]})),
        None,
    )
    .await;
    assert_eq!(
        without_timestamp(posted.body.clone()),
        without_timestamp(flagged.body.clone())
    );
    // The currencies HL7 binds Money to come from the ISO 4217 codes the server carries.
    let currencies = "http://hl7.org/fhir/ValueSet/currencies";
    let naira = send(
        &app,
        "GET",
        &format!("/ValueSet/$expand?url={currencies}&filter=naira"),
        None,
        None,
    )
    .await;
    assert_eq!(naira.status, StatusCode::OK, "{}", naira.body);
    assert_eq!(expanded_codes(&naira), ["NGN"]);
    let all = send(
        &app,
        "GET",
        &format!("/ValueSet/$expand?url={currencies}"),
        None,
        None,
    )
    .await;
    let codes = expanded_codes(&all);
    assert_eq!(codes.len(), 178);
    for code in ["NGN", "USD", "GBP"] {
        assert!(codes.iter().any(|item| item == code), "{code}");
    }
    let currency = |code: &str| {
        send(
            &app,
            "POST",
            "/ValueSet/$validate-code",
            Some(json!({"url": currencies, "system": "urn:iso:std:iso:4217", "code": code})),
            None,
        )
    };
    assert_eq!(
        currency("NGN").await.body["parameter"][0]["valueBoolean"],
        true
    );
    assert_eq!(
        currency("ZZZ").await.body["parameter"][0]["valueBoolean"],
        false
    );
    let checked = |code: &str| {
        send(
            &app,
            "POST",
            "/ValueSet/$validate-code",
            Some(json!({"url": category,
                "system": "http://terminology.hl7.org/CodeSystem/service-category", "code": code})),
            None,
        )
    };
    assert_eq!(
        checked("17").await.body["parameter"][0]["valueBoolean"],
        true
    );
    assert_eq!(
        checked("999").await.body["parameter"][0]["valueBoolean"],
        false
    );
}

#[tokio::test]
async fn implementation_guide_install_is_an_http_route() {
    let (db, catalog, store) = memory();
    let app = app(db, catalog, store);
    let unauth = send(
        &app,
        "POST",
        "/ImplementationGuide/$install",
        Some(json!({"name": "hl7.fhir.us.core", "version": "6.1.0"})),
        None,
    )
    .await;
    assert_eq!(unauth.status, StatusCode::UNAUTHORIZED);

    let wrong = send(
        &app,
        "POST",
        "/Patient/$install",
        Some(json!({"name": "hl7.fhir.us.core", "version": "6.1.0"})),
        None,
    )
    .await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);

    let smart = std::sync::Arc::new(crate::auth::SmartStore::with_defaults());
    smart.install_token("user-tok", &["user/*.cruds"], None);
    smart.install_token("system-tok", &["system/*.write"], None);
    let (db, catalog, store) = memory();
    let app = crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: true,
            smart_demo_login: true,
            smart: Some(smart),
            ..Default::default()
        },
    );
    let user = send_text(
        &app,
        "POST",
        "/ImplementationGuide/$install",
        &json!({"name": "hl7.fhir.us.core", "version": "6.1.0"}).to_string(),
        &[
            ("content-type", "application/fhir+json"),
            ("authorization", "Bearer user-tok"),
        ],
    )
    .await;
    assert_eq!(user.0, StatusCode::FORBIDDEN, "{}", user.1);

    let path = send_text(
        &app,
        "POST",
        "/ImplementationGuide/$install",
        &json!({"package": extra_dir().to_str().unwrap()}).to_string(),
        &[
            ("content-type", "application/fhir+json"),
            ("authorization", "Bearer system-tok"),
        ],
    )
    .await;
    assert_eq!(path.0, StatusCode::BAD_REQUEST, "{}", path.1);

    let registry = send_text(
        &app,
        "POST",
        "/ImplementationGuide/$install",
        &json!({
            "name": "hl7.fhir.us.core",
            "version": "6.1.0",
            "registry": "http://127.0.0.1"
        })
        .to_string(),
        &[
            ("content-type", "application/fhir+json"),
            ("authorization", "Bearer system-tok"),
        ],
    )
    .await;
    assert_eq!(registry.0, StatusCode::BAD_REQUEST, "{}", registry.1);
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
            sdc_archiver: false,
            ..Default::default()
        },
    );
    let xml =
        r#"<Patient xmlns="http://hl7.org/fhir"><id value="x1"/><active value="true"/></Patient>"#;
    let created = send_text(
        &app,
        "PUT",
        "/Patient/x1",
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
    // GraphQL create is the create interaction too: the server assigns the id.
    let graphql_id = created_gql.body["data"]["PatientCreate"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(graphql_id, "g1");
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
        .any(|item| item["id"] == graphql_id.as_str()));

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
    assert!(
        file.1.contains("\"id\":\"x1\"") || file.1.contains("\"id\": \"x1\""),
        "{}",
        file.1
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A live search is the point of the sync protocol, and with SMART on it failed for a reason that
/// had nothing to do with scopes: the server's own additions to the arguments were taken for
/// search parameters.
#[tokio::test]
async fn a_live_search_under_smart_returns_results_and_follows_writes() {
    let (db, catalog, store) = memory();
    let app = crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: true,
            smart_demo_login: true,
            ..Default::default()
        },
    );
    let reader = token(&app, "user/Patient.read").await;
    let writer = token(&app, "user/Patient.write").await;
    // A router is cheap to clone, and the server task below takes the original.
    let writes = app.clone();
    let put = |id: &'static str, active: bool| {
        let app = writes.clone();
        let writer = writer.clone();
        async move {
            let body = json!({"resourceType": "Patient", "id": id, "active": active}).to_string();
            send_text(
                &app,
                "PUT",
                &format!("/Patient/{id}"),
                &body,
                &[
                    ("authorization", &format!("Bearer {writer}")),
                    ("content-type", "application/fhir+json"),
                ],
            )
            .await
            .0
        }
    };
    assert!(put("p1", true).await.is_success());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (mut socket, _) = connect(&format!("ws://{addr}/sync?access_token={reader}")).await;
    socket
        .send(Message::Text(
            json!({"type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1, "modifications": [
                {"type": "Add", "queryId": 1, "udfPath": "Patient/search", "args": {"active": "true"}}
            ]})
            .to_string(),
        ))
        .await
        .unwrap();
    let total = |message: &Value| message["modifications"][0]["value"]["total"].clone();
    // The search ran, and found the patient that was already there.
    let first = next_json(&mut socket).await;
    assert_eq!(first["modifications"][0]["type"], "QueryUpdated", "{first}");
    assert_eq!(total(&first), 1);
    // A write that joins the result is pushed.
    assert!(put("p2", true).await.is_success());
    let joined = next_json(&mut socket).await;
    assert_eq!(
        joined["modifications"][0]["type"], "QueryUpdated",
        "{joined}"
    );
    assert_eq!(total(&joined), 2);
    // And one that leaves it.
    assert!(put("p1", false).await.is_success());
    let left = next_json(&mut socket).await;
    assert_eq!(total(&left), 1);
    assert_eq!(
        left["modifications"][0]["value"]["entry"][0]["resource"]["id"],
        "p2"
    );
}

#[tokio::test]
async fn terminology_operations_need_only_the_read_scope() {
    let (db, catalog, store) = memory();
    let app = crate::app_with(
        db.clone(),
        catalog.clone(),
        store.clone(),
        crate::ServerConfig {
            auth: true,
            smart_demo_login: true,
            export_dir: "export".into(),
            terminology_urls: Vec::new(),
            sdc_archiver: false,
            ..Default::default()
        },
    );
    let gender = "http://hl7.org/fhir/ValueSet/administrative-gender";
    let call = |access: String, method: &'static str, path: String, body: String| {
        let app = app.clone();
        async move {
            send_text(
                &app,
                method,
                &path,
                &body,
                &[
                    ("authorization", &format!("Bearer {access}")),
                    ("content-type", "application/fhir+json"),
                ],
            )
            .await
        }
    };

    // A client that may only read a type may use that type's terminology operations, as a POST
    // or as a GET. They change nothing, so they are not creates.
    let reader = token(&app, "user/ValueSet.read").await;
    let body = format!(r#"{{"url": "{gender}"}}"#);
    for (method, path, body) in [
        ("POST", "/ValueSet/$expand".to_string(), body.clone()),
        (
            "GET",
            format!("/ValueSet/$expand?url={gender}"),
            String::new(),
        ),
        ("POST", "/ValueSet/$validate-code".to_string(), body.clone()),
        (
            "GET",
            format!("/ValueSet/$validate-code?url={gender}&code=male"),
            String::new(),
        ),
        ("POST", "/ValueSet/$expandability".to_string(), body.clone()),
    ] {
        let reply = call(reader.clone(), method, path.clone(), body).await;
        // Whatever the operation answers about a value set this small package may not hold, the
        // scope let the request through.
        assert!(
            !matches!(reply.0, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED),
            "{method} {path}: {} {}",
            reply.0,
            reply.1
        );
    }
    // Reading is only that: it does not let the client write a value set, nor use the
    // operations of another type.
    let create = call(
        reader.clone(),
        "POST",
        "/ValueSet".to_string(),
        r#"{"resourceType":"ValueSet","url":"http://example.org/vs","status":"draft"}"#.to_string(),
    )
    .await;
    assert_eq!(create.0, StatusCode::FORBIDDEN, "{}", create.1);
    for (method, path) in [
        ("POST", "/CodeSystem/$lookup"),
        ("GET", "/CodeSystem/$lookup?system=x&code=y"),
        ("POST", "/ConceptMap/$translate"),
    ] {
        let reply = call(reader.clone(), method, path.to_string(), "{}".to_string()).await;
        assert_eq!(
            reply.0,
            StatusCode::FORBIDDEN,
            "{method} {path}: {}",
            reply.1
        );
    }

    // And a client that may only create is not a reader: the operations used to ask for `create`.
    let writer = token(&app, "user/ValueSet.c").await;
    for (method, path, body) in [
        ("POST", "/ValueSet/$expand".to_string(), body.clone()),
        (
            "GET",
            format!("/ValueSet/$expand?url={gender}"),
            String::new(),
        ),
    ] {
        let reply = call(writer.clone(), method, path.clone(), body).await;
        assert_eq!(
            reply.0,
            StatusCode::FORBIDDEN,
            "{method} {path}: {}",
            reply.1
        );
    }
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
            smart_demo_login: true,
            export_dir: "export".into(),
            terminology_urls: Vec::new(),
            sdc_archiver: false,
            ..Default::default()
        },
    );
    let denied = send(&app, "GET", "/Patient", None, None).await;
    assert_eq!(denied.status, StatusCode::UNAUTHORIZED);
    let access = token(&app, "user/Patient.read").await;
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
    let history = send_text(
        &app,
        "GET",
        "/_history",
        "",
        &[("authorization", &format!("Bearer {access}"))],
    )
    .await;
    assert_eq!(history.0, StatusCode::FORBIDDEN, "{}", history.1);
    let bundle = send_text(
        &app,
        "POST",
        "/",
        r#"{"resourceType":"Bundle","type":"transaction","entry":[]}"#,
        &[
            ("authorization", &format!("Bearer {access}")),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    assert_eq!(bundle.0, StatusCode::FORBIDDEN, "{}", bundle.1);

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
    let authed = format!("ws://{addr}/sync?access_token={access}");
    let (mut socket, _) = connect(&authed).await;
    socket
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
    let first = next_json(&mut socket).await;
    assert_eq!(first["type"], "Transition");
    // A failed query is also a Transition, so what is asked is that the search worked.
    assert_eq!(
        first["modifications"][0]["type"], "QueryUpdated",
        "the live search failed: {first}"
    );
    assert_eq!(first["modifications"][0]["value"]["resourceType"], "Bundle");
    // The token reads Patients only. A query it may not run says so with a code a client can
    // switch on, whatever the wording of the text.
    socket
        .send(Message::Text(
            json!({
                "type": "ModifyQuerySet",
                "baseVersion": 1,
                "newVersion": 2,
                "modifications": [{
                    "type": "Add",
                    "queryId": 2,
                    "udfPath": "Observation/search",
                    "args": {}
                }]
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let refused = next_json(&mut socket).await;
    assert_eq!(
        refused["modifications"][0]["type"], "QueryFailed",
        "{refused}"
    );
    assert_eq!(
        refused["modifications"][0]["code"], "forbidden",
        "{refused}"
    );
    socket
        .send(Message::Text(
            json!({
                "type": "Mutation",
                "requestId": 1,
                "udfPath": "Patient/update",
                "args": {"id": "p1", "resource": {"resourceType": "Patient", "id": "p1"}}
            })
            .to_string(),
        ))
        .await
        .unwrap();
    let denied_write = next_json(&mut socket).await;
    assert_eq!(denied_write["type"], "MutationResponse");
    assert_eq!(denied_write["success"], false);
    assert_eq!(denied_write["result"]["resourceType"], "OperationOutcome");
}

#[tokio::test]
async fn smart_app_launch_covers_discovery_pkce_oidc_and_backend() {
    let (db, catalog, store) = memory();
    {
        let cat = catalog.read().expect("catalog");
        refhir_fhir::import_resource(&db, &cat, &patient("p1")).expect("seed patient");
    }
    let app = crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: true,
            smart_demo_login: true,
            public_url: Some("http://127.0.0.1".into()),
            smart_patient: Some("p1".into()),
            smart_fhir_user: Some("Patient/smart".into()),
            smart_dcr_token: Some("dcr-test".into()),
            ..Default::default()
        },
    );

    let preflight = send_text(
        &app,
        "OPTIONS",
        "/.well-known/smart-configuration",
        "",
        &[
            ("origin", "http://app.example"),
            ("access-control-request-method", "GET"),
        ],
    )
    .await;
    assert_eq!(preflight.0, StatusCode::NO_CONTENT);

    let discovery = send(&app, "GET", "/.well-known/smart-configuration", None, None).await;
    assert_eq!(discovery.status, StatusCode::OK);
    assert_eq!(
        discovery.body["grant_types_supported"][0],
        "authorization_code"
    );
    assert!(discovery.body["grant_types_supported"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value == "client_credentials"));
    assert_eq!(
        discovery.body["code_challenge_methods_supported"],
        json!(["S256"])
    );
    assert!(discovery.body["authorization_endpoint"]
        .as_str()
        .unwrap()
        .starts_with("http://"));
    assert_eq!(discovery.body["issuer"], "http://127.0.0.1");
    assert!(discovery.body["jwks_uri"]
        .as_str()
        .unwrap()
        .ends_with("/.well-known/jwks.json"));
    let capabilities = discovery.body["capabilities"].as_array().unwrap();
    for needed in [
        "launch-ehr",
        "launch-standalone",
        "client-public",
        "client-confidential-symmetric",
        "client-confidential-asymmetric",
        "sso-openid-connect",
        "permission-patient",
        "permission-user",
        "permission-v1",
        "permission-v2",
        "context-ehr-patient",
        "context-standalone-patient",
    ] {
        assert!(
            capabilities.iter().any(|value| value == needed),
            "missing {needed}"
        );
    }

    let keys = send(&app, "GET", "/.well-known/jwks.json", None, None).await;
    assert_eq!(keys.body["keys"][0]["kty"], "RSA");

    let body = smart_token(
        &app,
        "launch/patient openid fhirUser offline_access patient/Patient.rs",
        "http://127.0.0.1/cb",
    )
    .await;
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["patient"], "p1");
    assert!(body["refresh_token"].as_str().is_some());
    let id_token = body["id_token"].as_str().unwrap();
    let n = keys.body["keys"][0]["n"].as_str().unwrap();
    let e = keys.body["keys"][0]["e"].as_str().unwrap();
    let decoding = jsonwebtoken::DecodingKey::from_rsa_components(n, e).unwrap();
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.set_audience(&["public"]);
    validation.set_issuer(&["http://127.0.0.1"]);
    let claims: Value = jsonwebtoken::decode::<Value>(id_token, &decoding, &validation)
        .unwrap()
        .claims;
    assert!(claims["fhirUser"].as_str().unwrap().contains("Patient/"));

    let access = body["access_token"].as_str().unwrap();
    let read = send_text(
        &app,
        "GET",
        "/Patient",
        "",
        &[("authorization", &format!("Bearer {access}"))],
    )
    .await;
    assert_eq!(read.0, StatusCode::OK, "{}", read.1);
    let create = send_text(
        &app,
        "POST",
        "/Patient",
        &patient("p9").to_string(),
        &[
            ("authorization", &format!("Bearer {access}")),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    assert_eq!(create.0, StatusCode::FORBIDDEN);

    let introspected = send_text(
        &app,
        "POST",
        "/oauth/introspect",
        &format!("token={access}"),
        &[
            ("content-type", "application/x-www-form-urlencoded"),
            ("authorization", &format!("Bearer {access}")),
        ],
    )
    .await;
    assert_eq!(introspected.0, StatusCode::OK, "{}", introspected.1);
    let intro: Value = serde_json::from_str(&introspected.1).unwrap();
    assert_eq!(intro["active"], true);
    assert_eq!(intro["patient"], "p1");
    assert!(intro["scope"]
        .as_str()
        .unwrap()
        .contains("patient/Patient.rs"));

    let refreshed = send_text(
        &app,
        "POST",
        "/oauth/token",
        &format!(
            "grant_type=refresh_token&refresh_token={}&client_id=public",
            body["refresh_token"].as_str().unwrap()
        ),
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(refreshed.0, StatusCode::OK, "{}", refreshed.1);

    let ehr = smart_token(
        &app,
        "launch openid fhirUser patient/*.read",
        "http://127.0.0.1/cb",
    )
    .await;
    assert_eq!(ehr["patient"], "p1");
    assert_eq!(ehr["need_patient_banner"], true);

    let (encoding, jwks) = backend_client_keys();
    let registered = send_text(
        &app,
        "POST",
        "/oauth/register",
        &json!({
            "token_endpoint_auth_method": "private_key_jwt",
            "grant_types": ["client_credentials"],
            "jwks": jwks,
            "scope": "system/*.cruds"
        })
        .to_string(),
        &[
            ("content-type", "application/json"),
            ("authorization", "Bearer dcr-test"),
        ],
    )
    .await;
    assert_eq!(registered.0, StatusCode::CREATED, "{}", registered.1);
    let client: Value = serde_json::from_str(&registered.1).unwrap();
    let client_id = client["client_id"].as_str().unwrap();
    let assertion = backend_assertion(&encoding, client_id, "http://127.0.0.1/oauth/token");
    let backend = send_text(
        &app,
        "POST",
        "/oauth/token",
        &format!(
            "grant_type=client_credentials&client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer&client_assertion={assertion}&scope=system/*.rs"
        ),
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(backend.0, StatusCode::OK, "{}", backend.1);
    let backend_body: Value = serde_json::from_str(&backend.1).unwrap();
    assert_eq!(backend_body["expires_in"], 300);
    assert!(backend_body["scope"]
        .as_str()
        .unwrap()
        .contains("system/*.rs"));
}

fn backend_client_keys() -> (jsonwebtoken::EncodingKey, Value) {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    use rsa::traits::PublicKeyParts;

    let mut rng = rand::thread_rng();
    let key = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
    let pem = key.to_pkcs8_pem(LineEnding::LF).unwrap();
    let encoding = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap();
    let public = rsa::RsaPublicKey::from(&key);
    (
        encoding,
        json!({
            "keys": [{
                "kty": "RSA",
                "alg": "RS256",
                "use": "sig",
                "kid": "client-1",
                "n": URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
                "e": URL_SAFE_NO_PAD.encode(public.e().to_bytes_be())
            }]
        }),
    )
}

fn backend_assertion(encoding: &jsonwebtoken::EncodingKey, client_id: &str, aud: &str) -> String {
    backend_assertion_kid(encoding, client_id, aud, "client-1")
}

fn backend_assertion_kid(
    encoding: &jsonwebtoken::EncodingKey,
    client_id: &str,
    aud: &str,
    kid: &str,
) -> String {
    #[derive(serde::Serialize)]
    struct Claims<'a> {
        iss: &'a str,
        sub: &'a str,
        aud: &'a str,
        exp: u64,
        iat: u64,
        jti: String,
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(kid.into());
    jsonwebtoken::encode(
        &header,
        &Claims {
            iss: client_id,
            sub: client_id,
            aud,
            exp: now + 60,
            iat: now,
            jti: uuid::Uuid::new_v4().to_string(),
        },
        encoding,
    )
    .unwrap()
}

#[tokio::test]
async fn smart_rejects_open_redirects_system_scopes_and_bad_clients() {
    let (db, catalog, store) = memory();
    let app = crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: true,
            smart_demo_login: true,
            public_url: Some("https://fhir.example.org".into()),
            smart_dcr_token: Some("dcr-test".into()),
            smart_confidential_secret: Some("secret".into()),
            ..Default::default()
        },
    );

    let stolen = send(
        &app,
        "GET",
        "/oauth/authorize?response_type=code&client_id=public&redirect_uri=https://evil.example/cb&scope=user/Patient.read&aud=https://fhir.example.org&code_challenge=abc&code_challenge_method=S256",
        None,
        None,
    )
    .await;
    assert_eq!(stolen.status, StatusCode::BAD_REQUEST, "{}", stolen.body);
    assert!(stolen.location.is_none());
    assert_eq!(stolen.body["error"], "invalid_request");

    let prefix = send(
        &app,
        "GET",
        "/oauth/authorize?response_type=code&client_id=public&redirect_uri=http://localhost.evil.example/cb&scope=user/Patient.read&aud=https://fhir.example.org&code_challenge=abc&code_challenge_method=S256",
        None,
        None,
    )
    .await;
    assert_eq!(prefix.status, StatusCode::BAD_REQUEST);

    let system_only = send(
        &app,
        "GET",
        "/oauth/authorize?response_type=code&client_id=public&redirect_uri=http://127.0.0.1/cb&scope=system/*.rs&aud=https://fhir.example.org&code_challenge=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA&code_challenge_method=S256",
        None,
        None,
    )
    .await;
    assert_eq!(system_only.status, StatusCode::FOUND);
    assert!(
        system_only
            .location
            .as_deref()
            .unwrap_or("")
            .contains("error=invalid_scope"),
        "{:?}",
        system_only.location
    );

    let mixed = smart_token_for(
        &app,
        "user/Patient.read system/*.cruds",
        "http://127.0.0.1/cb",
        "https://fhir.example.org",
    )
    .await;
    assert_eq!(mixed["scope"], "user/Patient.read");
    assert!(!mixed["scope"].as_str().unwrap().contains("system/"));

    let dcr_open = send_text(
        &app,
        "POST",
        "/oauth/register",
        &json!({
            "token_endpoint_auth_method": "private_key_jwt",
            "grant_types": ["client_credentials"],
            "jwks": {"keys": []},
            "scope": "system/*.cruds"
        })
        .to_string(),
        &[("content-type", "application/json")],
    )
    .await;
    assert_eq!(dcr_open.0, StatusCode::UNAUTHORIZED, "{}", dcr_open.1);

    let dcr = send_text(
        &app,
        "POST",
        "/oauth/register",
        &json!({
            "token_endpoint_auth_method": "client_secret_post",
            "grant_types": ["client_credentials"],
            "scope": "system/*.cruds"
        })
        .to_string(),
        &[
            ("content-type", "application/json"),
            ("authorization", "Bearer dcr-test"),
        ],
    )
    .await;
    assert_eq!(dcr.0, StatusCode::BAD_REQUEST, "{}", dcr.1);

    let jwks_only = send_text(
        &app,
        "POST",
        "/oauth/register",
        &json!({
            "token_endpoint_auth_method": "private_key_jwt",
            "grant_types": ["client_credentials"],
            "jwks_uri": "https://evil.example/jwks.json",
            "scope": "system/*.rs"
        })
        .to_string(),
        &[
            ("content-type", "application/json"),
            ("authorization", "Bearer dcr-test"),
        ],
    )
    .await;
    assert_eq!(jwks_only.0, StatusCode::BAD_REQUEST, "{}", jwks_only.1);

    let (encoding, jwks) = backend_client_keys();
    let registered = send_text(
        &app,
        "POST",
        "/oauth/register",
        &json!({
            "token_endpoint_auth_method": "private_key_jwt",
            "grant_types": ["client_credentials"],
            "jwks": jwks,
            "scope": "system/*.rs"
        })
        .to_string(),
        &[
            ("content-type", "application/json"),
            ("authorization", "Bearer dcr-test"),
        ],
    )
    .await;
    assert_eq!(registered.0, StatusCode::CREATED, "{}", registered.1);
    let client_id = serde_json::from_str::<Value>(&registered.1).unwrap()["client_id"]
        .as_str()
        .unwrap()
        .to_string();
    let assertion = backend_assertion(
        &encoding,
        &client_id,
        "https://fhir.example.org/oauth/token",
    );
    let backend = send_text(
        &app,
        "POST",
        "/oauth/token",
        &format!(
            "grant_type=client_credentials&client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer&client_assertion={assertion}&scope=system/*.rs"
        ),
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(backend.0, StatusCode::OK, "{}", backend.1);

    let replay_jti = send_text(
        &app,
        "POST",
        "/oauth/token",
        &format!(
            "grant_type=client_credentials&client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer&client_assertion={assertion}&scope=system/*.rs"
        ),
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(replay_jti.0, StatusCode::UNAUTHORIZED);

    let issuer_aud = backend_assertion(&encoding, &client_id, "https://fhir.example.org");
    let issuer_as_aud = send_text(
        &app,
        "POST",
        "/oauth/token",
        &format!(
            "grant_type=client_credentials&client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer&client_assertion={issuer_aud}&scope=system/*.rs"
        ),
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(issuer_as_aud.0, StatusCode::UNAUTHORIZED);

    let wrong_kid = backend_assertion_kid(
        &encoding,
        &client_id,
        "https://fhir.example.org/oauth/token",
        "other",
    );
    let kid_mismatch = send_text(
        &app,
        "POST",
        "/oauth/token",
        &format!(
            "grant_type=client_credentials&client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer&client_assertion={wrong_kid}&scope=system/*.rs"
        ),
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(kid_mismatch.0, StatusCode::UNAUTHORIZED);

    let none_alg = send_text(
        &app,
        "POST",
        "/oauth/token",
        "grant_type=client_credentials&client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer&client_assertion=eyJhbGciOiJub25lIn0.eyJpc3MiOiJwdWJsaWMifQ.&scope=system/*.rs",
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(none_alg.0, StatusCode::UNAUTHORIZED);

    let wrong_secret = send_text(
        &app,
        "POST",
        "/oauth/token",
        "grant_type=authorization_code&code=missing&redirect_uri=http://127.0.0.1/cb&client_id=confidential&client_secret=nope&code_verifier=abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(wrong_secret.0, StatusCode::UNAUTHORIZED);

    let verifier = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let redirected = send(
        &app,
        "GET",
        &format!(
            "/oauth/authorize?response_type=code&client_id=public&redirect_uri=http://127.0.0.1/cb&scope=user/Patient.read&aud=https://fhir.example.org&code_challenge={challenge}&code_challenge_method=S256&state=s1"
        ),
        None,
        None,
    )
    .await;
    let code = authorize_code_from_redirect(&app, &redirected, "user/Patient.read").await;
    let bad_pkce = send_text(
        &app,
        "POST",
        "/oauth/token",
        &format!(
            "grant_type=authorization_code&code={code}&redirect_uri=http://127.0.0.1/cb&client_id=public&code_verifier=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
        ),
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(bad_pkce.0, StatusCode::BAD_REQUEST, "{}", bad_pkce.1);
    let replay = send_text(
        &app,
        "POST",
        "/oauth/token",
        &format!(
            "grant_type=authorization_code&code={code}&redirect_uri=http://127.0.0.1/cb&client_id=public&code_verifier={verifier}"
        ),
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(replay.0, StatusCode::BAD_REQUEST);

    let issued = smart_token_for(
        &app,
        "openid fhirUser offline_access user/Patient.read",
        "http://127.0.0.1/cb",
        "https://fhir.example.org",
    )
    .await;
    let refresh_expand = send_text(
        &app,
        "POST",
        "/oauth/token",
        &format!(
            "grant_type=refresh_token&refresh_token={}&client_id=public&scope=user/*.cruds",
            issued["refresh_token"].as_str().unwrap()
        ),
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(refresh_expand.0, StatusCode::BAD_REQUEST);

    let id_token = issued["id_token"].as_str().unwrap();
    assert!(id_token.split('.').nth(1).is_some());

    let cors_ok = cors_header(
        &app,
        "OPTIONS",
        "/.well-known/smart-configuration",
        &[("origin", "https://app.example")],
        "access-control-allow-origin",
    )
    .await;
    assert_eq!(cors_ok.as_deref(), Some("*"));
    let cors_denied = cors_header(
        &app,
        "OPTIONS",
        "/Patient",
        &[("origin", "https://evil.example")],
        "access-control-allow-origin",
    )
    .await;
    assert_ne!(cors_denied.as_deref(), Some("*"));
    assert_ne!(cors_denied.as_deref(), Some("https://evil.example"));
    let cors_loopback = cors_header(
        &app,
        "OPTIONS",
        "/oauth/token",
        &[("origin", "http://127.0.0.1:3000")],
        "access-control-allow-origin",
    )
    .await;
    assert_eq!(cors_loopback.as_deref(), Some("http://127.0.0.1:3000"));

    let smart = std::sync::Arc::new(crate::auth::SmartStore::with_defaults());
    smart.install_token_exp("expired", &["user/Patient.read"], None, 1);
    let (db, catalog, store) = memory();
    let expired_app = crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: true,
            smart_demo_login: true,
            smart: Some(smart),
            ..Default::default()
        },
    );
    let expired = send_text(
        &expired_app,
        "GET",
        "/Patient",
        "",
        &[("authorization", "Bearer expired")],
    )
    .await;
    assert_eq!(expired.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn smart_dcr_launch_issuer_and_code_are_locked_down() {
    let (db, catalog, store) = memory();
    {
        let cat = catalog.read().expect("catalog");
        refhir_fhir::import_resource(&db, &cat, &patient("p1")).expect("seed patient");
    }
    let app = crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: true,
            smart_demo_login: true,
            public_url: Some("http://127.0.0.1".into()),
            smart_patient: Some("p1".into()),
            smart_dcr_token: Some("dcr-test".into()),
            smart_confidential_secret: Some("secret".into()),
            ..Default::default()
        },
    );

    let userinfo = send_text(
        &app,
        "POST",
        "/oauth/register",
        &json!({
            "token_endpoint_auth_method": "none",
            "redirect_uris": ["https://app.example@evil.example/cb"],
            "scope": "user/*.cruds"
        })
        .to_string(),
        &[
            ("content-type", "application/json"),
            ("authorization", "Bearer dcr-test"),
        ],
    )
    .await;
    assert_eq!(userinfo.0, StatusCode::BAD_REQUEST, "{}", userinfo.1);

    let remote_http = send_text(
        &app,
        "POST",
        "/oauth/register",
        &json!({
            "token_endpoint_auth_method": "none",
            "redirect_uris": ["http://evil.example/cb"],
            "scope": "user/*.cruds"
        })
        .to_string(),
        &[
            ("content-type", "application/json"),
            ("authorization", "Bearer dcr-test"),
        ],
    )
    .await;
    assert_eq!(remote_http.0, StatusCode::BAD_REQUEST, "{}", remote_http.1);

    let star = send_text(
        &app,
        "POST",
        "/oauth/register",
        &json!({
            "token_endpoint_auth_method": "none",
            "redirect_uris": ["http://127.0.0.1/cb"],
            "scope": "*"
        })
        .to_string(),
        &[
            ("content-type", "application/json"),
            ("authorization", "Bearer dcr-test"),
        ],
    )
    .await;
    assert_eq!(star.0, StatusCode::BAD_REQUEST, "{}", star.1);

    let launched = send(
        &app,
        "GET",
        "/oauth/ehr-launch?client_id=public&launch_uri=http://127.0.0.1/app&patient=victim",
        None,
        None,
    )
    .await;
    assert_eq!(launched.status, StatusCode::FOUND, "{}", launched.body);
    let launch = query_value(launched.location.as_deref().unwrap_or(""), "launch").unwrap();
    let verifier = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let redirected = send(
        &app,
        "GET",
        &format!(
            "/oauth/authorize?response_type=code&client_id=public&redirect_uri=http://127.0.0.1/cb&scope=launch%20patient/*.read&aud=http://127.0.0.1&code_challenge={challenge}&code_challenge_method=S256&launch={launch}"
        ),
        None,
        None,
    )
    .await;
    assert_eq!(redirected.status, StatusCode::FOUND, "{}", redirected.body);
    let code = authorize_code_from_redirect(&app, &redirected, "launch patient/*.read").await;
    let issued = send_text(
        &app,
        "POST",
        "/oauth/token",
        &format!(
            "grant_type=authorization_code&code={code}&redirect_uri=http://127.0.0.1/cb&client_id=public&code_verifier={verifier}"
        ),
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(issued.0, StatusCode::OK, "{}", issued.1);
    let body: Value = serde_json::from_str(&issued.1).unwrap();
    assert_eq!(body["patient"], "p1");
    assert_ne!(body["patient"], "victim");

    let pinned = send_text(
        &app,
        "GET",
        "/.well-known/smart-configuration",
        "",
        &[("host", "evil.example")],
    )
    .await;
    assert_eq!(pinned.0, StatusCode::OK, "{}", pinned.1);
    let pinned_body: Value = serde_json::from_str(&pinned.1).unwrap();
    assert_eq!(pinned_body["issuer"], "http://127.0.0.1");

    let (db, catalog, store) = memory();
    let unpinned = crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: true,
            smart_demo_login: true,
            ..Default::default()
        },
    );
    let injected = send_text(
        &unpinned,
        "GET",
        "/.well-known/smart-configuration",
        "",
        &[("host", "evil.example")],
    )
    .await;
    assert_eq!(injected.0, StatusCode::OK, "{}", injected.1);
    let discovery: Value = serde_json::from_str(&injected.1).unwrap();
    assert_eq!(discovery["issuer"], "http://127.0.0.1");
    assert!(
        !discovery["issuer"]
            .as_str()
            .unwrap_or("")
            .contains("evil.example"),
        "{}",
        discovery
    );

    let authorize = send(
        &app,
        "GET",
        &format!(
            "/oauth/authorize?response_type=code&client_id=public&redirect_uri=http://127.0.0.1/cb&scope=user/Patient.read&aud=http://127.0.0.1&code_challenge={challenge}&code_challenge_method=S256"
        ),
        None,
        None,
    )
    .await;
    let stolen = authorize_code_from_redirect(&app, &authorize, "user/Patient.read").await;
    let wrong_client = send_text(
        &app,
        "POST",
        "/oauth/token",
        &format!(
            "grant_type=authorization_code&code={stolen}&redirect_uri=http://127.0.0.1/cb&client_id=confidential&client_secret=secret&code_verifier={verifier}"
        ),
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(
        wrong_client.0,
        StatusCode::BAD_REQUEST,
        "{}",
        wrong_client.1
    );
    let right_client = send_text(
        &app,
        "POST",
        "/oauth/token",
        &format!(
            "grant_type=authorization_code&code={stolen}&redirect_uri=http://127.0.0.1/cb&client_id=public&code_verifier={verifier}"
        ),
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(right_client.0, StatusCode::OK, "{}", right_client.1);
    let token_body: Value = serde_json::from_str(&right_client.1).unwrap();
    let access = token_body["access_token"].as_str().unwrap();

    let revoke = send_text(
        &app,
        "POST",
        "/oauth/revoke",
        &format!("token={access}&client_id=confidential&client_secret=secret"),
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(revoke.0, StatusCode::UNAUTHORIZED, "{}", revoke.1);
    let still = send_text(
        &app,
        "GET",
        "/Patient",
        "",
        &[("authorization", &format!("Bearer {access}"))],
    )
    .await;
    assert_eq!(still.0, StatusCode::OK, "{}", still.1);
}

async fn cors_header(
    app: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    name: &str,
) -> Option<String> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (header_name, value) in headers {
        builder = builder.header(*header_name, *value);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from("")).unwrap())
        .await
        .unwrap();
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

#[tokio::test]
async fn access_token_query_is_decoded_and_patient_compartment_holds() {
    let smart = std::sync::Arc::new(crate::auth::SmartStore::with_defaults());
    smart.install_token("tok+1/2=", &["user/Patient.read"], None);
    smart.install_token("patient-tok", &["patient/Patient.read"], Some("p1"));
    smart.install_token("writer-tok", &["user/Patient.write"], None);
    let (db, catalog, store) = memory();
    let app = crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: true,
            smart_demo_login: true,
            export_dir: "export".into(),
            terminology_urls: Vec::new(),
            sdc_archiver: false,
            smart: Some(smart),
            ..Default::default()
        },
    );
    let decoded = send(
        &app,
        "GET",
        "/Patient?access_token=tok%2B1%2F2%3D",
        None,
        None,
    )
    .await;
    assert_eq!(decoded.status, StatusCode::OK, "{}", decoded.body);
    let other = send(
        &app,
        "GET",
        "/Patient/p2?access_token=patient-tok",
        None,
        None,
    )
    .await;
    assert_eq!(other.status, StatusCode::FORBIDDEN, "{}", other.body);
    let own = send(
        &app,
        "GET",
        "/Patient/p1?access_token=patient-tok",
        None,
        None,
    )
    .await;
    assert_ne!(own.status, StatusCode::FORBIDDEN, "{}", own.body);
    let untyped = send_text(
        &app,
        "POST",
        "/",
        &patient("p9").to_string(),
        &[
            ("authorization", "Bearer writer-tok"),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    assert_eq!(untyped.0, StatusCode::FORBIDDEN, "{}", untyped.1);
    let graphql = send_text(
        &app,
        "POST",
        "/graphql",
        "{}",
        &[
            ("authorization", "Bearer writer-tok"),
            ("content-type", "application/json"),
        ],
    )
    .await;
    assert_eq!(graphql.0, StatusCode::FORBIDDEN, "{}", graphql.1);
    let created = send_text(
        &app,
        "POST",
        "/Patient",
        &patient("p9").to_string(),
        &[
            ("authorization", "Bearer writer-tok"),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
}

#[tokio::test]
async fn patient_scope_cannot_leave_its_compartment() {
    let (db, catalog, store) = memory();
    {
        let loaded = catalog.read().expect("catalog");
        for (id, subject) in [("o1", "p1"), ("o2", "p2")] {
            refhir_fhir::import_resource(&db, &loaded, &patient(subject)).unwrap();
            refhir_fhir::import_resource(
                &db,
                &loaded,
                &json!({
                    "resourceType": "Observation",
                    "id": id,
                    "subject": {"reference": format!("Patient/{subject}")},
                    "code": {"coding": [{"code": "8480-6"}]}
                }),
            )
            .unwrap();
        }
        assert!(crate::auth::guard_patient_operation(
            &db,
            &loaded,
            "Patient/p1",
            "Observation/read",
            &json!({"id": "o2"}),
        )
        .is_err());
        assert!(crate::auth::guard_patient_operation(
            &db,
            &loaded,
            "Patient/p1",
            "Observation/read",
            &json!({"id": "o1"}),
        )
        .is_ok());
        assert!(crate::auth::guard_patient_operation(
            &db,
            &loaded,
            "Patient/p1",
            "Bundle/transaction",
            &json!({"resource": {"entry": [{
                "resource": {
                    "resourceType": "Observation",
                    "id": "o3",
                    "subject": {"reference": "Patient/p2"},
                    "code": {"coding": [{"code": "8480-6"}]}
                },
                "request": {"method": "POST", "url": "Observation"}
            }]}}),
        )
        .is_err());
        assert!(crate::auth::guard_patient_operation(
            &db,
            &loaded,
            "Patient/p1",
            "Patient/create",
            &json!({"resource": patient("p2")}),
        )
        .is_err());
    }
    let smart = std::sync::Arc::new(crate::auth::SmartStore::with_defaults());
    smart.install_token(
        "compartment-writer",
        &["patient/*.write", "patient/*.read"],
        Some("p1"),
    );
    let app = crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: true,
            smart_demo_login: true,
            export_dir: "export".into(),
            terminology_urls: Vec::new(),
            sdc_archiver: false,
            smart: Some(smart),
            ..Default::default()
        },
    );
    let other = send_text(
        &app,
        "GET",
        "/Observation/o2",
        "",
        &[("authorization", "Bearer compartment-writer")],
    )
    .await;
    assert_eq!(other.0, StatusCode::FORBIDDEN, "{}", other.1);
    let own = send_text(
        &app,
        "GET",
        "/Observation/o1",
        "",
        &[("authorization", "Bearer compartment-writer")],
    )
    .await;
    assert_eq!(own.0, StatusCode::OK, "{}", own.1);
    let create_other = send_text(
        &app,
        "POST",
        "/Patient",
        &patient("p3").to_string(),
        &[
            ("authorization", "Bearer compartment-writer"),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    assert_eq!(create_other.0, StatusCode::FORBIDDEN, "{}", create_other.1);
}

/// Two patients with one Observation each (`o1` for `p1` with code 8480-6, `o2` for `p2` with
/// code 8462-4, so a search by code can single one out), and tokens: `p1-reader` and `p1-writer`
/// are bound to `p1`, `admin` is not.
fn two_patients_app() -> axum::Router {
    let (db, catalog, store) = memory();
    {
        let loaded = catalog.read().expect("catalog");
        for (id, subject, code) in [("o1", "p1", "8480-6"), ("o2", "p2", "8462-4")] {
            refhir_fhir::import_resource(&db, &loaded, &patient(subject)).unwrap();
            refhir_fhir::import_resource(
                &db,
                &loaded,
                &json!({
                    "resourceType": "Observation",
                    "id": id,
                    "subject": {"reference": format!("Patient/{subject}")},
                    "code": {"coding": [{"code": code}]}
                }),
            )
            .unwrap();
        }
    }
    let smart = std::sync::Arc::new(crate::auth::SmartStore::with_defaults());
    smart.install_token("p1-reader", &["patient/*.read"], Some("p1"));
    smart.install_token(
        "p1-writer",
        &["patient/*.read", "patient/*.write"],
        Some("p1"),
    );
    smart.install_token("admin", &["user/*.read", "user/*.write"], None);
    // Bound to `p1` and may read the Patient, and nothing else.
    smart.install_token("p1-patient-only", &["patient/Patient.read"], Some("p1"));
    // Not bound to a patient, and may not read any Patient.
    smart.install_token("observations-only", &["user/Observation.read"], None);
    crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: true,
            smart_demo_login: true,
            export_dir: "export".into(),
            terminology_urls: Vec::new(),
            sdc_archiver: false,
            smart: Some(smart),
            ..Default::default()
        },
    )
}

async fn get_as(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let (status, body) = send_text(
        app,
        "GET",
        uri,
        "",
        &[("authorization", &format!("Bearer {token}"))],
    )
    .await;
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

async fn post_as(app: &axum::Router, token: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let (status, body) = send_text(
        app,
        "POST",
        uri,
        &body.to_string(),
        &[
            ("authorization", &format!("Bearer {token}")),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_patient_summary_is_read_by_get_or_post_and_a_patients_token_reaches_only_its_own() {
    let app = two_patients_app();
    let parameters = json!({"resourceType": "Parameters", "parameter": []});

    let own = get_as(&app, "p1-reader", "/Patient/p1/$summary").await;
    assert_eq!(own.0, StatusCode::OK, "{}", own.1);
    assert_eq!(own.1["type"], "document");
    assert_eq!(own.1["entry"][0]["resource"]["resourceType"], "Composition");
    assert!(resource_ids(&own.1).contains(&"p1".to_string()));
    assert!(!resource_ids(&own.1).contains(&"p2".to_string()));

    // A POST is a read of the patient too, and its body is parameters, not a resource to place
    // in the token's compartment.
    let posted = post_as(&app, "p1-reader", "/Patient/p1/$summary", &parameters).await;
    assert_eq!(posted.0, StatusCode::OK, "{}", posted.1);
    let admin_post = post_as(&app, "admin", "/Patient/p2/$summary", &parameters).await;
    assert_eq!(admin_post.0, StatusCode::OK, "{}", admin_post.1);

    // Another patient's summary, and the type-level form that finds a patient by identifier, are
    // not for a token confined to one patient.
    let other = get_as(&app, "p1-reader", "/Patient/p2/$summary").await;
    assert_eq!(other.0, StatusCode::FORBIDDEN, "{}", other.1);
    let other_post = post_as(&app, "p1-reader", "/Patient/p2/$summary", &parameters).await;
    assert_eq!(other_post.0, StatusCode::FORBIDDEN, "{}", other_post.1);
    let by_identifier = get_as(&app, "p1-reader", "/Patient/$summary?identifier=x").await;
    assert_eq!(
        by_identifier.0,
        StatusCode::FORBIDDEN,
        "{}",
        by_identifier.1
    );

    // A token that may not read Patients, or none at all, is refused.
    let blind = get_as(&app, "observations-only", "/Patient/p1/$summary").await;
    assert_eq!(blind.0, StatusCode::FORBIDDEN, "{}", blind.1);
    let anonymous = send(&app, "GET", "/Patient/p1/$summary", None, None).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    // A caller that is not confined to a patient names one by identifier, or does not find it.
    let unknown = get_as(
        &app,
        "admin",
        "/Patient/$summary?identifier=http%3A%2F%2Fh%7Cnone",
    )
    .await;
    assert_eq!(unknown.0, StatusCode::NOT_FOUND, "{}", unknown.1);
    let unnamed = get_as(&app, "admin", "/Patient/$summary").await;
    assert_eq!(unnamed.0, StatusCode::BAD_REQUEST, "{}", unnamed.1);
    let wrong_type = get_as(&app, "admin", "/Observation/o1/$summary").await;
    assert_eq!(wrong_type.0, StatusCode::BAD_REQUEST, "{}", wrong_type.1);
}

#[tokio::test]
async fn everything_holds_only_what_the_tokens_scopes_let_it_read() {
    let app = two_patients_app();
    let all = get_as(&app, "p1-reader", "/Patient/p1/$everything").await;
    assert_eq!(all.0, StatusCode::OK, "{}", all.1);
    let ids = resource_ids(&all.1);
    assert!(
        ids.contains(&"p1".to_string()) && ids.contains(&"o1".to_string()),
        "{ids:?}"
    );
    assert!(!ids.contains(&"o2".to_string()), "{ids:?}");

    // The same patient with a token that may read the Patient and nothing else.
    let narrow = get_as(&app, "p1-patient-only", "/Patient/p1/$everything").await;
    assert_eq!(narrow.0, StatusCode::OK, "{}", narrow.1);
    assert_eq!(resource_ids(&narrow.1), ["p1"]);
}

#[tokio::test]
async fn the_capability_statement_names_the_summary_operation_of_a_patient() {
    let app = two_patients_app();
    let metadata = send(&app, "GET", "/metadata", None, None).await;
    assert_eq!(metadata.status, StatusCode::OK);
    let patient = metadata.body["rest"][0]["resource"]
        .as_array()
        .unwrap()
        .iter()
        .find(|resource| resource["type"] == "Patient")
        .unwrap();
    assert_eq!(patient["operation"][0]["name"], "summary");
    assert_eq!(
        patient["operation"][0]["definition"],
        "http://hl7.org/fhir/uv/ips/OperationDefinition/summary"
    );
}

async fn post_bundle_as(app: &axum::Router, token: &str, bundle: &Value) -> (StatusCode, Value) {
    let (status, body) = send_text(
        app,
        "POST",
        "/",
        &bundle.to_string(),
        &[
            ("authorization", &format!("Bearer {token}")),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

/// The ids of every resource anywhere in a response, so a test can say "no resource of `p2`'s came
/// back" without caring whether it came as a search hit, a Bundle entry or a nested response.
fn resource_ids(value: &Value) -> Vec<String> {
    fn walk(value: &Value, found: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if map.contains_key("resourceType") {
                    if let Some(id) = map.get("id").and_then(Value::as_str) {
                        found.push(id.to_string());
                    }
                }
                map.values().for_each(|item| walk(item, found));
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    walk(value, &mut found);
    found
}

fn leaks_p2(value: &Value) -> bool {
    resource_ids(value)
        .iter()
        .any(|id| id == "o2" || id == "p2")
}

#[tokio::test]
async fn a_patient_token_cannot_point_a_search_at_another_compartment() {
    let app = two_patients_app();
    let (status, own) = get_as(&app, "p1-reader", "/Observation").await;
    assert_eq!(status, StatusCode::OK, "{own}");
    assert_eq!(resource_ids(&own), ["o1"]);
    // Naming its own compartment is allowed, and changes nothing.
    let (status, same) = get_as(&app, "p1-reader", "/Observation?_compartment=Patient/p1").await;
    assert_eq!(status, StatusCode::OK, "{same}");
    assert_eq!(resource_ids(&same), ["o1"]);

    for uri in [
        "/Observation?_compartment=Patient/p2",
        "/Observation?_compartment=Patient%2Fp2",
        "/Observation?_compartment=Patient/p1&_compartment=Patient/p2",
        "/Observation?_compartment=Patient/p2&_compartment=Patient/p1",
        "/Observation?_id=o2&_compartment=Patient/p2",
        // The name the server uses for its own restriction is not the client's to set either.
        "/Observation?_patientCompartment=Patient/p2",
        "/Observation?_patientCompartment=Patient/p2&_compartment=Patient/p2",
        "/Patient?_compartment=Patient/p2",
    ] {
        let (status, found) = get_as(&app, "p1-reader", uri).await;
        assert!(!status.is_server_error(), "{uri}: {status} {found}");
        assert!(
            !leaks_p2(&found),
            "{uri} returned another patient's data: {found}"
        );
    }
}

#[tokio::test]
async fn a_patient_token_cannot_search_outside_its_compartment_inside_a_bundle() {
    let app = two_patients_app();
    for kind in ["batch", "transaction"] {
        // The first entry is the control: it must come back with the token's own data, which
        // shows the bundle is reachable and the search ran.
        let (status, body) = post_bundle_as(
            &app,
            "p1-writer",
            &json!({"resourceType": "Bundle", "type": kind, "entry": [
                {"request": {"method": "GET", "url": "Observation?_count=100"}}
            ]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{kind}: {body}");
        assert_eq!(resource_ids(&body), ["o1"], "{kind}: {body}");

        for url in [
            "Observation?code=8462-4",
            "Observation?_compartment=Patient/p2",
            "Observation?_patientCompartment=Patient/p2",
            "Observation?_id=o2",
        ] {
            let (status, body) = post_bundle_as(
                &app,
                "p1-writer",
                &json!({"resourceType": "Bundle", "type": kind, "entry": [
                    {"request": {"method": "GET", "url": url}}
                ]}),
            )
            .await;
            assert!(!status.is_server_error(), "{kind} {url}: {status} {body}");
            assert!(
                !leaks_p2(&body),
                "{kind} {url} returned another patient's data: {body}"
            );
        }
    }
}

#[tokio::test]
async fn a_patient_token_cannot_change_another_patients_data_with_a_conditional_entry() {
    let app = two_patients_app();
    let other = "Observation?code=8462-4";
    let rewrite = json!({
        "resourceType": "Observation",
        "subject": {"reference": "Patient/p1"},
        "code": {"coding": [{"code": "8462-4"}]}
    });
    for kind in ["batch", "transaction"] {
        for entry in [
            json!({"request": {"method": "DELETE", "url": other}}),
            json!({"request": {"method": "PUT", "url": other}, "resource": rewrite}),
            json!({"request": {"method": "POST", "url": "Observation", "ifNoneExist": "code=8462-4"}, "resource": rewrite}),
        ] {
            let (status, body) = post_bundle_as(
                &app,
                "p1-writer",
                &json!({"resourceType": "Bundle", "type": kind, "entry": [entry]}),
            )
            .await;
            assert!(!status.is_server_error(), "{kind} {entry}: {status} {body}");
            // Whatever the answer, p2's observation is untouched and was not handed back.
            assert!(
                !leaks_p2(&body),
                "{kind} {entry} returned another patient's data: {body}"
            );
            let (status, kept) = get_as(&app, "admin", "/Observation/o2").await;
            assert_eq!(status, StatusCode::OK, "{kind} {entry} removed o2: {kept}");
            assert_eq!(
                kept["subject"]["reference"], "Patient/p2",
                "{kind} {entry} rewrote o2: {kept}"
            );
        }
    }
}

#[tokio::test]
async fn a_patient_token_cannot_point_a_live_search_at_another_compartment() {
    let app = two_patients_app();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, served).await.unwrap();
    });
    let (mut socket, _) = connect(&format!("ws://{addr}/sync?access_token=p1-writer")).await;
    let queries = [
        json!({}),
        json!({"_compartment": "Patient/p2"}),
        json!({"_patientCompartment": "Patient/p2"}),
        json!({"_id": "o2", "_compartment": "Patient/p2"}),
    ];
    let mods: Vec<Value> = queries
        .iter()
        .enumerate()
        .map(|(index, args)| {
            json!({"type": "Add", "queryId": index + 1, "udfPath": "Observation/search", "args": args})
        })
        .collect();
    socket
        .send(Message::Text(
            json!({"type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1, "modifications": mods})
                .to_string(),
        ))
        .await
        .unwrap();
    let first = next_json(&mut socket).await;
    let items = first["modifications"].as_array().expect("modifications");
    assert_eq!(items.len(), queries.len(), "{first}");
    // The unrestricted query is the control: it answers, with the token's own data only.
    assert_eq!(items[0]["type"], "QueryUpdated", "{first}");
    assert_eq!(resource_ids(&items[0]["value"]), ["o1"], "{first}");
    for (index, item) in items.iter().enumerate() {
        assert!(
            !leaks_p2(item),
            "query {index} returned another patient's data: {item}"
        );
    }

    // The same through a Bundle mutation: a search entry, and a conditional write on `o2`.
    let other = "Observation?code=8462-4";
    for (id, bundle) in [
        (
            10,
            json!({"resourceType": "Bundle", "type": "batch", "entry": [
                {"request": {"method": "GET", "url": "Observation?_compartment=Patient/p2"}},
                {"request": {"method": "GET", "url": "Observation?_id=o2"}},
                {"request": {"method": "DELETE", "url": other}},
            ]}),
        ),
        (
            11,
            json!({"resourceType": "Bundle", "type": "transaction", "entry": [
                {"request": {"method": "GET", "url": "Observation?_id=o2"}},
            ]}),
        ),
        (
            12,
            json!({"resourceType": "Bundle", "type": "transaction", "entry": [
                {"request": {"method": "DELETE", "url": other}},
            ]}),
        ),
    ] {
        socket
            .send(Message::Text(
                json!({"type": "Mutation", "requestId": id, "udfPath": format!("Bundle/{}", bundle["type"].as_str().unwrap()), "args": {"resource": bundle}})
                    .to_string(),
            ))
            .await
            .unwrap();
        let reply = next_json(&mut socket).await;
        assert_eq!(reply["type"], "MutationResponse", "{reply}");
        assert!(
            !leaks_p2(&reply),
            "bundle {id} returned another patient's data: {reply}"
        );
    }
    let (status, kept) = get_as(&app, "admin", "/Observation/o2").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a conditional entry removed o2: {kept}"
    );
}

#[tokio::test]
async fn include_search_invalidates_when_the_included_patient_changes() {
    let (db, catalog, store) = memory();
    {
        let loaded = catalog.read().expect("catalog");
        refhir_fhir::import_resource(&db, &loaded, &patient("p1")).unwrap();
        refhir_fhir::import_resource(
            &db,
            &loaded,
            &json!({
                "resourceType": "Observation",
                "id": "o1",
                "subject": {"reference": "Patient/p1"},
                "code": {"coding": [{"code": "8480-6"}]}
            }),
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

    let app = subscription_app();
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
    let patient = put(&app, patient("p9")).await;
    assert_eq!(patient.status, StatusCode::CREATED, "{}", patient.body);
    let body = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(8)))
        .await
        .unwrap()
        .expect("rest-hook");
    assert!(body.contains("p9"), "{body}");
}

pub(crate) async fn send_text(
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
        .get("location")
        .or_else(|| response.headers().get("content-location"))
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
            sdc_archiver: false,
            ..Default::default()
        },
    );
    put(&app, patient("p1")).await;
    put(&app, patient("p2")).await;
    send(
        &app,
        "PUT",
        "/Observation/o1",
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
        "PUT",
        "/Observation/o2",
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
    let job = started
        .location
        .unwrap()
        .trim_start_matches("/export/")
        .to_string();
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
    assert!(
        file.1.contains("\"id\":\"p1\"") || file.1.contains("\"id\": \"p1\""),
        "{}",
        file.1
    );
    assert!(
        file.1.contains("\"id\":\"o1\"") || file.1.contains("\"id\": \"o1\""),
        "{}",
        file.1
    );
    assert!(
        !file.1.contains("\"id\":\"p2\"") && !file.1.contains("\"id\": \"p2\""),
        "{}",
        file.1
    );
    assert!(
        !file.1.contains("\"id\":\"o2\"") && !file.1.contains("\"id\": \"o2\""),
        "{}",
        file.1
    );
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

    let app = subscription_app();
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
    let patient = put(&app, patient("retry1")).await;
    assert_eq!(patient.status, StatusCode::CREATED, "{}", patient.body);
    let body = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(8)))
        .await
        .unwrap()
        .expect("retried hook");
    assert!(body.contains("retry1"), "{body}");
}

/// An app whose `Subscription` has the elements a delivery reads (`criteria`, the channel and its
/// headers) and the ones the server writes back (`status` and `error`), as R4's does.
fn subscription_app() -> axum::Router {
    // Tests run side by side, and a clock is too coarse to tell them apart.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let package = std::env::temp_dir().join(format!(
        "refhir-subscription-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{"name":"example.subscriptions","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    std::fs::write(
        package.join("StructureDefinition-Subscription.json"),
        r#"{"resourceType":"StructureDefinition","url":"http://example.org/StructureDefinition/Subscription","name":"Subscription","status":"active","kind":"resource","abstract":false,"type":"Subscription","fhirVersion":"4.0.1","derivation":"specialization","snapshot":{"element":[{"path":"Subscription","min":0,"max":"*"},{"path":"Subscription.status","min":0,"max":"1","type":[{"code":"code"}]},{"path":"Subscription.criteria","min":0,"max":"1","type":[{"code":"string"}]},{"path":"Subscription.error","min":0,"max":"1","type":[{"code":"string"}]},{"path":"Subscription.channel","min":0,"max":"1","type":[{"code":"BackboneElement"}]},{"path":"Subscription.channel.type","min":0,"max":"1","type":[{"code":"code"}]},{"path":"Subscription.channel.endpoint","min":0,"max":"1","type":[{"code":"url"}]},{"path":"Subscription.channel.header","min":0,"max":"*","type":[{"code":"string"}]}]}}"#,
    )
    .unwrap();
    let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
    let db = Arc::new(Database::open(store.clone()).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    install_package_dir(store.as_ref(), &db, &catalog, minir4_dir()).unwrap();
    install_package_dir(store.as_ref(), &db, &catalog, &package).unwrap();
    let _ = std::fs::remove_dir_all(&package);
    catalog.write().unwrap().set_update_create(true);
    app(db, catalog, store)
}

/// A rest-hook receiver on a local port. It answers every request with `status` (for example
/// `204 No Content`) and sends the text of each request it gets down the channel.
pub(crate) fn hook_receiver(status: &'static str) -> (u16, std::sync::mpsc::Receiver<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .ok();
            let request = read_http_request(&mut stream);
            let reply =
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = std::io::Write::write_all(&mut stream, reply.as_bytes());
            let _ = std::io::Write::flush(&mut stream);
            let _ = stream.shutdown(std::net::Shutdown::Both);
            if tx.send(request).is_err() {
                break;
            }
        }
    });
    (port, rx)
}

/// Read the Subscription until `ready` is true of it, for as long as `seconds`.
async fn subscription_where(
    app: &axum::Router,
    id: &str,
    seconds: u64,
    ready: impl Fn(&Value) -> bool,
) -> Value {
    let mut last = Value::Null;
    for _ in 0..seconds * 20 {
        let read = send(app, "GET", &format!("/Subscription/{id}"), None, None).await;
        last = read.body;
        if ready(&last) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("Subscription/{id} never became what the test waits for: {last}");
}

async fn subscription_versions(app: &axum::Router, id: &str) -> usize {
    let history = send(
        app,
        "GET",
        &format!("/Subscription/{id}/_history"),
        None,
        None,
    )
    .await;
    history.body["entry"].as_array().map_or(0, Vec::len)
}

/// Delivery is one loop that takes commits in order. A Subscription on every patient, answered
/// at once, therefore says when it has got through everything committed before the patient the
/// notification is for: including any status it decided to write about another Subscription.
/// Waiting on it is how a test shows that something did *not* happen, with no fixed sleep.
struct Barrier {
    app: axum::Router,
    requests: std::sync::mpsc::Receiver<String>,
    reached: usize,
}

impl Barrier {
    async fn new(app: &axum::Router) -> Barrier {
        let (port, requests) = hook_receiver("204 No Content");
        let created = put(
            app,
            json!({
                "resourceType": "Subscription",
                "id": "sub-barrier",
                "status": "active",
                "criteria": "Patient",
                "channel": {"type": "rest-hook", "endpoint": format!("http://127.0.0.1:{port}/barrier")}
            }),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
        Barrier {
            app: app.clone(),
            requests,
            reached: 0,
        }
    }

    /// Return once delivery has got through everything committed so far. The patient is not
    /// active, so no Subscription on `Patient?active=true` is woken by it.
    async fn caught_up(&mut self) {
        self.reached += 1;
        let id = format!("barrier-{}", self.reached);
        let written = put(
            &self.app,
            json!({"resourceType": "Patient", "id": id, "active": false}),
        )
        .await;
        assert_eq!(written.status, StatusCode::CREATED, "{}", written.body);
        let wanted = format!("\"id\":\"{id}\"");
        for _ in 0..400 {
            if self.requests.try_iter().any(|text| text.contains(&wanted)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("delivery never got to {id}");
    }
}

/// The request bodies of everything a receiver has been sent since it was last asked.
fn bodies(requests: &std::sync::mpsc::Receiver<String>) -> Vec<String> {
    requests.try_iter().collect()
}

#[tokio::test]
async fn rest_hook_sends_the_channel_headers() {
    let (port, requests) = hook_receiver("204 No Content");
    let app = subscription_app();
    let created = put(
        &app,
        json!({
            "resourceType": "Subscription",
            "id": "sub-headers",
            "status": "active",
            "criteria": "Patient?active=true",
            "channel": {
                "type": "rest-hook",
                "endpoint": format!("http://127.0.0.1:{port}/hook"),
                "header": ["Authorization: Bearer secret-token", "X-Tenant:  acme "]
            }
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let patient = put(&app, patient("h1")).await;
    assert_eq!(patient.status, StatusCode::CREATED, "{}", patient.body);
    let request =
        tokio::task::spawn_blocking(move || requests.recv_timeout(Duration::from_secs(8)))
            .await
            .unwrap()
            .expect("rest-hook");
    let lines: Vec<String> = request.lines().map(str::to_ascii_lowercase).collect();
    assert!(
        lines.contains(&"authorization: bearer secret-token".to_string()),
        "{request}"
    );
    assert!(lines.contains(&"x-tenant: acme".to_string()), "{request}");
    assert!(
        lines.contains(&"content-type: application/fhir+json".to_string()),
        "{request}"
    );
    assert!(request.contains("\"id\":\"h1\""), "{request}");
    // The notification worked, so the Subscription was not touched.
    assert_eq!(subscription_versions(&app, "sub-headers").await, 1);
}

#[tokio::test]
async fn a_channel_header_replaces_the_default_content_type_whatever_its_case() {
    let (port, requests) = hook_receiver("204 No Content");
    let app = subscription_app();
    let created = put(
        &app,
        json!({
            "resourceType": "Subscription",
            "id": "sub-type",
            "status": "active",
            "criteria": "Patient?active=true",
            "channel": {
                "type": "rest-hook",
                "endpoint": format!("http://127.0.0.1:{port}/hook"),
                "header": ["content-type: application/json"]
            }
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let patient = put(&app, patient("t1")).await;
    assert_eq!(patient.status, StatusCode::CREATED, "{}", patient.body);
    let request =
        tokio::task::spawn_blocking(move || requests.recv_timeout(Duration::from_secs(8)))
            .await
            .unwrap()
            .expect("rest-hook");
    let types: Vec<String> = request
        .lines()
        .map(str::to_ascii_lowercase)
        .filter(|line| line.starts_with("content-type:"))
        .collect();
    assert_eq!(types, vec!["content-type: application/json"], "{request}");
}

#[tokio::test]
async fn rest_hook_that_always_fails_ends_in_error() {
    let (port, requests) = hook_receiver("500 Internal Server Error");
    let app = subscription_app();
    let mut barrier = Barrier::new(&app).await;
    let created = put(
        &app,
        json!({
            "resourceType": "Subscription",
            "id": "sub-down",
            "status": "active",
            "criteria": "Patient?active=true",
            "channel": {"type": "rest-hook", "endpoint": format!("http://127.0.0.1:{port}/hook")}
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let patient = put(&app, patient("down1")).await;
    assert_eq!(patient.status, StatusCode::CREATED, "{}", patient.body);
    // Three attempts at once, then the rest when the queue runs and the notification is due:
    // eight in all.
    let failed = subscription_where(&app, "sub-down", 30, |sub| sub["status"] == "error").await;
    let error = failed["error"].as_str().expect("the failure is recorded");
    assert!(error.contains("8 attempts"), "{error}");
    assert!(error.contains("500"), "{error}");
    assert_eq!(bodies(&requests).len(), 8);
    // The server's own write is a second version, and did not start another round of delivery.
    barrier.caught_up().await;
    assert_eq!(bodies(&requests).len(), 0);
    assert_eq!(subscription_versions(&app, "sub-down").await, 2);
}

#[tokio::test]
async fn subscription_in_error_returns_to_active_after_a_delivery() {
    let (port, requests) = hook_receiver("204 No Content");
    let app = subscription_app();
    let created = put(
        &app,
        json!({
            "resourceType": "Subscription",
            "id": "sub-back",
            "status": "error",
            "error": "an earlier failure",
            "criteria": "Patient?active=true",
            "channel": {"type": "rest-hook", "endpoint": format!("http://127.0.0.1:{port}/hook")}
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    // `error` is not `off`: the Subscription is still delivered, and a delivery clears it.
    let patient = put(&app, patient("back1")).await;
    assert_eq!(patient.status, StatusCode::CREATED, "{}", patient.body);
    let body = tokio::task::spawn_blocking(move || requests.recv_timeout(Duration::from_secs(8)))
        .await
        .unwrap()
        .expect("rest-hook");
    assert!(body.contains("back1"), "{body}");
    let healed = subscription_where(&app, "sub-back", 8, |sub| sub["status"] == "active").await;
    assert!(healed.get("error").is_none(), "{healed}");
    assert_eq!(subscription_versions(&app, "sub-back").await, 2);
}

#[tokio::test]
async fn invalid_channel_header_ends_in_error_and_is_not_repeated() {
    let app = subscription_app();
    let mut barrier = Barrier::new(&app).await;
    let created = put(
        &app,
        json!({
            "resourceType": "Subscription",
            "id": "sub-bad-header",
            "status": "active",
            "criteria": "Patient?active=true",
            // Nothing listens here: a header that cannot be sent stops the request being made.
            "channel": {
                "type": "rest-hook",
                "endpoint": "http://127.0.0.1:9/hook",
                "header": ["Authorization Bearer s3cret"]
            }
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let first = put(&app, patient("bad1")).await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    let failed =
        subscription_where(&app, "sub-bad-header", 8, |sub| sub["status"] == "error").await;
    let error = failed["error"].as_str().expect("the failure is recorded");
    assert!(error.contains("channel.header entry 1"), "{error}");
    // Stamped with the time like every other failure, and the value of a header is a credential:
    // it is not repeated in the Subscription.
    assert!(error.starts_with("20"), "{error}");
    assert!(!error.contains("s3cret"), "{error}");
    assert_eq!(subscription_versions(&app, "sub-bad-header").await, 2);
    // Another matching resource fails the same way, and finds the status already `error`.
    let second = put(&app, patient("bad2")).await;
    assert_eq!(second.status, StatusCode::CREATED, "{}", second.body);
    barrier.caught_up().await;
    assert_eq!(subscription_versions(&app, "sub-bad-header").await, 2);
}

#[tokio::test]
async fn a_rest_hook_without_an_endpoint_ends_in_error() {
    let app = subscription_app();
    let created = put(
        &app,
        json!({
            "resourceType": "Subscription",
            "id": "sub-nowhere",
            "status": "active",
            "criteria": "Patient?active=true",
            "channel": {"type": "rest-hook"}
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let patient = put(&app, patient("nowhere1")).await;
    assert_eq!(patient.status, StatusCode::CREATED, "{}", patient.body);
    let failed = subscription_where(&app, "sub-nowhere", 8, |sub| sub["status"] == "error").await;
    let error = failed["error"].as_str().expect("the failure is recorded");
    assert!(error.contains("no endpoint"), "{error}");
}

#[tokio::test]
async fn a_client_edit_wins_over_the_status_write_and_stops_the_retries() {
    let (port, requests) = hook_receiver("500 Internal Server Error");
    let app = subscription_app();
    let mut barrier = Barrier::new(&app).await;
    let subscription = json!({
        "resourceType": "Subscription",
        "id": "sub-edited",
        "status": "active",
        "criteria": "Patient?active=true",
        "channel": {"type": "rest-hook", "endpoint": format!("http://127.0.0.1:{port}/hook")}
    });
    let created = put(&app, subscription.clone()).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let patient = put(&app, patient("edited1")).await;
    assert_eq!(patient.status, StatusCode::CREATED, "{}", patient.body);
    // The three quick attempts are in; the rest are waiting in the queue. The client turns the
    // Subscription off.
    let requests = tokio::task::spawn_blocking(move || {
        for _ in 0..3 {
            requests
                .recv_timeout(Duration::from_secs(8))
                .expect("a quick attempt");
        }
        requests
    })
    .await
    .unwrap();
    let mut switched_off = subscription.clone();
    switched_off["status"] = json!("off");
    let off = put(&app, switched_off).await;
    assert_eq!(off.status, StatusCode::OK, "{}", off.body);
    // Long enough for the queue to have run twice, which it would have used to try again.
    tokio::time::sleep(Duration::from_secs(4)).await;
    barrier.caught_up().await;
    assert_eq!(bodies(&requests).len(), 0, "retries went on after `off`");
    let stored = send(&app, "GET", "/Subscription/sub-edited", None, None).await;
    assert_eq!(stored.body["status"], "off", "{}", stored.body);
    assert!(stored.body.get("error").is_none(), "{}", stored.body);
    assert_eq!(subscription_versions(&app, "sub-edited").await, 2);
}

#[tokio::test]
async fn commits_do_not_use_up_the_retries() {
    let (port, requests) = hook_receiver("500 Internal Server Error");
    let app = subscription_app();
    let mut barrier = Barrier::new(&app).await;
    let created = put(
        &app,
        json!({
            "resourceType": "Subscription",
            "id": "sub-busy",
            "status": "active",
            "criteria": "Patient?active=true",
            "channel": {"type": "rest-hook", "endpoint": format!("http://127.0.0.1:{port}/hook")}
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let patient = put(&app, patient("busy1")).await;
    assert_eq!(patient.status, StatusCode::CREATED, "{}", patient.body);
    // The three quick attempts are made, and the rest are queued for later.
    let requests = tokio::task::spawn_blocking(move || {
        for _ in 0..3 {
            requests
                .recv_timeout(Duration::from_secs(8))
                .expect("a quick attempt");
        }
        requests
    })
    .await
    .unwrap();
    // Every commit runs the retry queue. A burst of them, which is a busy server, must not spend
    // the attempts that are meant to be spread over about ten seconds. (One may be due on a slow
    // machine; five would be the burst.)
    for _ in 0..5 {
        barrier.caught_up().await;
    }
    let extra = bodies(&requests).len();
    assert!(
        extra <= 1,
        "{extra} retries were made during a burst of commits"
    );
    let stored = send(&app, "GET", "/Subscription/sub-busy", None, None).await;
    assert_eq!(stored.body["status"], "active", "{}", stored.body);
}

#[tokio::test]
async fn a_subscription_on_subscriptions_is_not_woken_by_its_own_status_write() {
    let app = subscription_app();
    let mut barrier = Barrier::new(&app).await;
    // The criteria match every Subscription, so the `error` the server writes is a change this
    // one is for. Its entry cannot be sent, which fails every notification at once.
    let created = put(
        &app,
        json!({
            "resourceType": "Subscription",
            "id": "sub-self",
            "status": "active",
            "criteria": "Subscription",
            "channel": {"type": "rest-hook", "endpoint": "http://127.0.0.1:9/hook", "header": ["bad"]}
        }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    subscription_where(&app, "sub-self", 8, |sub| sub["status"] == "error").await;
    barrier.caught_up().await;
    // Create, then the one write that says what went wrong; the write did not start another.
    assert_eq!(subscription_versions(&app, "sub-self").await, 2);
}

#[tokio::test]
async fn a_notification_of_a_subscription_leaves_out_its_header_credentials() {
    let (port, requests) = hook_receiver("204 No Content");
    let app = subscription_app();
    // This one is told about every Subscription that is written.
    let watcher = put(
        &app,
        json!({
            "resourceType": "Subscription",
            "id": "sub-watcher",
            "status": "active",
            "criteria": "Subscription",
            "channel": {"type": "rest-hook", "endpoint": format!("http://127.0.0.1:{port}/hook")}
        }),
    )
    .await;
    assert_eq!(watcher.status, StatusCode::CREATED, "{}", watcher.body);
    let victim = put(
        &app,
        json!({
            "resourceType": "Subscription",
            "id": "sub-victim",
            "status": "active",
            "criteria": "Patient?active=true",
            "channel": {
                "type": "rest-hook",
                "endpoint": "http://127.0.0.1:9/hook",
                "header": ["Authorization: Bearer s3cret"]
            }
        }),
    )
    .await;
    assert_eq!(victim.status, StatusCode::CREATED, "{}", victim.body);
    let told = tokio::task::spawn_blocking(move || {
        for _ in 0..4 {
            let request = requests
                .recv_timeout(Duration::from_secs(8))
                .expect("the watcher is told");
            if request.contains("sub-victim") {
                return request;
            }
        }
        panic!("the watcher was not told about sub-victim");
    })
    .await
    .unwrap();
    assert!(told.contains("http://127.0.0.1:9/hook"), "{told}");
    assert!(!told.contains("s3cret"), "{told}");
    assert!(!told.contains("Authorization"), "{told}");
    // The stored Subscription is untouched: only what is sent on is left out.
    let stored = send(&app, "GET", "/Subscription/sub-victim", None, None).await;
    assert_eq!(
        stored.body["channel"]["header"][0], "Authorization: Bearer s3cret",
        "{}",
        stored.body
    );
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

#[tokio::test]
async fn sdc_archiver_hides_responses_and_receiver_reads_them() {
    let (db, catalog, store) = memory();
    install_package_dir(store.as_ref(), &db, &catalog, sdc_dir()).unwrap();
    let receiver = crate::app_with(
        db.clone(),
        catalog.clone(),
        store.clone(),
        crate::ServerConfig {
            auth: false,
            export_dir: "export".into(),
            terminology_urls: Vec::new(),
            sdc_archiver: false,
            ..Default::default()
        },
    );
    let created = send(
        &receiver,
        "POST",
        "/QuestionnaireResponse",
        Some(json!({
            "resourceType": "QuestionnaireResponse",
            "status": "in-progress",
            "item": [{"linkId": "q1", "answer": [{"valueString": "yes"}]}]
        })),
        None,
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.body["id"].as_str().unwrap();
    let read = send(
        &receiver,
        "GET",
        &format!("/QuestionnaireResponse/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);
    let patient = send(
        &receiver,
        "POST",
        "/Patient",
        Some(json!({
            "resourceType": "Patient",
            "id": "p1",
            "active": true,
            "name": [{"family": "Ada"}]
        })),
        None,
    )
    .await;
    assert_eq!(patient.status, StatusCode::CREATED);
    let answered = send(
        &receiver,
        "POST",
        "/QuestionnaireResponse",
        Some(json!({
            "resourceType": "QuestionnaireResponse",
            "status": "in-progress",
            "subject": {"reference": "Patient/p1"},
            "item": [{"linkId": "q1", "answer": [{"valueString": "secret"}]}]
        })),
        None,
    )
    .await;
    assert_eq!(answered.status, StatusCode::CREATED);

    let archiver_db = db.clone();
    let archiver_catalog = catalog.clone();
    let archiver_store = store.clone();
    let archiver = crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: false,
            export_dir: "export".into(),
            terminology_urls: Vec::new(),
            sdc_archiver: true,
            ..Default::default()
        },
    );
    let hidden = send(
        &archiver,
        "GET",
        &format!("/QuestionnaireResponse/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(hidden.status, StatusCode::FORBIDDEN);
    let stored = send(
        &archiver,
        "POST",
        "/QuestionnaireResponse",
        Some(json!({
            "resourceType": "QuestionnaireResponse",
            "status": "completed",
            "questionnaire": "http://example.org/Questionnaire/missing"
        })),
        None,
    )
    .await;
    assert_eq!(stored.status, StatusCode::CREATED);
    let compartment = send(
        &archiver,
        "GET",
        "/Patient/p1/QuestionnaireResponse",
        None,
        None,
    )
    .await;
    assert_eq!(compartment.status, StatusCode::FORBIDDEN);
    let bundled = send(
        &archiver,
        "POST",
        "/",
        Some(json!({
            "resourceType": "Bundle",
            "type": "batch",
            "entry": [{"request": {"method": "GET", "url": format!("QuestionnaireResponse/{id}")}}]
        })),
        None,
    )
    .await;
    assert_eq!(bundled.status, StatusCode::OK);
    assert!(
        bundled.body["entry"][0]["response"]["status"]
            .as_str()
            .unwrap_or("")
            .starts_with("403"),
        "{}",
        bundled.body
    );
    let everything = send(
        &archiver,
        "GET",
        &format!("/QuestionnaireResponse/{id}/$everything"),
        None,
        None,
    )
    .await;
    assert_eq!(everything.status, StatusCode::FORBIDDEN);
    let meta = send(
        &archiver,
        "GET",
        &format!("/QuestionnaireResponse/{id}/$meta"),
        None,
        None,
    )
    .await;
    assert_eq!(meta.status, StatusCode::FORBIDDEN);
    let patched = send(
        &archiver,
        "PATCH",
        &format!("/QuestionnaireResponse/{id}"),
        Some(json!([{"op": "replace", "path": "/status", "value": "in-progress"}])),
        None,
    )
    .await;
    assert_eq!(patched.status, StatusCode::FORBIDDEN, "{}", patched.body);
    let validated = send(
        &archiver,
        "POST",
        &format!("/QuestionnaireResponse/{id}/$validate"),
        None,
        None,
    )
    .await;
    assert_eq!(validated.status, StatusCode::FORBIDDEN);
    let revinclude = send(
        &archiver,
        "GET",
        "/Patient?_revinclude=QuestionnaireResponse:subject",
        None,
        None,
    )
    .await;
    assert_eq!(revinclude.status, StatusCode::OK, "{}", revinclude.body);
    let leaked = revinclude.body["entry"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["resource"]["resourceType"] == "QuestionnaireResponse");
    assert!(!leaked, "{}", revinclude.body);
    let synced = crate::http::execute(
        &archiver_db,
        &archiver_catalog,
        &archiver_store,
        "QuestionnaireResponse/read",
        json!({"id": id, "_sdcArchiver": false}),
    )
    .await;
    assert!(synced.is_err());
}

#[tokio::test]
async fn populate_respects_observation_scopes() {
    let (db, catalog, store) = memory();
    install_package_dir(store.as_ref(), &db, &catalog, sdc_dir()).unwrap();
    let app = crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: true,
            smart_demo_login: true,
            export_dir: "export".into(),
            terminology_urls: Vec::new(),
            sdc_archiver: false,
            ..Default::default()
        },
    );
    let admin = token(&app, "user/*.*").await;
    send_text(
        &app,
        "PUT",
        "/Patient/p1",
        &json!({
            "resourceType": "Patient",
            "id": "p1",
            "active": true,
            "name": [{"family": "Ada"}]
        })
        .to_string(),
        &[
            ("authorization", &format!("Bearer {admin}")),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    send_text(
        &app,
        "POST",
        "/Observation",
        &json!({
            "resourceType": "Observation",
            "status": "final",
            "code": {"coding": [{"system": "http://loinc.org", "code": "29463-7"}]},
            "subject": {"reference": "Patient/p1"},
            "effectiveDateTime": "2099-01-01T00:00:00Z",
            "valueString": "70"
        })
        .to_string(),
        &[
            ("authorization", &format!("Bearer {admin}")),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    let form = json!({
        "resourceType": "Questionnaire",
        "url": "http://example.org/Questionnaire/weight",
        "status": "active",
        "item": [{
            "linkId": "weight",
            "type": "string",
            "code": [{"system": "http://loinc.org", "code": "29463-7"}],
            "extension": [{
                "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-observationLinkPeriod",
                "valueDuration": {"value": 1, "code": "a"}
            }]
        }]
    });
    let limited = token(&app, "user/Questionnaire.*").await;
    let hidden = send_text(
        &app,
        "POST",
        "/Questionnaire/$populate",
        &json!({"resourceType": "Parameters", "parameter": [
            {"name": "questionnaire", "resource": form.clone()},
            {"name": "subject", "valueString": "Patient/p1"}
        ]})
        .to_string(),
        &[
            ("authorization", &format!("Bearer {limited}")),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    assert_eq!(hidden.0, StatusCode::OK, "{}", hidden.1);
    let hidden_body: Value = serde_json::from_str(&hidden.1).unwrap();
    assert!(hidden_body["item"]
        .as_array()
        .map(|items| items.is_empty())
        .unwrap_or(true));

    let read_only = token(&app, "user/Questionnaire.read").await;
    let read_populate = send_text(
        &app,
        "POST",
        "/Questionnaire/$populate",
        &json!({"resourceType": "Parameters", "parameter": [
            {"name": "questionnaire", "resource": form.clone()},
            {"name": "subject", "valueString": "Patient/p1"}
        ]})
        .to_string(),
        &[
            ("authorization", &format!("Bearer {read_only}")),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    assert_eq!(read_populate.0, StatusCode::OK, "{}", read_populate.1);

    let reader = token(&app, "user/Questionnaire.* user/Observation.read").await;
    let shown = send_text(
        &app,
        "POST",
        "/Questionnaire/$populate",
        &json!({"resourceType": "Parameters", "parameter": [
            {"name": "questionnaire", "resource": form},
            {"name": "subject", "valueString": "Patient/p1"}
        ]})
        .to_string(),
        &[
            ("authorization", &format!("Bearer {reader}")),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    assert_eq!(shown.0, StatusCode::OK, "{}", shown.1);
    let shown_body: Value = serde_json::from_str(&shown.1).unwrap();
    assert_eq!(shown_body["item"][0]["answer"][0]["valueString"], "70");
}

#[tokio::test]
async fn bundle_and_graphql_keep_read_scopes() {
    let (db, catalog, store) = memory();
    install_package_dir(store.as_ref(), &db, &catalog, sdc_dir()).unwrap();
    let app = crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: true,
            smart_demo_login: true,
            export_dir: "export".into(),
            terminology_urls: Vec::new(),
            sdc_archiver: false,
            ..Default::default()
        },
    );
    let admin = token(&app, "user/*.*").await;
    let created = send_text(
        &app,
        "PUT",
        "/Patient/p1",
        &json!({
            "resourceType": "Patient",
            "id": "p1",
            "active": true,
            "name": [{"family": "Ada"}]
        })
        .to_string(),
        &[
            ("authorization", &format!("Bearer {admin}")),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);

    let writer = token(&app, "user/*.write").await;
    let bundle = launch_bundle().to_string();
    let denied = send_text(
        &app,
        "POST",
        "/",
        &bundle,
        &[
            ("authorization", &format!("Bearer {writer}")),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    assert_eq!(denied.0, StatusCode::BAD_REQUEST, "{}", denied.1);
    assert!(
        denied.1.contains("unknown variable"),
        "bundle write must not read Patient: {}",
        denied.1
    );

    let allowed = send_text(
        &app,
        "POST",
        "/",
        &bundle,
        &[
            ("authorization", &format!("Bearer {admin}")),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    assert_eq!(allowed.0, StatusCode::OK, "{}", allowed.1);
    let allowed_body: Value = serde_json::from_str(&allowed.1).unwrap();
    assert_eq!(
        allowed_body["entry"][0]["resource"]["item"][0]["answer"][0]["valueString"],
        "secret"
    );

    let resource = launch_bundle()["entry"][0]["resource"].to_string();
    let query = format!("mutation {{ QuestionnaireResponseCreate(resource: '{resource}') }}");
    let gql_denied = send_text(
        &app,
        "POST",
        "/graphql",
        &json!({"query": query}).to_string(),
        &[
            ("authorization", &format!("Bearer {writer}")),
            ("content-type", "application/json"),
        ],
    )
    .await;
    assert_eq!(gql_denied.0, StatusCode::BAD_REQUEST, "{}", gql_denied.1);
    assert!(
        gql_denied.1.contains("unknown variable"),
        "graphql write must not read Patient: {}",
        gql_denied.1
    );

    let gql_allowed = send_text(
        &app,
        "POST",
        "/graphql",
        &json!({"query": query}).to_string(),
        &[
            ("authorization", &format!("Bearer {admin}")),
            ("content-type", "application/json"),
        ],
    )
    .await;
    assert_eq!(gql_allowed.0, StatusCode::OK, "{}", gql_allowed.1);
    let gql_body: Value = serde_json::from_str(&gql_allowed.1).unwrap();
    assert_eq!(
        gql_body["data"]["QuestionnaireResponseCreate"]["item"][0]["answer"][0]["valueString"],
        "secret"
    );
}

fn launch_bundle() -> Value {
    json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": [{
            "resource": {
                "resourceType": "QuestionnaireResponse",
                "status": "completed",
                "questionnaire": "#q",
                "subject": {"reference": "Patient/p1"},
                "contained": [{
                    "resourceType": "Questionnaire",
                    "id": "q",
                    "status": "active",
                    "extension": [{
                        "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-launchContext",
                        "extension": [
                            {"url": "name", "valueCoding": {"code": "patient"}},
                            {"url": "type", "valueCode": "Patient"}
                        ]
                    }],
                    "item": [{
                        "linkId": "note",
                        "type": "string",
                        "extension": [{
                            "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-enableWhenExpression",
                            "valueExpression": {
                                "language": "text/fhirpath",
                                "expression": "%patient.active"
                            }
                        }]
                    }]
                }],
                "item": [{"linkId": "note", "answer": [{"valueString": "secret"}]}]
            },
            "request": {"method": "POST", "url": "QuestionnaireResponse"}
        }]
    })
}

async fn token(app: &axum::Router, scope: &str) -> String {
    let body = smart_token(app, scope, "http://127.0.0.1/cb").await;
    body["access_token"].as_str().unwrap().to_string()
}

async fn smart_token(app: &axum::Router, scope: &str, redirect: &str) -> Value {
    smart_token_for(app, scope, redirect, "http://127.0.0.1").await
}

async fn authorize_code_from_redirect(
    app: &axum::Router,
    redirected: &Reply,
    scope: &str,
) -> String {
    let location = redirected.location.as_deref().unwrap_or("");
    if location.contains("/oauth/interact") {
        let session = query_value(location, "session").expect("session");
        let needs_patient = scope
            .split_whitespace()
            .any(|item| item == "launch/patient")
            && !scope.split_whitespace().any(|item| item == "launch");
        let mut body = format!(
            "session={}&username=dev&action=approve",
            encode_query(&session)
        );
        if needs_patient {
            body.push_str("&patient_id=p1");
        }
        let approved = send_text(
            app,
            "POST",
            "/oauth/interact",
            &body,
            &[("content-type", "application/x-www-form-urlencoded")],
        )
        .await;
        assert_eq!(approved.0, StatusCode::FOUND, "{}", approved.1);
        query_value(&approved.1, "code").expect("code")
    } else {
        query_value(location, "code").expect("code")
    }
}

async fn smart_token_for(app: &axum::Router, scope: &str, redirect: &str, aud: &str) -> Value {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use sha2::{Digest, Sha256};

    let verifier = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut query = format!(
        "response_type=code&client_id=public&redirect_uri={}&scope={}&aud={}&code_challenge={}&code_challenge_method=S256&state=s1",
        encode_query(redirect),
        encode_query(scope),
        encode_query(aud),
        challenge
    );
    if scope.split_whitespace().any(|item| item == "launch") {
        let launched = send(
            app,
            "GET",
            "/oauth/ehr-launch?client_id=public&launch_uri=http://127.0.0.1/app&patient=p1",
            None,
            None,
        )
        .await;
        assert_eq!(launched.status, StatusCode::FOUND, "{}", launched.body);
        let launch =
            query_value(launched.location.as_deref().unwrap_or(""), "launch").expect("launch");
        query.push_str("&launch=");
        query.push_str(&encode_query(&launch));
    }
    let redirected = send(app, "GET", &format!("/oauth/authorize?{query}"), None, None).await;
    assert_eq!(redirected.status, StatusCode::FOUND, "{}", redirected.body);
    let code = authorize_code_from_redirect(app, &redirected, scope).await;
    let issued = send_text(
        app,
        "POST",
        "/oauth/token",
        &format!(
            "grant_type=authorization_code&code={code}&redirect_uri={}&client_id=public&code_verifier={verifier}",
            encode_query(redirect)
        ),
        &[("content-type", "application/x-www-form-urlencoded")],
    )
    .await;
    assert_eq!(issued.0, StatusCode::OK, "{}", issued.1);
    serde_json::from_str(&issued.1).unwrap()
}

fn encode_query(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn query_value(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        if key == name {
            return Some(
                value
                    .replace('+', " ")
                    .replace("%20", " ")
                    .replace("%2F", "/")
                    .replace("%3A", ":"),
            );
        }
    }
    None
}

mod oidc {
    use super::*;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::{get, post};
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    use rsa::traits::PublicKeyParts;
    use sha2::{Digest, Sha256};
    use std::sync::Mutex;

    const CLIENT: &str = "refhir-client";

    #[derive(Default)]
    struct Mock {
        base: String,
        /// (nonce, code_challenge) recorded from the authorize redirect.
        flow: Option<(String, String)>,
        wrong_nonce: bool,
        used_basic_auth: bool,
        /// Claims added to (or replacing those in) the ID token.
        claims: Value,
        /// Leave `fhirUser` out of the ID token, as an IdP that knows nothing about FHIR does.
        omit_fhir_user: bool,
    }

    struct Idp {
        base: String,
        mock: Arc<Mutex<Mock>>,
    }

    async fn start_idp() -> Idp {
        let rsa = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
        let pem = rsa.to_pkcs8_pem(LineEnding::LF).unwrap();
        let encoding = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap();
        let public = rsa::RsaPublicKey::from(&rsa);
        let jwks = json!({"keys": [{
            "kty": "RSA", "kid": "k1", "alg": "RS256", "use": "sig",
            "n": URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
            "e": URL_SAFE_NO_PAD.encode(public.e().to_bytes_be()),
        }]});
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mock = Arc::new(Mutex::new(Mock {
            base: base.clone(),
            ..Default::default()
        }));
        let router = axum::Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(|State(m): State<Arc<Mutex<Mock>>>| async move {
                    let base = m.lock().unwrap().base.clone();
                    axum::Json(json!({
                        "issuer": base,
                        "authorization_endpoint": format!("{base}/authorize"),
                        "token_endpoint": format!("{base}/token"),
                        "jwks_uri": format!("{base}/jwks"),
                    }))
                }),
            )
            .route(
                "/jwks",
                get(move || {
                    let jwks = jwks.clone();
                    async move { axum::Json(jwks) }
                }),
            )
            .route(
                "/token",
                post(
                    move |State(m): State<Arc<Mutex<Mock>>>,
                          headers: HeaderMap,
                          body: axum::body::Bytes| {
                        let encoding = encoding.clone();
                        async move {
                            let form = crate::auth::parse_authorize_form(&body);
                            let mut mock = m.lock().unwrap();
                            mock.used_basic_auth = headers
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .is_some_and(|v| v.starts_with("Basic "));
                            let (nonce, challenge) = mock.flow.clone().unwrap();
                            let verifier = form.get("code_verifier").cloned().unwrap_or_default();
                            let proven = URL_SAFE_NO_PAD
                                .encode(Sha256::digest(verifier.as_bytes()))
                                == challenge;
                            if form.get("code").map(String::as_str) != Some("good-code") || !proven
                            {
                                return (
                                    StatusCode::BAD_REQUEST,
                                    axum::Json(json!({"error": "invalid_grant"})),
                                );
                            }
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_secs();
                            let mut claims = json!({
                                "iss": mock.base, "aud": CLIENT, "sub": "idp-user-1",
                                "exp": now + 300, "iat": now,
                                "nonce": if mock.wrong_nonce { "other".to_string() } else { nonce },
                                "preferred_username": "Dr One",
                                "fhirUser": "Practitioner/dr-1",
                            });
                            if mock.omit_fhir_user {
                                claims.as_object_mut().unwrap().remove("fhirUser");
                            }
                            for (name, value) in mock.claims.as_object().into_iter().flatten() {
                                claims[name.as_str()] = value.clone();
                            }
                            let mut header =
                                jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
                            header.kid = Some("k1".into());
                            let id_token =
                                jsonwebtoken::encode(&header, &claims, &encoding).unwrap();
                            (
                                StatusCode::OK,
                                axum::Json(json!({"id_token": id_token, "access_token": "unused"})),
                            )
                        }
                    },
                ),
            )
            .with_state(mock.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Idp { base, mock }
    }

    fn smart_app(oidc: Option<&Idp>, demo: bool) -> axum::Router {
        let (db, catalog, store) = memory();
        crate::app_with(
            db,
            catalog,
            store,
            crate::ServerConfig {
                auth: true,
                smart_demo_login: demo,
                public_url: Some("http://127.0.0.1".into()),
                oidc: oidc.map(|idp| {
                    let mut cfg = crate::OidcConfig::new(idp.base.clone(), CLIENT);
                    cfg.client_secret = Some("idp-secret".into());
                    cfg
                }),
                ..Default::default()
            },
        )
    }

    async fn raw(
        app: &axum::Router,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (StatusCode, HeaderMap, String) {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8_lossy(&bytes).to_string())
    }

    fn location(headers: &HeaderMap) -> String {
        headers["location"].to_str().unwrap().to_string()
    }

    const VERIFIER: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

    /// Authorize, then follow `/oauth/interact` to the provider. Returns the client session id,
    /// the provider `state` and the browser cookie.
    async fn start_login(app: &axum::Router, idp: &Idp) -> (String, String, String) {
        start_login_with(app, idp, "openid fhirUser user/Patient.read", "").await
    }

    async fn start_login_with(
        app: &axum::Router,
        idp: &Idp,
        scope: &str,
        extra_query: &str,
    ) -> (String, String, String) {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
        let query = format!(
            "response_type=code&client_id=public&redirect_uri={}&scope={}&aud=http%3A%2F%2F127.0.0.1&code_challenge={challenge}&code_challenge_method=S256&state=s1{extra_query}",
            encode_query("http://127.0.0.1/cb"),
            encode_query(scope),
        );
        let (status, headers, body) =
            raw(app, "GET", &format!("/oauth/authorize?{query}"), &[], "").await;
        assert_eq!(status, StatusCode::FOUND, "{body}");
        let interact = location(&headers);
        assert!(interact.starts_with("/oauth/interact"), "{interact}");
        let session = query_value(&interact, "session").unwrap();

        let (status, headers, body) = raw(app, "GET", &interact, &[], "").await;
        assert_eq!(status, StatusCode::FOUND, "{body}");
        let to_idp = location(&headers);
        assert!(
            to_idp.starts_with(&format!("{}/authorize?", idp.base)),
            "{to_idp}"
        );
        assert_eq!(
            query_value(&to_idp, "code_challenge_method").as_deref(),
            Some("S256")
        );
        assert_eq!(query_value(&to_idp, "client_id").as_deref(), Some(CLIENT));
        let cookie = headers["set-cookie"].to_str().unwrap();
        assert!(
            cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"),
            "{cookie}"
        );
        let cookie = cookie.split(';').next().unwrap().to_string();
        let state = query_value(&to_idp, "state").unwrap();
        idp.mock.lock().unwrap().flow = Some((
            query_value(&to_idp, "nonce").unwrap(),
            query_value(&to_idp, "code_challenge").unwrap(),
        ));
        (session, state, cookie)
    }

    fn callback(state: &str) -> String {
        format!(
            "/oauth/oidc/callback?code=good-code&state={}",
            encode_query(state)
        )
    }

    #[tokio::test]
    async fn signs_in_through_the_provider() {
        let idp = start_idp().await;
        let app = smart_app(Some(&idp), false);
        let (session, state, cookie) = start_login(&app, &idp).await;

        // Approving before the provider has authenticated anyone issues no code.
        let (status, headers, _) = raw(
            &app,
            "POST",
            "/oauth/interact",
            &[("content-type", "application/x-www-form-urlencoded")],
            &format!(
                "session={}&username=attacker&action=approve",
                encode_query(&session)
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FOUND);
        assert!(
            location(&headers).starts_with("/oauth/interact"),
            "{headers:?}"
        );
        assert!(!location(&headers).contains("code="));

        let (status, headers, body) =
            raw(&app, "GET", &callback(&state), &[("cookie", &cookie)], "").await;
        assert_eq!(status, StatusCode::FOUND, "{body}");
        assert!(location(&headers).starts_with("/oauth/interact"));
        assert!(idp.mock.lock().unwrap().used_basic_auth);

        let page = raw(&app, "GET", &location(&headers), &[], "").await;
        assert_eq!(page.0, StatusCode::OK);
        assert!(
            page.2.contains("Signed in as <strong>Dr One</strong>"),
            "{}",
            page.2
        );
        assert!(!page.2.contains("name=\"username\""));

        // Another browser holding only the session id cannot approve for the signed-in user.
        let (status, headers, _) = raw(
            &app,
            "POST",
            "/oauth/interact",
            &[("content-type", "application/x-www-form-urlencoded")],
            &format!("session={}&action=approve", encode_query(&session)),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(headers.get("location").is_none());

        // The typed username is ignored: the provider's subject is what the token carries.
        let (status, headers, body) = raw(
            &app,
            "POST",
            "/oauth/interact",
            &[
                ("content-type", "application/x-www-form-urlencoded"),
                ("cookie", &cookie),
            ],
            &format!(
                "session={}&username=attacker&action=approve",
                encode_query(&session)
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FOUND, "{body}");
        let code = query_value(&location(&headers), "code").expect("code");
        let (status, _, body) = raw(
            &app,
            "POST",
            "/oauth/token",
            &[("content-type", "application/x-www-form-urlencoded")],
            &format!(
                "grant_type=authorization_code&code={code}&redirect_uri={}&client_id=public&code_verifier={VERIFIER}",
                encode_query("http://127.0.0.1/cb")
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let tokens: Value = serde_json::from_str(&body).unwrap();
        let payload = tokens["id_token"]
            .as_str()
            .unwrap()
            .split('.')
            .nth(1)
            .unwrap();
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
        assert_eq!(claims["sub"], "idp-user-1");
        assert_eq!(claims["fhirUser"], "http://127.0.0.1/Practitioner/dr-1");
    }

    #[tokio::test]
    async fn rejects_a_callback_from_another_browser() {
        let idp = start_idp().await;
        let app = smart_app(Some(&idp), false);
        let (_, state, _) = start_login(&app, &idp).await;
        let (status, _, _) = raw(&app, "GET", &callback(&state), &[], "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (session, state, _) = start_login(&app, &idp).await;
        let forged = format!("refhir_oidc_{session}=forged");
        let (status, _, _) = raw(&app, "GET", &callback(&state), &[("cookie", &forged)], "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn two_logins_in_one_browser_both_complete() {
        let idp = start_idp().await;
        let app = smart_app(Some(&idp), false);
        let (_, first_state, first_cookie) = start_login(&app, &idp).await;
        let first_flow = idp.mock.lock().unwrap().flow.clone();
        let (_, _, second_cookie) = start_login(&app, &idp).await;
        idp.mock.lock().unwrap().flow = first_flow;
        let cookies = format!("{first_cookie}; {second_cookie}");
        let (status, headers, body) = raw(
            &app,
            "GET",
            &callback(&first_state),
            &[("cookie", &cookies)],
            "",
        )
        .await;
        assert_eq!(status, StatusCode::FOUND, "{body}");
        assert!(location(&headers).starts_with("/oauth/interact"));
    }

    #[tokio::test]
    async fn state_is_single_use_and_must_match() {
        let idp = start_idp().await;
        let app = smart_app(Some(&idp), false);
        let (session, state, cookie) = start_login(&app, &idp).await;
        let forged = format!("{session}.0000");
        let (status, _, _) = raw(&app, "GET", &callback(&forged), &[("cookie", &cookie)], "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // A forged state does not end the real login.
        let (status, _, _) = raw(&app, "GET", &callback(&state), &[("cookie", &cookie)], "").await;
        assert_eq!(status, StatusCode::FOUND);
        // The real state cannot be replayed.
        let (status, _, _) = raw(&app, "GET", &callback(&state), &[("cookie", &cookie)], "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn an_id_token_with_the_wrong_nonce_is_refused() {
        let idp = start_idp().await;
        let app = smart_app(Some(&idp), false);
        let (_, state, cookie) = start_login(&app, &idp).await;
        idp.mock.lock().unwrap().wrong_nonce = true;
        let (status, headers, _) =
            raw(&app, "GET", &callback(&state), &[("cookie", &cookie)], "").await;
        assert_eq!(status, StatusCode::FOUND);
        let back = location(&headers);
        assert!(back.starts_with("http://127.0.0.1/cb"), "{back}");
        assert_eq!(
            query_value(&back, "error").as_deref(),
            Some("access_denied")
        );
        assert!(query_value(&back, "code").is_none());
    }

    #[tokio::test]
    async fn the_provider_refusing_sends_access_denied_to_the_client() {
        let idp = start_idp().await;
        let app = smart_app(Some(&idp), false);
        let (_, state, cookie) = start_login(&app, &idp).await;
        let uri = format!(
            "/oauth/oidc/callback?error=access_denied&state={}",
            encode_query(&state)
        );
        let (status, headers, _) = raw(&app, "GET", &uri, &[("cookie", &cookie)], "").await;
        assert_eq!(status, StatusCode::FOUND);
        assert_eq!(
            query_value(&location(&headers), "error").as_deref(),
            Some("access_denied")
        );
    }

    #[tokio::test]
    async fn without_oidc_or_demo_login_nobody_can_sign_in() {
        let app = smart_app(None, false);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()));
        let query = format!(
            "response_type=code&client_id=public&redirect_uri={}&scope=openid&aud=http%3A%2F%2F127.0.0.1&code_challenge={challenge}&code_challenge_method=S256&state=s1",
            encode_query("http://127.0.0.1/cb"),
        );
        let (_, headers, _) = raw(&app, "GET", &format!("/oauth/authorize?{query}"), &[], "").await;
        let (status, _, _) = raw(&app, "GET", &location(&headers), &[], "").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let (status, _, _) = raw(
            &app,
            "POST",
            "/oauth/interact",
            &[("content-type", "application/x-www-form-urlencoded")],
            "session=x&username=dev&action=approve",
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }
    // ---- who may sign in, who they are, and who a token was for ----

    fn identified_patient(id: &str, system: &str, value: &str) -> Value {
        let mut resource = patient(id);
        resource["identifier"] = json!([{"system": system, "value": value}]);
        resource
    }

    /// A server in front of `idp` with `seed` loaded, `tweak` applied to the OIDC settings, and
    /// secrets a gateway can use.
    fn app_with_oidc(
        idp: &Idp,
        seed: &[Value],
        tweak: impl FnOnce(&mut crate::OidcConfig),
    ) -> axum::Router {
        let (db, catalog, store) = memory();
        {
            let cat = catalog.read().expect("catalog");
            for resource in seed {
                refhir_fhir::import_resource(&db, &cat, resource).expect("seed");
            }
        }
        let mut cfg = crate::OidcConfig::new(idp.base.clone(), CLIENT);
        cfg.client_secret = Some("idp-secret".into());
        tweak(&mut cfg);
        crate::app_with(
            db,
            catalog,
            store,
            crate::ServerConfig {
                auth: true,
                public_url: Some("http://127.0.0.1".into()),
                smart_patient: Some("p1".into()),
                smart_dcr_token: Some("dcr-test".into()),
                smart_confidential_secret: Some("gw-secret".into()),
                oidc: Some(cfg),
                ..Default::default()
            },
        )
    }

    /// Sign in at the provider and come back: where the browser is sent after the callback.
    async fn after_callback(
        app: &axum::Router,
        idp: &Idp,
        scope: &str,
    ) -> (String, String, String) {
        let (session, state, cookie) = start_login_with(app, idp, scope, "").await;
        let (status, headers, body) =
            raw(app, "GET", &callback(&state), &[("cookie", &cookie)], "").await;
        assert_eq!(status, StatusCode::FOUND, "{body}");
        (session, cookie, location(&headers))
    }

    /// Sign in and reach the consent page.
    async fn to_consent(app: &axum::Router, idp: &Idp, scope: &str) -> (String, String) {
        let (session, cookie, back) = after_callback(app, idp, scope).await;
        assert!(back.starts_with("/oauth/interact"), "{back}");
        (session, cookie)
    }

    async fn consent_page(app: &axum::Router, session: &str) -> String {
        let uri = format!("/oauth/interact?session={}", encode_query(session));
        let (status, _, body) = raw(app, "GET", &uri, &[], "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    /// Approve as the browser that signed in; `extra` is more form fields.
    async fn approve(
        app: &axum::Router,
        session: &str,
        cookie: &str,
        extra: &str,
    ) -> (StatusCode, HeaderMap, String) {
        raw(
            app,
            "POST",
            "/oauth/interact",
            &[
                ("content-type", "application/x-www-form-urlencoded"),
                ("cookie", cookie),
            ],
            &format!("session={}&action=approve{extra}", encode_query(session)),
        )
        .await
    }

    async fn redeem(app: &axum::Router, back: &str) -> Value {
        let code = query_value(back, "code").unwrap_or_else(|| panic!("no code in {back}"));
        let (status, _, body) = raw(
            app,
            "POST",
            "/oauth/token",
            &[("content-type", "application/x-www-form-urlencoded")],
            &format!(
                "grant_type=authorization_code&code={code}&redirect_uri={}&client_id=public&code_verifier={VERIFIER}",
                encode_query("http://127.0.0.1/cb")
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    fn denied(back: &str) -> String {
        assert!(back.starts_with("http://127.0.0.1/cb"), "{back}");
        assert!(query_value(back, "code").is_none(), "{back}");
        assert_eq!(query_value(back, "state").as_deref(), Some("s1"));
        assert_eq!(
            query_value(back, "error").as_deref(),
            Some("access_denied"),
            "{back}"
        );
        query_value(back, "error_description").unwrap_or_default()
    }

    const OPEN_SCOPE: &str = "openid fhirUser user/Patient.read";

    #[tokio::test]
    async fn anyone_the_provider_signs_in_is_accepted_unless_an_allowlist_is_set() {
        let idp = start_idp().await;
        idp.mock.lock().unwrap().claims = json!({"organization_id": "org_other"});
        let app = app_with_oidc(&idp, &[], |_| {});
        to_consent(&app, &idp, OPEN_SCOPE).await;
    }

    #[tokio::test]
    async fn allowed_claims_turn_other_organizations_away_before_consent() {
        let idp = start_idp().await;
        let app = app_with_oidc(&idp, &[], |cfg| {
            cfg.allowed_claims.insert(
                "organization_id".into(),
                vec!["org_ok".into(), "org_two".into()],
            );
        });

        idp.mock.lock().unwrap().claims = json!({"organization_id": "org_other"});
        let (session, cookie, back) = after_callback(&app, &idp, OPEN_SCOPE).await;
        let reason = denied(&back);
        assert!(reason.contains("not permitted"), "{reason}");
        // The reason says nothing about which organizations are let in.
        assert!(!reason.contains("org_"), "{reason}");
        // The login is over: there is no consent page and nothing to approve.
        let uri = format!("/oauth/interact?session={}", encode_query(&session));
        assert_eq!(
            raw(&app, "GET", &uri, &[], "").await.0,
            StatusCode::BAD_REQUEST
        );
        let (status, headers, _) = approve(&app, &session, &cookie, "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(headers.get("location").is_none());

        // No such claim at all is refused too.
        idp.mock.lock().unwrap().claims = json!({});
        let (_, _, back) = after_callback(&app, &idp, OPEN_SCOPE).await;
        denied(&back);

        // A member of a listed organization, whether the claim is a string or a list.
        idp.mock.lock().unwrap().claims = json!({"organization_id": "org_two"});
        to_consent(&app, &idp, OPEN_SCOPE).await;
        idp.mock.lock().unwrap().claims = json!({"organization_id": ["org_x", "org_ok"]});
        to_consent(&app, &idp, OPEN_SCOPE).await;
    }

    #[tokio::test]
    async fn allowed_email_domains_need_a_verified_address_in_the_domain() {
        let idp = start_idp().await;
        let app = app_with_oidc(&idp, &[], |cfg| {
            cfg.allowed_email_domains = vec!["example.com".into()];
        });
        for (claims, admitted) in [
            (
                json!({"email": "ada@example.com", "email_verified": true}),
                true,
            ),
            (
                json!({"email": "Ada@Example.com", "email_verified": true}),
                true,
            ),
            // The provider does not say the address is verified.
            (json!({"email": "ada@example.com"}), false),
            (json!({"email": "ada@elsewhere.com"}), false),
            (json!({"email": "ada@mail.example.com"}), false),
            (
                json!({"email": "ada@example.com", "email_verified": false}),
                false,
            ),
            (json!({}), false),
        ] {
            idp.mock.lock().unwrap().claims = claims.clone();
            let (_, _, back) = after_callback(&app, &idp, OPEN_SCOPE).await;
            if admitted {
                assert!(back.starts_with("/oauth/interact"), "{claims}: {back}");
            } else {
                let reason = denied(&back);
                assert!(reason.contains("not permitted"), "{claims}: {reason}");
            }
        }
    }

    #[tokio::test]
    async fn without_a_fhir_user_claim_or_a_lookup_the_fhir_user_scope_is_refused() {
        let idp = start_idp().await;
        idp.mock.lock().unwrap().omit_fhir_user = true;
        let app = app_with_oidc(
            &idp,
            &[identified_patient("pr1", "urn:idp", "idp-user-1")],
            |_| {},
        );
        let (session, cookie) = to_consent(&app, &idp, OPEN_SCOPE).await;
        let (status, headers, _) = approve(&app, &session, &cookie, "").await;
        assert_eq!(status, StatusCode::FOUND);
        let back = location(&headers);
        assert_eq!(
            query_value(&back, "error").as_deref(),
            Some("invalid_request")
        );
        assert!(query_value(&back, "error_description")
            .unwrap()
            .contains("did not supply a valid fhirUser claim"));
    }

    fn patient_lookup(system: Option<&str>) -> crate::FhirUserLookup {
        crate::FhirUserLookup {
            system: system.map(str::to_string),
            ..crate::FhirUserLookup::new("Patient")
        }
    }

    #[tokio::test]
    async fn a_lookup_binds_the_subject_to_the_resource_that_carries_it() {
        let idp = start_idp().await;
        idp.mock.lock().unwrap().omit_fhir_user = true;
        let app = app_with_oidc(
            &idp,
            &[
                identified_patient("other", "urn:idp", "someone-else"),
                identified_patient("pr1", "urn:idp", "idp-user-1"),
                // Same value under another system is somebody else's identifier.
                identified_patient("wrong-system", "urn:elsewhere", "idp-user-1"),
            ],
            |cfg| cfg.fhir_user_lookup = Some(patient_lookup(Some("urn:idp"))),
        );
        let (session, cookie) = to_consent(&app, &idp, OPEN_SCOPE).await;
        let (status, headers, body) = approve(&app, &session, &cookie, "").await;
        assert_eq!(status, StatusCode::FOUND, "{body}");
        let tokens = redeem(&app, &location(&headers)).await;
        assert_eq!(tokens["fhirUser"], "http://127.0.0.1/Patient/pr1");
        let payload = tokens["id_token"]
            .as_str()
            .unwrap()
            .split('.')
            .nth(1)
            .unwrap();
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
        assert_eq!(claims["sub"], "idp-user-1");
        assert_eq!(claims["fhirUser"], "http://127.0.0.1/Patient/pr1");
    }

    #[tokio::test]
    async fn a_lookup_can_search_on_another_claim() {
        let idp = start_idp().await;
        {
            let mut mock = idp.mock.lock().unwrap();
            mock.omit_fhir_user = true;
            mock.claims = json!({"email": "ada@example.com", "email_verified": true});
        }
        let app = app_with_oidc(
            &idp,
            &[identified_patient("pr1", "urn:email", "ada@example.com")],
            |cfg| {
                cfg.fhir_user_lookup = Some(crate::FhirUserLookup {
                    claim: "email".into(),
                    ..patient_lookup(Some("urn:email"))
                })
            },
        );
        let (session, cookie) = to_consent(&app, &idp, OPEN_SCOPE).await;
        let (_, headers, _) = approve(&app, &session, &cookie, "").await;
        let tokens = redeem(&app, &location(&headers)).await;
        assert_eq!(tokens["fhirUser"], "http://127.0.0.1/Patient/pr1");

        // An address the provider has not verified binds nobody.
        idp.mock.lock().unwrap().claims =
            json!({"email": "ada@example.com", "email_verified": false});
        let (session, cookie) = to_consent(&app, &idp, OPEN_SCOPE).await;
        let (_, headers, _) = approve(&app, &session, &cookie, "").await;
        assert_eq!(
            query_value(&location(&headers), "error").as_deref(),
            Some("invalid_request")
        );
    }

    #[tokio::test]
    async fn a_lookup_binds_nobody_when_it_finds_nothing_or_several() {
        let idp = start_idp().await;
        idp.mock.lock().unwrap().omit_fhir_user = true;
        let lookup = |cfg: &mut crate::OidcConfig| {
            cfg.fhir_user_lookup = Some(patient_lookup(Some("urn:idp")))
        };
        let nothing = app_with_oidc(
            &idp,
            &[identified_patient("pr1", "urn:idp", "not-this-user")],
            lookup,
        );
        let several = app_with_oidc(
            &idp,
            &[
                identified_patient("pr1", "urn:idp", "idp-user-1"),
                identified_patient("pr2", "urn:idp", "idp-user-1"),
            ],
            lookup,
        );
        for app in [&nothing, &several] {
            let (session, cookie) = to_consent(app, &idp, OPEN_SCOPE).await;
            let (_, headers, _) = approve(app, &session, &cookie, "").await;
            let back = location(&headers);
            assert_eq!(
                query_value(&back, "error").as_deref(),
                Some("invalid_request"),
                "{back}"
            );
            assert!(query_value(&back, "code").is_none());
        }
    }

    #[tokio::test]
    async fn a_lookup_does_not_run_when_the_token_names_the_person() {
        let idp = start_idp().await;
        // The token says Practitioner/dr-1; the lookup would say Patient/pr1.
        let app = app_with_oidc(
            &idp,
            &[identified_patient("pr1", "urn:idp", "idp-user-1")],
            |cfg| cfg.fhir_user_lookup = Some(patient_lookup(Some("urn:idp"))),
        );
        let (session, cookie) = to_consent(&app, &idp, OPEN_SCOPE).await;
        let (_, headers, _) = approve(&app, &session, &cookie, "").await;
        let tokens = redeem(&app, &location(&headers)).await;
        assert_eq!(tokens["fhirUser"], "http://127.0.0.1/Practitioner/dr-1");
    }

    #[tokio::test]
    async fn a_lookup_replaces_an_unusable_claim() {
        let idp = start_idp().await;
        idp.mock.lock().unwrap().claims = json!({"fhirUser": "Patient/../Patient/x"});
        let app = app_with_oidc(
            &idp,
            &[identified_patient("pr1", "urn:idp", "idp-user-1")],
            |cfg| cfg.fhir_user_lookup = Some(patient_lookup(Some("urn:idp"))),
        );
        let (session, cookie) = to_consent(&app, &idp, OPEN_SCOPE).await;
        let (_, headers, _) = approve(&app, &session, &cookie, "").await;
        let tokens = redeem(&app, &location(&headers)).await;
        assert_eq!(tokens["fhirUser"], "http://127.0.0.1/Patient/pr1");
    }

    const PATIENT_SCOPE: &str = "openid fhirUser patient/Patient.rs";

    #[tokio::test]
    async fn the_picker_stays_open_to_every_patient_unless_binding_is_turned_on() {
        let idp = start_idp().await;
        idp.mock.lock().unwrap().claims = json!({"fhirUser": "Patient/p1"});
        let app = app_with_oidc(&idp, &[patient("p1"), patient("p2")], |_| {});
        let (session, cookie) = to_consent(&app, &idp, PATIENT_SCOPE).await;
        let page = consent_page(&app, &session).await;
        assert!(page.contains("name=\"patient_id\""), "{page}");
        let (_, headers, _) = approve(&app, &session, &cookie, "&patient_id=p2").await;
        let tokens = redeem(&app, &location(&headers)).await;
        assert_eq!(tokens["patient"], "p2");
    }

    #[tokio::test]
    async fn the_picker_lists_the_patients_on_the_server() {
        let idp = start_idp().await;
        let app = app_with_oidc(&idp, &[patient("p1"), patient("p2")], |_| {});
        let (session, _) = to_consent(&app, &idp, PATIENT_SCOPE).await;
        let page = consent_page(&app, &session).await;
        for id in ["p1", "p2"] {
            assert!(page.contains(&format!("<option value=\"{id}\">")), "{page}");
        }
    }

    #[tokio::test]
    async fn a_bound_patient_is_the_launch_patient_and_the_picker_is_gone() {
        let idp = start_idp().await;
        idp.mock.lock().unwrap().claims = json!({"fhirUser": "Patient/p1"});
        let app = app_with_oidc(&idp, &[patient("p1"), patient("p2")], |cfg| {
            cfg.patient_from_fhir_user = true;
        });
        let (session, cookie) = to_consent(&app, &idp, PATIENT_SCOPE).await;
        let page = consent_page(&app, &session).await;
        assert!(!page.contains("name=\"patient_id\""), "{page}");
        assert!(!page.contains("p2"), "{page}");
        assert!(page.contains("patient p1"), "{page}");
        // A patient named in the form is not a way around it.
        let (status, headers, body) = approve(&app, &session, &cookie, "&patient_id=p2").await;
        assert_eq!(status, StatusCode::FOUND, "{body}");
        let tokens = redeem(&app, &location(&headers)).await;
        assert_eq!(tokens["patient"], "p1");
        assert_eq!(tokens["fhirUser"], "http://127.0.0.1/Patient/p1");
    }

    #[tokio::test]
    async fn a_patient_bound_by_lookup_is_the_launch_patient_too() {
        let idp = start_idp().await;
        idp.mock.lock().unwrap().omit_fhir_user = true;
        let app = app_with_oidc(
            &idp,
            &[
                identified_patient("p1", "urn:idp", "someone-else"),
                identified_patient("p2", "urn:idp", "idp-user-1"),
            ],
            |cfg| {
                cfg.fhir_user_lookup = Some(patient_lookup(Some("urn:idp")));
                cfg.patient_from_fhir_user = true;
            },
        );
        // No fhirUser scope is requested: the binding is still found for the patient context.
        let (session, cookie) = to_consent(&app, &idp, "openid patient/Patient.rs").await;
        assert!(!consent_page(&app, &session)
            .await
            .contains("name=\"patient_id\""));
        let (_, headers, _) = approve(&app, &session, &cookie, "").await;
        let tokens = redeem(&app, &location(&headers)).await;
        assert_eq!(tokens["patient"], "p2");
    }

    #[tokio::test]
    async fn only_patient_bindings_fix_the_patient() {
        let idp = start_idp().await;
        // The default token claim is Practitioner/dr-1: the picker stays.
        let app = app_with_oidc(&idp, &[patient("p1")], |cfg| {
            cfg.patient_from_fhir_user = true
        });
        let (session, _) = to_consent(&app, &idp, PATIENT_SCOPE).await;
        assert!(consent_page(&app, &session)
            .await
            .contains("name=\"patient_id\""));

        // A Patient on some other server is not a patient here.
        idp.mock.lock().unwrap().claims =
            json!({"fhirUser": "https://other.example/fhir/Patient/p1"});
        let (session, _) = to_consent(&app, &idp, PATIENT_SCOPE).await;
        assert!(consent_page(&app, &session)
            .await
            .contains("name=\"patient_id\""));
    }

    #[tokio::test]
    async fn a_bound_patient_that_does_not_exist_ends_the_login() {
        let idp = start_idp().await;
        idp.mock.lock().unwrap().claims = json!({"fhirUser": "Patient/ghost"});
        let app = app_with_oidc(&idp, &[patient("p1")], |cfg| {
            cfg.patient_from_fhir_user = true
        });
        let (_, _, back) = after_callback(&app, &idp, PATIENT_SCOPE).await;
        let reason = denied(&back);
        assert!(reason.contains("does not exist"), "{reason}");
    }

    #[tokio::test]
    async fn a_bound_patient_must_be_the_patient_the_launch_is_for() {
        let idp = start_idp().await;
        let launch_scope = "launch openid fhirUser patient/Patient.rs";
        for (bound, same_patient) in [("Patient/p1", true), ("Patient/p2", false)] {
            idp.mock.lock().unwrap().claims = json!({ "fhirUser": bound });
            // The launch is for the configured patient, p1.
            let app = app_with_oidc(&idp, &[patient("p1"), patient("p2")], |cfg| {
                cfg.patient_from_fhir_user = true;
            });
            let (status, headers, body) = raw(
                &app,
                "GET",
                "/oauth/ehr-launch?client_id=public&launch_uri=http://127.0.0.1/app",
                &[],
                "",
            )
            .await;
            assert_eq!(status, StatusCode::FOUND, "{body}");
            let launch = query_value(&location(&headers), "launch").expect("launch");
            let extra = format!("&launch={}", encode_query(&launch));
            let (session, state, cookie) = start_login_with(&app, &idp, launch_scope, &extra).await;
            let (status, headers, body) =
                raw(&app, "GET", &callback(&state), &[("cookie", &cookie)], "").await;
            assert_eq!(status, StatusCode::FOUND, "{body}");
            let back = location(&headers);
            if same_patient {
                assert!(back.starts_with("/oauth/interact"), "{back}");
                let (_, headers, _) = approve(&app, &session, &cookie, "").await;
                assert_eq!(redeem(&app, &location(&headers)).await["patient"], "p1");
            } else {
                // The person who signed in is not the patient the EHR launched for.
                let reason = denied(&back);
                assert!(reason.contains("not the patient"), "{reason}");
            }
        }
    }

    // ---- token introspection for a resource gateway ----

    const FORM: (&str, &str) = ("content-type", "application/x-www-form-urlencoded");

    async fn introspect(
        app: &axum::Router,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (StatusCode, Value) {
        let (status, _, text) = raw(app, "POST", "/oauth/introspect", headers, body).await;
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn a_gateway_learns_who_a_token_was_issued_to() {
        let idp = start_idp().await;
        let app = app_with_oidc(&idp, &[], |_| {});
        let (session, cookie) = to_consent(&app, &idp, OPEN_SCOPE).await;
        let (_, headers, _) = approve(&app, &session, &cookie, "").await;
        let tokens = redeem(&app, &location(&headers)).await;
        let access = tokens["access_token"].as_str().unwrap().to_string();
        assert!(access.starts_with("tok-"));
        let body = format!("token={access}");

        let check = |intro: &Value| {
            assert_eq!(intro["active"], true, "{intro}");
            assert_eq!(intro["sub"], "idp-user-1");
            assert_eq!(intro["client_id"], "public");
            assert_eq!(intro["scope"], "openid fhirUser user/Patient.read");
            assert!(intro["exp"].as_u64().unwrap() > 0);
        };

        // The built-in confidential client, with its secret in HTTP Basic or in the form.
        let basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("confidential:gw-secret")
        );
        let (status, intro) = introspect(&app, &[FORM, ("authorization", &basic)], &body).await;
        assert_eq!(status, StatusCode::OK);
        check(&intro);
        let (status, intro) = introspect(
            &app,
            &[FORM],
            &format!("{body}&client_id=confidential&client_secret=gw-secret"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        check(&intro);

        // A registered backend service, with a signed assertion addressed to either endpoint.
        let (encoding, jwks) = backend_client_keys();
        let (status, _, registered) = raw(
            &app,
            "POST",
            "/oauth/register",
            &[
                ("content-type", "application/json"),
                ("authorization", "Bearer dcr-test"),
            ],
            &json!({
                "token_endpoint_auth_method": "private_key_jwt",
                "grant_types": ["client_credentials"],
                "jwks": jwks,
                "scope": "system/*.rs"
            })
            .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{registered}");
        let gateway = serde_json::from_str::<Value>(&registered).unwrap()["client_id"]
            .as_str()
            .unwrap()
            .to_string();
        for aud in [
            "http://127.0.0.1/oauth/introspect",
            "http://127.0.0.1/oauth/token",
        ] {
            let assertion = backend_assertion(&encoding, &gateway, aud);
            let (status, intro) = introspect(
                &app,
                &[FORM],
                &format!(
                    "{body}&client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer&client_assertion={assertion}"
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{aud}: {intro}");
            check(&intro);
        }
        // An assertion addressed to somewhere else, or to the issuer alone, is not accepted.
        for aud in [
            "http://127.0.0.1",
            "http://127.0.0.1/oauth/revoke",
            "https://evil.example/oauth/introspect",
        ] {
            let assertion = backend_assertion(&encoding, &gateway, aud);
            let (status, _) = introspect(
                &app,
                &[FORM],
                &format!(
                    "{body}&client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer&client_assertion={assertion}"
                ),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{aud}");
        }
        // An assertion made for introspection cannot be replayed at the token endpoint.
        let assertion = backend_assertion(&encoding, &gateway, "http://127.0.0.1/oauth/introspect");
        let (status, _, body_text) = raw(
            &app,
            "POST",
            "/oauth/token",
            &[FORM],
            &format!(
                "grant_type=client_credentials&scope=system/*.rs&client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer&client_assertion={assertion}"
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body_text}");

        // Nobody else gets to ask: no credentials, a wrong secret, or a public client.
        let (status, _) = introspect(&app, &[FORM], &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = introspect(
            &app,
            &[FORM],
            &format!("{body}&client_id=confidential&client_secret=wrong"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = introspect(&app, &[FORM], &format!("{body}&client_id=public")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // A token that is gone is reported inactive, with nothing about who it was for.
        let basic = [FORM, ("authorization", &basic)];
        let (status, intro) = introspect(&app, &basic, "token=tok-unknown").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(intro, json!({"active": false}));
    }
}

// ---- several processes on one database: a writer, and replicas that follow it ----

mod cluster {
    use super::*;
    use crate::{app_with, ServerConfig};
    use axum::http::HeaderMap;

    struct Pair {
        path: std::path::PathBuf,
        primary_db: Arc<Database>,
        primary_catalog: Arc<RwLock<Catalog>>,
        primary_addr: std::net::SocketAddr,
        replica_db: Arc<Database>,
        replica_app: axum::Router,
        replica_store: Arc<dyn ConformanceStore>,
        replica_catalog: Arc<RwLock<Catalog>>,
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm", ".lock"] {
                let mut name = self.path.as_os_str().to_owned();
                name.push(suffix);
                let _ = std::fs::remove_file(name);
            }
            let _ = std::fs::remove_dir_all(self.path.with_extension("export"));
        }
    }

    /// A primary serving on a real port and a replica of the same database file, as two
    /// processes would be. The replica refreshes only when a test says so.
    async fn pair(config: impl FnOnce(&mut ServerConfig)) -> Pair {
        pair_with(|_| {}, config).await
    }

    /// `pair`, with a say in the primary's settings too.
    async fn pair_with(
        primary: impl FnOnce(&mut ServerConfig),
        config: impl FnOnce(&mut ServerConfig),
    ) -> Pair {
        // A counter, not just the clock: tests start in the same microsecond.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "refhir-cluster-{}-{}.sqlite",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let primary_store = Arc::new(SqlitePersistence::open(&path).unwrap());
        let primary_db = Arc::new(Database::open(primary_store.clone()).unwrap());
        let primary_catalog = Arc::new(RwLock::new(Catalog::empty()));
        install_package_dir(
            primary_store.as_ref(),
            &primary_db,
            &primary_catalog,
            minir4_dir(),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let primary_addr = listener.local_addr().unwrap();
        let mut primary_settings = ServerConfig {
            export_dir: path.with_extension("export").display().to_string(),
            ..ServerConfig::default()
        };
        primary(&mut primary_settings);
        let primary_app = app_with(
            primary_db.clone(),
            primary_catalog.clone(),
            primary_store.clone(),
            primary_settings,
        );
        tokio::spawn(async move {
            axum::serve(listener, primary_app).await.unwrap();
        });

        let replica_store = Arc::new(SqlitePersistence::open(&path).unwrap());
        let replica_db = Arc::new(Database::open_read_only(replica_store.clone()).unwrap());
        let (loaded, _) =
            Catalog::open_consistent(replica_store.as_ref(), refhir_fhir::CatalogClock::Stable)
                .unwrap();
        let replica_catalog = Arc::new(RwLock::new(loaded));
        let mut settings = ServerConfig {
            primary_url: Some(format!("http://{primary_addr}")),
            min_ts_wait: Duration::from_millis(100),
            max_staleness: Duration::from_millis(300),
            ..ServerConfig::default()
        };
        config(&mut settings);
        let replica_store: Arc<dyn ConformanceStore> = replica_store;
        let replica_app = app_with(
            replica_db.clone(),
            replica_catalog.clone(),
            replica_store.clone(),
            settings,
        );
        Pair {
            path,
            primary_db,
            primary_catalog,
            primary_addr,
            replica_db,
            replica_app,
            replica_store,
            replica_catalog,
        }
    }

    struct Full {
        status: StatusCode,
        headers: HeaderMap,
        body: Value,
    }

    async fn call(
        app: &axum::Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
        extra: &[(&str, &str)],
    ) -> Full {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in extra {
            builder = builder.header(*name, *value);
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/fhir+json");
        }
        let payload = body.map(|value| value.to_string()).unwrap_or_default();
        let response = app
            .clone()
            .oneshot(builder.body(Body::from(payload)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        Full {
            status,
            headers,
            body,
        }
    }

    fn header<'a>(reply: &'a Full, name: &str) -> &'a str {
        reply
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
    }

    fn create_on_primary(pair: &Pair, id: &str) -> u64 {
        // `import_resource` keeps the id, which a client's create no longer does.
        let loaded = pair.primary_catalog.read().unwrap();
        refhir_fhir::import_resource(&pair.primary_db, &loaded, &patient(id))
            .unwrap()
            .ts
            .as_u64()
    }

    #[tokio::test]
    async fn healthz_tells_a_balancer_which_processes_can_take_traffic() {
        let cluster = pair(|_| {}).await;
        let primary = format!("http://{}", cluster.primary_addr);
        let primary_health = tokio::task::spawn_blocking(move || {
            let body = ureq::get(&format!("{primary}/healthz?verbose=1"))
                .call()
                .unwrap()
                .into_string()
                .unwrap();
            serde_json::from_str::<Value>(&body).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(primary_health["role"], "primary");
        assert_eq!(primary_health["status"], "ok");
        assert!(primary_health["pool"]["max"].as_u64().unwrap() >= 1);
        assert!(primary_health["time_ns"]["persist"].is_number());

        let fresh = call(&cluster.replica_app, "GET", "/healthz", None, &[]).await;
        assert_eq!(fresh.status, StatusCode::OK);
        assert_eq!(fresh.body["role"], "replica");
        assert!(fresh.body["lag_ts"].is_number());

        // A replica that stops following the primary leaves rotation, and comes back when it
        // catches up.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let stale = call(&cluster.replica_app, "GET", "/healthz", None, &[]).await;
        assert_eq!(stale.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(stale.body["status"], "unhealthy");
        let db = cluster.replica_db.clone();
        tokio::task::spawn_blocking(move || db.refresh().unwrap())
            .await
            .unwrap();
        let back = call(&cluster.replica_app, "GET", "/healthz", None, &[]).await;
        assert_eq!(back.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_verbose_health_report_needs_a_token_when_smart_is_on() {
        let smart = std::sync::Arc::new(crate::auth::SmartStore::with_defaults());
        smart.install_token("system-tok", &["system/*.cruds"], None);
        let (db, catalog, store) = memory();
        let app = crate::app_with(
            db,
            catalog,
            store,
            crate::ServerConfig {
                auth: true,
                smart: Some(smart),
                ..Default::default()
            },
        );
        // A load balancer needs no token.
        let plain = call(&app, "GET", "/healthz", None, &[]).await;
        assert_eq!(plain.status, StatusCode::OK);
        assert!(plain.body.get("pool").is_none());
        // The counters and timings do.
        for uri in ["/healthz?verbose=1", "/healthz?verbose"] {
            let anonymous = call(&app, "GET", uri, None, &[]).await;
            assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "{uri}");
        }
        let with_token = call(
            &app,
            "GET",
            "/healthz?verbose=1",
            None,
            &[("authorization", "Bearer system-tok")],
        )
        .await;
        assert_eq!(with_token.status, StatusCode::OK, "{}", with_token.body);
        assert!(with_token.body["pool"]["max"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn responses_carry_the_timestamp_a_client_echoes_back() {
        let cluster = pair(|_| {}).await;
        let primary = cluster_primary_app(&cluster);
        let created = call(&primary, "POST", "/Patient", Some(patient("p1")), &[]).await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
        let ts: u64 = header(&created, "x-refhir-commit-ts").parse().unwrap();
        assert_eq!(created.body["meta"]["versionId"], ts.to_string());
        assert_eq!(header(&created, "x-refhir-role"), "primary");
        // The server picks the id of a created resource.
        let id = created.body["id"].as_str().unwrap().to_string();

        // A read reports the snapshot it ran at, so a client's reads never go backwards.
        let read = call(&primary, "GET", &format!("/Patient/{id}"), None, &[]).await;
        let read_ts: u64 = header(&read, "x-refhir-commit-ts").parse().unwrap();
        assert!(read_ts >= ts);

        // Every kind of write reports it: a bundle, an update, and a delete (which has no body).
        let bundle = call(
            &primary,
            "POST",
            "/",
            Some(json!({
                "resourceType": "Bundle",
                "type": "transaction",
                "entry": [{
                    "resource": patient("p2"),
                    "request": {"method": "POST", "url": "Patient"}
                }]
            })),
            &[],
        )
        .await;
        assert_eq!(bundle.status, StatusCode::OK, "{}", bundle.body);
        let bundle_ts: u64 = header(&bundle, "x-refhir-commit-ts").parse().unwrap();
        assert!(bundle_ts > ts);
        let deleted = call(&primary, "DELETE", &format!("/Patient/{id}"), None, &[]).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        let delete_ts: u64 = header(&deleted, "x-refhir-commit-ts").parse().unwrap();
        assert!(delete_ts > bundle_ts);
    }

    /// The primary as a router in this process (the replica forwards to its TCP listener).
    fn cluster_primary_app(cluster: &Pair) -> axum::Router {
        let store: Arc<dyn ConformanceStore> =
            Arc::new(SqlitePersistence::open(&cluster.path).unwrap());
        app_with(
            cluster.primary_db.clone(),
            cluster.primary_catalog.clone(),
            store,
            ServerConfig::default(),
        )
    }

    #[tokio::test]
    async fn a_replica_sends_writes_to_the_primary() {
        let cluster = pair(|_| {}).await;
        let created = call(
            &cluster.replica_app,
            "POST",
            "/Patient",
            Some(patient("via-replica")),
            &[],
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
        assert_eq!(header(&created, "x-refhir-role"), "primary");
        let ts: u64 = header(&created, "x-refhir-commit-ts").parse().unwrap();
        let id = created.body["id"].as_str().unwrap().to_string();

        // The write is in the shared database, and the primary has it.
        {
            let loaded = cluster.primary_catalog.read().unwrap();
            let result = refhir_fhir::run(
                &cluster.primary_db,
                &loaded,
                "Patient/read",
                &json!({"id": id}),
            )
            .unwrap();
            assert_eq!(result.value["id"], id.as_str());
        }
        assert!(cluster.primary_db.latest_ts().as_u64() >= ts);
        // The replica itself has not moved, which is why the header exists.
        assert!(cluster.replica_db.latest_ts().as_u64() < ts);

        // A write is not run on the replica to find out it is refused: the primary validates it,
        // so even a rejection comes from there.
        let mut invalid = patient("invalid");
        invalid["birthDate"] = json!("yesterday");
        let rejected = call(&cluster.replica_app, "POST", "/Patient", Some(invalid), &[]).await;
        assert_eq!(
            rejected.status,
            StatusCode::BAD_REQUEST,
            "{}",
            rejected.body
        );
        assert_eq!(header(&rejected, "x-refhir-role"), "primary");

        // Reads and the searches and operations sent by POST are still the replica's to answer.
        let read = call(&cluster.replica_app, "GET", "/Patient/nobody", None, &[]).await;
        assert_eq!(header(&read, "x-refhir-role"), "replica");

        // A replica buffers no more of a request than the primary's handlers would take.
        let mut big = patient("big");
        big["text"] = json!({"status": "generated", "div": "x".repeat(3 * 1024 * 1024)});
        let refused = call(&cluster.replica_app, "POST", "/Patient", Some(big), &[]).await;
        assert_eq!(refused.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            refused.headers.get("x-refhir-role").is_none(),
            "the replica refused it itself, without forwarding"
        );
    }

    /// What a replica recorded about itself, read back through it.
    async fn recorded_by(cluster: &Pair) -> (Vec<String>, Vec<Value>) {
        let usage = call(&cluster.replica_app, "GET", "/_usage", None, &[]).await;
        let classes = usage.body["usage"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["class"].as_str().unwrap().to_string())
            .collect();
        let audit = call(&cluster.replica_app, "GET", "/_audit", None, &[]).await;
        let entries = audit.body["entry"].as_array().cloned().unwrap_or_default();
        (classes, entries)
    }

    fn observed(
        rows: &Arc<refhir_persistence::MemoryGovernanceStore>,
    ) -> impl FnOnce(&mut ServerConfig) {
        let rows = rows.clone();
        move |settings| {
            settings.governance = crate::GovernanceConfig::observing();
            settings.governance_store = Some(crate::SharedGovernanceStore::new(rows));
        }
    }

    #[tokio::test]
    async fn a_write_a_replica_sends_on_is_not_counted_or_audited_by_the_replica_as_well() {
        // The primary limits, counts and audits what it is sent, as a request of its own. Counted
        // and audited by the replica too, a forwarded write would use up twice its quota and
        // be in the audit trail twice.
        let rows = Arc::new(refhir_persistence::MemoryGovernanceStore::default());
        let cluster = pair(observed(&rows)).await;
        let created = call(
            &cluster.replica_app,
            "POST",
            "/Patient",
            Some(patient("via-replica")),
            &[],
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
        assert_eq!(header(&created, "x-refhir-role"), "primary");
        let (classes, entries) = recorded_by(&cluster).await;
        assert!(
            !classes.iter().any(|class| class == "write"),
            "the primary counts the write: {classes:?}"
        );
        assert!(
            entries
                .iter()
                .all(|entry| entry["resource"]["action"] != "C"),
            "the primary audits the write, and here it has the audit off: {entries:?}"
        );
    }

    #[tokio::test]
    async fn a_transaction_a_replica_tries_and_then_sends_on_leaves_no_row_of_its_own() {
        // A transaction is run where it arrives, and only found to need the primary when the
        // commit is refused. That refusal is not an operation anyone performed, and the primary
        // records the one that was.
        let rows = Arc::new(refhir_persistence::MemoryGovernanceStore::default());
        let cluster = pair(observed(&rows)).await;
        let bundle = json!({"resourceType": "Bundle", "type": "transaction", "entry": [
            {"resource": patient("in-a-bundle"), "request": {"method": "POST", "url": "Patient"}}
        ]});
        let sent = call(&cluster.replica_app, "POST", "/", Some(bundle), &[]).await;
        assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
        assert_eq!(header(&sent, "x-refhir-role"), "primary");
        let (classes, entries) = recorded_by(&cluster).await;
        assert!(
            !classes.iter().any(|class| class == "bundle"),
            "{classes:?}"
        );
        assert!(
            entries
                .iter()
                .all(|entry| entry["resource"]["subtype"][0]["code"] != "transaction"),
            "{entries:?}"
        );
    }

    #[tokio::test]
    async fn a_write_a_replica_cannot_send_on_is_refused_and_recorded_once() {
        let rows = Arc::new(refhir_persistence::MemoryGovernanceStore::default());
        let cluster = pair(|settings| {
            settings.primary_url = None;
            observed(&rows)(settings);
        })
        .await;
        let refused = call(
            &cluster.replica_app,
            "POST",
            "/Patient",
            Some(patient("nowhere")),
            &[],
        )
        .await;
        assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
        let (_, entries) = recorded_by(&cluster).await;
        let attempts: Vec<_> = entries
            .iter()
            .filter(|entry| entry["resource"]["action"] == "C")
            .collect();
        // Not once by the engine that could not commit it and once at the edge.
        assert_eq!(attempts.len(), 1, "{entries:?}");
        assert_eq!(attempts[0]["resource"]["outcome"], "8");
    }

    #[tokio::test]
    async fn a_read_after_a_write_waits_for_the_replica_or_asks_the_primary() {
        let cluster = pair(|_| {}).await;
        let ts = create_on_primary(&cluster, "p1");
        let min = ts.to_string();

        // The replica has not refreshed: it waits briefly, then the primary answers.
        let behind = call(
            &cluster.replica_app,
            "GET",
            "/Patient/p1",
            None,
            &[("x-refhir-min-ts", min.as_str())],
        )
        .await;
        assert_eq!(behind.status, StatusCode::OK);
        assert_eq!(behind.body["id"], "p1");
        assert_eq!(header(&behind, "x-refhir-role"), "primary");

        // Without the header a replica answers from what it has.
        let stale = call(&cluster.replica_app, "GET", "/Patient/p1", None, &[]).await;
        assert_eq!(stale.status, StatusCode::NOT_FOUND);
        assert_eq!(header(&stale, "x-refhir-role"), "replica");

        // Once it has caught up it answers the same request itself.
        let db = cluster.replica_db.clone();
        tokio::task::spawn_blocking(move || db.refresh().unwrap())
            .await
            .unwrap();
        let caught_up = call(
            &cluster.replica_app,
            "GET",
            "/Patient/p1",
            None,
            &[("x-refhir-min-ts", min.as_str())],
        )
        .await;
        assert_eq!(caught_up.status, StatusCode::OK);
        assert_eq!(header(&caught_up, "x-refhir-role"), "replica");
    }

    #[tokio::test]
    async fn a_replica_writes_only_through_a_primary_of_its_own_tenant() {
        // (the replica's tenant, the primary's tenant, whether the write is accepted)
        let cases: [(Option<&str>, Option<&str>, bool); 5] = [
            (Some("a"), Some("a"), true),
            (None, None, true),
            (Some("a"), Some("b"), false),
            (None, Some("b"), false),
            (Some("a"), None, false),
        ];
        for (mine, theirs, accepted) in cases {
            let cluster = pair_with(
                |primary| primary.tenant = theirs.map(String::from),
                |replica| replica.tenant = mine.map(String::from),
            )
            .await;
            let before = cluster.primary_db.latest_ts();
            let created = call(
                &cluster.replica_app,
                "POST",
                "/Patient",
                Some(patient("via-replica")),
                &[],
            )
            .await;
            if accepted {
                assert_eq!(
                    created.status,
                    StatusCode::CREATED,
                    "{mine:?} to {theirs:?}"
                );
                continue;
            }
            assert_eq!(
                created.status,
                StatusCode::MISDIRECTED_REQUEST,
                "{mine:?} to {theirs:?}: {}",
                created.body
            );
            let said = created.body["issue"][0]["diagnostics"].as_str().unwrap();
            assert!(said.contains("--primary-url"), "{said}");
            assert_eq!(
                cluster.primary_db.latest_ts(),
                before,
                "the refused write reached the primary ({mine:?} to {theirs:?})"
            );
        }
    }

    #[tokio::test]
    async fn a_client_that_is_not_a_replica_is_not_held_to_a_tenant() {
        let cluster = pair_with(|primary| primary.tenant = Some("b".into()), |_| {}).await;
        let primary = format!("http://{}", cluster.primary_addr);
        let status = tokio::task::spawn_blocking(move || {
            ureq::post(&format!("{primary}/Patient"))
                .set("content-type", "application/fhir+json")
                .send_string(&patient("direct").to_string())
                .map(|response| response.status())
                .unwrap_or(0)
        })
        .await
        .unwrap();
        assert_eq!(status, 201);
    }

    #[tokio::test]
    async fn a_mutation_over_sync_goes_only_to_a_primary_of_the_same_tenant() {
        for (primary_tenant, accepted) in [("a", true), ("b", false)] {
            let cluster = pair_with(
                |primary| primary.tenant = Some(primary_tenant.into()),
                |replica| replica.tenant = Some("a".into()),
            )
            .await;
            create_on_primary(&cluster, "p1");
            let db = cluster.replica_db.clone();
            tokio::task::spawn_blocking(move || db.refresh().unwrap())
                .await
                .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let replica_addr = listener.local_addr().unwrap();
            let replica = cluster.replica_app.clone();
            tokio::spawn(async move {
                axum::serve(listener, replica).await.unwrap();
            });
            let before = cluster.primary_db.latest_ts();
            let (mut writer, _) = connect(&format!("ws://{replica_addr}/sync")).await;
            writer
                .send(Message::Text(
                    json!({
                        "type": "Mutation",
                        "requestId": 3,
                        "udfPath": "Patient/update",
                        "args": {
                            "id": "p1",
                            "resource": {"resourceType": "Patient", "id": "p1", "active": true}
                        }
                    })
                    .to_string(),
                ))
                .await
                .unwrap();
            let reply = next_json(&mut writer).await;
            assert_eq!(reply["type"], "MutationResponse");
            assert_eq!(
                reply["success"], accepted,
                "primary of tenant {primary_tenant}: {reply}"
            );
            if !accepted {
                assert_eq!(
                    cluster.primary_db.latest_ts(),
                    before,
                    "the refused mutation reached the primary"
                );
            }
        }
    }

    #[tokio::test]
    async fn healthz_names_the_tenant_a_process_serves() {
        let cluster = pair_with(
            |primary| primary.tenant = Some("acme".into()),
            |replica| replica.tenant = Some("acme".into()),
        )
        .await;
        let health = call(&cluster.replica_app, "GET", "/healthz", None, &[]).await;
        assert_eq!(health.body["tenant"], "acme");
        let plain = pair(|_| {}).await;
        let health = call(&plain.replica_app, "GET", "/healthz", None, &[]).await;
        assert!(health.body.get("tenant").is_none(), "{}", health.body);
    }

    #[tokio::test]
    async fn a_replica_waits_for_a_refresh_that_is_about_to_land() {
        let cluster = pair(|settings| settings.min_ts_wait = Duration::from_secs(5)).await;
        cluster
            .replica_db
            .start_refresher(Duration::from_millis(20));
        let ts = create_on_primary(&cluster, "p1");
        let min = ts.to_string();
        let read = call(
            &cluster.replica_app,
            "GET",
            "/Patient/p1",
            None,
            &[("x-refhir-min-ts", min.as_str())],
        )
        .await;
        assert_eq!(read.status, StatusCode::OK);
        assert_eq!(
            header(&read, "x-refhir-role"),
            "replica",
            "it caught up in time"
        );
    }

    /// A package that only teaches the catalog the resource types terminology is written as: the
    /// minimal R4 package has none of them.
    fn conformance_types_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "refhir-cluster-types-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"example.conformance.types","version":"1.0.0","fhirVersion":"4.0.1"}"#,
        )
        .unwrap();
        for type_name in ["SearchParameter", "ValueSet", "CodeSystem", "ConceptMap"] {
            std::fs::write(
                dir.join(format!("StructureDefinition-{type_name}.json")),
                format!(
                    r#"{{"resourceType":"StructureDefinition","url":"http://hl7.org/fhir/StructureDefinition/{type_name}","name":"{type_name}","status":"active","kind":"resource","abstract":false,"type":"{type_name}","fhirVersion":"4.0.1","derivation":"specialization","snapshot":{{"element":[{{"path":"{type_name}","min":0,"max":"*"}}]}}}}"#
                ),
            )
            .unwrap();
        }
        dir
    }

    #[tokio::test]
    async fn a_replica_follows_terminology_the_primary_is_given_through_the_api() {
        use refhir_fhir::{reload_catalog, CatalogClock::Stable};
        let cluster = pair(|_| {}).await;
        let primary = cluster_primary_app(&cluster);
        let replica_store = cluster.replica_store.as_ref();
        let replica_follows = || Stable.stamp(replica_store).unwrap();

        // The primary is given the resource types, which is an install: the replica is told, and
        // loads everything.
        let mut loaded_at = replica_follows();
        let dir = conformance_types_dir();
        let store = SqlitePersistence::open(&cluster.path).unwrap();
        install_package_dir(&store, &cluster.primary_db, &cluster.primary_catalog, &dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        loaded_at =
            reload_catalog(replica_store, &cluster.replica_catalog, loaded_at, Stable).unwrap();
        assert_eq!(loaded_at, replica_follows());
        assert!(loaded_at.clock > 0);

        // A client writes a code system and a value set over it.
        let system = call(
            &primary,
            "POST",
            "/CodeSystem",
            Some(json!({
                "resourceType": "CodeSystem", "url": "http://example.org/animals",
                "status": "active", "content": "complete",
                "concept": [{"code": "dog"}, {"code": "cat"}]
            })),
            &[],
        )
        .await;
        assert_eq!(system.status, StatusCode::CREATED, "{}", system.body);
        let set = call(
            &primary,
            "POST",
            "/ValueSet",
            Some(json!({
                "resourceType": "ValueSet", "url": "http://example.org/pets",
                "status": "active",
                "compose": {"include": [{"system": "http://example.org/animals"}]}
            })),
            &[],
        )
        .await;
        assert_eq!(set.status, StatusCode::CREATED, "{}", set.body);

        let expand = || {
            call(
                &cluster.replica_app,
                "POST",
                "/ValueSet/$expand",
                Some(json!({"url": "http://example.org/pets"})),
                &[],
            )
        };
        // The replica has not looked yet, and answers from what it loaded.
        assert_ne!(expand().await.status, StatusCode::OK);

        // What a watcher does when it sees the counters move. They used to stand still: a
        // terminology write moved none, so the replica went on answering from the old terminology
        // until something else (a package, a search parameter) did. Only the terminology changed,
        // so it is all the replica loads.
        let now =
            reload_catalog(replica_store, &cluster.replica_catalog, loaded_at, Stable).unwrap();
        assert!(
            now.terminology > loaded_at.terminology,
            "the replica was not told of the write"
        );
        assert_eq!(now.clock, loaded_at.clock, "only the terminology changed");
        let answered = expand().await;
        assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
        assert_eq!(header(&answered, "x-refhir-role"), "replica");
        let codes: Vec<&str> = answered.body["expansion"]["contains"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["code"].as_str().unwrap())
            .collect();
        assert_eq!(codes, ["cat", "dog"]);
    }

    #[tokio::test]
    async fn work_that_lives_on_the_primary_is_sent_there() {
        let cluster = pair(|_| {}).await;
        // Export jobs are kept by the process that accepted them.
        let export = call(&cluster.replica_app, "POST", "/$export", None, &[]).await;
        assert_eq!(export.status, StatusCode::ACCEPTED);
        assert_eq!(header(&export, "x-refhir-role"), "primary");
        // Installing a package writes the conformance tables, which a replica never does: the
        // operation answers `ReadOnly`, which the replica turns into a request to the primary.
        let install = refhir_fhir::commit_operation(
            cluster.replica_store.as_ref(),
            &cluster.replica_db,
            &cluster.replica_catalog,
            "ImplementationGuide/$install",
            &json!({}),
        )
        .unwrap_err();
        assert!(
            matches!(
                install,
                refhir_fhir::RunError::Commit(refhir_database::CommitError::ReadOnly)
            ),
            "{install:?}"
        );
    }

    #[tokio::test]
    async fn a_replica_without_a_primary_refuses_writes_and_never_loops() {
        let cluster = pair(|settings| settings.primary_url = None).await;
        let refused = call(
            &cluster.replica_app,
            "POST",
            "/Patient",
            Some(patient("p1")),
            &[],
        )
        .await;
        assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(header(&refused, "x-refhir-read-only"), "1");
        assert_eq!(refused.body["issue"][0]["code"], "transient");

        // A request that was already forwarded once is not forwarded again.
        let looping = pair(|_| {}).await;
        let bounced = call(
            &looping.replica_app,
            "POST",
            "/Patient",
            Some(patient("p2")),
            &[("x-refhir-forwarded", "1")],
        )
        .await;
        assert_eq!(bounced.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(header(&bounced, "x-refhir-role"), "replica");
    }

    #[tokio::test]
    async fn a_replica_that_cannot_ask_the_primary_never_answers_older_than_requested() {
        // No --primary-url, so a replica that is behind cannot forward. It must not quietly
        // answer from an older snapshot than the one the caller asked for: it says to retry.
        let cluster = pair(|settings| settings.primary_url = None).await;
        let ts = create_on_primary(&cluster, "p1");
        let min = ts.to_string();

        let behind = call(
            &cluster.replica_app,
            "GET",
            "/Patient/p1",
            None,
            &[("x-refhir-min-ts", min.as_str())],
        )
        .await;
        assert_eq!(
            behind.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            behind.body
        );
        assert_eq!(header(&behind, "retry-after"), "1");
        assert_eq!(behind.body["issue"][0]["code"], "transient");

        // Without the header the caller did not ask for anything newer, so it gets the snapshot
        // the replica has.
        let plain = call(&cluster.replica_app, "GET", "/Patient/p1", None, &[]).await;
        assert_eq!(plain.status, StatusCode::NOT_FOUND);

        // Once it has caught up, the same request is answered.
        let db = cluster.replica_db.clone();
        tokio::task::spawn_blocking(move || db.refresh().unwrap())
            .await
            .unwrap();
        let caught_up = call(
            &cluster.replica_app,
            "GET",
            "/Patient/p1",
            None,
            &[("x-refhir-min-ts", min.as_str())],
        )
        .await;
        assert_eq!(caught_up.status, StatusCode::OK);
        assert_eq!(header(&caught_up, "x-refhir-role"), "replica");
    }

    #[tokio::test]
    async fn the_smart_patient_picker_reads_the_replicas_snapshot() {
        // The picker's lookups (`patient_exists`) read the process's own snapshot. On a replica
        // that is a few tens of milliseconds behind the primary, and the decision is to accept
        // that by default: a patient created a moment ago may not be pickable until the replica
        // refreshes. A caller that must see it sends X-Refhir-Min-Ts, which the next test shows.
        let cluster = pair(|_| {}).await;
        create_on_primary(&cluster, "fresh-patient");
        let app = crate::AppState {
            db: cluster.replica_db.clone(),
            catalog: cluster.replica_catalog.clone(),
            store: cluster.replica_store.clone(),
            config: ServerConfig::default(),
            smart: Arc::new(crate::SmartStore::with_defaults()),
            oidc: None,
            jobs: Arc::new(crate::jobs::JobRunner::new(Arc::new(
                refhir_persistence::MemoryJobStore::default(),
            ))),
            governance: crate::Governance::new(crate::GovernanceConfig::default(), false, None),
            ready_probe: Default::default(),
        };
        assert!(
            !crate::auth::patient_exists(&app, "fresh-patient"),
            "the replica has not refreshed, so the picker does not see the new patient yet"
        );
        let db = cluster.replica_db.clone();
        tokio::task::spawn_blocking(move || db.refresh().unwrap())
            .await
            .unwrap();
        assert!(
            crate::auth::patient_exists(&app, "fresh-patient"),
            "after a refresh the picker sees it"
        );
    }

    #[tokio::test]
    async fn smart_pages_honor_min_ts_on_a_replica() {
        // `/oauth/*` is not exempt from the read-your-writes gate, so a SMART page request that
        // carries X-Refhir-Min-Ts is answered from a snapshot at least that new: by the primary
        // when the replica has not got there, by the replica itself once it has.
        let cluster = pair(|_| {}).await;
        let ts = create_on_primary(&cluster, "p1");
        let min = ts.to_string();
        let page = "/oauth/interact?session=none";

        let without = call(&cluster.replica_app, "GET", page, None, &[]).await;
        assert_eq!(header(&without, "x-refhir-role"), "replica");

        let behind = call(
            &cluster.replica_app,
            "GET",
            page,
            None,
            &[("x-refhir-min-ts", min.as_str())],
        )
        .await;
        assert_eq!(
            header(&behind, "x-refhir-role"),
            "primary",
            "a replica that is behind asks the primary"
        );

        let db = cluster.replica_db.clone();
        tokio::task::spawn_blocking(move || db.refresh().unwrap())
            .await
            .unwrap();
        let caught_up = call(
            &cluster.replica_app,
            "GET",
            page,
            None,
            &[("x-refhir-min-ts", min.as_str())],
        )
        .await;
        assert_eq!(
            header(&caught_up, "x-refhir-role"),
            "replica",
            "once it has caught up it serves the page itself"
        );
    }

    #[tokio::test]
    async fn a_replica_serves_live_queries_and_sends_mutations_to_the_primary() {
        let cluster = pair(|_| {}).await;
        create_on_primary(&cluster, "p1");
        let db = cluster.replica_db.clone();
        tokio::task::spawn_blocking(move || db.refresh().unwrap())
            .await
            .unwrap();
        cluster
            .replica_db
            .start_refresher(Duration::from_millis(15));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let replica_addr = listener.local_addr().unwrap();
        let replica = cluster.replica_app.clone();
        tokio::spawn(async move {
            axum::serve(listener, replica).await.unwrap();
        });

        let url = format!("ws://{replica_addr}/sync");
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
        assert_eq!(first["modifications"][0]["value"]["total"], 1);

        // The mutation goes in through the replica; the primary commits it.
        let (mut writer, _) = connect(&url).await;
        writer
            .send(Message::Text(
                json!({
                    "type": "Mutation",
                    "requestId": 9,
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
        assert_eq!(mutation["success"], true, "{mutation}");
        assert_eq!(mutation["requestId"], 9);
        let committed: u64 = mutation["ts"].as_str().unwrap().parse().unwrap();
        // The caller is only told once the replica itself has the write.
        assert!(cluster.replica_db.latest_ts().as_u64() >= committed);

        // The live query on the replica heard about the primary's commit.
        let transition = next_json(&mut subscriber).await;
        assert_eq!(transition["type"], "Transition");
        let family =
            &transition["modifications"][0]["value"]["entry"][0]["resource"]["name"][0]["family"];
        assert_eq!(family, "Bee");
        let _ = (&cluster.replica_store, &cluster.replica_catalog);
    }

    #[tokio::test]
    async fn a_replica_reloads_its_catalog_when_the_primary_changes_it() {
        let cluster = pair(|_| {}).await;
        let before = cluster
            .replica_catalog
            .read()
            .unwrap()
            .capability_statement()["rest"][0]["resource"]
            .as_array()
            .unwrap()
            .len();
        let (_, loaded_version) = Catalog::open_consistent(
            cluster.replica_store.as_ref(),
            refhir_fhir::CatalogClock::Stable,
        )
        .unwrap();
        refhir_fhir::watch_catalog(
            cluster.replica_store.clone(),
            &cluster.replica_catalog,
            loaded_version,
            Duration::from_millis(20),
            refhir_fhir::CatalogClock::Stable,
        );
        // The primary installs another package: more resource types, and a version bump.
        let primary_store: Arc<dyn ConformanceStore> =
            Arc::new(SqlitePersistence::open(&cluster.path).unwrap());
        install_package_dir(
            primary_store.as_ref(),
            &cluster.primary_db,
            &cluster.primary_catalog,
            extra_dir(),
        )
        .unwrap();
        let expected = cluster
            .primary_catalog
            .read()
            .unwrap()
            .capability_statement()["rest"][0]["resource"]
            .as_array()
            .unwrap()
            .len();
        assert!(expected > before, "the extra package adds resource types");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let now = cluster
                .replica_catalog
                .read()
                .unwrap()
                .capability_statement()["rest"][0]["resource"]
                .as_array()
                .unwrap()
                .len();
            if now == expected {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the replica never reloaded: {now} of {expected} resource types"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn a_replica_sends_a_batch_that_writes_to_the_primary() {
        let cluster = pair(|_| {}).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = cluster.replica_app.clone();
        tokio::spawn(async move {
            axum::serve(listener, served).await.unwrap();
        });
        let (mut socket, _) = connect(&format!("ws://{addr}/sync")).await;
        let ask = |id: u64, path: &str, args: Value| {
            json!({"type": "Mutation", "requestId": id, "udfPath": path, "args": args}).to_string()
        };
        // A plain write is sent on, which is what a batch has to be too.
        socket
            .send(Message::Text(ask(
                1,
                "Patient/create",
                json!({"resource": patient("single")}),
            )))
            .await
            .unwrap();
        let created = next_json(&mut socket).await;
        assert_eq!(created["success"], true, "{created}");

        socket
            .send(Message::Text(ask(
                2,
                "Bundle/batch",
                json!({"resource": {"resourceType": "Bundle", "type": "batch", "entry": [
                    {"request": {"method": "POST", "url": "Patient"}, "resource": patient("batched")},
                    {"request": {"method": "GET", "url": "Patient?_count=1"}}
                ]}}),
            )))
            .await
            .unwrap();
        let batch = next_json(&mut socket).await;
        assert_eq!(batch["success"], true, "{batch}");
        let status = batch["result"]["entry"][0]["response"]["status"]
            .as_str()
            .unwrap_or("");
        assert!(
            status.starts_with("201"),
            "the replica did not send the batch's write on: {batch}"
        );
        let loaded = cluster.primary_catalog.read().unwrap();
        let found =
            refhir_fhir::run(&cluster.primary_db, &loaded, "Patient/search", &json!({})).unwrap();
        assert_eq!(found.value["total"], 2, "{}", found.value);
    }
}

mod durable_jobs {
    use std::path::{Path, PathBuf};

    use refhir_persistence::{JobStatus, JobStore};

    use super::*;

    fn unique(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "refhir-jobs-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn remove_sqlite(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(name));
        }
    }

    /// What `refhir` builds from a database file at start-up. `first` installs the FHIR
    /// package into a new file; a restart loads the catalog back from it.
    struct Boot {
        persistence: Arc<SqlitePersistence>,
        app: axum::Router,
    }

    fn boot(sqlite: &Path, export_dir: &Path, first: bool) -> Boot {
        let persistence = Arc::new(SqlitePersistence::open(sqlite).unwrap());
        let db = Arc::new(Database::open(persistence.clone()).unwrap());
        let catalog = if first {
            let catalog = Arc::new(RwLock::new(Catalog::empty()));
            install_package_dir(persistence.as_ref(), &db, &catalog, minir4_dir()).unwrap();
            catalog
        } else {
            Arc::new(RwLock::new(Catalog::open(persistence.as_ref()).unwrap()))
        };
        // A reopened catalog starts from the stored definitions, which hold no deployment
        // settings, so the setting is applied on every boot as the server does.
        catalog.write().unwrap().set_update_create(true);
        let job_store: Arc<dyn JobStore> = persistence.clone();
        let store: Arc<dyn ConformanceStore> = persistence.clone();
        let app = crate::app_with(
            db,
            catalog,
            store,
            crate::ServerConfig {
                export_dir: export_dir.display().to_string(),
                jobs: Some(crate::SharedJobStore::new(job_store)),
                ..Default::default()
            },
        );
        Boot { persistence, app }
    }

    /// Poll the status endpoint until the job is `completed` or `error`.
    async fn finished(app: &axum::Router, job: &str) -> Value {
        for _ in 0..200 {
            let polled = send(app, "GET", &format!("/export/{job}"), None, None).await;
            if matches!(polled.body["status"].as_str(), Some("completed" | "error")) {
                return polled.body;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("export job {job} never finished");
    }

    #[tokio::test]
    async fn an_export_accepted_before_a_restart_is_still_there_after_it() {
        let sqlite = unique("restart.sqlite");
        let export_dir = unique("restart-export");
        let first = boot(&sqlite, &export_dir, true);
        let created = put(&first.app, patient("kept")).await;
        assert_eq!(created.status, StatusCode::CREATED);

        let accepted = send(&first.app, "POST", "/$export", Some(json!({})), None).await;
        assert_eq!(accepted.status, StatusCode::ACCEPTED);
        let job = accepted
            .location
            .expect("content-location")
            .trim_start_matches("/export/")
            .to_string();
        // The row exists by the time the 202 is back, before anything has been polled.
        let row = first.persistence.job_get(&job).unwrap().expect("job row");
        assert_eq!(row.kind, "export");
        assert_eq!(row.request, json!({"type": null, "id": null}));

        let before = finished(&first.app, &job).await;
        assert_eq!(before["status"], "completed", "{before}");
        assert_eq!(before["kind"], "export");
        drop(first);

        // A new app on the same file: nothing in this process remembers the job.
        let second = boot(&sqlite, &export_dir, false);
        let after = send(&second.app, "GET", &format!("/export/{job}"), None, None).await;
        assert_eq!(after.status, StatusCode::OK);
        assert_eq!(after.body["id"], job.as_str());
        assert_eq!(after.body["status"], "completed");
        assert_eq!(after.body["error"], Value::Null);
        assert_eq!(after.body["kind"], "export");
        assert_eq!(after.body["createdAt"], before["createdAt"]);
        assert!(after.body["completedAt"].as_str().unwrap().ends_with('Z'));
        let file = send_text(&second.app, "GET", &format!("/export/{job}/file"), "", &[]).await;
        assert_eq!(file.0, StatusCode::OK);
        assert!(file.1.contains("\"id\":\"kept\""), "{}", file.1);

        // A completed job is not run again by the restart.
        let row = second.persistence.job_get(&job).unwrap().unwrap();
        assert_eq!(row.attempts, 1);

        let unknown = send(&second.app, "GET", "/export/no-such-job", None, None).await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        assert_eq!(unknown.body["resourceType"], "OperationOutcome");
        let unknown_file = send_text(&second.app, "GET", "/export/no-such-job/file", "", &[]).await;
        assert_eq!(unknown_file.0, StatusCode::NOT_FOUND);

        drop(second);
        let _ = std::fs::remove_dir_all(&export_dir);
        remove_sqlite(&sqlite);
    }

    #[tokio::test]
    async fn jobs_left_running_are_run_again_when_the_server_starts() {
        let sqlite = unique("recover.sqlite");
        let export_dir = unique("recover-export");
        let first = boot(&sqlite, &export_dir, true);
        put(&first.app, patient("again")).await;

        // What a process killed mid-export leaves behind, plus rows that must not be rerun. The
        // dead server's leases ran out long ago.
        let store = &first.persistence;
        let request = json!({"type": "Patient", "id": null});
        let dead = refhir_persistence::JobLease {
            owner: "dead-server",
            ms: 1,
        };
        store
            .job_create("was-running", "export", &request, 1, Some(dead))
            .unwrap();
        assert!(store
            .job_start("was-running", "dead-server", 2, 1)
            .unwrap()
            .is_some());
        store
            .job_create("never-started", "export", &request, 3, None)
            .unwrap();
        store
            .job_create("stuck-in-a-loop", "export", &request, 4, Some(dead))
            .unwrap();
        for at in 5..8 {
            store
                .job_start("stuck-in-a-loop", "dead-server", at, 1)
                .unwrap();
        }
        store
            .job_create("from-the-future", "mystery", &json!({}), 9, None)
            .unwrap();
        store
            .job_create("already-failed", "export", &request, 10, Some(dead))
            .unwrap();
        store
            .job_finish(
                "already-failed",
                "dead-server",
                JobStatus::Failed,
                None,
                Some("disk full"),
                11,
            )
            .unwrap();
        drop(first);

        let second = boot(&sqlite, &export_dir, false);
        for job in ["was-running", "never-started"] {
            let done = finished(&second.app, job).await;
            assert_eq!(done["status"], "completed", "{job}: {done}");
            let file = send_text(&second.app, "GET", &format!("/export/{job}/file"), "", &[]).await;
            assert_eq!(file.0, StatusCode::OK, "{job}");
            assert!(file.1.contains("\"id\":\"again\""), "{job}: {}", file.1);
        }
        let running = second.persistence.job_get("was-running").unwrap().unwrap();
        assert_eq!(running.attempts, 2, "once before the restart, once after");
        assert!(running.completed_ms.is_some());

        // A job that keeps being interrupted is given up on instead of looping forever.
        let looped = finished(&second.app, "stuck-in-a-loop").await;
        assert_eq!(looped["status"], "error");
        assert!(
            looped["error"].as_str().unwrap().contains("gave up"),
            "{looped}"
        );
        // A kind this server has no handler for is left alone, unheld, for a server that has:
        // an older server in a rolling deploy must not fail a job it has never heard of.
        let mystery = second
            .persistence
            .job_get("from-the-future")
            .unwrap()
            .unwrap();
        assert_eq!(
            (mystery.status, mystery.attempts, mystery.lease_owner),
            (JobStatus::Accepted, 0, None)
        );
        // Other kinds of job are not served by the export routes.
        let hidden = send(&second.app, "GET", "/export/from-the-future", None, None).await;
        assert_eq!(hidden.status, StatusCode::NOT_FOUND);

        // A job that had already failed stays as it was.
        let failed = send(&second.app, "GET", "/export/already-failed", None, None).await;
        assert_eq!(failed.body["status"], "error");
        assert_eq!(failed.body["error"], "disk full");
        let failed_file =
            send_text(&second.app, "GET", "/export/already-failed/file", "", &[]).await;
        assert_eq!(failed_file.0, StatusCode::NOT_FOUND);
        let untouched = second
            .persistence
            .job_get("already-failed")
            .unwrap()
            .unwrap();
        assert_eq!(untouched.attempts, 0);

        drop(second);
        let _ = std::fs::remove_dir_all(&export_dir);
        remove_sqlite(&sqlite);
    }

    #[tokio::test]
    async fn a_failed_export_is_recorded_with_its_error() {
        let sqlite = unique("failed.sqlite");
        // A regular file where the export directory should be.
        let blocker = unique("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();
        let first = boot(&sqlite, &blocker, true);
        let accepted = send(&first.app, "POST", "/$export", Some(json!({})), None).await;
        assert_eq!(accepted.status, StatusCode::ACCEPTED);
        let job = accepted
            .location
            .unwrap()
            .trim_start_matches("/export/")
            .to_string();
        let done = finished(&first.app, &job).await;
        assert_eq!(done["status"], "error", "{done}");
        assert!(done["error"].as_str().is_some_and(|text| !text.is_empty()));
        assert!(done["completedAt"].is_string());
        let file = send_text(&first.app, "GET", &format!("/export/{job}/file"), "", &[]).await;
        assert_eq!(file.0, StatusCode::NOT_FOUND);
        let row = first.persistence.job_get(&job).unwrap().unwrap();
        assert_eq!(row.status, JobStatus::Failed);

        drop(first);
        let _ = std::fs::remove_file(&blocker);
        remove_sqlite(&sqlite);
    }
}

// ---- the /sync protocol: gaps found while documenting it ----

pub(crate) mod sync_gaps {
    use super::*;
    use crate::auth::SmartStore;

    pub(crate) type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    pub(crate) async fn serve(app: axum::Router) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    async fn say(socket: &mut Socket, message: Value) {
        socket
            .send(Message::Text(message.to_string()))
            .await
            .unwrap();
    }

    pub(crate) async fn add(
        socket: &mut Socket,
        base: u64,
        query_id: u64,
        path: &str,
        args: Value,
    ) {
        say(
            socket,
            json!({
                "type": "ModifyQuerySet",
                "baseVersion": base,
                "newVersion": base + 1,
                "modifications": [{"type": "Add", "queryId": query_id, "udfPath": path, "args": args}]
            }),
        )
        .await;
    }

    fn mutation(id: u64, path: &str, args: Value) -> Value {
        json!({"type": "Mutation", "requestId": id, "udfPath": path, "args": args})
    }

    /// The next message, or `None` when nothing came within `wait` or the connection ended.
    async fn next_within(socket: &mut Socket, wait: Duration) -> Option<Value> {
        match timeout(wait, socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => Some(serde_json::from_str(&text).unwrap()),
            _ => None,
        }
    }

    /// Whether the server has ended the connection, reading what is left of it.
    async fn ended(socket: &mut Socket) -> bool {
        loop {
            match timeout(Duration::from_secs(3), socket.next()).await {
                Ok(Some(Ok(Message::Text(_)))) | Ok(Some(Ok(Message::Ping(_)))) => continue,
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(_))) | Ok(None) => return true,
                Err(_) => return false,
            }
        }
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[tokio::test]
    async fn a_live_query_that_names_a_write_is_refused_before_it_runs_even_without_smart() {
        let (db, catalog, store) = memory();
        let app = app(db, catalog, store);
        let addr = serve(app.clone()).await;
        let (mut socket, _) = connect(&format!("ws://{addr}/sync")).await;
        let writes = [
            ("Patient/create", json!({"resource": patient("w1")})),
            (
                "Patient/update",
                json!({"id": "w2", "resource": patient("w2")}),
            ),
            (
                "Bundle/transaction",
                json!({"resource": {"resourceType": "Bundle", "type": "transaction", "entry": [
                    {"request": {"method": "POST", "url": "Patient"}, "resource": patient("w3")}
                ]}}),
            ),
        ];
        for (index, (path, args)) in writes.into_iter().enumerate() {
            let base = index as u64;
            add(&mut socket, base, 10 + base, path, args).await;
            let reply = next_json(&mut socket).await;
            let item = &reply["modifications"][0];
            assert_eq!(item["type"], "QueryFailed", "{path}: {reply}");
            assert!(
                item["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("cannot be subscribed"),
                "{path}: {reply}"
            );
            assert_eq!(item["code"], "invalid", "{path}: {reply}");
        }
        let found = send(&app, "GET", "/Patient", None, None).await;
        assert_eq!(
            found.body["total"], 0,
            "a subscribed write ran: {}",
            found.body
        );
    }

    #[tokio::test]
    async fn a_live_query_with_a_value_that_cannot_be_read_fails_instead_of_staying_empty() {
        let (db, catalog, store) = memory();
        let addr = serve(app(db, catalog, store)).await;
        let (mut socket, _) = connect(&format!("ws://{addr}/sync")).await;
        // The second asks to be lenient, which only the header of a `GET` can do: a live query is
        // never set aside to wait for a date that will never parse.
        let queries = [
            json!({"date": "notadate"}),
            json!({"date": "notadate", "_handling": "lenient"}),
        ];
        for (index, args) in queries.into_iter().enumerate() {
            let base = index as u64;
            add(&mut socket, base, 10 + base, "Observation/search", args).await;
            let reply = next_json(&mut socket).await;
            let item = &reply["modifications"][0];
            assert_eq!(item["type"], "QueryFailed", "{reply}");
            assert_eq!(item["code"], "invalid", "{reply}");
            assert_eq!(item["queryId"], 10 + base);
            let error = item["error"].as_str().unwrap_or("");
            assert!(
                error.contains("\"notadate\"")
                    && error.contains("search parameter date on Observation"),
                "{reply}"
            );
        }
        // A value it can read still subscribes.
        add(
            &mut socket,
            2,
            12,
            "Observation/search",
            json!({"date": "2024"}),
        )
        .await;
        let reply = next_json(&mut socket).await;
        assert_eq!(reply["modifications"][0]["type"], "QueryUpdated", "{reply}");
        assert_eq!(reply["modifications"][0]["value"]["total"], 0);
    }

    #[tokio::test]
    async fn a_query_that_failed_is_forgotten_by_the_server() {
        let (db, catalog, store) = memory();
        {
            let loaded = catalog.read().unwrap();
            refhir_fhir::import_resource(&db, &loaded, &patient("p1")).unwrap();
        }
        let addr = serve(app(db, catalog, store)).await;
        let url = format!("ws://{addr}/sync");
        let (mut watcher, _) = connect(&url).await;
        add(&mut watcher, 0, 1, "Patient/read", json!({"id": "p1"})).await;
        let first = next_json(&mut watcher).await;
        assert_eq!(first["modifications"][0]["type"], "QueryUpdated", "{first}");

        let (mut writer, _) = connect(&url).await;
        say(
            &mut writer,
            mutation(1, "Patient/delete", json!({"id": "p1"})),
        )
        .await;
        assert_eq!(next_json(&mut writer).await["success"], true);
        let failed = next_json(&mut watcher).await;
        assert_eq!(
            failed["modifications"][0]["type"], "QueryFailed",
            "{failed}"
        );
        // A watched resource that was deleted says so by its code, not only in its text.
        assert_eq!(failed["modifications"][0]["code"], "deleted", "{failed}");

        // The same id is written again. Both clients dropped the query when it failed, so the
        // server must have too: nothing may revive it, or it would hold a place under
        // `max_live_queries` for a query nobody is listening to.
        say(
            &mut writer,
            mutation(
                2,
                "Patient/update",
                json!({"id": "p1", "resource": patient("p1")}),
            ),
        )
        .await;
        let rewritten = next_json(&mut writer).await;
        assert_eq!(rewritten["success"], true, "{rewritten}");
        let revived = next_within(&mut watcher, Duration::from_millis(600)).await;
        assert!(revived.is_none(), "a failed query was revived: {revived:?}");
    }

    #[tokio::test]
    async fn a_query_set_change_sent_while_a_push_is_in_flight_is_answered_not_fatal() {
        let (db, catalog, store) = memory();
        let addr = serve(app(db, catalog, store)).await;
        let url = format!("ws://{addr}/sync");
        let (mut subscriber, _) = connect(&url).await;
        add(
            &mut subscriber,
            0,
            1,
            "Patient/search",
            json!({"active": "true"}),
        )
        .await;
        let first = next_json(&mut subscriber).await;
        assert_eq!(first["endVersion"], 1, "{first}");

        let (mut writer, _) = connect(&url).await;
        let mut active = patient("p1");
        active["active"] = json!(true);
        say(
            &mut writer,
            mutation(1, "Patient/create", json!({"resource": active})),
        )
        .await;
        assert_eq!(next_json(&mut writer).await["success"], true);
        // The push is on its way to the subscriber, who has not read it yet, and the subscriber
        // sends a change of its own, based on the version it has.
        tokio::time::sleep(Duration::from_millis(300)).await;
        add(&mut subscriber, 1, 2, "Patient/search", json!({})).await;

        let push = next_json(&mut subscriber).await;
        assert_eq!(push["type"], "Transition", "{push}");
        assert_eq!(push["startVersion"], 1, "{push}");
        assert_eq!(push["endVersion"], 2, "{push}");
        assert_eq!(push["modifications"][0]["queryId"], 1, "{push}");
        let answer = next_json(&mut subscriber).await;
        assert_eq!(answer["type"], "Transition", "{answer}");
        // The answer carries on from the push, and says which version the socket is now at.
        assert_eq!(answer["startVersion"], 2, "{answer}");
        assert_eq!(answer["endVersion"], 3, "{answer}");
        assert_eq!(answer["modifications"][0]["queryId"], 2, "{answer}");
        assert_eq!(
            answer["modifications"][0]["type"], "QueryUpdated",
            "{answer}"
        );

        // The socket carries on from there.
        add(
            &mut subscriber,
            3,
            3,
            "Patient/search",
            json!({"active": "false"}),
        )
        .await;
        let next = next_json(&mut subscriber).await;
        assert_eq!(next["startVersion"], 3, "{next}");
        assert_eq!(next["endVersion"], 4, "{next}");
    }

    #[tokio::test]
    async fn a_query_set_change_whose_base_the_server_cannot_place_is_still_fatal() {
        let (db, catalog, store) = memory();
        let addr = serve(app(db, catalog, store)).await;
        let url = format!("ws://{addr}/sync");
        for base in [5u64, 1u64] {
            // Ahead of the server, and (after one change) behind where the last change left it.
            let (mut socket, _) = connect(&url).await;
            if base == 1 {
                add(&mut socket, 0, 1, "Patient/search", json!({})).await;
                add(&mut socket, 1, 2, "Patient/search", json!({})).await;
                next_json(&mut socket).await;
                next_json(&mut socket).await;
                add(&mut socket, 0, 3, "Patient/search", json!({})).await;
            } else {
                add(&mut socket, base, 1, "Patient/search", json!({})).await;
            }
            let reply = next_json(&mut socket).await;
            assert_eq!(reply["type"], "FatalError", "base {base}: {reply}");
            assert!(
                ended(&mut socket).await,
                "base {base}: the connection stayed open"
            );
        }
    }

    fn smart_app(smart: Arc<SmartStore>, recheck: Duration) -> axum::Router {
        let (db, catalog, store) = memory();
        crate::app_with(
            db,
            catalog,
            store,
            crate::ServerConfig {
                auth: true,
                smart_demo_login: true,
                smart: Some(smart),
                sync_token_recheck: recheck,
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn a_socket_ends_when_its_token_is_revoked_even_if_it_only_listens() {
        let smart = Arc::new(SmartStore::with_defaults());
        smart.install_token("listener", &["user/Patient.read"], None);
        let addr = serve(smart_app(smart.clone(), Duration::from_millis(200))).await;
        let (mut socket, _) = connect(&format!("ws://{addr}/sync?access_token=listener")).await;
        add(&mut socket, 0, 1, "Patient/search", json!({})).await;
        assert_eq!(
            next_json(&mut socket).await["modifications"][0]["type"],
            "QueryUpdated"
        );

        smart.remove_token("listener");
        let fatal = next_within(&mut socket, Duration::from_secs(5)).await;
        assert_eq!(
            fatal.as_ref().map(|message| &message["type"]),
            Some(&json!("FatalError")),
            "{fatal:?}"
        );
        assert!(ended(&mut socket).await, "the socket stayed open");
    }

    #[tokio::test]
    async fn a_message_on_a_socket_whose_token_was_revoked_is_not_run() {
        let smart = Arc::new(SmartStore::with_defaults());
        smart.install_token("writer", &["user/Patient.read", "user/Patient.write"], None);
        // The periodic check is an hour away, so only a check on the message can catch this.
        let app = smart_app(smart.clone(), Duration::from_secs(3600));
        let addr = serve(app.clone()).await;
        let (mut socket, _) = connect(&format!("ws://{addr}/sync?access_token=writer")).await;
        say(
            &mut socket,
            mutation(1, "Patient/create", json!({"resource": patient("a1")})),
        )
        .await;
        assert_eq!(next_json(&mut socket).await["success"], true);

        smart.remove_token("writer");
        say(
            &mut socket,
            mutation(2, "Patient/create", json!({"resource": patient("a2")})),
        )
        .await;
        let reply = next_within(&mut socket, Duration::from_secs(5)).await;
        assert_eq!(
            reply.as_ref().map(|message| &message["type"]),
            Some(&json!("FatalError")),
            "{reply:?}"
        );
        smart.install_token("checker", &["user/Patient.read"], None);
        let found = send_text(
            &app,
            "GET",
            "/Patient",
            "",
            &[("authorization", "Bearer checker")],
        )
        .await;
        let found: Value = serde_json::from_str(&found.1).unwrap();
        assert_eq!(found["total"], 1, "a revoked token's write ran: {found}");
    }

    #[tokio::test]
    async fn a_socket_ends_when_its_token_expires() {
        let smart = Arc::new(SmartStore::with_defaults());
        smart.install_token_exp("brief", &["user/Patient.read"], None, now() + 2);
        // Only the token's own expiry can end this socket in time.
        let addr = serve(smart_app(smart, Duration::from_secs(3600))).await;
        let (mut socket, _) = connect(&format!("ws://{addr}/sync?access_token=brief")).await;
        add(&mut socket, 0, 1, "Patient/search", json!({})).await;
        assert_eq!(
            next_json(&mut socket).await["modifications"][0]["type"],
            "QueryUpdated"
        );
        let fatal = next_within(&mut socket, Duration::from_secs(6)).await;
        assert_eq!(
            fatal.as_ref().map(|message| &message["type"]),
            Some(&json!("FatalError")),
            "{fatal:?}"
        );
        assert!(ended(&mut socket).await, "the socket stayed open");
    }
}

/// A server over a tiny package (Patient and Immunization) with a signing key (or none), holding
/// three patients, two of them with an immunization. `p1-reader` is bound to `p1`; `admin` is not.
fn cards_app(signing: bool) -> axum::Router {
    let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
    let db = Arc::new(Database::open(store.clone()).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    install_package_dir(store.as_ref(), &db, &catalog, cards_dir()).unwrap();
    {
        let loaded = catalog.read().expect("catalog");
        for resource in [
            // p1 links to p5, as a merged or duplicate record does: a document that follows
            // references from p1's Composition reaches p5.
            json!({"resourceType": "Patient", "id": "p5", "name": [{"family": "Linked"}]}),
            json!({"resourceType": "Patient", "id": "p1", "name": [{"family": "Lovelace", "given": ["Ada"]}],
                "birthDate": "1815-12-10", "gender": "female",
                "link": [{"other": {"reference": "Patient/p5"}, "type": "seealso"}]}),
            json!({"resourceType": "Composition", "id": "c1", "status": "final", "type": {"text": "Summary"},
                "subject": {"reference": "Patient/p1"}, "date": "2026-10-18", "title": "Lovelace"}),
            json!({"resourceType": "Patient", "id": "p2", "name": [{"family": "Other"}], "birthDate": "1990-01-01"}),
            json!({"resourceType": "Patient", "id": "p3", "name": [{"family": "Empty"}], "birthDate": "2000-01-01"}),
            json!({"resourceType": "Immunization", "id": "i1", "status": "completed",
                "vaccineCode": {"coding": [{"system": "http://hl7.org/fhir/sid/cvx", "code": "140", "display": "Influenza"}]},
                "patient": {"reference": "Patient/p1"}, "occurrenceDateTime": "2023-10-01"}),
            json!({"resourceType": "Immunization", "id": "i2", "status": "completed",
                "vaccineCode": {"coding": [{"system": "http://hl7.org/fhir/sid/cvx", "code": "88"}]},
                "patient": {"reference": "Patient/p2"}, "occurrenceDateTime": "2022-01-01"}),
        ] {
            refhir_fhir::import_resource(&db, &loaded, &resource).unwrap();
        }
    }
    let smart = Arc::new(crate::auth::SmartStore::with_defaults());
    smart.install_token("p1-reader", &["patient/*.read"], Some("p1"));
    smart.install_token("admin", &["user/*.read"], None);
    smart.install_token("observations-only", &["user/Observation.read"], None);
    crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            auth: true,
            smart_demo_login: true,
            export_dir: "export".into(),
            terminology_urls: Vec::new(),
            sdc_archiver: false,
            smart: Some(smart),
            public_url: Some("https://refhir.example.org/fhir".into()),
            health_cards: signing.then(|| {
                Arc::new(crate::IssuerKey::from_pem(&crate::health_cards::generated_pem()).unwrap())
            }),
            ..Default::default()
        },
    )
}

fn card_parameters(extra: Value) -> Value {
    let mut parameter = vec![json!({"name": "credentialType", "valueUri": "Immunization"})];
    parameter.extend(extra.as_array().cloned().unwrap_or_default());
    json!({"resourceType": "Parameters", "parameter": parameter})
}

/// The claims of a card, once its signature has been checked against `jwks`.
fn checked_claims(card: &str, jwks: &Value) -> Value {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use ring::signature::{UnparsedPublicKey, ECDSA_P256_SHA256_FIXED};
    use std::io::Read;

    let parts: Vec<&str> = card.split('.').collect();
    assert_eq!(parts.len(), 3);
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
    let key = jwks["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|key| key["kid"] == header["kid"])
        .expect("the key set holds the key the card names");
    let mut point = vec![4u8];
    point.extend(URL_SAFE_NO_PAD.decode(key["x"].as_str().unwrap()).unwrap());
    point.extend(URL_SAFE_NO_PAD.decode(key["y"].as_str().unwrap()).unwrap());
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point)
        .verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
        )
        .expect("the signature checks");
    let mut payload = String::new();
    flate2::read::DeflateDecoder::new(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()[..])
        .read_to_string(&mut payload)
        .unwrap();
    serde_json::from_str(&payload).unwrap()
}

/// Loading the R4 release is most of what a card test costs, so the tests of a server with a
/// signing key are the steps of one test, over one server.
#[tokio::test]
async fn a_patients_document_follows_references_to_their_own_record_and_no_further() {
    let app = cards_app(false);
    // p1's Composition reaches p1, and p1 links to p5, another patient's record.
    let own = get_as(&app, "p1-reader", "/Composition/c1/$document").await;
    assert_eq!(own.0, StatusCode::OK, "{}", own.1);
    assert_eq!(own.1["type"], "document");
    let ids = resource_ids(&own.1);
    assert!(
        ids.contains(&"c1".to_string()) && ids.contains(&"p1".to_string()),
        "{ids:?}"
    );
    assert!(
        !ids.contains(&"p5".to_string()),
        "another patient's record is not in it: {ids:?}"
    );
    assert!(
        !own.1.to_string().contains("Linked"),
        "nothing of it is: {}",
        own.1
    );

    // A caller who is not limited to a patient follows the reference.
    let admin = get_as(&app, "admin", "/Composition/c1/$document").await;
    assert_eq!(admin.0, StatusCode::OK, "{}", admin.1);
    assert!(resource_ids(&admin.1).contains(&"p5".to_string()));
}

#[tokio::test]
async fn a_server_with_a_signing_key_issues_cards_that_its_published_key_checks() {
    let app = cards_app(true);
    a_health_card_is_signed_by_the_server_and_checked_with_its_published_key(&app).await;
    a_health_card_is_issued_to_the_patient_it_is_about_and_to_no_one_else(&app).await;
    a_health_card_request_that_cannot_be_met_is_refused_and_says_why(&app).await;
    a_health_card_for_what_is_new_holds_only_what_is_new(&app).await;
    a_server_with_a_key_says_it_issues_cards(&app).await;
}

async fn a_health_card_for_what_is_new_holds_only_what_is_new(app: &axum::Router) {
    let asked = |since: &str| {
        let app = app.clone();
        let parameters = card_parameters(json!([{"name": "_since", "valueDateTime": since}]));
        async move {
            let reply = post_as(
                &app,
                "admin",
                "/Patient/p1/$health-cards-issue",
                &parameters,
            )
            .await;
            assert_eq!(reply.0, StatusCode::OK, "{}", reply.1);
            reply.1
        }
    };
    // Everything here was written when the server was made, so a time before that holds it all, and
    // one after it holds nothing, and an empty answer is not an error.
    let all = asked("2000-01-01").await;
    assert_eq!(all["parameter"][0]["name"], "verifiableCredential");
    assert_eq!(
        asked("2999-01-01T00:00:00Z").await,
        json!({"resourceType": "Parameters"})
    );
}

async fn a_health_card_is_signed_by_the_server_and_checked_with_its_published_key(
    app: &axum::Router,
) {
    // The key set is open, and readable from a browser.
    let anonymous = send(
        app,
        "GET",
        "/health-cards/.well-known/jwks.json",
        None,
        None,
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::OK, "{}", anonymous.body);
    let jwks = anonymous.body.clone();
    assert_eq!(jwks["keys"][0]["alg"], "ES256");
    assert!(jwks["keys"][0].get("d").is_none());
    // Any origin may read it, a GET or a preflight: that is how a wallet in a browser checks a card.
    for method in ["GET", "OPTIONS"] {
        let origin = cors_header(
            app,
            method,
            "/health-cards/.well-known/jwks.json",
            &[("origin", "https://wallet.example")],
            "access-control-allow-origin",
        )
        .await;
        assert_eq!(origin.as_deref(), Some("*"), "{method}");
    }

    let issued = post_as(
        app,
        "admin",
        "/Patient/p1/$health-cards-issue",
        &card_parameters(json!([])),
    )
    .await;
    assert_eq!(issued.0, StatusCode::OK, "{}", issued.1);
    let parameters = issued.1["parameter"].as_array().unwrap();
    let card = parameters[0]["valueString"].as_str().unwrap();
    assert_eq!(parameters[0]["name"], "verifiableCredential");

    let claims = checked_claims(card, &jwks);
    assert_eq!(
        claims["iss"],
        "https://refhir.example.org/fhir/health-cards"
    );
    assert!(claims["nbf"].as_u64().unwrap() > 1_700_000_000);
    assert_eq!(
        claims["vc"]["type"],
        json!([
            "https://smarthealth.cards#health-card",
            "https://smarthealth.cards#immunization"
        ])
    );
    assert_eq!(claims["vc"]["credentialSubject"]["fhirVersion"], "4.0.1");
    let bundle = &claims["vc"]["credentialSubject"]["fhirBundle"];
    assert_eq!(bundle["type"], "collection");
    let entries = bundle["entry"].as_array().unwrap();
    assert_eq!(
        entries.len(),
        2,
        "p1 and p1's immunization, not p2's: {bundle}"
    );
    assert_eq!(entries[0]["resource"]["name"][0]["family"], "Lovelace");
    assert_eq!(entries[1]["resource"]["patient"]["reference"], "resource:0");
    assert!(
        !card.contains("Lovelace"),
        "the card is compressed, not readable text"
    );

    // Where each resource came from.
    let link = |bundled: &str| {
        parameters
            .iter()
            .filter(|parameter| parameter["name"] == "resourceLink")
            .find(|parameter| parameter["part"][1]["valueUri"] == bundled)
            .map(|parameter| parameter["part"][2]["valueUri"].clone())
            .unwrap()
    };
    assert_eq!(
        link("resource:0"),
        "https://refhir.example.org/fhir/Patient/p1"
    );
    assert_eq!(
        link("resource:1"),
        "https://refhir.example.org/fhir/Immunization/i1"
    );

    // A patient with nothing to put in a card gets no card, not an error.
    let none = post_as(
        app,
        "admin",
        "/Patient/p3/$health-cards-issue",
        &card_parameters(json!([])),
    )
    .await;
    assert_eq!(none.0, StatusCode::OK, "{}", none.1);
    assert_eq!(none.1, json!({"resourceType": "Parameters"}));
}

async fn a_health_card_is_issued_to_the_patient_it_is_about_and_to_no_one_else(app: &axum::Router) {
    let own = post_as(
        app,
        "p1-reader",
        "/Patient/p1/$health-cards-issue",
        &card_parameters(json!([])),
    )
    .await;
    assert_eq!(own.0, StatusCode::OK, "{}", own.1);
    let other = post_as(
        app,
        "p1-reader",
        "/Patient/p2/$health-cards-issue",
        &card_parameters(json!([])),
    )
    .await;
    assert_eq!(other.0, StatusCode::FORBIDDEN, "{}", other.1);
    let blind = post_as(
        app,
        "observations-only",
        "/Patient/p1/$health-cards-issue",
        &card_parameters(json!([])),
    )
    .await;
    assert_eq!(blind.0, StatusCode::FORBIDDEN, "{}", blind.1);
    let anonymous = send(
        app,
        "POST",
        "/Patient/p1/$health-cards-issue",
        Some(card_parameters(json!([]))),
        None,
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
}

async fn a_health_card_request_that_cannot_be_met_is_refused_and_says_why(app: &axum::Router) {
    let refused = |parameters: Value| {
        let app = app.clone();
        async move {
            let reply = post_as(
                &app,
                "admin",
                "/Patient/p1/$health-cards-issue",
                &parameters,
            )
            .await;
            assert_eq!(reply.0, StatusCode::BAD_REQUEST, "{}", reply.1);
            reply.1["issue"][0]["diagnostics"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let since = refused(card_parameters(
        json!([{"name": "_since", "valueString": "last week"}]),
    ))
    .await;
    assert!(since.contains("_since is not a date"), "{since}");
    let unknown = refused(json!({"resourceType": "Parameters", "parameter": [
        {"name": "credentialType", "valueUri": "https://smarthealth.cards#covid19"}
    ]}))
    .await;
    assert!(unknown.contains("covid19"), "{unknown}");
    let nothing = refused(json!({"resourceType": "Parameters", "parameter": []})).await;
    assert!(nothing.contains("credentialType is required"), "{nothing}");
    let no_body = send_text(
        app,
        "POST",
        "/Patient/p1/$health-cards-issue",
        "",
        &[
            ("authorization", "Bearer admin"),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    assert_eq!(no_body.0, StatusCode::BAD_REQUEST, "{}", no_body.1);
    let wrong_type = post_as(
        app,
        "admin",
        "/Immunization/i1/$health-cards-issue",
        &card_parameters(json!([])),
    )
    .await;
    assert_eq!(wrong_type.0, StatusCode::BAD_REQUEST, "{}", wrong_type.1);
    let unknown_patient = post_as(
        app,
        "admin",
        "/Patient/nobody/$health-cards-issue",
        &card_parameters(json!([])),
    )
    .await;
    assert_eq!(
        unknown_patient.0,
        StatusCode::NOT_FOUND,
        "{}",
        unknown_patient.1
    );
}

#[tokio::test]
async fn a_server_without_a_signing_key_issues_no_cards_and_does_not_say_it_does() {
    let app = cards_app(false);
    let reply = post_as(
        &app,
        "admin",
        "/Patient/p1/$health-cards-issue",
        &card_parameters(json!([])),
    )
    .await;
    assert_eq!(reply.0, StatusCode::NOT_IMPLEMENTED, "{}", reply.1);
    assert!(reply.1["issue"][0]["diagnostics"]
        .as_str()
        .unwrap()
        .contains("no signing key"));
    let keys = send(
        &app,
        "GET",
        "/health-cards/.well-known/jwks.json",
        None,
        None,
    )
    .await;
    assert_eq!(keys.status, StatusCode::NOT_FOUND);

    let without = send(&app, "GET", "/metadata", None, None).await;
    assert_eq!(patient_operations(&without.body), ["summary", "match"]);
    let capabilities = send(&app, "GET", "/.well-known/smart-configuration", None, None).await;
    assert!(!capabilities.body["capabilities"]
        .to_string()
        .contains("health-cards"));
}

fn patient_operations(statement: &Value) -> Vec<String> {
    statement["rest"][0]["resource"]
        .as_array()
        .unwrap()
        .iter()
        .find(|resource| resource["type"] == "Patient")
        .unwrap()["operation"]
        .as_array()
        .unwrap()
        .iter()
        .map(|operation| operation["name"].as_str().unwrap().to_string())
        .collect()
}

async fn a_server_with_a_key_says_it_issues_cards(signing: &axum::Router) {
    let with = send(signing, "GET", "/metadata", None, None).await;
    assert_eq!(
        patient_operations(&with.body),
        ["summary", "match", "health-cards-issue"]
    );
    let capabilities = send(
        signing,
        "GET",
        "/.well-known/smart-configuration",
        None,
        None,
    )
    .await;
    assert!(capabilities.body["capabilities"]
        .as_array()
        .unwrap()
        .contains(&json!("health-cards")));
}

#[tokio::test]
async fn over_http_a_resource_says_when_it_was_written_and_is_found_and_listed_by_it() {
    let app = cards_app(false);
    let read = get_as(&app, "admin", "/Patient/p1").await;
    assert_eq!(read.0, StatusCode::OK, "{}", read.1);
    let written = read.1["meta"]["lastUpdated"].as_str().unwrap().to_string();
    assert!(written.ends_with('Z') && written.len() == 24, "{written}");

    let found = |uri: String| {
        let app = app.clone();
        async move {
            let reply = get_as(&app, "admin", &uri).await;
            assert_eq!(reply.0, StatusCode::OK, "{uri}: {}", reply.1);
            resource_ids(&reply.1)
        }
    };
    // Searching by it.
    assert_eq!(
        found("/Patient?_lastUpdated=ge2000-01-01".into())
            .await
            .len(),
        4
    );
    assert!(found(format!("/Patient?_lastUpdated=ge{written}"))
        .await
        .contains(&"p1".to_string()));
    assert!(found("/Patient?_lastUpdated=gt2999-01-01".into())
        .await
        .is_empty());
    assert!(found("/Patient?_lastUpdated=lt2000-01-01".into())
        .await
        .is_empty());
    let nonsense = get_as(&app, "admin", "/Patient?_lastUpdated=notadate").await;
    assert_eq!(nonsense.0, StatusCode::BAD_REQUEST, "{}", nonsense.1);

    // Listing by it: of a type, and of the whole server.
    assert!(found(format!("/Patient/_history?_since={written}"))
        .await
        .contains(&"p1".to_string()));
    assert!(
        found("/Patient/_history?_since=2999-01-01T00:00:00Z".into())
            .await
            .is_empty()
    );
    assert!(found("/_history?_since=2999-01-01T00:00:00Z".into())
        .await
        .is_empty());
    assert!(!found("/_history?_since=2000-01-01".into()).await.is_empty());
    let refused = get_as(&app, "admin", "/Patient/_history?_since=yesterday").await;
    assert_eq!(refused.0, StatusCode::BAD_REQUEST, "{}", refused.1);
    assert!(refused.1["issue"][0]["diagnostics"]
        .as_str()
        .unwrap()
        .contains("invalid _since"));
}
