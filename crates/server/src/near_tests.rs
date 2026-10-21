//! Searching for places by distance over REST, as a provider directory does: `near`, sorted
//! nearest first with the distance on every entry, paged; and what the server does with a
//! `_sort` (or another control) it cannot apply, with and without `Prefer: handling`.

use std::sync::{Arc, OnceLock, RwLock};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use refhir_database::Database;
use refhir_fhir::{import_resource, install_package_dir, r4_dir, Catalog};
use refhir_sqlite::SqlitePersistence;
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::app;

const DISTANCE: &str = "http://hl7.org/fhir/StructureDefinition/location-distance";

/// Eight places along the coast road east of Lagos, each about 5 km from the one before, a place
/// in Abuja, and one with no position. Reading is all the tests do, so one server serves them all.
fn places() -> axum::Router {
    static SEEDED: OnceLock<axum::Router> = OnceLock::new();
    SEEDED
        .get_or_init(|| {
            let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
            let db = Arc::new(Database::open(store.clone()).unwrap());
            let catalog = Arc::new(RwLock::new(Catalog::empty()));
            install_package_dir(store.as_ref(), &db, &catalog, r4_dir()).unwrap();
            let mut resources: Vec<Value> = (0..8)
                .map(|n| {
                    json!({"resourceType": "Location", "id": format!("c{n}"),
                        "status": "active", "name": format!("Clinic {n}"),
                        "position": {"latitude": 6.45, "longitude": 3.40 + f64::from(n) * 0.045}})
                })
                .collect();
            resources.push(
                json!({"resourceType": "Location", "id": "abuja", "status": "active",
                "name": "Clinic Abuja",
                "position": {"latitude": 9.0765, "longitude": 7.3986}}),
            );
            resources.push(json!({"resourceType": "Location", "id": "nowhere",
                "status": "active", "name": "Clinic Nowhere"}));
            for resource in &resources {
                import_resource(&db, &catalog.read().unwrap(), resource)
                    .unwrap_or_else(|err| panic!("{resource}: {err}"));
            }
            app(db, catalog, store)
        })
        .clone()
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

/// A GET. A `|` is not valid in a request target, so the way a client sends one is `%7C`.
async fn get(uri: &str, prefer: Option<&str>) -> Answer {
    let mut builder = Request::builder()
        .method("GET")
        .uri(uri.replace('|', "%7C"));
    if let Some(prefer) = prefer {
        builder = builder.header("prefer", prefer);
    }
    let response = places()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap(),
    }
}

fn link(body: &Value, relation: &str) -> Option<String> {
    body["link"]
        .as_array()?
        .iter()
        .find(|link| link["relation"] == relation)?["url"]
        .as_str()
        .map(str::to_string)
}

fn matches(body: &Value) -> Vec<&Value> {
    body["entry"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["search"]["mode"] == "match")
        .collect()
}

fn ids(body: &Value) -> Vec<String> {
    matches(body)
        .iter()
        .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string())
        .collect()
}

fn distance(entry: &Value) -> f64 {
    let extensions = entry["search"]["extension"]
        .as_array()
        .expect("an extension");
    assert_eq!(extensions[0]["url"], DISTANCE, "{entry}");
    let found = &extensions[0]["valueDistance"];
    assert_eq!(found["unit"], "km");
    assert_eq!(found["system"], "http://unitsofmeasure.org");
    assert_eq!(found["code"], "km");
    found["value"].as_f64().unwrap()
}

#[tokio::test]
async fn near_finds_the_places_within_the_distance_and_says_how_far_each_is() {
    // Lagos, 6.45 N 3.40 E: the first clinic is at it, the next about 5 km on, and so on.
    let answer = get("/Location?near=6.45|3.40|12|km", None).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let mut found = ids(&answer.body);
    found.sort();
    assert_eq!(found, ["c0", "c1", "c2"]);
    assert_eq!(answer.body["total"], 3);
    for entry in matches(&answer.body) {
        let km = distance(entry);
        assert!((0.0..=12.0).contains(&km), "{km}");
    }
    // Kilometres when the units are left out, and ten (the third clinic is 9.95 km out) when the
    // distance is.
    for near in ["6.45|3.40|12", "6.45|3.40|12|"] {
        let answer = get(&format!("/Location?near={near}"), None).await;
        assert_eq!(answer.body["total"], 3, "{near}");
    }
    assert_eq!(get("/Location?near=6.45|3.40", None).await.body["total"], 3);
    assert_eq!(
        get("/Location?near=6.45|3.40|9|km", None).await.body["total"],
        2
    );
    // Places far away and places with no position are not near.
    let all = get("/Location?near=6.45|3.40|1000|km", None).await;
    assert_eq!(all.body["total"], 9);
    assert!(!ids(&all.body).contains(&"nowhere".to_string()));
}

#[tokio::test]
async fn sorting_by_near_gives_nearest_first_and_a_distance_on_every_entry() {
    for (sort, farthest_first) in [("near", false), ("-near", true)] {
        let answer = get(
            &format!("/Location?near=6.45|3.40|1000|km&_sort={sort}"),
            None,
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
        let mut want: Vec<String> = (0..8).map(|n| format!("c{n}")).collect();
        want.push("abuja".into());
        if farthest_first {
            want.reverse();
        }
        assert_eq!(ids(&answer.body), want, "_sort={sort}");
        let distances: Vec<f64> = matches(&answer.body).into_iter().map(distance).collect();
        assert!(
            distances
                .windows(2)
                .all(|pair| (pair[0] <= pair[1]) != farthest_first),
            "{distances:?}"
        );
        // The self link keeps the sort, which was applied.
        assert!(link(&answer.body, "self")
            .unwrap()
            .contains(&format!("_sort={sort}")));
    }
}

#[tokio::test]
async fn a_page_of_a_sorted_near_search_continues_where_the_last_ended() {
    for (sort, farthest_first) in [("near", false), ("-near", true)] {
        let mut want: Vec<String> = (0..8).map(|n| format!("c{n}")).collect();
        want.push("abuja".into());
        if farthest_first {
            want.reverse();
        }
        let mut answer = get(
            &format!("/Location?near=6.45|3.40|1000|km&_sort={sort}&_count=2"),
            None,
        )
        .await;
        let mut read = ids(&answer.body);
        let mut pages = 1;
        while let Some(next) = link(&answer.body, "next") {
            assert_eq!(answer.body["total"], 9, "{sort} page {pages}");
            for entry in matches(&answer.body) {
                distance(entry);
            }
            answer = get(&format!("/{next}"), None).await;
            assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
            read.extend(ids(&answer.body));
            pages += 1;
        }
        assert_eq!(read, want, "_sort={sort}");
        assert_eq!(pages, 5, "nine places, two to a page");
    }
}

fn said(body: &Value) -> String {
    body["issue"][0]["diagnostics"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn a_sort_that_cannot_be_applied_is_a_400_unless_the_client_is_lenient() {
    // Without a preference the search is strict, and so it is when the client asks for it.
    for prefer in [None, Some("handling=strict"), Some("return=minimal")] {
        let answer = get("/Location?_sort=bogus", prefer).await;
        assert_eq!(
            answer.status,
            StatusCode::BAD_REQUEST,
            "{prefer:?}: {}",
            answer.body
        );
        assert_eq!(answer.body["resourceType"], "OperationOutcome");
        assert_eq!(answer.body["issue"][0]["code"], "invalid");
        let message = said(&answer.body);
        assert!(
            message.contains("_sort") && message.contains("bogus"),
            "{message}"
        );
        assert!(answer.headers.get("preference-applied").is_none());
    }
    // Lenient: the search runs without the term, and the `self` link says what ran.
    let answer = get(
        "/Location?_sort=bogus&near=6.45|3.40|12|km",
        Some("handling=lenient"),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(
        answer.headers.get("preference-applied").unwrap(),
        "handling=lenient"
    );
    let url = link(&answer.body, "self").unwrap();
    assert_eq!(url, "Location?_count=100&near=6.45|3.40|12|km");
    assert!(!url.contains("_sort") && !url.contains("bogus"));
    assert_eq!(answer.body["total"], 3);
    // The outcome entry says which parameter was set aside, and it is not counted.
    let outcome: Vec<&Value> = answer.body["entry"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["search"]["mode"] == "outcome")
        .collect();
    assert_eq!(outcome.len(), 1);
    let issue = &outcome[0]["resource"]["issue"][0];
    assert_eq!(issue["expression"], json!(["_sort"]));
    assert_eq!(issue["severity"], "warning");
    // A term that is understood stays in the link, beside the one that is left out.
    let answer = get("/Location?_sort=-name,bogus", Some("handling=lenient")).await;
    assert_eq!(
        link(&answer.body, "self").unwrap(),
        "Location?_count=100&_sort=-name"
    );
    // `_sort=near` with no near to measure from is the same kind of thing.
    let answer = get("/Location?_sort=near", None).await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{}", answer.body);
    let answer = get("/Location?_sort=near", Some("handling=lenient")).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert!(!link(&answer.body, "self").unwrap().contains("near"));
}

#[tokio::test]
async fn the_other_controls_the_server_ignored_are_held_to_the_same_rule() {
    for (uri, shown) in [
        ("/Location?_summary=bogus", "_summary"),
        ("/Location?_total=bogus", "_total"),
        ("/Location?_include=Location:bogus", "_include"),
        (
            "/Location?_revinclude=HealthcareService:bogus",
            "_revinclude",
        ),
    ] {
        for prefer in [None, Some("handling=strict")] {
            let answer = get(uri, prefer).await;
            assert_eq!(
                answer.status,
                StatusCode::BAD_REQUEST,
                "{uri}: {}",
                answer.body
            );
            assert!(said(&answer.body).contains(shown), "{uri}: {}", answer.body);
        }
        let answer = get(uri, Some("handling=lenient")).await;
        assert_eq!(answer.status, StatusCode::OK, "{uri}: {}", answer.body);
        let url = link(&answer.body, "self").unwrap();
        assert!(!url.contains(shown), "{uri}: {url}");
        assert_eq!(answer.body["total"], 10, "{uri}");
    }
    // Controls the server applies are in the link.
    let answer = get("/Location?_summary=count&_total=accurate", None).await;
    let url = link(&answer.body, "self").unwrap();
    assert!(
        url.contains("_summary=count") && url.contains("_total=accurate"),
        "{url}"
    );
}
