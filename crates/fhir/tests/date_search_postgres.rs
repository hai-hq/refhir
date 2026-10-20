//! A date search on Postgres compares moments, as it does on SQLite.
//!
//! The dates of a resource are indexed as text, in the zone each was written in, and a search reads
//! ranges of that index and then compares the moments. The ranges are read by the store, so this
//! is the same thing the unit tests of the crate check, against a database of its own on the server
//! named by `REFHIR_TEST_POSTGRES_URL`. It is skipped when that is not set, and fails when
//! `REFHIR_REQUIRE_POSTGRES` is.

use std::sync::{Arc, RwLock};

use postgres::{Client, NoTls};
use refhir_database::Database;
use refhir_fhir::{import_resource, install_package_dir, r4_dir, run, Catalog};
use refhir_postgres::test_support::with_database;
use refhir_postgres::PostgresPersistence;
use serde_json::{json, Value};

/// One moment, 2026-10-31T22:30:00Z, written in six zones.
const SAME: &[(&str, &str)] = &[
    ("same-z", "2026-10-31T22:30:00Z"),
    ("same-plus01", "2026-10-31T23:30:00+01:00"),
    ("same-minus05", "2026-10-31T17:30:00-05:00"),
    ("same-plus0530", "2026-11-01T04:00:00+05:30"),
    ("same-plus14", "2026-11-01T12:30:00+14:00"),
    ("same-minus12", "2026-10-31T10:30:00-12:00"),
];

/// Moments at the ends of the window 2026-09-30T23:00:00Z up to 2026-10-31T23:00:00Z, and two whose
/// clocks read inside it though they are outside it.
const EDGES: &[(&str, &str)] = &[
    // 2026-09-30T23:00:00Z
    ("edge-open", "2026-10-01T00:00:00+01:00"),
    // 2026-09-30T22:59:59Z
    ("edge-before-open", "2026-10-01T00:59:59+02:00"),
    // 2026-10-31T22:59:59Z
    ("edge-before-close", "2026-11-01T00:59:59+02:00"),
    // 2026-10-31T23:00:00Z
    ("edge-close", "2026-11-01T00:00:00+01:00"),
    // 2026-11-01T03:30:00Z
    ("trap-late", "2026-10-31T22:30:00-05:00"),
    // 2026-09-30T10:30:00Z
    ("trap-early", "2026-10-01T00:30:00+14:00"),
];

/// The same window, 2026-09-30T23:00:00Z up to 2026-10-31T23:00:00Z, written in four zones.
const WINDOWS: &[(&str, &str)] = &[
    ("2026-09-30T23:00:00Z", "2026-10-31T23:00:00Z"),
    ("2026-10-01T00:00:00+01:00", "2026-11-01T00:00:00+01:00"),
    ("2026-09-30T18:00:00-05:00", "2026-10-31T18:00:00-05:00"),
    ("2026-10-01T13:00:00+14:00", "2026-11-01T13:00:00+14:00"),
];

/// A second before the moment, the moment, and a second after, in three zones, and for each prefix
/// whether a stored second is found by it: before, at, after.
const AROUND: &[[&str; 3]] = &[
    [
        "2026-10-31T22:29:59Z",
        "2026-10-31T22:30:00Z",
        "2026-10-31T22:30:01Z",
    ],
    [
        "2026-10-31T23:29:59+01:00",
        "2026-10-31T23:30:00+01:00",
        "2026-10-31T23:30:01+01:00",
    ],
    [
        "2026-11-01T03:59:59+05:30",
        "2026-11-01T04:00:00+05:30",
        "2026-11-01T04:00:01+05:30",
    ],
];

const PREFIXES: &[(&str, [bool; 3])] = &[
    ("eq", [false, true, false]),
    ("ne", [true, false, true]),
    ("gt", [true, false, false]),
    ("lt", [false, false, true]),
    ("ge", [true, true, false]),
    ("le", [false, true, true]),
    ("sa", [true, false, false]),
    ("eb", [false, false, true]),
];

/// Periods of an EpisodeOfCare, whose `date` is its `period`: a start and an end, either of which
/// may be missing.
#[allow(clippy::type_complexity)]
const PERIODS: &[(&str, Option<&str>, Option<&str>)] = &[
    // 2026-10-29T08:00:00Z up to 2026-10-31T12:00:01Z.
    (
        "ep-closed",
        Some("2026-10-29T08:00:00Z"),
        Some("2026-10-31T12:00:00Z"),
    ),
    // From 2026-10-29T08:00:00Z, and not ended.
    ("ep-open-end", Some("2026-10-29T08:00:00Z"), None),
    // Until 2026-10-31T12:00:01Z, with no start.
    ("ep-open-start", None, Some("2026-10-31T12:00:00Z")),
];

fn observation(id: &str, written: &str) -> Value {
    json!({
        "resourceType": "Observation", "id": id, "status": "final", "code": {"text": "visit"},
        "subject": {"reference": "Patient/p1"}, "effectiveDateTime": written
    })
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

#[test]
fn a_date_search_compares_moments_on_postgres() {
    let admin_url = match std::env::var("REFHIR_TEST_POSTGRES_URL") {
        Ok(url) => url,
        Err(_) if std::env::var("REFHIR_REQUIRE_POSTGRES").is_ok() => {
            panic!("REFHIR_REQUIRE_POSTGRES is set but REFHIR_TEST_POSTGRES_URL is not")
        }
        Err(_) => {
            eprintln!("REFHIR_TEST_POSTGRES_URL unset, skipping the Postgres date search test");
            return;
        }
    };
    let database = format!("refhir_date_search_{}", std::process::id());
    let mut admin = Client::connect(&admin_url, NoTls).expect("admin connection");
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {database}"))
        .unwrap();
    admin
        .batch_execute(&format!("CREATE DATABASE {database}"))
        .unwrap();
    let store = PostgresPersistence::connect(&with_database(&admin_url, &database)).unwrap();
    let db = Database::open(Arc::new(store.clone())).unwrap();
    let catalog = RwLock::new(Catalog::empty());
    install_package_dir(&store, &db, &catalog, r4_dir()).unwrap();
    let catalog = catalog.read().unwrap();

    import_resource(
        &db,
        &catalog,
        &json!({"resourceType": "Patient", "id": "p1", "name": [{"family": "Ann"}]}),
    )
    .expect("patient");
    for (id, written) in SAME.iter().chain(EDGES) {
        import_resource(&db, &catalog, &observation(id, written)).expect(id);
    }
    for (id, start, end) in PERIODS {
        let mut period = serde_json::Map::new();
        for (name, value) in [("start", start), ("end", end)] {
            if let Some(value) = value {
                period.insert(name.to_string(), json!(value));
            }
        }
        import_resource(
            &db,
            &catalog,
            &json!({"resourceType": "EpisodeOfCare", "id": id, "status": "active",
                "patient": {"reference": "Patient/p1"}, "period": period}),
        )
        .expect(id);
    }
    let search_type = |type_name: &str, args: Value| -> Vec<String> {
        let bundle = run(&db, &catalog, &format!("{type_name}/search"), &args)
            .expect("search")
            .value;
        bundle["entry"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string())
            .collect()
    };
    let search = |args: Value| search_type("Observation", args);
    let same: Vec<String> = sorted(SAME.iter().map(|(id, _)| id.to_string()).collect());

    // The window finds the same moments, whichever zone it and they are written in: the six writings
    // of 22:30Z, the edge at the opening, and the second before the close; and none of the three
    // whose clock says one thing and whose moment another.
    let mut inside = same.clone();
    inside.extend(["edge-open", "edge-before-close"].map(String::from));
    let inside = sorted(inside);
    for (open, close) in WINDOWS {
        let found = search(json!({"date": format!("ge{open}\u{1f}lt{close}")}));
        assert_eq!(sorted(found), inside, "ge{open} lt{close}");
    }

    // Every prefix answers alike for the moment in any zone.
    for (prefix, answers) in PREFIXES {
        for zone in AROUND {
            for (value, found_it) in zone.iter().zip(answers) {
                let found: Vec<String> = search(json!({"date": format!("{prefix}{value}")}))
                    .into_iter()
                    .filter(|id| id.starts_with("same-"))
                    .collect();
                let expected = if *found_it { same.clone() } else { Vec::new() };
                assert_eq!(sorted(found), expected, "{prefix}{value}");
            }
        }
    }

    // A period is the range between its ends, and one with no end, or no start, has not ended or
    // has no beginning: found far from the dates it holds, by being open.
    let periods = |query: &[&str]| -> Vec<String> {
        sorted(search_type(
            "EpisodeOfCare",
            json!({"date": query.join("\u{1f}")}),
        ))
    };
    assert_eq!(periods(&["lt2020-01-01"]), ["ep-open-start"]);
    assert_eq!(periods(&["gt2030-01-01"]), ["ep-open-end"]);
    assert_eq!(periods(&["eq2026-10"]), ["ep-closed"]);
    assert_eq!(periods(&["ne2026-10"]), ["ep-open-end", "ep-open-start"]);
    assert_eq!(
        periods(&["ge2026-11-02T00:00:00Z", "lt2026-11-03T00:00:00Z"]),
        ["ep-open-end"]
    );
    assert_eq!(
        periods(&["ge2026-10-30T00:00:00Z", "lt2026-10-31T00:00:00Z"]),
        ["ep-closed", "ep-open-end", "ep-open-start"]
    );

    // Newest first, by the moment and not by the clock in the text.
    let newest_first = search(json!({"_sort": "-date", "_count": "50"}));
    assert_eq!(
        &newest_first[..3],
        ["trap-late", "edge-close", "edge-before-close"],
        "{newest_first:?}"
    );
    assert_eq!(
        &newest_first[newest_first.len() - 3..],
        ["edge-open", "edge-before-open", "trap-early"],
        "{newest_first:?}"
    );
}
