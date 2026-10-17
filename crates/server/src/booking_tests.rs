//! The searches a booking client sends over REST, against the data a booking system holds:
//! schedules whose actors are a healthcare service and a location, free slots on them, and
//! appointments with a patient and a location as participants.
//!
//! Every search here is read back the way FHIR R4 defines it: a comma between the values of one
//! parameter is OR; a parameter given more than once is AND, each occurrence judged against the
//! resource on its own, so one resource can satisfy both through different elements of a repeating
//! one (`Schedule.actor`, `Appointment.participant`).

use std::sync::{Arc, OnceLock, RwLock};

use axum::http::StatusCode;
use refhir_database::Database;
use refhir_fhir::{import_resource, install_package_dir, r4_dir, Catalog};
use refhir_sqlite::SqlitePersistence;
use serde_json::{json, Value};

use crate::app;
use crate::tests::send;

/// Everything the tests share. Reading is all they do, so one server is built once for the module:
/// installing the R4 package is most of the time a test would otherwise take.
fn booking() -> axum::Router {
    static SEEDED: OnceLock<axum::Router> = OnceLock::new();
    SEEDED.get_or_init(seed).clone()
}

fn slot(id: &str, schedule: &str, hour: u32) -> Value {
    json!({
        "resourceType": "Slot", "id": id,
        "schedule": {"reference": format!("Schedule/{schedule}")},
        "status": "free",
        "start": format!("2030-01-01T{hour:02}:00:00Z"),
        "end": format!("2030-01-01T{hour:02}:30:00Z")
    })
}

fn appointment(id: &str, location: &str) -> Value {
    json!({
        "resourceType": "Appointment", "id": id, "status": "booked",
        "start": "2030-01-01T09:00:00Z", "end": "2030-01-01T09:30:00Z",
        "participant": [
            {"actor": {"reference": "Patient/p1"}, "status": "accepted"},
            {"actor": {"reference": format!("Location/{location}")}, "status": "accepted"}
        ]
    })
}

/// Schedule `S1` has the actors `HealthcareService/h1` and `Location/l1`, schedule `S2` has
/// `HealthcareService/h1` and `Location/l2`, and each has free slots (`sl1` and `sl2` on `S1`,
/// `sl3` and `sl4` on `S2`). Appointment `a1` has the participants `Patient/p1` and `Location/l1`,
/// and `a2` has `Patient/p1` and `Location/l2`.
///
/// Reference checking is the default (strict) one, so what a resource points at is created first.
fn seed() -> axum::Router {
    let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
    let db = Arc::new(Database::open(store.clone()).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    install_package_dir(store.as_ref(), &db, &catalog, r4_dir()).unwrap();
    let resources = [
        json!({"resourceType": "HealthcareService", "id": "h1"}),
        json!({"resourceType": "Location", "id": "l1"}),
        json!({"resourceType": "Location", "id": "l2"}),
        json!({"resourceType": "Patient", "id": "p1"}),
        json!({"resourceType": "Schedule", "id": "S1", "actor": [
            {"reference": "HealthcareService/h1"}, {"reference": "Location/l1"}]}),
        json!({"resourceType": "Schedule", "id": "S2", "actor": [
            {"reference": "HealthcareService/h1"}, {"reference": "Location/l2"}]}),
        slot("sl1", "S1", 9),
        slot("sl2", "S1", 10),
        slot("sl3", "S2", 11),
        slot("sl4", "S2", 12),
        appointment("a1", "l1"),
        appointment("a2", "l2"),
    ];
    for resource in &resources {
        import_resource(&db, &catalog.read().unwrap(), resource)
            .unwrap_or_else(|err| panic!("{resource}: {err}"));
    }
    app(db, catalog, store)
}

/// The ids a GET search matched, sorted, and the `total` it reported.
async fn search(uri: &str) -> (Vec<String>, u64) {
    let reply = send(&booking(), "GET", uri, None, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.body);
    let mut ids: Vec<String> = reply.body["entry"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["search"]["mode"] == "match")
        .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    let total = reply.body["total"].as_u64().unwrap();
    assert_eq!(total as usize, ids.len(), "{uri}: total and entries differ");
    (ids, total)
}

async fn ids(uri: &str) -> Vec<String> {
    search(uri).await.0
}

// The numbered cases of the report, with the data it describes.

#[tokio::test]
async fn case_1_schedules_with_one_actor() {
    assert_eq!(
        ids("/Schedule?actor=HealthcareService/h1").await,
        ["S1", "S2"]
    );
}

#[tokio::test]
async fn case_2_schedules_with_a_location_actor() {
    assert_eq!(ids("/Schedule?actor=Location/l1").await, ["S1"]);
}

#[tokio::test]
async fn case_3_a_repeated_actor_is_and_over_the_elements_of_one_schedule() {
    // `actor` repeats: S1 holds both, through two different elements.
    assert_eq!(
        ids("/Schedule?actor=HealthcareService/h1&actor=Location/l1").await,
        ["S1"]
    );
    assert_eq!(
        ids("/Schedule?actor=Location/l1&actor=HealthcareService/h1").await,
        ["S1"]
    );
    // No schedule has both locations.
    assert!(ids("/Schedule?actor=Location/l1&actor=Location/l2")
        .await
        .is_empty());
}

#[tokio::test]
async fn case_4_a_comma_between_actors_is_or() {
    assert_eq!(
        ids("/Schedule?actor=HealthcareService/h1,Location/l1").await,
        ["S1", "S2"]
    );
    assert_eq!(
        ids("/Schedule?actor=Location/l1,Location/l2").await,
        ["S1", "S2"]
    );
    assert_eq!(
        ids("/Schedule?actor=Location/l1,Location/nobody").await,
        ["S1"]
    );
}

#[tokio::test]
async fn case_5_free_slots_of_one_service_at_one_location() {
    let uri = "/Slot?status=free&schedule.actor=HealthcareService/h1&schedule.actor=Location/l1";
    assert_eq!(ids(uri).await, ["sl1", "sl2"]);
    let uri = "/Slot?status=free&schedule.actor=Location/l2&schedule.actor=HealthcareService/h1";
    assert_eq!(ids(uri).await, ["sl3", "sl4"]);
    let uri = "/Slot?status=free&schedule.actor=Location/l1&schedule.actor=Location/l2";
    assert!(
        ids(uri).await.is_empty(),
        "no schedule is at both locations"
    );
}

#[tokio::test]
async fn case_6_a_repeated_identical_parameter_is_the_single_one() {
    let single = ids("/Slot?status=free&schedule.actor=HealthcareService/h1").await;
    assert_eq!(single, ["sl1", "sl2", "sl3", "sl4"]);
    assert_eq!(
        ids("/Slot?status=free&schedule.actor=HealthcareService/h1&schedule.actor=HealthcareService/h1")
            .await,
        single
    );
    // The same for every shape a parameter takes: a token, a reference, a chain, a date, a
    // parameter on an array element, and a list.
    for (once, twice) in [
        ("/Slot?status=free", "/Slot?status=free&status=free"),
        (
            "/Slot?schedule=Schedule/S1",
            "/Slot?schedule=Schedule/S1&schedule=Schedule/S1",
        ),
        (
            "/Slot?start=ge2030-01-01T10:00:00Z",
            "/Slot?start=ge2030-01-01T10:00:00Z&start=ge2030-01-01T10:00:00Z",
        ),
        (
            "/Schedule?actor=Location/l1",
            "/Schedule?actor=Location/l1&actor=Location/l1",
        ),
        (
            "/Appointment?actor=Patient/p1",
            "/Appointment?actor=Patient/p1&actor=Patient/p1",
        ),
        (
            "/Slot?schedule.actor=Location/l1,Location/l2",
            "/Slot?schedule.actor=Location/l1,Location/l2&schedule.actor=Location/l1,Location/l2",
        ),
    ] {
        let single = ids(once).await;
        assert!(!single.is_empty(), "{once} should find something");
        assert_eq!(ids(twice).await, single, "{twice} against {once}");
    }
}

#[tokio::test]
async fn case_7_a_comma_between_chained_actors_is_or() {
    assert_eq!(
        ids("/Slot?schedule.actor=HealthcareService/h1,Location/l1").await,
        ["sl1", "sl2", "sl3", "sl4"]
    );
    assert_eq!(
        ids("/Slot?schedule.actor=Location/l1,Location/l2").await,
        ["sl1", "sl2", "sl3", "sl4"]
    );
    assert_eq!(
        ids("/Slot?schedule.actor=Location/l2,Location/nobody").await,
        ["sl3", "sl4"]
    );
}

#[tokio::test]
async fn case_8_a_repeated_participant_actor_is_and_over_the_participants() {
    assert_eq!(
        ids("/Appointment?actor=Patient/p1&actor=Location/l1").await,
        ["a1"]
    );
    assert_eq!(
        ids("/Appointment?actor=Location/l2&actor=Patient/p1").await,
        ["a2"]
    );
    // The patient is in both appointments; a second occurrence narrows.
    assert_eq!(ids("/Appointment?actor=Patient/p1").await, ["a1", "a2"]);
    assert!(ids("/Appointment?actor=Location/l1&actor=Location/l2")
        .await
        .is_empty());
    assert_eq!(
        ids("/Appointment?actor=Patient/p1,Location/l9").await,
        ["a1", "a2"]
    );
}

#[tokio::test]
async fn and_and_or_combine() {
    // `a=1,2&b=3`: a resource has to have one of the first and the second.
    assert_eq!(
        ids("/Schedule?actor=Location/l1,Location/l2&actor=HealthcareService/h1").await,
        ["S1", "S2"]
    );
    assert_eq!(
        ids("/Schedule?actor=Location/l1,Location/l2&actor=Location/l1").await,
        ["S1"]
    );
    assert_eq!(
        ids("/Schedule?actor=HealthcareService/h1&actor=Location/l2,Location/nobody").await,
        ["S2"]
    );
    // A chain: either location AND the service, then narrowed to one location.
    let uri = "/Slot?status=free&schedule.actor=Location/l1,Location/l2\
               &schedule.actor=HealthcareService/h1";
    assert_eq!(ids(uri).await, ["sl1", "sl2", "sl3", "sl4"]);
    let uri = "/Slot?schedule.actor=Location/l1,Location/l2\
               &schedule.actor=HealthcareService/h1&schedule.actor=Location/l2";
    assert_eq!(ids(uri).await, ["sl3", "sl4"]);
    // An array element two levels down.
    assert_eq!(
        ids("/Appointment?actor=Patient/p1,Patient/p2&actor=Location/l1,Location/l2").await,
        ["a1", "a2"]
    );
    assert_eq!(
        ids("/Appointment?actor=Patient/p1,Patient/p2&actor=Location/l1,Location/l9").await,
        ["a1"]
    );
    // With parameters of other kinds around them.
    assert_eq!(
        ids("/Slot?status=free,busy&schedule=Schedule/S1,Schedule/S2&start=ge2030-01-01T11:00:00Z")
            .await,
        ["sl3", "sl4"]
    );
}

// The same searches as live queries over /sync. A JSON object has one value for a name, so a
// parameter given more than once is an array of its values.

use crate::tests::sync_gaps::{add, serve, Socket};

/// A connection that adds live queries one at a time, each as the next version of its set.
struct Live {
    socket: Socket,
    version: u64,
}

impl Live {
    async fn connect(app: axum::Router) -> Self {
        let addr = serve(app).await;
        let (socket, _) = crate::tests::connect(&format!("ws://{addr}/sync")).await;
        Live { socket, version: 0 }
    }

    /// The ids the query found, sorted, or the error it failed with.
    async fn search(&mut self, path: &str, args: Value) -> Result<Vec<String>, String> {
        add(&mut self.socket, self.version, self.version + 1, path, args).await;
        self.version += 1;
        let reply = crate::tests::next_json(&mut self.socket).await;
        let change = &reply["modifications"][0];
        if change["type"] == "QueryFailed" {
            return Err(change["error"].as_str().unwrap_or_default().to_string());
        }
        assert_eq!(change["type"], "QueryUpdated", "{reply}");
        let mut ids: Vec<String> = change["value"]["entry"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        Ok(ids)
    }

    async fn ids(&mut self, path: &str, args: Value) -> Vec<String> {
        self.search(path, args).await.unwrap()
    }
}

#[tokio::test]
async fn the_numbered_cases_as_live_queries() {
    let mut live = Live::connect(booking()).await;
    // 1 and 2.
    assert_eq!(
        live.ids("Schedule/search", json!({"actor": "HealthcareService/h1"}))
            .await,
        ["S1", "S2"]
    );
    assert_eq!(
        live.ids("Schedule/search", json!({"actor": "Location/l1"}))
            .await,
        ["S1"]
    );
    // 3: an array is a parameter given more than once.
    assert_eq!(
        live.ids(
            "Schedule/search",
            json!({"actor": ["HealthcareService/h1", "Location/l1"]})
        )
        .await,
        ["S1"]
    );
    // 4.
    assert_eq!(
        live.ids(
            "Schedule/search",
            json!({"actor": "HealthcareService/h1,Location/l1"})
        )
        .await,
        ["S1", "S2"]
    );
    // 5.
    assert_eq!(
        live.ids(
            "Slot/search",
            json!({"status": "free",
                "schedule.actor": ["HealthcareService/h1", "Location/l1"]})
        )
        .await,
        ["sl1", "sl2"]
    );
    // 6: the same value twice is the same as once.
    let once = live
        .ids(
            "Slot/search",
            json!({"status": "free", "schedule.actor": "HealthcareService/h1"}),
        )
        .await;
    assert_eq!(once, ["sl1", "sl2", "sl3", "sl4"]);
    assert_eq!(
        live.ids(
            "Slot/search",
            json!({"status": "free",
                "schedule.actor": ["HealthcareService/h1", "HealthcareService/h1"]})
        )
        .await,
        once
    );
    // 7.
    assert_eq!(
        live.ids(
            "Slot/search",
            json!({"schedule.actor": "HealthcareService/h1,Location/l1"})
        )
        .await,
        ["sl1", "sl2", "sl3", "sl4"]
    );
    // 8.
    assert_eq!(
        live.ids(
            "Appointment/search",
            json!({"actor": ["Patient/p1", "Location/l1"]})
        )
        .await,
        ["a1"]
    );
    // AND and OR together, and an array of one.
    assert_eq!(
        live.ids(
            "Schedule/search",
            json!({"actor": ["Location/l1,Location/l2", "Location/l1"]})
        )
        .await,
        ["S1"]
    );
    assert_eq!(
        live.ids("Schedule/search", json!({"actor": ["Location/l2"]}))
            .await,
        ["S2"]
    );
    // What is still not a search argument is refused, and says what it takes.
    for bad in [json!([]), json!(["Location/l1", 3]), json!(true)] {
        let error = live
            .search("Schedule/search", json!({"actor": bad}))
            .await
            .unwrap_err();
        assert!(
            error.contains("search parameter actor must be a string"),
            "{bad}: {error}"
        );
    }
}

// A search that names no _count returns a page, with a next link that leads on.

fn small_app(page: usize, patients: usize) -> axum::Router {
    let (db, catalog, store) = crate::tests::memory();
    {
        let catalog = catalog.read().unwrap();
        for n in 0..patients {
            let patient = json!({"resourceType": "Patient", "id": format!("p{n:03}"),
                "active": true, "name": [{"family": format!("Fam{n:03}")}]});
            import_resource(&db, &catalog, &patient).unwrap();
            let observation = json!({"resourceType": "Observation", "id": format!("o{n:03}"),
                "status": "final", "code": {"coding": [{"code": "x"}]},
                "subject": {"reference": format!("Patient/p{n:03}")}});
            import_resource(&db, &catalog, &observation).unwrap();
        }
    }
    crate::app_with(
        db,
        catalog,
        store,
        crate::ServerConfig {
            default_page_size: page,
            ..Default::default()
        },
    )
}

async fn get(app: &axum::Router, uri: &str) -> Value {
    let reply = send(app, "GET", uri, None, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.body);
    reply.body
}

fn entry_count(bundle: &Value) -> usize {
    bundle["entry"].as_array().map_or(0, Vec::len)
}

fn link(bundle: &Value, relation: &str) -> Option<String> {
    bundle["link"]
        .as_array()?
        .iter()
        .find(|link| link["relation"] == relation)
        .map(|link| link["url"].as_str().unwrap().to_string())
}

#[tokio::test]
async fn a_search_without_a_count_is_a_page_of_a_hundred_with_a_next_link() {
    // The default page of a server that was not set up otherwise.
    assert_eq!(crate::ServerConfig::default().default_page_size, 100);
    let app = small_app(crate::DEFAULT_PAGE_SIZE, 230);
    let first = get(&app, "/Patient").await;
    assert_eq!(entry_count(&first), 100);
    assert_eq!(first["total"], 230, "the total still counts every match");
    assert!(
        link(&first, "self").unwrap().contains("_count=100"),
        "the self link says what applied"
    );
    // Following next reads the rest, without repeating or skipping.
    let mut seen: Vec<String> = Vec::new();
    let mut page = first;
    let mut pages = 0;
    loop {
        seen.extend(
            page["entry"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string()),
        );
        pages += 1;
        let Some(next) = link(&page, "next") else {
            break;
        };
        page = get(&app, &format!("/{next}")).await;
    }
    assert_eq!(pages, 3);
    assert_eq!(seen.len(), 230);
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 230);
    // A count of the client's is used as it is, whatever the default.
    assert_eq!(entry_count(&get(&app, "/Patient?_count=7").await), 7);
    assert_eq!(entry_count(&get(&app, "/Patient?_count=230").await), 230);
    assert_eq!(entry_count(&get(&app, "/Patient?_count=500").await), 230);
    // `_count=0` is the total alone, and `_summary=count` has no entries either way.
    let none = get(&app, "/Patient?_count=0").await;
    assert_eq!((entry_count(&none), &none["total"]), (0, &json!(230)));
    let summary = get(&app, "/Patient?_summary=count").await;
    assert_eq!((entry_count(&summary), &summary["total"]), (0, &json!(230)));
    // The same for a search on a compartment, and with other parameters.
    assert_eq!(entry_count(&get(&app, "/Patient?active=true").await), 100);
    let named = get(&app, "/Patient?name=Fam").await;
    assert_eq!((entry_count(&named), &named["total"]), (100, &json!(230)));
    assert_eq!(entry_count(&get(&app, "/Patient?name=Fam2").await), 30);
    assert_eq!(entry_count(&get(&app, "/Patient?name=Fam22").await), 10);
    let compartment = get(&app, "/Patient/p001/Observation").await;
    assert_eq!(entry_count(&compartment), 1);
}

#[tokio::test]
async fn only_the_searches_a_client_sends_are_paged() {
    let app = small_app(25, 60);
    // A chain looks at every Patient to find the Observations: a page of them would lose the rest.
    let chained = get(&app, "/Observation?subject.name=Fam&_count=100").await;
    assert_eq!(chained["total"], 60);
    assert_eq!(entry_count(&chained), 60);
    let chained = get(&app, "/Observation?subject.name=Fam").await;
    assert_eq!(chained["total"], 60, "the chain read every patient");
    assert_eq!(entry_count(&chained), 25, "and the answer is a page");
    let reverse = get(&app, "/Patient?_has:Observation:subject:code=x").await;
    assert_eq!(reverse["total"], 60);
    // A conditional delete matches every Patient, however many a page holds.
    let refused = send(&app, "DELETE", "/Patient?active=true", None, None).await;
    assert_eq!(
        refused.status,
        StatusCode::PRECONDITION_FAILED,
        "{}",
        refused.body
    );
    // A live query is not paged.
    let found = Live::connect(app.clone())
        .await
        .ids("Patient/search", json!({}))
        .await;
    assert_eq!(found.len(), 60);
    // A Bundle's own searches read every match.
    let batch = send(
        &app,
        "POST",
        "/",
        Some(json!({"resourceType": "Bundle", "type": "batch", "entry": [
            {"request": {"method": "GET", "url": "Patient"}}]})),
        None,
    )
    .await;
    assert_eq!(batch.status, StatusCode::OK, "{}", batch.body);
    assert_eq!(entry_count(&batch.body["entry"][0]["resource"]), 60);
}

#[tokio::test]
async fn the_default_page_can_be_changed_or_turned_off() {
    let small = small_app(25, 60);
    assert_eq!(entry_count(&get(&small, "/Patient").await), 25);
    let everything = small_app(0, 130);
    let all = get(&everything, "/Patient").await;
    assert_eq!(entry_count(&all), 130);
    assert!(link(&all, "next").is_none());
    // The page is the server's to set: a request, or a live query, that names the control gets
    // nothing from it.
    assert_eq!(
        entry_count(&get(&everything, "/Patient?_defaultCount=5").await),
        130
    );
    let found = Live::connect(everything.clone())
        .await
        .ids("Patient/search", json!({"_defaultCount": "5"}))
        .await;
    assert_eq!(found.len(), 130);
}
