use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use refhir_database::{Database, Put};
use refhir_persistence::ConformanceStore;
use refhir_sqlite::SqlitePersistence;
use refhir_types::{exact_key, token_interval, IndexId, Order, TabletId};
use serde_json::{json, Value};

use refhir_sdc::Chart;
use refhir_validate::{TermAnswer, Terminology};

use crate::catalog::{Catalog, ReferencePolicy};
use crate::conform::{commit_operation, repair_conformance};
use crate::error::FhirError;
use crate::extract::ReferenceId;
use crate::npm::{install_package, install_with, PackageFetcher};
use crate::package::{
    cards_dir, cross_version_dir, expression_clash_dir, extension_copies_dir, extra_dir,
    finish_install, install_package_dir, ips_package, minir4_dir, newer_terminology_dir, publish,
    r4_dir, r4_examples_dir, run_backfills, sdc_dir, stage_package_deferred, stage_package_dir,
    version_clash_dir,
};
use crate::run::{run, RunError};

struct Harness {
    store: SqlitePersistence,
    db: Database,
    catalog: std::sync::RwLock<Catalog>,
}

fn empty() -> Harness {
    let store = SqlitePersistence::open_in_memory().unwrap();
    let db = Database::open(Arc::new(store.clone())).unwrap();
    Harness {
        store,
        db,
        catalog: std::sync::RwLock::new(Catalog::empty()),
    }
}

fn ready() -> Harness {
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, minir4_dir()).unwrap();
    harness
}

/// Run `path` and expect it to succeed.
///
/// Fixtures need stable ids, but the create interaction ignores a client's id (FHIR R4), so a
/// create that names one is seeded under it. Tests of the interaction itself use `run_op`.
fn apply(harness: &Harness, path: &str, args: Value) -> Value {
    let catalog = harness.catalog.read().expect("catalog");
    if path.ends_with("/create") && args.pointer("/resource/id").is_some() {
        return crate::run::import_resource(&harness.db, &catalog, &args["resource"])
            .expect(path)
            .value;
    }
    run(&harness.db, &catalog, path, &args).expect(path).value
}

fn run_op(harness: &Harness, path: &str, args: &Value) -> Result<crate::run::RunResult, RunError> {
    let catalog = harness.catalog.read().expect("catalog");
    run(&harness.db, &catalog, path, args)
}

/// The OperationOutcome of a request the server refused as HTTP 422.
fn unprocessable<T: std::fmt::Debug>(result: Result<T, RunError>) -> Value {
    match result {
        Err(RunError::Fhir(FhirError::Unprocessable(outcome))) => outcome,
        other => panic!("expected an unprocessable request, got {other:?}"),
    }
}

fn patient(id: &str, active: bool, family: &str) -> Value {
    json!({
        "resourceType": "Patient",
        "id": id,
        "active": active,
        "name": [{"family": family, "given": ["Ann"]}],
        "identifier": [{"system": "http://hospital.example", "value": format!("mrn-{id}")}]
    })
}

#[test]
fn unknown_resource_is_rejected() {
    let harness = empty();
    let patient = run_op(&harness, "Patient/read", &json!({"id": "x"})).unwrap_err();
    assert!(matches!(
        patient,
        RunError::Fhir(FhirError::UnknownResource(_))
    ));
    let encounter = run_op(&harness, "Encounter/read", &json!({"id": "x"})).unwrap_err();
    assert!(matches!(
        encounter,
        RunError::Fhir(FhirError::UnknownResource(_))
    ));
}

#[test]
fn patient_crud_history_and_search() {
    let db = ready();
    let created = apply(
        &db,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    );
    assert_eq!(created["meta"]["versionId"], "1");
    assert_eq!(created["id"], "p1");

    let by_id = db
        .db
        .reader()
        .index_scan(
            &IndexId::new("Patient/by_id"),
            db.db.latest_ts(),
            &refhir_types::Interval::all(),
            Order::Asc,
        )
        .unwrap();
    assert_eq!(by_id.len(), 1);

    let read = apply(&db, "Patient/read", json!({"id": "p1"}));
    assert_eq!(read["meta"]["versionId"], "1");

    let missing = run_op(&db, "Patient/read", &json!({"id": "nope"})).unwrap_err();
    assert!(matches!(missing, RunError::Fhir(FhirError::NotFound(_))));

    let mut updated_body = patient("p1", true, "Ann");
    updated_body["birthDate"] = json!("1990-01-01");
    updated_body["meta"] = json!({"versionId": "9"});
    let wrong = run_op(
        &db,
        "Patient/update",
        &json!({"id": "p1", "resource": updated_body}),
    )
    .unwrap_err();
    assert!(matches!(wrong, RunError::Fhir(FhirError::Conflict(_))));

    let mut updated_body = patient("p1", true, "Ann");
    updated_body["birthDate"] = json!("1990-01-01");
    let updated = apply(
        &db,
        "Patient/update",
        json!({"id": "p1", "resource": updated_body, "ifMatch": "W/\"1\""}),
    );
    assert_eq!(updated["meta"]["versionId"], "2");
    assert_eq!(updated["birthDate"], "1990-01-01");

    let found = apply(&db, "Patient/search", json!({"name": "an"}));
    assert_eq!(found["total"], 1);
    assert_eq!(found["entry"][0]["resource"]["id"], "p1");
    let by_mrn = apply(
        &db,
        "Patient/search",
        json!({"identifier": "http://hospital.example|mrn-p1"}),
    );
    assert_eq!(by_mrn["total"], 1);
    let by_active = apply(
        &db,
        "Patient/search",
        json!({"active": "true", "_id": "p1"}),
    );
    assert_eq!(by_active["total"], 1);
    let none = apply(&db, "Patient/search", json!({"name": "zzz"}));
    assert_eq!(none["total"], 0);

    let history = apply(&db, "Patient/history", json!({"id": "p1"}));
    assert_eq!(history["total"], 2);
    assert_eq!(history["entry"][0]["request"]["method"], "PUT");
    assert_eq!(history["entry"][1]["request"]["method"], "POST");

    let old = apply(&db, "Patient/vread", json!({"id": "p1", "versionId": "1"}));
    assert!(old.get("birthDate").is_none());

    apply(&db, "Patient/delete", json!({"id": "p1"}));
    let after = run_op(&db, "Patient/read", &json!({"id": "p1"})).unwrap_err();
    assert!(matches!(after, RunError::Fhir(FhirError::NotFound(_))));
    let history = apply(&db, "Patient/history", json!({"id": "p1"}));
    assert_eq!(history["entry"][0]["request"]["method"], "DELETE");
    assert!(history["entry"][0].get("resource").is_none());
    let gone = run_op(&db, "Patient/vread", &json!({"id": "p1", "versionId": "3"})).unwrap_err();
    assert!(matches!(gone, RunError::Fhir(FhirError::Gone(_))));
    let still = apply(&db, "Patient/vread", json!({"id": "p1", "versionId": "1"}));
    assert_eq!(still["meta"]["versionId"], "1");

    let hidden = db
        .db
        .reader()
        .index_scan(
            &IndexId::new("Patient/by_id"),
            db.db.latest_ts(),
            &refhir_types::Interval::all(),
            Order::Asc,
        )
        .unwrap();
    assert!(hidden.is_empty());
}

#[test]
fn search_conflicts_with_a_matching_update() {
    let db = ready();
    apply(
        &db,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    );
    apply(
        &db,
        "Patient/create",
        json!({"resource": patient("p2", false, "Bea")}),
    );

    let mut reader = db.db.begin();
    let catalog = db.catalog.read().expect("catalog");
    crate::dispatch(
        &mut reader,
        &catalog,
        "Patient/search",
        &json!({"active": "true"}),
    )
    .unwrap();
    let mut p2 = patient("p2", false, "Bea");
    p2["birthDate"] = json!("1980-01-01");
    crate::dispatch(
        &mut reader,
        &catalog,
        "Patient/update",
        &json!({"id": "p2", "resource": p2}),
    )
    .unwrap();

    let mut writer = db.db.begin();
    let mut p1 = patient("p1", true, "Ann");
    p1["birthDate"] = json!("1970-01-01");
    crate::dispatch(
        &mut writer,
        &catalog,
        "Patient/update",
        &json!({"id": "p1", "resource": p1}),
    )
    .unwrap();
    db.db.commit_with(writer, crate::stamp_stored_json).unwrap();
    let err = db
        .db
        .commit_with(reader, crate::stamp_stored_json)
        .unwrap_err();
    assert!(err.is_occ(), "{err}");

    let _ = token_interval("true");
}

#[test]
fn observation_requires_a_patient_and_is_searchable() {
    let db = ready();
    let missing = run_op(
        &db,
        "Observation/create",
        &json!({"resource": {
            "resourceType": "Observation",
            "subject": {"reference": "Patient/p1"}
        }}),
    )
    .unwrap_err();
    assert!(matches!(missing, RunError::Fhir(FhirError::Invalid(_))));

    apply(
        &db,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    );
    let observation = apply(
        &db,
        "Observation/create",
        json!({"resource": {
            "resourceType": "Observation",
            "id": "o1",
            "status": "final",
            "subject": {"reference": "Patient/p1"},
            "code": {"coding": [{"system": "http://loinc.org", "code": "8480-6"}]},
            "effectiveDateTime": "2020-01-02T00:00:00Z"
        }}),
    );
    assert_eq!(observation["id"], "o1");

    let by_subject = apply(&db, "Observation/search", json!({"subject": "Patient/p1"}));
    assert_eq!(by_subject["total"], 1);
    let by_code = apply(
        &db,
        "Observation/search",
        json!({"code": "http://loinc.org|8480-6"}),
    );
    assert_eq!(by_code["total"], 1);
    let by_date = apply(&db, "Observation/search", json!({"date": "2020-01"}));
    assert_eq!(by_date["total"], 1);
    let absolute = run_op(
        &db,
        "Observation/create",
        &json!({"resource": {
            "resourceType": "Observation",
            "subject": {"reference": "http://example.org/Patient/p1"}
        }}),
    )
    .unwrap_err();
    assert!(matches!(absolute, RunError::Fhir(FhirError::Invalid(_))));
}

#[test]
fn transaction_bundle_is_one_commit() {
    let db = ready();
    let patient_url = "urn:uuid:11111111-1111-4111-8111-111111111111";
    let bundle = json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": [
            {
                "fullUrl": patient_url,
                "request": {"method": "POST", "url": "Patient"},
                // The id a client puts on a resource it creates is not the id it gets.
                "resource": patient("chosen", true, "Ann")
            },
            {
                "request": {"method": "POST", "url": "Observation"},
                "resource": {
                    "resourceType": "Observation",
                    "subject": {"reference": patient_url},
                    "effectiveDateTime": "2020-05-01"
                }
            }
        ]
    });
    let response = apply(&db, "Bundle/transaction", json!({"resource": bundle}));
    assert_eq!(response["type"], "transaction-response");
    assert_eq!(response["entry"][0]["response"]["status"], "201 Created");
    assert!(response["entry"][0]["response"]["location"]
        .as_str()
        .unwrap()
        .contains("/_history/"));
    let id = response["entry"][0]["resource"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(id, "chosen");
    assert_eq!(
        response["entry"][1]["resource"]["subject"]["reference"],
        format!("Patient/{id}")
    );
    assert_eq!(
        response["entry"][0]["resource"]["meta"]["versionId"],
        response["entry"][1]["resource"]["meta"]["versionId"]
    );
    let stored = apply(&db, "Patient/read", json!({"id": id}));
    assert_eq!(stored["name"][0]["family"], "Ann");
    let chosen = run_op(&db, "Patient/read", &json!({"id": "chosen"})).unwrap_err();
    assert!(matches!(chosen, RunError::Fhir(FhirError::NotFound(_))));

    // One failing entry undoes the others.
    let failed = json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": [
            {
                "request": {"method": "POST", "url": "Patient"},
                "resource": patient("p2", true, "Bea")
            },
            {
                "request": {"method": "POST", "url": "Observation"},
                "resource": {
                    "resourceType": "Observation",
                    "subject": {"reference": "Patient/missing"}
                }
            }
        ]
    });
    let err = run_op(&db, "Bundle/transaction", &json!({"resource": failed})).unwrap_err();
    assert!(matches!(err, RunError::Fhir(FhirError::Invalid(_))));
    assert_eq!(patient_count(&db), 1);

    let mut invalid = patient("bad", true, "Cam");
    invalid["birthDate"] = json!("yesterday");
    let failed = json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": [
            {
                "request": {"method": "POST", "url": "Patient"},
                "resource": patient("p3", true, "Cam")
            },
            {
                "request": {"method": "POST", "url": "Patient"},
                "resource": invalid
            }
        ]
    });
    let err = run_op(&db, "Bundle/transaction", &json!({"resource": failed})).unwrap_err();
    let RunError::Fhir(FhirError::Outcome(outcome)) = err else {
        panic!("expected a validation outcome, got {err}");
    };
    let diagnostics = outcome["issue"][0]["diagnostics"].as_str().unwrap();
    assert!(diagnostics.starts_with("entry 1:"), "{diagnostics}");
    assert_eq!(patient_count(&db), 1);

    // A POST names a type. There is no id in it to choose.
    let named = json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": [{
            "request": {"method": "POST", "url": "Patient/p4"},
            "resource": patient("p4", true, "Dee")
        }]
    });
    let err = run_op(&db, "Bundle/transaction", &json!({"resource": named})).unwrap_err();
    assert!(
        matches!(err, RunError::Fhir(FhirError::Invalid(_))),
        "{err}"
    );
}

fn patient_count(harness: &Harness) -> u64 {
    apply(harness, "Patient/search", json!({}))["total"]
        .as_u64()
        .unwrap()
}

#[test]
fn transaction_creates_a_referenced_patient_before_the_observation() {
    let db = ready();
    let patient_url = "urn:uuid:22222222-2222-4222-8222-222222222222";
    let bundle = json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": [
            {
                "request": {"method": "POST", "url": "Observation"},
                "resource": {
                    "resourceType": "Observation",
                    "subject": {"reference": patient_url},
                    "effectiveDateTime": "2020-05-01"
                }
            },
            {
                "fullUrl": patient_url,
                "request": {"method": "POST", "url": "Patient"},
                "resource": patient("p9", true, "Ann")
            }
        ]
    });
    // The Observation comes first but names a Patient that does not exist until the second
    // entry is applied, so the Patient is applied first.
    let response = apply(&db, "Bundle/transaction", json!({"resource": bundle}));
    assert_eq!(response["entry"][0]["response"]["status"], "201 Created");
    assert_eq!(response["entry"][1]["response"]["status"], "201 Created");
    let patient_id = response["entry"][1]["resource"]["id"].as_str().unwrap();
    assert_eq!(
        response["entry"][0]["resource"]["subject"]["reference"],
        format!("Patient/{patient_id}")
    );
    assert_eq!(
        response["entry"][0]["resource"]["meta"]["versionId"],
        response["entry"][1]["resource"]["meta"]["versionId"]
    );
    let observation_id = response["entry"][0]["resource"]["id"].as_str().unwrap();
    let stored = apply(&db, "Observation/read", json!({"id": observation_id}));
    assert_eq!(
        stored["subject"]["reference"],
        format!("Patient/{patient_id}")
    );

    // Entries that name each other by the id they chose get no such help: the id is ignored, so
    // the reference points at nothing.
    let chosen = json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": [
            {"request": {"method": "POST", "url": "Patient"}, "resource": patient("p10", true, "Bea")},
            {"request": {"method": "POST", "url": "Observation"}, "resource": {
                "resourceType": "Observation",
                "subject": {"reference": "Patient/p10"}
            }}
        ]
    });
    let err = run_op(&db, "Bundle/transaction", &json!({"resource": chosen})).unwrap_err();
    assert!(
        err.to_string().contains("Patient/p10 does not exist"),
        "{err}"
    );
}

#[test]
fn capability_lists_shipped_resources_only() {
    let db = ready();
    let statement = db.catalog.read().expect("catalog").capability_statement();
    let resources = statement["rest"][0]["resource"].as_array().unwrap();
    let mut types: Vec<_> = resources
        .iter()
        .map(|resource| resource["type"].as_str().unwrap())
        .collect();
    types.sort();
    assert_eq!(types, ["Observation", "Patient"]);
    assert!(statement["rest"][0]["interaction"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["code"] == "transaction"));
}

#[test]
fn second_package_backfills_birthdate_without_a_new_version() {
    let db = ready();
    let mut body = patient("p1", true, "Ann");
    body["birthDate"] = json!("1990-01-01");
    let created = apply(&db, "Patient/create", json!({"resource": body}));
    let version = created["meta"]["versionId"].clone();

    stage_package_dir(&db.store, &db.db, &db.catalog, extra_dir()).unwrap();
    let pending = run_op(&db, "Patient/search", &json!({"birthdate": "1990-01-01"})).unwrap_err();
    assert!(matches!(pending, RunError::Fhir(FhirError::Invalid(_))));

    run_backfills(&db.store, &db.db, &db.catalog).unwrap();
    let found = apply(&db, "Patient/search", json!({"birthdate": "1990-01-01"}));
    assert_eq!(found["total"], 1);
    assert_eq!(found["entry"][0]["resource"]["id"], "p1");
    let read = apply(&db, "Patient/read", json!({"id": "p1"}));
    assert_eq!(read["meta"]["versionId"], version);

    let encounter = apply(
        &db,
        "Encounter/create",
        json!({"resource": {
            "resourceType": "Encounter",
            "id": "e1",
            "status": "finished"
        }}),
    );
    assert_eq!(encounter["id"], "e1");

    let statement = db.catalog.read().expect("catalog").capability_statement();
    let patient_params = statement["rest"][0]["resource"]
        .as_array()
        .unwrap()
        .iter()
        .find(|resource| resource["type"] == "Patient")
        .unwrap();
    let names: Vec<_> = patient_params["searchParam"]
        .as_array()
        .unwrap()
        .iter()
        .map(|param| param["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"birthdate"));
    assert!(names.contains(&"phonetic"));
}

struct MapFetcher {
    packages: BTreeMap<(String, String), PathBuf>,
}

impl PackageFetcher for MapFetcher {
    fn fetch(&self, name: &str, version: &str, dest: &Path) -> Result<(), FhirError> {
        let src = self
            .packages
            .get(&(name.to_string(), version.to_string()))
            .ok_or_else(|| {
                FhirError::NotFound(format!("{name}@{version} is not in the test registry"))
            })?;
        copy_tree(src, dest)
    }
}

fn copy_tree(src: &Path, dest: &Path) -> Result<(), FhirError> {
    std::fs::create_dir_all(dest).map_err(|err| FhirError::Storage(err.to_string()))?;
    for entry in std::fs::read_dir(src).map_err(|err| FhirError::Storage(err.to_string()))? {
        let entry = entry.map_err(|err| FhirError::Storage(err.to_string()))?;
        let to = dest.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), to).map_err(|err| FhirError::Storage(err.to_string()))?;
        }
    }
    Ok(())
}

fn temp_dir(tag: &str) -> PathBuf {
    // Tests run in parallel and several use the same tag (`dep-example.lib-2.0.0`). A clock
    // reading is not enough to tell them apart: two tests can read the same value, and on a
    // virtual machine a clock can even repeat or step back, which gave two tests one directory.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "refhir-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn write_tgz(src: &Path, dest: &Path) {
    let file = std::fs::File::create(dest).unwrap();
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    archive.append_dir_all("package", src).unwrap();
    archive.finish().unwrap();
}

#[test]
fn install_package_accepts_a_directory_and_a_tarball() {
    let db = ready();
    install_package(
        &db.store,
        &db.db,
        &db.catalog,
        extra_dir().to_str().unwrap(),
    )
    .unwrap();
    assert!(db.catalog.read().expect("catalog").contains("Encounter"));

    let db = ready();
    let scratch = temp_dir("tgz");
    let tarball = scratch.join("extra.tgz");
    write_tgz(extra_dir(), &tarball);
    install_package(&db.store, &db.db, &db.catalog, tarball.to_str().unwrap()).unwrap();
    assert!(db.catalog.read().expect("catalog").contains("Encounter"));
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn npm_install_fetches_dependencies_then_the_package() {
    let db = ready();
    let depends = temp_dir("depends");
    std::fs::write(
        depends.join("package.json"),
        r#"{"name":"example.depends","version":"1.0.0","fhirVersion":"4.0.1","dependencies":{"example.extra":"1.0.0"}}"#,
    )
    .unwrap();
    let cache = temp_dir("pkg-cache");
    let fetcher = MapFetcher {
        packages: BTreeMap::from([(
            ("example.extra".into(), "1.0.0".into()),
            extra_dir().to_path_buf(),
        )]),
    };
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        depends.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap();
    assert!(db.catalog.read().expect("catalog").contains("Encounter"));
    let _ = std::fs::remove_dir_all(&depends);
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn implementation_guide_install_rejects_paths_and_registry() {
    let db = ready();
    let path = commit_operation(
        &db.store,
        &db.db,
        &db.catalog,
        "ImplementationGuide/$install",
        &json!({"package": extra_dir().to_str().unwrap()}),
    )
    .unwrap_err();
    assert!(matches!(path, RunError::Fhir(FhirError::Invalid(_))));
    let registry = commit_operation(
        &db.store,
        &db.db,
        &db.catalog,
        "ImplementationGuide/$install",
        &json!({
            "name": "hl7.fhir.us.core",
            "version": "6.1.0",
            "registry": "http://127.0.0.1"
        }),
    )
    .unwrap_err();
    assert!(matches!(registry, RunError::Fhir(FhirError::Invalid(_))));
    let missing = commit_operation(
        &db.store,
        &db.db,
        &db.catalog,
        "ImplementationGuide/$install",
        &json!({"name": "hl7.fhir.us.core"}),
    )
    .unwrap_err();
    assert!(matches!(missing, RunError::Fhir(FhirError::Invalid(_))));
}

#[test]
fn npm_install_rejects_dependency_cycles_and_version_ranges() {
    let db = ready();
    let cycle_a = temp_dir("cycle-a");
    let cycle_b = temp_dir("cycle-b");
    std::fs::write(
        cycle_a.join("package.json"),
        r#"{"name":"example.cycle.a","version":"1.0.0","fhirVersion":"4.0.1","dependencies":{"example.cycle.b":"1.0.0"}}"#,
    )
    .unwrap();
    std::fs::write(
        cycle_b.join("package.json"),
        r#"{"name":"example.cycle.b","version":"1.0.0","fhirVersion":"4.0.1","dependencies":{"example.cycle.a":"1.0.0"}}"#,
    )
    .unwrap();
    let cache = temp_dir("cycle-cache");
    let fetcher = MapFetcher {
        packages: BTreeMap::from([
            (("example.cycle.a".into(), "1.0.0".into()), cycle_a.clone()),
            (("example.cycle.b".into(), "1.0.0".into()), cycle_b.clone()),
        ]),
    };
    let err = install_with(
        &db.store,
        &db.db,
        &db.catalog,
        cycle_a.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap_err();
    assert!(matches!(err, FhirError::Invalid(_)), "{err}");

    let ranged = temp_dir("ranged");
    std::fs::write(
        ranged.join("package.json"),
        r#"{"name":"example.ranged","version":"1.0.0","fhirVersion":"4.0.1","dependencies":{"example.extra":"^1.0.0"}}"#,
    )
    .unwrap();
    let ranged_err = install_with(
        &db.store,
        &db.db,
        &db.catalog,
        ranged.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap_err();
    assert!(matches!(ranged_err, FhirError::Invalid(_)));
    let _ = std::fs::remove_dir_all(&cycle_a);
    let _ = std::fs::remove_dir_all(&cycle_b);
    let _ = std::fs::remove_dir_all(&cache);
    let _ = std::fs::remove_dir_all(&ranged);
}

fn dep_package(name: &str, version: &str, deps: &[(&str, &str)]) -> PathBuf {
    let dir = temp_dir(&format!("dep-{name}-{version}"));
    let deps: Vec<String> = deps
        .iter()
        .map(|(n, v)| format!(r#""{n}":"{v}""#))
        .collect();
    std::fs::write(
        dir.join("package.json"),
        format!(
            r#"{{"name":"{name}","version":"{version}","fhirVersion":"4.0.1","dependencies":{{{}}}}}"#,
            deps.join(",")
        ),
    )
    .unwrap();
    dir
}

fn installed_packages(db: &Harness) -> Vec<String> {
    let mut rows: Vec<String> = db
        .store
        .load_packages()
        .unwrap()
        .into_iter()
        .map(|package| format!("{}@{}", package.name, package.version))
        .collect();
    rows.sort();
    rows
}

fn registry(packages: &[(&str, &str, &PathBuf)]) -> MapFetcher {
    MapFetcher {
        packages: packages
            .iter()
            .map(|(name, version, dir)| ((name.to_string(), version.to_string()), (*dir).clone()))
            .collect(),
    }
}

#[test]
fn dependency_versions_resolve_to_the_highest_one_installed_once() {
    // app needs lib 1.0.0 and mid; mid needs lib 2.0.0 (like sdc, tools and extensions).
    let app = dep_package(
        "example.app",
        "1.0.0",
        &[("example.lib", "1.0.0"), ("example.mid", "1.0.0")],
    );
    let mid = dep_package("example.mid", "1.0.0", &[("example.lib", "2.0.0")]);
    let lib1 = dep_package("example.lib", "1.0.0", &[]);
    let lib2 = dep_package("example.lib", "2.0.0", &[]);
    let fetcher = registry(&[
        ("example.mid", "1.0.0", &mid),
        ("example.lib", "1.0.0", &lib1),
        ("example.lib", "2.0.0", &lib2),
    ]);
    let db = empty();
    let cache = temp_dir("resolve-cache");
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        app.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap();
    assert_eq!(
        installed_packages(&db),
        [
            "example.app@1.0.0",
            "example.lib@2.0.0",
            "example.mid@1.0.0"
        ]
    );
    for dir in [app, mid, lib1, lib2, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// A registry that counts what is fetched from it, and how many fetches are under way at once.
struct CountingFetcher {
    inner: MapFetcher,
    fetched: std::sync::Mutex<BTreeMap<(String, String), usize>>,
    running: std::sync::atomic::AtomicUsize,
    most_at_once: std::sync::atomic::AtomicUsize,
}

impl PackageFetcher for CountingFetcher {
    fn fetch(&self, name: &str, version: &str, dest: &Path) -> Result<(), FhirError> {
        use std::sync::atomic::Ordering::SeqCst;
        *self
            .fetched
            .lock()
            .unwrap()
            .entry((name.to_string(), version.to_string()))
            .or_default() += 1;
        let now = self.running.fetch_add(1, SeqCst) + 1;
        self.most_at_once.fetch_max(now, SeqCst);
        // Long enough that fetches that are meant to overlap do.
        std::thread::sleep(std::time::Duration::from_millis(150));
        let result = self.inner.fetch(name, version, dest);
        self.running.fetch_sub(1, SeqCst);
        result
    }
}

#[test]
fn packages_that_do_not_need_each_other_are_fetched_side_by_side_and_once() {
    let names = [
        "example.a",
        "example.b",
        "example.c",
        "example.d",
        "example.e",
    ];
    let deps: Vec<(&str, &str)> = names.iter().map(|name| (*name, "1.0.0")).collect();
    let app = dep_package("example.app", "1.0.0", &deps);
    // The last one is needed by two of the others: still fetched once.
    let libs: Vec<PathBuf> = names
        .iter()
        .map(|name| {
            let needs: &[(&str, &str)] = if *name == "example.e" {
                &[]
            } else {
                &[("example.shared", "1.0.0")]
            };
            dep_package(name, "1.0.0", needs)
        })
        .collect();
    let shared = dep_package("example.shared", "1.0.0", &[]);
    let mut listed: Vec<(&str, &str, &PathBuf)> = names
        .iter()
        .zip(&libs)
        .map(|(name, dir)| (*name, "1.0.0", dir))
        .collect();
    listed.push(("example.shared", "1.0.0", &shared));
    let fetcher = CountingFetcher {
        inner: registry(&listed),
        fetched: Default::default(),
        running: Default::default(),
        most_at_once: Default::default(),
    };
    let db = empty();
    let cache = temp_dir("side-by-side-cache");
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        app.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap();
    let mut expected: Vec<String> = names.iter().map(|name| format!("{name}@1.0.0")).collect();
    expected.push("example.app@1.0.0".into());
    expected.push("example.shared@1.0.0".into());
    expected.sort();
    assert_eq!(installed_packages(&db), expected);
    let fetched = fetcher.fetched.lock().unwrap();
    assert_eq!(fetched.len(), 6, "{fetched:?}");
    assert!(fetched.values().all(|count| *count == 1), "{fetched:?}");
    assert!(
        fetcher
            .most_at_once
            .load(std::sync::atomic::Ordering::SeqCst)
            > 1,
        "the five that app needs were fetched one after another"
    );
    drop(fetched);
    for dir in libs.into_iter().chain([app, shared, cache]) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn a_package_that_cannot_be_fetched_stops_the_install_before_anything_is_stored() {
    let app = dep_package(
        "example.app",
        "1.0.0",
        &[("example.a", "1.0.0"), ("example.missing", "1.0.0")],
    );
    let a = dep_package("example.a", "1.0.0", &[]);
    let fetcher = registry(&[("example.a", "1.0.0", &a)]);
    let db = empty();
    let cache = temp_dir("missing-sibling-cache");
    let err = install_with(
        &db.store,
        &db.db,
        &db.catalog,
        app.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap_err();
    assert!(
        matches!(&err, FhirError::NotFound(message) if message.contains("example.missing")),
        "{err:?}"
    );
    assert!(installed_packages(&db).is_empty());
    for dir in [app, a, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

const EXAMPLES: &str = "hl7.fhir.r4.examples";

fn counting(packages: &[(&str, &str, &PathBuf)]) -> CountingFetcher {
    CountingFetcher {
        inner: registry(packages),
        fetched: Default::default(),
        running: Default::default(),
        most_at_once: Default::default(),
    }
}

#[test]
fn the_r4_examples_package_is_not_installed_for_a_package_that_depends_on_it() {
    // app needs lib, and mid, which needs the examples package (as sdc does, through its own
    // dependencies in a deeper tree). The examples package needs a package of its own.
    let app = dep_package(
        "example.app",
        "1.0.0",
        &[("example.lib", "1.0.0"), ("example.mid", "1.0.0")],
    );
    let mid = dep_package("example.mid", "1.0.0", &[(EXAMPLES, "4.0.1")]);
    let lib = dep_package("example.lib", "1.0.0", &[(EXAMPLES, "4.0.1")]);
    let examples = dep_package(EXAMPLES, "4.0.1", &[("example.only-for-examples", "1.0.0")]);
    let behind = dep_package("example.only-for-examples", "1.0.0", &[]);
    let fetcher = counting(&[
        ("example.mid", "1.0.0", &mid),
        ("example.lib", "1.0.0", &lib),
        (EXAMPLES, "4.0.1", &examples),
        ("example.only-for-examples", "1.0.0", &behind),
    ]);
    let db = empty();
    let cache = temp_dir("skip-examples-cache");
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        app.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap();
    assert_eq!(
        installed_packages(&db),
        [
            "example.app@1.0.0",
            "example.lib@1.0.0",
            "example.mid@1.0.0"
        ]
    );
    // Neither it nor what only it needs was downloaded or unpacked.
    let fetched: Vec<String> = fetcher
        .fetched
        .lock()
        .unwrap()
        .keys()
        .map(|(name, _)| name.clone())
        .collect();
    assert_eq!(fetched, ["example.lib", "example.mid"]);
    assert!(!cache.join(EXAMPLES).exists());
    for dir in [app, mid, lib, examples, behind, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn the_r4_examples_package_is_installed_when_it_is_asked_for() {
    let examples = dep_package(EXAMPLES, "4.0.1", &[("example.lib", "1.0.0")]);
    let lib = dep_package("example.lib", "1.0.0", &[]);
    let fetcher = counting(&[
        (EXAMPLES, "4.0.1", &examples),
        ("example.lib", "1.0.0", &lib),
    ]);
    let db = empty();
    let cache = temp_dir("ask-examples-cache");
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        &format!("{EXAMPLES}@4.0.1"),
        &fetcher,
        &cache,
    )
    .unwrap();
    // As any package: itself, and what it depends on.
    assert_eq!(
        installed_packages(&db),
        ["example.lib@1.0.0".to_string(), format!("{EXAMPLES}@4.0.1")]
    );
    for dir in [examples, lib, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn a_copy_of_the_r4_examples_package_that_is_installed_stays_when_a_dependent_arrives() {
    // A database an earlier version started, which installed the examples package for sdc.
    let examples = dep_package(EXAMPLES, "4.0.1", &[]);
    let app = dep_package("example.app", "1.0.0", &[(EXAMPLES, "4.0.1")]);
    let db = empty();
    install_package_dir(&db.store, &db.db, &db.catalog, &examples).unwrap();
    let cache = temp_dir("kept-examples-cache");
    // Nothing is fetched: the package is there and the dependency is skipped.
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        app.to_str().unwrap(),
        &NoNetwork,
        &cache,
    )
    .unwrap();
    assert_eq!(
        installed_packages(&db),
        ["example.app@1.0.0".to_string(), format!("{EXAMPLES}@4.0.1")]
    );
    for dir in [examples, app, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// The codes `url` expands to, sorted.
fn expansion_codes(harness: &Harness, url: &str) -> Vec<String> {
    let mut codes: Vec<String> = expansion_of(harness, url)
        .iter()
        .filter_map(|item| item["code"].as_str().map(str::to_string))
        .collect();
    codes.sort();
    codes
}

#[test]
fn leaving_the_r4_examples_package_out_changes_which_package_holds_a_value_set_but_not_its_codes() {
    // R4 core, and a package that depends on the examples package, which carries core's
    // `marital-status` value set and its code system again, with generated narrative. The value
    // set in use is the last one at the release, so with the examples package installed it is
    // the examples package's copy, and without it core's. Either way it expands to the same codes.
    let marital = "http://hl7.org/fhir/ValueSet/marital-status";
    let app = dep_package(
        "example.sdc",
        "1.0.0",
        &[("hl7.fhir.r4.core", "4.0.1"), (EXAMPLES, "4.0.1")],
    );

    // As the server now installs it: the dependency is skipped.
    let (without, _) = watched_r4();
    let fetcher = counting(&[(EXAMPLES, "4.0.1", &r4_examples_dir().to_path_buf())]);
    let cache = temp_dir("scope-without-cache");
    install_with(
        &without.store,
        &without.db,
        &without.catalog,
        app.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap();
    assert!(fetcher.fetched.lock().unwrap().is_empty());
    assert!(!installed_packages(&without)
        .iter()
        .any(|name| name.starts_with(EXAMPLES)));
    let core = package_id(&without, "hl7.fhir.r4.core");
    let rows = stored_rows(&without, marital);
    assert_eq!(rows.len(), 1, "one expansion is stored for it");
    assert_eq!(rows[0].package_id, core, "core holds the value set in use");

    // As an earlier version, or someone who asked for the package, has it installed.
    let (with, _) = watched_r4();
    install_package_dir(&with.store, &with.db, &with.catalog, r4_examples_dir()).unwrap();
    let examples = package_id(&with, EXAMPLES);
    let owners: Vec<i64> = stored_rows(&with, marital)
        .iter()
        .map(|row| row.package_id)
        .collect();
    assert!(
        owners.contains(&examples),
        "the later copy is the one in use: {owners:?}"
    );

    // The codes are the same, and so is what a start reads from what was stored.
    let codes = expansion_codes(&without, marital);
    assert_eq!(codes.len(), 11, "ten statuses and UNK: {codes:?}");
    assert_eq!(expansion_codes(&with, marital), codes);
    publish(&without.store, &without.catalog).unwrap();
    publish(&with.store, &with.catalog).unwrap();
    assert_eq!(expansion_codes(&without, marital), codes, "after a restart");
    assert_eq!(expansion_codes(&with, marital), codes, "after a restart");
    for dir in [app, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn a_cycle_created_only_by_version_substitution_is_allowed() {
    // Mirrors hl7.fhir.uv.extensions.r4 5.3.0-ballot <-> hl7.terminology.r4 6.5.0/7.1.0.
    let app = dep_package(
        "example.app",
        "1.0.0",
        &[("example.ext", "2.0.0"), ("example.term", "2.0.0")],
    );
    let ext1 = dep_package("example.ext", "1.0.0", &[]);
    let ext2 = dep_package("example.ext", "2.0.0", &[("example.term", "1.0.0")]);
    let term1 = dep_package("example.term", "1.0.0", &[("example.ext", "1.0.0")]);
    let term2 = dep_package("example.term", "2.0.0", &[("example.ext", "1.0.0")]);
    let fetcher = registry(&[
        ("example.ext", "1.0.0", &ext1),
        ("example.ext", "2.0.0", &ext2),
        ("example.term", "1.0.0", &term1),
        ("example.term", "2.0.0", &term2),
    ]);
    let db = empty();
    let cache = temp_dir("subst-cache");
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        app.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap();
    assert_eq!(
        installed_packages(&db),
        [
            "example.app@1.0.0",
            "example.ext@2.0.0",
            "example.term@2.0.0"
        ]
    );
    for dir in [app, ext1, ext2, term1, term2, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

struct NoNetwork;

impl PackageFetcher for NoNetwork {
    fn fetch(&self, name: &str, version: &str, _dest: &Path) -> Result<(), FhirError> {
        panic!("{name}@{version} was fetched although it is installed");
    }
}

#[test]
fn an_installed_package_is_never_fetched_again() {
    let lib = dep_package("example.lib", "1.0.0", &[]);
    let db = empty();
    let cache = temp_dir("no-fetch-cache");
    install_package_dir(&db.store, &db.db, &db.catalog, &lib).unwrap();
    assert!(crate::packages_installed(&db.store, &["example.lib@1.0.0"]).unwrap());
    assert!(
        !crate::packages_installed(&db.store, &["example.lib@1.0.0", "example.other@1.0.0"])
            .unwrap()
    );
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        "example.lib@1.0.0",
        &NoNetwork,
        &cache,
    )
    .unwrap();
    assert!(
        !cache.join("example.lib").exists(),
        "the cache was not touched either"
    );
    for dir in [lib, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn a_failed_install_still_backfills_the_packages_already_stored() {
    let db = ready();
    let mut body = patient("p1", true, "Ann");
    body["birthDate"] = json!("1990-01-01");
    apply(&db, "Patient/create", json!({"resource": body}));
    // example.extra stores fine; example.invalid, installed after it, does not.
    let invalid = dep_package("example.invalid", "1.0.0", &[]);
    for (file, expression) in [
        ("a", "Patient.extension('a')"),
        ("b", "Patient.extension('b')"),
    ] {
        std::fs::write(
            invalid.join(format!("SearchParameter-{file}.json")),
            format!(
                r#"{{"resourceType":"SearchParameter","url":"http://example.org/sp/{file}",
                    "status":"draft","experimental":true,"code":"nickname","base":["Patient"],
                    "type":"string","expression":"{expression}"}}"#
            ),
        )
        .unwrap();
    }
    let app = dep_package(
        "example.app",
        "1.0.0",
        &[("example.extra", "1.0.0"), ("example.invalid", "1.0.0")],
    );
    let extra = extra_dir().to_path_buf();
    let fetcher = registry(&[
        ("example.extra", "1.0.0", &extra),
        ("example.invalid", "1.0.0", &invalid),
    ]);
    let cache = temp_dir("partial-cache");
    let err = install_with(
        &db.store,
        &db.db,
        &db.catalog,
        app.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("defined twice"), "{err}");
    let installed = installed_packages(&db);
    assert!(
        installed.contains(&"example.extra@1.0.0".to_string()),
        "{installed:?}"
    );
    assert!(!installed
        .iter()
        .any(|name| name.starts_with("example.invalid") || name.starts_with("example.app")));
    // The stored package reached the catalog and its backfill ran, so its parameter works.
    let found = apply(&db, "Patient/search", json!({"birthdate": "1990-01-01"}));
    assert_eq!(found["total"], 1);
    for dir in [invalid, app, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn backfills_are_queued_after_the_catalog_is_published() {
    let db = ready();
    let mut body = patient("p1", true, "Ann");
    body["birthDate"] = json!("1990-01-01");
    apply(&db, "Patient/create", json!({"resource": body}));

    let pending = stage_package_deferred(&db.store, &db.db, &db.catalog, extra_dir())
        .unwrap()
        .expect("the package has pending search parameters");
    // Jobs from the fixture install are finished; only unfinished ones matter here.
    let unfinished = || -> Vec<_> {
        db.store
            .load_backfills()
            .unwrap()
            .into_iter()
            .filter(|job| !job.done)
            .collect()
    };
    assert!(unfinished().is_empty(), "nothing is queued while staging");

    // A writer that still runs on the old catalog: it indexes nothing for the new parameter.
    let mut late = patient("p2", true, "Bea");
    late["birthDate"] = json!("1990-01-01");
    apply(&db, "Patient/create", json!({"resource": late}));

    publish(&db.store, &db.catalog).unwrap();
    pending.create(&db.store, &db.db).unwrap();
    // The watermark is not older than that write, so backfill covers it.
    let latest = db.db.latest_ts().to_i64().unwrap();
    let jobs = unfinished();
    assert!(!jobs.is_empty());
    assert!(
        jobs.iter().all(|job| job.watermark_ts == latest),
        "{jobs:?}"
    );

    run_backfills(&db.store, &db.db, &db.catalog).unwrap();
    let found = apply(&db, "Patient/search", json!({"birthdate": "1990-01-01"}));
    assert_eq!(found["total"], 2);
}

#[test]
fn json_files_that_are_not_objects_do_not_stop_an_install() {
    let dir = temp_dir("not-objects");
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"example.lists","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    for (file, text) in [
        ("list.json", "[1, 2]"),
        ("empty.json", "[]"),
        ("nothing.json", "null"),
        ("number.json", "42"),
        ("text.json", "\"text\""),
        (
            "Patient-example.json",
            r#"{"resourceType":"Patient","id":"x"}"#,
        ),
    ] {
        std::fs::write(dir.join(file), text).unwrap();
    }
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, &dir).unwrap();
    assert!(installed_packages(&harness).contains(&"example.lists@1.0.0".to_string()));

    // Invalid JSON is still an error, and names the file.
    let other = dep_package("example.broken", "1.0.0", &[]);
    std::fs::write(other.join("broken.json"), "{not json").unwrap();
    let err = install_package_dir(&harness.store, &harness.db, &harness.catalog, &other)
        .unwrap_err()
        .to_string();
    assert!(err.contains("broken.json"), "{err}");
    for path in [dir, other] {
        let _ = std::fs::remove_dir_all(path);
    }
}

#[test]
fn a_dependency_newer_than_the_installed_version_installs_next_to_it() {
    let lib1 = dep_package("example.lib", "1.0.0", &[]);
    let lib2 = dep_package("example.lib", "2.0.0", &[]);
    let app = dep_package("example.app", "1.0.0", &[("example.lib", "2.0.0")]);
    let fetcher = registry(&[("example.lib", "2.0.0", &lib2)]);
    let db = empty();
    let cache = temp_dir("newer-cache");
    install_package_dir(&db.store, &db.db, &db.catalog, &lib1).unwrap();
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        app.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap();
    assert_eq!(
        installed_packages(&db),
        [
            "example.app@1.0.0",
            "example.lib@1.0.0",
            "example.lib@2.0.0"
        ]
    );
    for dir in [lib1, lib2, app, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn another_version_of_an_installed_root_package_installs_next_to_it() {
    let v1 = dep_package("example.ig", "1.0.0", &[]);
    let v2 = dep_package("example.ig", "2.0.0", &[]);
    let db = empty();
    let cache = temp_dir("root-cache");
    install_package_dir(&db.store, &db.db, &db.catalog, &v1).unwrap();
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        v2.to_str().unwrap(),
        &registry(&[]),
        &cache,
    )
    .unwrap();
    assert_eq!(
        installed_packages(&db),
        ["example.ig@1.0.0", "example.ig@2.0.0"]
    );
    for dir in [v1, v2, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn a_replaced_version_does_not_pull_in_its_own_dependencies() {
    // a needs x 1.0.0, b needs x 2.0.0. x 1.0.0 would need y, which the registry lacks,
    // but x 1.0.0 is replaced by 2.0.0 and must not be expanded.
    let app = dep_package(
        "example.app",
        "1.0.0",
        &[("example.a", "1.0.0"), ("example.b", "1.0.0")],
    );
    let a = dep_package("example.a", "1.0.0", &[("example.x", "1.0.0")]);
    let b = dep_package("example.b", "1.0.0", &[("example.x", "2.0.0")]);
    let x1 = dep_package("example.x", "1.0.0", &[("example.y", "1.0.0")]);
    let x2 = dep_package("example.x", "2.0.0", &[]);
    let fetcher = registry(&[
        ("example.a", "1.0.0", &a),
        ("example.b", "1.0.0", &b),
        ("example.x", "1.0.0", &x1),
        ("example.x", "2.0.0", &x2),
    ]);
    let db = empty();
    let cache = temp_dir("replaced-cache");
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        app.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap();
    assert_eq!(
        installed_packages(&db),
        [
            "example.a@1.0.0",
            "example.app@1.0.0",
            "example.b@1.0.0",
            "example.x@2.0.0"
        ]
    );
    for dir in [app, a, b, x1, x2, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn non_numeric_dependency_versions_still_install() {
    let app = dep_package(
        "example.app",
        "1.0.0",
        &[("example.lib", "1.0"), ("example.mid", "1.0.0")],
    );
    let mid = dep_package("example.mid", "1.0.0", &[("example.lib", "current")]);
    let lib1 = dep_package("example.lib", "1.0", &[]);
    let current = dep_package("example.lib", "current", &[]);
    let fetcher = registry(&[
        ("example.mid", "1.0.0", &mid),
        ("example.lib", "1.0", &lib1),
        ("example.lib", "current", &current),
    ]);
    let db = empty();
    let cache = temp_dir("current-cache");
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        app.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap();
    assert_eq!(
        installed_packages(&db),
        [
            "example.app@1.0.0",
            "example.lib@current",
            "example.mid@1.0.0"
        ]
    );
    for dir in [app, mid, lib1, current, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn a_requested_version_is_not_overridden_by_a_dependency() {
    let root = dep_package("example.lib", "1.0.0", &[("example.mid", "1.0.0")]);
    let mid = dep_package("example.mid", "1.0.0", &[("example.lib", "2.0.0")]);
    let fetcher = registry(&[("example.mid", "1.0.0", &mid)]);
    let db = empty();
    let cache = temp_dir("requested-cache");
    let err = install_with(
        &db.store,
        &db.db,
        &db.catalog,
        root.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("version conflict") && err.contains("was requested"),
        "{err}"
    );
    assert!(installed_packages(&db).is_empty());
    for dir in [root, mid, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn an_older_dependency_request_uses_the_installed_newer_version() {
    let lib2 = dep_package("example.lib", "2.0.0", &[]);
    let app = dep_package("example.app", "1.0.0", &[("example.lib", "1.0.0")]);
    let db = empty();
    let cache = temp_dir("older-cache");
    install_package_dir(&db.store, &db.db, &db.catalog, &lib2).unwrap();
    // lib 1.0.0 is not in the registry: it must not be fetched at all.
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        app.to_str().unwrap(),
        &registry(&[]),
        &cache,
    )
    .unwrap();
    assert_eq!(
        installed_packages(&db),
        ["example.app@1.0.0", "example.lib@2.0.0"]
    );
    for dir in [lib2, app, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn official_r4_package_installs_concrete_types() {
    let db = empty();
    install_package_dir(&db.store, &db.db, &db.catalog, r4_dir()).unwrap();
    install_package_dir(&db.store, &db.db, &db.catalog, r4_dir()).unwrap();
    let catalog = db.catalog.read().expect("catalog");
    assert_eq!(catalog.fhir_version(), "4.0.1");
    assert!(catalog.contains("Patient"));
    assert!(catalog.contains("Encounter"));
    assert!(catalog.contains("Observation"));
    let statement = catalog.capability_statement();
    let patient = statement["rest"][0]["resource"]
        .as_array()
        .unwrap()
        .iter()
        .find(|resource| resource["type"] == "Patient")
        .unwrap();
    let names: Vec<_> = patient["searchParam"]
        .as_array()
        .unwrap()
        .iter()
        .map(|param| param["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"name"));
    assert!(names.contains(&"identifier"));
    assert!(names.contains(&"active"));
    assert!(names.contains(&"gender"));
    assert!(names.contains(&"_lastUpdated"));
    assert!(names.contains(&"_filter"));
    assert!(names.contains(&"age"));
    assert_eq!(
        catalog.terms().contains(
            "http://hl7.org/fhir/ValueSet/administrative-gender",
            Some("http://hl7.org/fhir/administrative-gender"),
            "female",
        ),
        TermAnswer::Yes
    );
    assert_eq!(
        catalog.terms().contains(
            "http://hl7.org/fhir/ValueSet/administrative-gender",
            Some("http://hl7.org/fhir/administrative-gender"),
            "not-a-gender",
        ),
        TermAnswer::No
    );
    let accepted = run(
        &db.db,
        &catalog,
        "Patient/$validate",
        &json!({"resource": {"resourceType": "Patient", "gender": "female", "name": [{"family": "Ann"}]}}),
    )
    .unwrap();
    assert!(
        !accepted.value["issue"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["severity"] == "error"
                    && issue["diagnostics"]
                        .as_str()
                        .unwrap_or("")
                        .contains("gender")
            }),
        "{}",
        accepted.value
    );
    let rejected = run(
        &db.db,
        &catalog,
        "Patient/$validate",
        &json!({"resource": {"resourceType": "Patient", "gender": "not-a-gender"}}),
    )
    .unwrap();
    assert!(
        rejected.value["issue"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| { issue["severity"] == "error" && issue["code"] == "code-invalid" }),
        "{}",
        rejected.value
    );
    run(
        &db.db,
        &catalog,
        "Patient/create",
        &json!({"resource": {"resourceType": "Patient", "name": [{"family": "Ann"}]}}),
    )
    .unwrap();
    let filtered = run(
        &db.db,
        &catalog,
        "Patient/search",
        &json!({"_filter": "name eq Ann"}),
    )
    .unwrap();
    assert_eq!(filtered.value["total"], 1);
    let query = run(
        &db.db,
        &catalog,
        "Patient/search",
        &json!({"_query": "missing"}),
    )
    .unwrap_err();
    let RunError::Fhir(FhirError::Invalid(message)) = query else {
        panic!("expected an unknown query");
    };
    assert!(message.contains("unknown query missing"), "{message}");
}

#[test]
fn package_install_rejects_version_and_expression_conflicts() {
    let db = ready();
    let version = install_package_dir(&db.store, &db.db, &db.catalog, version_clash_dir());
    assert!(matches!(version, Err(FhirError::Invalid(_))));
    let expression = install_package_dir(&db.store, &db.db, &db.catalog, expression_clash_dir());
    assert!(matches!(expression, Err(FhirError::Invalid(_))));
}

#[test]
fn invalid_patient_is_rejected_and_validate_reports_issues() {
    let db = ready();
    let mut body = patient("p1", true, "Ann");
    body["birthDate"] = json!("yesterday");
    let rejected = run_op(&db, "Patient/create", &json!({"resource": body})).unwrap_err();
    assert!(matches!(rejected, RunError::Fhir(FhirError::Outcome(_))));
    let catalog = db.catalog.read().expect("catalog");
    let report = run(
        &db.db,
        &catalog,
        "Patient/$validate",
        &json!({"resource": body}),
    )
    .unwrap();
    assert_eq!(report.value["resourceType"], "OperationOutcome");
    assert!(report.value["issue"]
        .as_array()
        .unwrap()
        .iter()
        .any(|issue| issue["severity"] == "error"));
}

#[test]
fn a_package_search_parameter_without_a_base_is_skipped() {
    let dir = std::env::temp_dir().join(format!("refhir-no-base-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"example.nobase","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    // Shaped like the draft `*-extensions-*` parameters in hl7.fhir.r4.core.
    std::fs::write(
        dir.join("SearchParameter-valueset-extensions-ValueSet-effective.json"),
        r#"{
            "resourceType": "SearchParameter",
            "url": "http://hl7.org/fhir/SearchParameter/valueset-extensions-ValueSet-effective",
            "name": "effective",
            "status": "draft",
            "code": "effective",
            "type": "date"
        }"#,
    )
    .unwrap();
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, &dir).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(harness
        .store
        .load_search_parameters()
        .unwrap()
        .iter()
        .all(|param| param.code != "effective"));
}

#[test]
fn an_experimental_parameter_yields_to_the_real_one_in_the_same_package() {
    let dir = std::env::temp_dir().join(format!("refhir-sample-param-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"example.sample","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    // The sample sorts before the real parameter by file name, as in hl7.fhir.r4.core.
    std::fs::write(
        dir.join("SearchParameter-example.json"),
        r#"{"resourceType":"SearchParameter","url":"http://hl7.org/fhir/SearchParameter/example",
            "status":"draft","experimental":true,"code":"_id","base":["Resource"],
            "type":"token","expression":"meta.id"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("SearchParameter-Resource-id.json"),
        r#"{"resourceType":"SearchParameter","url":"http://hl7.org/fhir/SearchParameter/Resource-id",
            "status":"active","code":"_id","base":["Resource"],
            "type":"token","expression":"Resource.id"}"#,
    )
    .unwrap();
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, &dir).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let rows: Vec<_> = harness
        .store
        .load_search_parameters()
        .unwrap()
        .into_iter()
        .filter(|param| param.code == "_id")
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].expression.as_deref(), Some("Resource.id"));
}

#[test]
fn two_experimental_parameters_that_disagree_in_one_package_are_an_error() {
    let dir = std::env::temp_dir().join(format!("refhir-two-draft-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"example.drafts","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    for (file, expression) in [
        ("a", "Patient.extension('a')"),
        ("b", "Patient.extension('b')"),
    ] {
        std::fs::write(
            dir.join(format!("SearchParameter-{file}.json")),
            format!(
                r#"{{"resourceType":"SearchParameter","url":"http://example.org/sp/{file}",
                    "status":"draft","experimental":true,"code":"nickname","base":["Patient"],
                    "type":"string","expression":"{expression}"}}"#
            ),
        )
        .unwrap();
    }
    let harness = empty();
    let err = install_package_dir(&harness.store, &harness.db, &harness.catalog, &dir)
        .unwrap_err()
        .to_string();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        err.contains("Patient.nickname") && err.contains("different expressions"),
        "{err}"
    );
}

fn write_param_package(name: &str, experimental: bool, expression: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("refhir-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        format!(r#"{{"name":"{name}","version":"1.0.0","fhirVersion":"4.0.1"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("SearchParameter-maiden.json"),
        format!(
            r#"{{"resourceType":"SearchParameter","url":"http://example.org/sp/{name}",
                "status":"{}","experimental":{experimental},"code":"maiden","base":["Patient"],
                "type":"string","expression":"{expression}"}}"#,
            if experimental { "draft" } else { "active" }
        ),
    )
    .unwrap();
    dir
}

#[test]
fn an_experimental_parameter_yields_to_a_real_one_from_another_package() {
    for sample_first in [true, false] {
        let sample = write_param_package("sample", true, "Patient.extension('draft')");
        let real = write_param_package("real", false, "Patient.extension('real').value");
        let harness = empty();
        let order = if sample_first {
            [&sample, &real]
        } else {
            [&real, &sample]
        };
        for dir in order {
            install_package_dir(&harness.store, &harness.db, &harness.catalog, dir).unwrap();
        }
        let _ = std::fs::remove_dir_all(&sample);
        let _ = std::fs::remove_dir_all(&real);
        let rows: Vec<_> = harness
            .store
            .load_search_parameters()
            .unwrap()
            .into_iter()
            .filter(|param| param.code == "maiden")
            .collect();
        assert_eq!(rows.len(), 1, "sample_first={sample_first}: {rows:?}");
        assert_eq!(
            rows[0].expression.as_deref(),
            Some("Patient.extension('real').value")
        );
    }
}

fn write_terms_package(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"example.terms","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("CodeSystem-animals.json"),
        r#"{
            "resourceType": "CodeSystem",
            "url": "http://example.org/animals",
            "name": "Animals",
            "content": "complete",
            "concept": [
                {"code": "mammal", "display": "Mammal", "definition": "A mammal", "concept": [
                    {"code": "dog", "display": "Dog"}
                ]}
            ]
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("ValueSet-pets.json"),
        r#"{
            "resourceType": "ValueSet",
            "url": "http://example.org/pets",
            "compose": {"include": [{"system": "http://example.org/animals", "filter": [
                {"property": "concept", "op": "is-a", "value": "mammal"}
            ]}]}
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("ConceptMap-pets.json"),
        r#"{
            "resourceType": "ConceptMap",
            "url": "http://example.org/map",
            "group": [{
                "source": "http://example.org/animals",
                "target": "http://example.org/dst",
                "element": [{"code": "dog", "target": [{"code": "canine", "equivalence": "equivalent"}]}]
            }]
        }"#,
    )
    .unwrap();
}

#[test]
fn terminology_operations_read_the_installed_expansion() {
    let dir = std::env::temp_dir().join(format!(
        "refhir-terms-ops-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_terms_package(&dir);
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, &dir).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let expanded = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://example.org/pets", "filter": "dog"}),
    );
    assert_eq!(expanded["resourceType"], "ValueSet");
    assert_eq!(expanded["expansion"]["contains"][0]["code"], "dog");
    assert!(
        !run_op(
            &harness,
            "ValueSet/$expand",
            &json!({"url": "http://example.org/pets", "filter": "dog"})
        )
        .unwrap()
        .wrote
    );
    let missing = unprocessable(run_op(
        &harness,
        "ValueSet/$expand",
        &json!({"url": "http://example.org/missing"}),
    ));
    assert_eq!(missing["resourceType"], "OperationOutcome");
    assert_eq!(missing["issue"][0]["severity"], "error");
    assert_eq!(missing["issue"][0]["code"], "not-supported");

    let valid = apply(
        &harness,
        "ValueSet/$validate-code",
        json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "url", "valueUri": "http://example.org/pets"},
                {"name": "system", "valueUri": "http://example.org/animals"},
                {"name": "code", "valueCode": "dog"},
                {"name": "display", "valueString": "Dog"}
            ]
        }),
    );
    assert_eq!(valid["parameter"][0]["valueBoolean"], true);
    let invalid = apply(
        &harness,
        "CodeSystem/$validate-code",
        json!({"url": "http://example.org/animals", "code": "fish"}),
    );
    assert_eq!(invalid["parameter"][0]["valueBoolean"], false);

    let looked = apply(
        &harness,
        "CodeSystem/$lookup",
        json!({"system": "http://example.org/animals", "code": "mammal"}),
    );
    assert_eq!(looked["resourceType"], "Parameters");
    assert!(looked["parameter"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["name"] == "display" && item["valueString"] == "Mammal"));
    assert!(looked["parameter"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["name"] == "definition" && item["valueString"] == "A mammal"));

    let translated = apply(
        &harness,
        "ConceptMap/$translate",
        json!({"url": "http://example.org/map", "system": "http://example.org/animals", "code": "dog"}),
    );
    assert_eq!(translated["parameter"][0]["valueBoolean"], true);
    assert_eq!(
        translated["parameter"][1]["part"][1]["valueCoding"]["code"],
        "canine"
    );
    let unsupported = run_op(
        &harness,
        "Patient/$expand",
        &json!({"url": "http://example.org/pets"}),
    );
    assert!(matches!(
        unsupported,
        Err(RunError::Fhir(FhirError::Unsupported(_)))
    ));
}

#[test]
fn sqlite_reloads_a_stored_expansion_without_the_package_directory() {
    let dir = std::env::temp_dir().join(format!(
        "refhir-terms-pkg-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let path = std::env::temp_dir().join(format!(
        "refhir-terms-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    write_terms_package(&dir);
    {
        let store = SqlitePersistence::open(&path).unwrap();
        let db = Database::open(Arc::new(store.clone())).unwrap();
        let catalog = std::sync::RwLock::new(Catalog::empty());
        install_package_dir(&store, &db, &catalog, &dir).unwrap();
        assert!(!store.load_expansions().unwrap().is_empty());
    }
    std::fs::remove_dir_all(&dir).unwrap();
    let store = SqlitePersistence::open(&path).unwrap();
    let catalog = Catalog::open(&store).unwrap();
    assert_eq!(
        catalog.terms().contains(
            "http://example.org/pets",
            Some("http://example.org/animals"),
            "dog",
        ),
        TermAnswer::Yes
    );
    assert_eq!(
        catalog.terms().contains(
            "http://example.org/pets",
            Some("http://example.org/animals"),
            "fish",
        ),
        TermAnswer::No
    );
    drop(catalog);
    drop(store);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
}

fn commit(harness: &Harness, path: &str, args: Value) -> Result<Value, RunError> {
    commit_operation(&harness.store, &harness.db, &harness.catalog, path, &args)
        .map(|result| result.value)
}

fn install_resource_types(harness: &Harness) {
    let dir = std::env::temp_dir().join(format!(
        "refhir-local-types-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"example.structures","version":"1.0.0","fhirVersion":"4.0.1"}"#,
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
    install_package_dir(&harness.store, &harness.db, &harness.catalog, &dir).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn number_search_matches_precision_and_comparators() {
    let harness = ready();
    install_resource_types(&harness);
    commit(
        &harness,
        "SearchParameter/create",
        json!({"resource": {
            "resourceType": "SearchParameter",
            "id": "year",
            "url": "http://example.org/SearchParameter/year",
            "name": "year",
            "status": "active",
            "description": "Birth year",
            "code": "year",
            "base": ["Patient"],
            "type": "number",
            "expression": "Patient.birthDate.substring(0, 4).toInteger()"
        }}),
    )
    .unwrap();
    let mut born = patient("p1", true, "Ann");
    born["birthDate"] = json!("1990-01-01");
    commit(&harness, "Patient/create", json!({"resource": born})).unwrap();
    let found = commit(&harness, "Patient/search", json!({"year": "1990"})).unwrap();
    assert_eq!(found["total"], 1);
    let later = commit(&harness, "Patient/search", json!({"year": "gt1989"})).unwrap();
    assert_eq!(later["total"], 1);
    let earlier = commit(&harness, "Patient/search", json!({"year": "lt1990"})).unwrap();
    assert_eq!(earlier["total"], 0);
}

fn label_parameter(expression: &str) -> Value {
    json!({
        "resourceType": "SearchParameter",
        "id": "label",
        "url": "http://example.org/SearchParameter/label",
        "name": "label",
        "status": "active",
        "description": "A label",
        "code": "label",
        "base": ["Patient"],
        "type": "string",
        "expression": expression
    })
}

/// What a catalog says about its search parameters: a line for each parameter of each type, with
/// its name, kind and status.
fn parameter_summary(catalog: &Catalog) -> Vec<String> {
    catalog
        .resource_models()
        .flat_map(|model| {
            model.params.iter().map(|param| {
                format!(
                    "{}.{} {:?} {:?}",
                    model.name, param.name, param.kind, param.status
                )
            })
        })
        .collect()
}

#[test]
fn compiling_the_parameters_in_place_gives_what_compiling_them_beside_the_catalog_gives() {
    let harness = ready();
    let parameters = harness.store.load_search_parameters().unwrap();
    assert!(!parameters.is_empty());
    let mut beside = harness.catalog.read().unwrap().clone();
    let compiled = beside.compiled_parameters(&parameters);
    beside.set_parameters(compiled);
    let mut in_place = harness.catalog.read().unwrap().clone();
    in_place.recompile_parameters(&parameters);
    assert_eq!(parameter_summary(&in_place), parameter_summary(&beside));
    assert_eq!(
        parameter_summary(&in_place),
        parameter_summary(&harness.catalog.read().unwrap())
    );
}

#[test]
fn user_search_parameter_reindexes_when_the_expression_changes() {
    let harness = ready();
    install_resource_types(&harness);
    let label = commit(
        &harness,
        "SearchParameter/create",
        json!({"resource": label_parameter("Patient.name")}),
    )
    .unwrap();
    let label_id = label["id"].as_str().unwrap().to_string();
    let created = commit(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    )
    .unwrap();
    let found = commit(&harness, "Patient/search", json!({"label": "Ann"})).unwrap();
    assert_eq!(found["total"], 1);
    assert_eq!(found["entry"][0]["resource"]["id"], created["id"]);

    let mut moved = label_parameter("Patient.birthDate");
    moved["id"] = json!(label_id);
    commit(
        &harness,
        "SearchParameter/update",
        json!({"id": label_id, "resource": moved}),
    )
    .unwrap();
    let missed = commit(&harness, "Patient/search", json!({"label": "Ann"})).unwrap();
    assert_eq!(missed["total"], 0);

    commit(&harness, "SearchParameter/delete", json!({"id": label_id})).unwrap();
    let unsupported = commit(&harness, "Patient/search", json!({"label": "Ann"})).unwrap_err();
    let RunError::Fhir(FhirError::Invalid(message)) = unsupported else {
        panic!("expected an unsupported parameter");
    };
    assert!(
        message.contains("unsupported search parameter label"),
        "{message}"
    );
}

#[test]
fn user_value_set_expands_until_it_is_deleted() {
    let harness = ready();
    install_resource_types(&harness);
    let mut colors = json!({
        "resourceType": "ValueSet",
        "id": "colors",
        "url": "http://example.org/colors",
        "status": "active",
        "compose": {"include": [{"system": "http://example.org/colors", "concept": [
            {"code": "red", "display": "Red"},
            {"code": "blue", "display": "Blue"}
        ]}]}
    });
    let created = commit(&harness, "ValueSet/create", json!({"resource": colors})).unwrap();
    let colors_id = created["id"].as_str().unwrap().to_string();
    colors["id"] = json!(colors_id);
    let expanded = commit(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://example.org/colors"}),
    )
    .unwrap();
    let codes: Vec<_> = expanded["expansion"]["contains"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry.get("code").and_then(|code| code.as_str()))
        .collect();
    assert!(codes.contains(&"red"), "{codes:?}");
    assert!(codes.contains(&"blue"), "{codes:?}");
    assert_eq!(
        harness.catalog.read().unwrap().terms().contains(
            "http://example.org/colors",
            Some("http://example.org/colors"),
            "blue",
        ),
        TermAnswer::Yes
    );

    colors["compose"]["include"][0]["concept"] = json!([{"code": "red", "display": "Red"}]);
    commit(
        &harness,
        "ValueSet/update",
        json!({"id": colors_id, "resource": colors}),
    )
    .unwrap();
    assert_eq!(
        harness.catalog.read().unwrap().terms().contains(
            "http://example.org/colors",
            Some("http://example.org/colors"),
            "blue",
        ),
        TermAnswer::No
    );

    commit(&harness, "ValueSet/delete", json!({"id": colors_id})).unwrap();
    let gone = unprocessable(commit(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://example.org/colors"}),
    ));
    let text = gone.to_string();
    assert!(text.contains("not supported"), "{text}");
}

#[test]
fn user_value_set_cannot_reuse_a_package_url() {
    let harness = ready();
    install_resource_types(&harness);
    let dir = std::env::temp_dir().join(format!(
        "refhir-owned-vs-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"example.owned","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("ValueSet-owned.json"),
        r#"{"resourceType":"ValueSet","url":"http://example.org/owned","status":"active","compose":{"include":[{"system":"http://example.org/owned","concept":[{"code":"a"}]}]}}"#,
    )
    .unwrap();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, &dir).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();

    let err = commit(
        &harness,
        "ValueSet/create",
        json!({"resource": {
            "resourceType": "ValueSet",
            "id": "owned",
            "url": "http://example.org/owned",
            "status": "active",
            "compose": {"include": [{"system": "http://example.org/owned", "concept": [{"code": "b"}]}]}
        }}),
    )
    .unwrap_err();
    let RunError::Fhir(FhirError::Invalid(message)) = err else {
        panic!("expected a canonical clash");
    };
    assert!(
        message.contains("belongs to an installed package"),
        "{message}"
    );
}

#[test]
fn repair_projects_a_value_set_that_was_only_stored() {
    let harness = ready();
    install_resource_types(&harness);
    let colors = json!({
        "resourceType": "ValueSet",
        "id": "repair",
        "url": "http://example.org/repair",
        "status": "active",
        "compose": {"include": [{"system": "http://example.org/repair", "concept": [
            {"code": "red", "display": "Red"}
        ]}]}
    });
    apply(&harness, "ValueSet/create", json!({"resource": colors}));
    let before = unprocessable(run_op(
        &harness,
        "ValueSet/$expand",
        &json!({"url": "http://example.org/repair"}),
    ));
    assert!(before.to_string().contains("not supported"), "{before}");

    repair_conformance(&harness.store, &harness.db, &harness.catalog).unwrap();
    let expanded = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://example.org/repair"}),
    );
    assert_eq!(expanded["expansion"]["contains"][0]["code"], "red");
}

#[test]
fn fhir_reference_policy_allows_absolute_and_missing_targets() {
    let harness = ready();
    harness
        .catalog
        .write()
        .expect("catalog")
        .set_reference_policy(ReferencePolicy::Fhir);
    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    );
    let absolute = apply(
        &harness,
        "Observation/create",
        json!({"resource": {
            "resourceType": "Observation",
            "id": "abs",
            "subject": {"reference": "http://example.org/fhir/Patient/p1"}
        }}),
    );
    assert_eq!(absolute["id"], "abs");
    let missing = apply(
        &harness,
        "Observation/create",
        json!({"resource": {
            "resourceType": "Observation",
            "id": "gone",
            "subject": {"reference": "Patient/missing"}
        }}),
    );
    assert_eq!(missing["id"], "gone");
    let wrong = run_op(
        &harness,
        "Observation/create",
        &json!({"resource": {
            "resourceType": "Observation",
            "subject": {"reference": "http://example.org/fhir/Organization/o1"}
        }}),
    )
    .unwrap_err();
    assert!(matches!(wrong, RunError::Fhir(FhirError::Invalid(_))));
    let resolved = apply(
        &harness,
        "Observation/search",
        json!({"subject": "Patient/p1"}),
    );
    assert_eq!(resolved["total"], 0);
}

fn observation(id: &str, patient: &str, date: &str, code: &str) -> Value {
    json!({
        "resourceType": "Observation",
        "id": id,
        "subject": {"reference": format!("Patient/{patient}")},
        "effectiveDateTime": date,
        "code": {"coding": [{"code": code}]}
    })
}

#[test]
fn search_modifiers_prefixes_paging_include_chain_has_and_compartment() {
    let harness = ready();
    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    );
    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p2", false, "Bee")}),
    );
    apply(
        &harness,
        "Observation/create",
        json!({"resource": observation("o1", "p1", "2020-01-02", "8480-6")}),
    );
    apply(
        &harness,
        "Observation/create",
        json!({"resource": observation("o2", "p2", "2018-01-02", "8480-6")}),
    );

    let exact = apply(&harness, "Patient/search", json!({"name:exact": "Bee"}));
    assert_eq!(exact["total"], 1);
    let loose = apply(&harness, "Patient/search", json!({"name:exact": "bee"}));
    assert_eq!(loose["total"], 0);
    let contains = apply(&harness, "Patient/search", json!({"name:contains": "ee"}));
    assert_eq!(contains["total"], 1);
    let missing = apply(
        &harness,
        "Patient/search",
        json!({"active:missing": "false"}),
    );
    assert_eq!(missing["total"], 2);
    let negated = apply(&harness, "Patient/search", json!({"active:not": "true"}));
    assert_eq!(negated["total"], 1);
    assert_eq!(negated["entry"][0]["resource"]["id"], "p2");

    let later = apply(&harness, "Observation/search", json!({"date": "gt2019"}));
    assert_eq!(later["total"], 1);
    assert_eq!(later["entry"][0]["resource"]["id"], "o1");
    let earlier = apply(&harness, "Observation/search", json!({"date": "lt2019"}));
    assert_eq!(earlier["total"], 1);

    let page = apply(
        &harness,
        "Patient/search",
        json!({"_count": "1", "_sort": "name"}),
    );
    assert_eq!(page["total"], 2);
    assert_eq!(page["entry"].as_array().unwrap().len(), 1);
    let next_link = page["link"]
        .as_array()
        .unwrap()
        .iter()
        .find(|link| link["relation"] == "next")
        .unwrap();
    let next_url = next_link["url"].as_str().unwrap();
    assert!(next_url.contains("_cursor="), "{next_url}");
    assert!(!next_url.contains("_offset="), "{next_url}");
    let cursor = next_url.split("_cursor=").nth(1).unwrap();
    let followed = apply(
        &harness,
        "Patient/search",
        json!({"_count": "1", "_sort": "name", "_cursor": cursor}),
    );
    assert_eq!(followed["entry"].as_array().unwrap().len(), 1);
    assert_ne!(
        page["entry"][0]["resource"]["id"],
        followed["entry"][0]["resource"]["id"]
    );
    assert!(
        followed["link"]
            .as_array()
            .unwrap()
            .iter()
            .all(|link| link["relation"] != "next"),
        "{}",
        followed["link"]
    );
    let full_page = apply(
        &harness,
        "Patient/search",
        json!({"_count": "2", "_sort": "name"}),
    );
    assert_eq!(full_page["entry"].as_array().unwrap().len(), 2);
    assert!(
        full_page["link"]
            .as_array()
            .unwrap()
            .iter()
            .all(|link| link["relation"] != "next"),
        "{}",
        full_page["link"]
    );

    let summary = apply(&harness, "Patient/search", json!({"_summary": "count"}));
    assert_eq!(summary["total"], 2);
    assert!(summary["entry"].as_array().unwrap().is_empty());

    let included = apply(
        &harness,
        "Observation/search",
        json!({"_id": "o1", "_include": "Observation:subject"}),
    );
    let modes: Vec<_> = included["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["search"]["mode"].as_str().unwrap().to_string())
        .collect();
    assert!(modes.contains(&"match".to_string()));
    assert!(modes.contains(&"include".to_string()));

    let chained = apply(
        &harness,
        "Observation/search",
        json!({"subject:Patient.name": "Bee"}),
    );
    assert_eq!(chained["total"], 1);
    assert_eq!(chained["entry"][0]["resource"]["id"], "o2");

    let has = apply(
        &harness,
        "Patient/search",
        json!({"_has:Observation:subject:code": "8480-6"}),
    );
    assert_eq!(has["total"], 2);

    let compartment = apply(
        &harness,
        "Observation/search",
        json!({"_compartment": "Patient/p1"}),
    );
    assert_eq!(compartment["total"], 1);
    assert_eq!(compartment["entry"][0]["resource"]["id"], "o1");
}

/// The full R4 definitions, for the types that give a reference parameter several targets.
fn r4() -> Harness {
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();
    harness
}

/// The ids of the resources a search matched, in order.
fn matched_ids(bundle: &Value) -> Vec<String> {
    let mut ids: Vec<String> = bundle["entry"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["search"]["mode"] == "match")
        .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

fn r4_observation(id: &str, status: &str, subject: &str) -> Value {
    json!({
        "resourceType": "Observation",
        "id": id,
        "status": status,
        "code": {"text": "vitals"},
        "subject": {"reference": subject}
    })
}

#[test]
fn a_bare_id_finds_what_type_slash_id_finds() {
    let harness = r4();
    for id in ["p1", "p2"] {
        apply(
            &harness,
            "Patient/create",
            json!({"resource": patient(id, true, "Ann")}),
        );
    }
    apply(
        &harness,
        "Group/create",
        json!({"resource": {"resourceType": "Group", "id": "g1", "type": "person", "actual": true}}),
    );
    apply(
        &harness,
        "Practitioner/create",
        json!({"resource": {"resourceType": "Practitioner", "id": "x1"}}),
    );
    apply(
        &harness,
        "Organization/create",
        json!({"resource": {"resourceType": "Organization", "id": "x1", "name": "Acme"}}),
    );
    for (id, status, subject) in [
        ("o1", "final", "Patient/p1"),
        ("o2", "final", "Patient/p2"),
        ("o3", "preliminary", "Patient/p1"),
        ("o4", "final", "Group/g1"),
    ] {
        apply(
            &harness,
            "Observation/create",
            json!({"resource": r4_observation(id, status, subject)}),
        );
    }
    // o5 is performed by a Practitioner and an Organization that share the id x1, o6 by the
    // Organization alone.
    let mut both = r4_observation("o5", "final", "Patient/p2");
    both["performer"] = json!([
        {"reference": "Practitioner/x1"},
        {"reference": "Organization/x1"}
    ]);
    let mut one = r4_observation("o6", "final", "Patient/p2");
    one["performer"] = json!([{"reference": "Organization/x1"}]);
    for resource in [both, one] {
        apply(
            &harness,
            "Observation/create",
            json!({"resource": resource}),
        );
    }
    // What a bare id stands for is what the type can hold, not everything the parameter is listed
    // for: `patient` is a Patient for most types, but `DeviceUseStatement.subject` can be a Group.
    let stands_for = |type_name: &str, parameter: &str| {
        let catalog = harness.catalog.read().unwrap();
        let param = catalog.get(type_name).unwrap().ready_param(parameter);
        param.unwrap().extractor.reference_id("1")
    };
    let type_tokens = |tokens: &[&str]| {
        ReferenceId::Typed(tokens.iter().map(|token| token.to_string()).collect())
    };
    assert_eq!(
        stands_for("Observation", "patient"),
        type_tokens(&["Patient/1"])
    );
    assert_eq!(
        stands_for("AllergyIntolerance", "patient"),
        type_tokens(&["Patient/1"])
    );
    assert_eq!(
        stands_for("DeviceUseStatement", "patient"),
        type_tokens(&["Patient/1", "Group/1"])
    );
    assert_eq!(
        stands_for("Observation", "subject"),
        type_tokens(&["Group/1", "Device/1", "Patient/1", "Location/1"])
    );
    let search_bundle = |query: Value| apply(&harness, "Observation/search", query);
    let search = |query: Value| matched_ids(&search_bundle(query));
    let in_order = |bundle: &Value| -> Vec<String> {
        bundle["entry"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string())
            .collect()
    };
    let has_next = |bundle: &Value| {
        bundle["link"]
            .as_array()
            .unwrap()
            .iter()
            .any(|link| link["relation"] == "next")
    };

    // `patient` can only be a Patient, so the id is that Patient's.
    assert_eq!(search(json!({"patient": "p1", "status": "final"})), ["o1"]);
    assert_eq!(
        search(json!({"patient": "Patient/p1", "status": "final"})),
        ["o1"]
    );
    assert_eq!(search(json!({"patient": "p1"})), ["o1", "o3"]);
    assert_eq!(search(json!({"patient": "p2"})), ["o2", "o5", "o6"]);
    assert!(search(json!({"patient": "nobody"})).is_empty());

    // `subject` can be a Patient, a Group, a Device or a Location: the id is looked for as each.
    assert_eq!(search(json!({"subject": "p1"})), ["o1", "o3"]);
    assert_eq!(search(json!({"subject": "Patient/p1"})), ["o1", "o3"]);
    assert_eq!(search(json!({"subject": "g1"})), ["o4"]);
    assert_eq!(search(json!({"subject": "Group/g1"})), ["o4"]);

    // `patient` holds no Group, however it is asked for.
    assert!(search(json!({"patient": "g1"})).is_empty());
    assert!(search(json!({"patient": "Group/g1"})).is_empty());

    // A comma lists alternatives, and each is read as it would be alone: a bare id, a `Type/id`,
    // or one of each.
    let either = ["o1", "o2", "o3", "o5", "o6"];
    assert_eq!(search(json!({"patient": "p1,p2"})), either);
    assert_eq!(search(json!({"patient": "p1,Patient/p2"})), either);
    assert_eq!(search(json!({"patient": "Patient/p1,Patient/p2"})), either);
    assert_eq!(search(json!({"subject": "p1,g1"})), ["o1", "o3", "o4"]);
    assert_eq!(search(json!({"patient:not": "p1,p2"})), ["o4"]);

    // The modifiers that take an id read a bare one the same way. `:missing` takes true or false,
    // which are valid ids and are not looked for as one.
    assert_eq!(
        search(json!({"patient:not": "p1"})),
        ["o2", "o4", "o5", "o6"]
    );
    assert_eq!(
        search(json!({"patient:not": "p1"})),
        search(json!({"patient:not": "Patient/p1"}))
    );
    assert_eq!(
        search(json!({"patient:missing": "false"})),
        ["o1", "o2", "o3", "o5", "o6"]
    );
    assert_eq!(search(json!({"_filter": "patient eq p1"})), ["o1", "o3"]);

    // Totals and pages count what matches, not the ranges read.
    assert_eq!(
        search_bundle(json!({"patient": "p1", "_summary": "count"}))["total"],
        2
    );
    let page = search_bundle(json!({"patient": "p1", "_count": "1"}));
    assert_eq!(page["total"], 2);
    assert_eq!(page["entry"].as_array().unwrap().len(), 1);
    assert!(has_next(&page));

    // Without a total the search stops reading once the page is full, which across several ranges
    // means reading them all and cutting the merged rows.
    let first = search_bundle(json!({"subject": "p1", "_count": "1", "_total": "none"}));
    assert!(first.get("total").is_none());
    assert_eq!(first["entry"].as_array().unwrap().len(), 1);
    assert!(has_next(&first), "o3 is one more");
    let last = search_bundle(json!({"subject": "p1", "_count": "2", "_total": "none"}));
    assert_eq!(last["entry"].as_array().unwrap().len(), 2);
    assert!(!has_next(&last), "there is nothing past o3");

    // Each of these checks the rows one by one instead of reading the page off the index.
    let sorted = search_bundle(json!({"subject": "p1", "_sort": "-status"}));
    assert_eq!(in_order(&sorted), ["o3", "o1"]);
    assert_eq!(
        search(json!({"patient": "p1", "_compartment": "Patient/p1"})),
        ["o1", "o3"]
    );

    // A document that references the id under two types is still one match.
    let found = search_bundle(json!({"performer": "x1"}));
    assert_eq!(found["total"], 2);
    assert_eq!(found["entry"].as_array().unwrap().len(), 2, "o5 once");
    assert_eq!(matched_ids(&found), ["o5", "o6"]);
    assert_eq!(
        search_bundle(json!({"performer": "x1", "_count": "1"}))["total"],
        2
    );
    assert_eq!(search(json!({"performer": "Practitioner/x1"})), ["o5"]);

    // The bundle is the same whichever way the reference is written.
    let bare = search_bundle(json!({"subject": "p1"}));
    let typed = search_bundle(json!({"subject": "Patient/p1"}));
    assert_eq!(bare["entry"], typed["entry"]);
    assert_eq!(bare["total"], typed["total"]);
}

#[test]
fn a_bare_id_the_parameter_cannot_place_is_refused_and_says_what_to_send() {
    let refused = |harness: &Harness, path: &str, query: Value| match commit(harness, path, query) {
        Err(RunError::Fhir(FhirError::Unsupported(message))) => message,
        other => panic!("expected a refusal, got {other:?}"),
    };

    // `Task.subject` can point at any resource: an id alone does not say which.
    let harness = r4();
    let message = refused(&harness, "Task/search", json!({"subject": "p1"}));
    assert!(message.contains("subject"), "{message}");
    assert!(message.contains("145 resource types"), "{message}");
    assert!(message.contains("use Type/p1"), "{message}");
    refused(&harness, "Task/search", json!({"subject:not": "p1"}));
    // A `_filter` comparison reads the id the same way, so it is refused too, not left to match
    // nothing.
    refused(&harness, "Task/search", json!({"_filter": "subject eq p1"}));
    // A reference that names its type, and a modifier whose value is not an id, still work.
    for query in [
        json!({"subject": "Patient/p1"}),
        json!({"subject": "http://example.org/fhir/Patient/p1"}),
        json!({"subject:missing": "false"}),
        json!({"_filter": "subject eq Patient/p1"}),
    ] {
        let found = commit(&harness, "Task/search", query.clone()).unwrap();
        assert_eq!(found["total"], 0, "{query}");
    }

    // An id counts once for every type it stands for toward the 1000 index ranges a search may
    // read: `Observation.subject` points at four types, so 250 ids are 1000 ranges and 251 are too
    // many.
    let ids = |count: usize| {
        (0..count)
            .map(|n| format!("p{n}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let found = commit(&harness, "Observation/search", json!({"subject": ids(250)})).unwrap();
    assert_eq!(found["total"], 0);
    match commit(&harness, "Observation/search", json!({"subject": ids(251)})) {
        Err(RunError::Fhir(FhirError::Invalid(message))) => {
            assert!(message.contains("1004 index ranges"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    // A parameter that lists no target type cannot place an id either.
    let harness = ready();
    install_resource_types(&harness);
    commit(
        &harness,
        "SearchParameter/create",
        json!({"resource": {
            "resourceType": "SearchParameter",
            "id": "focus",
            "url": "http://example.org/SearchParameter/Observation-focus",
            "name": "focus",
            "status": "active",
            "description": "Who the observation is about",
            "code": "focus",
            "base": ["Observation"],
            "type": "reference",
            "expression": "Observation.subject"
        }}),
    )
    .unwrap();
    // The server picks the id, so ask for the one it gave.
    let created = commit(
        &harness,
        "Patient/create",
        json!({"resource": patient("ignored", true, "Ann")}),
    )
    .unwrap();
    let id = created["id"].as_str().unwrap();
    commit(
        &harness,
        "Observation/create",
        json!({"resource": {
            "resourceType": "Observation",
            "code": {"coding": [{"code": "8480-6"}]},
            "subject": {"reference": format!("Patient/{id}")}
        }}),
    )
    .unwrap();
    let message = refused(&harness, "Observation/search", json!({"focus": id}));
    assert!(
        message.contains("does not say which resource types"),
        "{message}"
    );
    assert!(message.contains(&format!("use Type/{id}")), "{message}");
    let typed = commit(
        &harness,
        "Observation/search",
        json!({"focus": format!("Patient/{id}")}),
    )
    .unwrap();
    assert_eq!(typed["total"], 1);

    // SDC population searches through the same code. An id the parameter cannot place found
    // nothing before the server read bare ids, so a form's query still finds nothing; it does not
    // fail the whole operation.
    let catalog = harness.catalog.read().unwrap();
    let mut tx = harness.db.begin();
    let mut chart = crate::sdc::Bridge {
        tx: &mut tx,
        catalog: &catalog,
        scopes: None,
    };
    let query = |value: String| [("focus", value)];
    assert_eq!(
        Chart::search_type(&mut chart, "Observation", &query(id.to_string())),
        Ok(Vec::new())
    );
    let found = Chart::search_type(&mut chart, "Observation", &query(format!("Patient/{id}")));
    assert_eq!(found.unwrap().len(), 1);
}

#[test]
fn a_search_by_bare_id_conflicts_only_with_a_write_that_references_that_id() {
    let conflicts = |written_for: &str| {
        let db = ready();
        for id in ["p1", "p2"] {
            apply(
                &db,
                "Patient/create",
                json!({"resource": patient(id, true, "Ann")}),
            );
        }
        let catalog = db.catalog.read().expect("catalog");
        let mut reader = db.db.begin();
        crate::dispatch(
            &mut reader,
            &catalog,
            "Observation/search",
            &json!({"subject": "p1"}),
        )
        .unwrap();
        // A transaction that wrote nothing has nothing to validate: write something unrelated.
        let mut other = patient("p2", true, "Ann");
        other["birthDate"] = json!("1980-01-01");
        crate::dispatch(
            &mut reader,
            &catalog,
            "Patient/update",
            &json!({"id": "p2", "resource": other}),
        )
        .unwrap();

        let mut writer = db.db.begin();
        let mut resource = observation("o1", written_for, "2020-01-02", "8480-6");
        resource.as_object_mut().unwrap().remove("id");
        crate::dispatch(
            &mut writer,
            &catalog,
            "Observation/create",
            &json!({"resource": resource}),
        )
        .unwrap();
        db.db.commit_with(writer, crate::stamp_stored_json).unwrap();
        match db.db.commit_with(reader, crate::stamp_stored_json) {
            Ok(_) => false,
            Err(err) => {
                assert!(err.is_occ(), "{err}");
                true
            }
        }
    };
    assert!(conflicts("p1"), "a new match changes the result");
    assert!(!conflicts("p2"), "an Observation of someone else does not");
}

#[test]
fn the_servers_patient_limit_is_combined_with_the_callers_compartment_and_never_replaced() {
    let harness = ready();
    for (id, name) in [("p1", "Ann"), ("p2", "Bee")] {
        apply(
            &harness,
            "Patient/create",
            json!({"resource": patient(id, true, name)}),
        );
    }
    for (id, subject) in [("o1", "p1"), ("o2", "p2")] {
        apply(
            &harness,
            "Observation/create",
            json!({"resource": observation(id, subject, "2020-01-02", "8480-6")}),
        );
    }
    let ids = |result: &Value| -> Vec<String> {
        result["entry"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.pointer("/resource/id"))
            .filter_map(|id| id.as_str().map(str::to_string))
            .collect()
    };

    // The limit alone.
    let limited = apply(
        &harness,
        "Observation/search",
        json!({"_patientCompartment": "Patient/p1"}),
    );
    assert_eq!(ids(&limited), ["o1"]);
    // The caller's own `_compartment` cannot take a search past the limit, whichever it names.
    for asked in ["Patient/p2", "Patient/p1"] {
        let combined = apply(
            &harness,
            "Observation/search",
            json!({"_patientCompartment": "Patient/p1", "_compartment": asked}),
        );
        let expected: &[&str] = if asked == "Patient/p1" { &["o1"] } else { &[] };
        assert_eq!(ids(&combined), expected, "_compartment={asked}");
        assert_eq!(combined["total"], expected.len(), "_compartment={asked}");
    }
    // Paging links do not carry either parameter: each request gets its limit from the server.
    let page = apply(
        &harness,
        "Observation/search",
        json!({"_patientCompartment": "Patient/p1", "_count": "1"}),
    );
    assert!(
        !page.to_string().contains("_patientCompartment"),
        "the limit leaked into a link: {page}"
    );
    // A limit the search cannot read is an error, never a search without it.
    for bad in [json!("p1"), json!(7)] {
        let refused = run_op(
            &harness,
            "Observation/search",
            &json!({"_patientCompartment": bad}),
        );
        assert!(refused.is_err(), "{bad} was accepted");
    }

    // The same limit holds for the searches inside a Bundle. What a URL puts under the server's
    // name is replaced by the server's own, and with none set it is dropped: it is not the
    // client's name to use.
    let batch = |url: &str, controls: Value| {
        let mut args = json!({"resource": {"resourceType": "Bundle", "type": "batch", "entry": [
            {"request": {"method": "GET", "url": url}}
        ]}});
        args.as_object_mut()
            .unwrap()
            .extend(controls.as_object().unwrap().clone());
        let result = run_op(&harness, "Bundle/batch", &args).expect("batch");
        result.value["entry"][0]["resource"].clone()
    };
    let limit = json!({"_patientCompartment": "Patient/p1"});
    assert_eq!(ids(&batch("Observation?_count=100", limit.clone())), ["o1"]);
    assert_eq!(
        ids(&batch(
            "Observation?_patientCompartment=Patient/p2&_compartment=Patient/p2",
            limit.clone()
        )),
        Vec::<String>::new()
    );
    assert_eq!(
        ids(&batch(
            "Observation?_patientCompartment=Patient/p1",
            json!({})
        )),
        ["o1", "o2"]
    );
    let transaction = run_op(
        &harness,
        "Bundle/transaction",
        &json!({"resource": {"resourceType": "Bundle", "type": "transaction", "entry": [
            {"request": {"method": "GET", "url": "Observation?_patientCompartment=Patient/p2"}}
        ]}, "_patientCompartment": "Patient/p1"}),
    )
    .expect("transaction");
    assert_eq!(
        ids(&transaction.value["entry"][0]["resource"]),
        ["o1"],
        "{}",
        transaction.value
    );
}

#[test]
fn patch_conditional_history_batch_and_urn_references() {
    let harness = ready();
    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    );
    let patched = apply(
        &harness,
        "Patient/patch",
        json!({"id": "p1", "patch": [{"op": "add", "path": "/birthDate", "value": "1990-01-01"}]}),
    );
    assert_eq!(patched["birthDate"], "1990-01-01");

    let again = run_op(
        &harness,
        "Patient/conditional-create",
        &json!({
            "resource": patient("other", true, "Ann"),
            "ifNoneExist": "identifier=http://hospital.example|mrn-p1"
        }),
    )
    .unwrap();
    assert_eq!(again.status, 200);
    assert_eq!(again.value["id"], "p1");

    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p2", true, "Ann")}),
    );
    let many = run_op(
        &harness,
        "Patient/conditional-delete",
        &json!({"query": "name=Ann"}),
    )
    .unwrap_err();
    assert!(matches!(many, RunError::Fhir(FhirError::Precondition(_))));

    let history = apply(&harness, "Patient/history", json!({}));
    assert_eq!(history["type"], "history");
    assert!(history["total"].as_u64().unwrap() >= 2);
    let system = apply(&harness, "system/history", json!({}));
    assert_eq!(system["type"], "history");

    let batch = apply(
        &harness,
        "Bundle/batch",
        json!({"resource": {
            "resourceType": "Bundle",
            "type": "batch",
            "entry": [
                {"request": {"method": "POST", "url": "Patient"}, "resource": patient("b1", true, "Batch")},
                {"request": {"method": "GET", "url": "Patient?name=Batch"}}
            ]
        }}),
    );
    assert_eq!(batch["type"], "batch-response");
    assert_eq!(batch["entry"][0]["response"]["status"], "201 Created");
    let batch_id = batch["entry"][0]["resource"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(batch_id, "b1");
    assert_eq!(batch["entry"][1]["resource"]["total"], 1);
    let mut stale = patient("p1", true, "Ann");
    stale["meta"] = json!({"versionId": "999"});
    let failed = apply(
        &harness,
        "Bundle/batch",
        json!({"resource": {
            "resourceType": "Bundle",
            "type": "batch",
            "entry": [
                {"request": {"method": "GET", "url": "Patient/missing"}},
                {"request": {"method": "PUT", "url": "Patient/p1"}, "resource": stale},
                {"request": {"method": "DELETE", "url": "Patient?name=Ann"}}
            ]
        }}),
    );
    assert_eq!(failed["entry"][0]["response"]["status"], "404 Not Found");
    assert_eq!(
        failed["entry"][0]["response"]["outcome"]["issue"][0]["code"],
        "not-found"
    );
    assert_eq!(failed["entry"][1]["response"]["status"], "409 Conflict");
    assert_eq!(
        failed["entry"][1]["response"]["outcome"]["issue"][0]["code"],
        "conflict"
    );
    assert_eq!(
        failed["entry"][2]["response"]["status"],
        "412 Precondition Failed"
    );
    assert_eq!(
        failed["entry"][2]["response"]["outcome"]["issue"][0]["code"],
        "conflict"
    );
    let stored = apply(&harness, "Patient/read", json!({"id": batch_id}));
    assert_eq!(stored["name"][0]["family"], "Batch");

    let urn = apply(
        &harness,
        "Bundle/transaction",
        json!({"resource": {
            "resourceType": "Bundle",
            "type": "transaction",
            "entry": [
                {
                    "fullUrl": "urn:uuid:11111111-1111-1111-1111-111111111111",
                    "request": {"method": "POST", "url": "Patient"},
                    "resource": {"resourceType": "Patient", "active": true, "name": [{"family": "Urn"}]}
                },
                {
                    "request": {"method": "POST", "url": "Observation"},
                    "resource": {
                        "resourceType": "Observation",
                        "subject": {"reference": "urn:uuid:11111111-1111-1111-1111-111111111111"}
                    }
                }
            ]
        }}),
    );
    assert_eq!(urn["type"], "transaction-response");
    let obs_id = urn["entry"][1]["resource"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let obs = apply(&harness, "Observation/read", json!({"id": obs_id}));
    assert!(obs["subject"]["reference"]
        .as_str()
        .unwrap()
        .starts_with("Patient/"));
}

#[test]
fn meta_everything_and_instance_validate() {
    let harness = ready();
    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    );
    apply(
        &harness,
        "Observation/create",
        json!({"resource": observation("o1", "p1", "2020-01-02", "8480-6")}),
    );
    let added = apply(
        &harness,
        "Patient/$meta-add",
        json!({"id": "p1", "resource": {"meta": {"tag": [{"system": "http://example.org/tags", "code": "vip"}]}}}),
    );
    assert_eq!(added["parameter"][0]["valueMeta"]["tag"][0]["code"], "vip");
    let current = apply(&harness, "Patient/$meta", json!({"id": "p1"}));
    assert_eq!(
        current["parameter"][0]["valueMeta"]["tag"][0]["code"],
        "vip"
    );
    let removed = apply(
        &harness,
        "Patient/$meta-delete",
        json!({"id": "p1", "resource": {"meta": {"tag": [{"system": "http://example.org/tags", "code": "vip"}]}}}),
    );
    assert!(removed["parameter"][0]["valueMeta"].get("tag").is_none());

    let everything = apply(&harness, "Patient/$everything", json!({"id": "p1"}));
    assert!(everything["total"].as_u64().unwrap() >= 2);

    let validated = apply(
        &harness,
        "Patient/$validate",
        json!({"id": "p1", "profile": "http://hl7.org/fhir/StructureDefinition/Patient"}),
    );
    assert_eq!(validated["resourceType"], "OperationOutcome");

    let document = run_op(&harness, "Observation/$document", &json!({"id": "o1"})).unwrap_err();
    assert!(matches!(
        document,
        RunError::Fhir(FhirError::Unsupported(_))
    ));
}

#[test]
fn sdc_capability_populate_and_completed_response() {
    let harness = ready();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, sdc_dir()).unwrap();
    let statement = harness.catalog.read().unwrap().capability_statement();
    let instantiates = statement["instantiates"].to_string();
    assert!(instantiates.contains("sdc-form-fill-manager"));
    let questionnaire = statement["rest"][0]["resource"]
        .as_array()
        .unwrap()
        .iter()
        .find(|resource| resource["type"] == "Questionnaire")
        .unwrap();
    let names: Vec<_> = questionnaire["operation"]
        .as_array()
        .unwrap()
        .iter()
        .map(|operation| operation["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"populate"));
    assert!(names.contains(&"assemble"));
    assert!(questionnaire.to_string().contains("text/fhirpath"));
    assert!(questionnaire.to_string().contains("text/cql-expression"));

    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ada")}),
    );
    apply(
        &harness,
        "Observation/create",
        json!({"resource": {
            "resourceType": "Observation",
            "status": "final",
            "code": {"coding": [{"system": "http://loinc.org", "code": "29463-7"}]},
            "subject": {"reference": "Patient/p1"},
            "effectiveDateTime": "2099-01-01T00:00:00Z",
            "valueString": "70"
        }}),
    );
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
                "valueDuration": {"value": 1, "unit": "a", "system": "http://unitsofmeasure.org", "code": "a"}
            }]
        }]
    });
    let populated = apply(
        &harness,
        "Questionnaire/$populate",
        json!({"resource": form.clone(), "subject": "Patient/p1"}),
    );
    assert_eq!(populated["item"][0]["answer"][0]["valueString"], "70");
    let view = apply(
        &harness,
        "Questionnaire/$behavior",
        json!({"resource": {
            "questionnaire": form,
            "response": {"resourceType": "QuestionnaireResponse", "status": "in-progress", "item": [{
                "linkId": "weight",
                "answer": [{"valueString": "70"}]
            }]}
        }}),
    );
    assert_eq!(view["enabled"]["weight"], true);

    apply(
        &harness,
        "Questionnaire/create",
        json!({"resource": {
            "resourceType": "Questionnaire",
            "url": "http://example.org/Questionnaire/required",
            "status": "active",
            "item": [{"linkId": "name", "type": "string", "required": true}]
        }}),
    );
    let rejected = run_op(
        &harness,
        "QuestionnaireResponse/create",
        &json!({"resource": {
            "resourceType": "QuestionnaireResponse",
            "status": "completed",
            "questionnaire": "http://example.org/Questionnaire/required"
        }}),
    )
    .unwrap_err();
    assert!(matches!(rejected, RunError::Fhir(FhirError::Outcome(_))));
}

#[test]
fn bundle_store_uses_caller_read_scopes() {
    let harness = ready();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, sdc_dir()).unwrap();
    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ada")}),
    );
    let denied = run_op(
        &harness,
        "Bundle/transaction",
        &json!({
            "_readScopes": ["user/QuestionnaireResponse.write"],
            "resource": launch_bundle()
        }),
    )
    .unwrap_err();
    let message = denied.to_string();
    assert!(
        message.contains("unknown variable"),
        "write scopes must not read Patient during store: {message}"
    );

    let allowed = apply(
        &harness,
        "Bundle/transaction",
        json!({
            "_readScopes": ["user/Patient.read"],
            "resource": launch_bundle()
        }),
    );
    assert_eq!(
        allowed["entry"][0]["resource"]["item"][0]["answer"][0]["valueString"],
        "secret"
    );
}

#[test]
fn patch_checks_completed_responses_and_archiver_rejects_it() {
    let harness = ready();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, sdc_dir()).unwrap();
    apply(
        &harness,
        "Questionnaire/create",
        json!({"resource": {
            "resourceType": "Questionnaire",
            "url": "http://example.org/Questionnaire/required",
            "status": "active",
            "item": [{"linkId": "name", "type": "string", "required": true}]
        }}),
    );
    let created = apply(
        &harness,
        "QuestionnaireResponse/create",
        json!({"resource": {
            "resourceType": "QuestionnaireResponse",
            "status": "in-progress",
            "questionnaire": "http://example.org/Questionnaire/required"
        }}),
    );
    let id = created["id"].as_str().unwrap();
    let patch =
        json!({"id": id, "patch": [{"op": "replace", "path": "/status", "value": "completed"}]});
    let rejected = run_op(&harness, "QuestionnaireResponse/patch", &patch).unwrap_err();
    assert!(
        matches!(rejected, RunError::Fhir(FhirError::Outcome(_))),
        "{rejected}"
    );
    harness.catalog.write().unwrap().set_sdc_archiver(true);
    let hidden = run_op(&harness, "QuestionnaireResponse/patch", &patch).unwrap_err();
    assert!(
        matches!(hidden, RunError::Fhir(FhirError::Forbidden(_))),
        "{hidden}"
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

#[test]
fn sdc_package_installs_on_r4() {
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, sdc_dir()).unwrap();
    let statement = harness.catalog.read().unwrap().capability_statement();
    let questionnaire = statement["rest"][0]["resource"]
        .as_array()
        .unwrap()
        .iter()
        .find(|resource| resource["type"] == "Questionnaire")
        .unwrap();
    let names: Vec<_> = questionnaire["operation"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|operation| operation["name"].as_str())
        .collect();
    assert!(names.contains(&"populate"));
    assert!(!names.contains(&"extract"));
    let response = statement["rest"][0]["resource"]
        .as_array()
        .unwrap()
        .iter()
        .find(|resource| resource["type"] == "QuestionnaireResponse")
        .unwrap();
    assert!(response["operation"]
        .as_array()
        .unwrap()
        .iter()
        .any(|operation| operation["name"] == "extract"));
    let rejected = commit(
        &harness,
        "Questionnaire/create",
        json!({"resource": {
            "resourceType": "Questionnaire",
            "status": "active",
            "notAField": true
        }}),
    );
    let RunError::Fhir(FhirError::Outcome(outcome)) = rejected.unwrap_err() else {
        panic!("expected a validation outcome");
    };
    let message = outcome.to_string();
    assert!(
        message.contains("unknown element"),
        "sdc package must not wipe the R4 snapshot: {message}"
    );

    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ada")}),
    );
    let response = apply(
        &harness,
        "QuestionnaireResponse/create",
        json!({"resource": {
            "resourceType": "QuestionnaireResponse",
            "status": "in-progress",
            "author": {"reference": "Patient/p1"},
            "item": [{"linkId": "q1", "answer": [{"valueString": "secret"}]}]
        }}),
    );
    let response_id = response["id"].as_str().unwrap();
    apply(
        &harness,
        "Observation/create",
        json!({"resource": {
            "resourceType": "Observation",
            "status": "final",
            "code": {"text": "from the form"},
            "derivedFrom": [{"reference": format!("QuestionnaireResponse/{response_id}")}]
        }}),
    );
    let included = apply(
        &harness,
        "Patient/search",
        json!({"_id": "p1", "_revinclude": "QuestionnaireResponse:author"}),
    );
    assert!(bundle_has_type(&included, "QuestionnaireResponse"));
    let forward = apply(
        &harness,
        "Observation/search",
        json!({"derived-from": format!("QuestionnaireResponse/{response_id}"), "_include": "Observation:derived-from"}),
    );
    assert!(bundle_has_type(&forward, "QuestionnaireResponse"));
    // section.entry's targetProfile is Resource, so validation rejects a
    // QuestionnaireResponse reference. Persist the composition on the document
    // log so $document still has a response to walk.
    let composition_id = "comp1";
    store_composition(&harness, composition_id, response_id);
    let document = apply(
        &harness,
        "Composition/$document",
        json!({"id": composition_id}),
    );
    assert!(bundle_has_type(&document, "QuestionnaireResponse"));

    harness.catalog.write().unwrap().set_sdc_archiver(true);
    let hidden = commit(
        &harness,
        "QuestionnaireResponse/$everything",
        json!({"id": response_id}),
    );
    assert!(matches!(
        hidden.unwrap_err(),
        RunError::Fhir(FhirError::Forbidden(_))
    ));
    let meta = commit(
        &harness,
        "QuestionnaireResponse/$meta",
        json!({"id": response_id}),
    );
    assert!(matches!(
        meta.unwrap_err(),
        RunError::Fhir(FhirError::Forbidden(_))
    ));
    let validated = commit(
        &harness,
        "QuestionnaireResponse/$validate",
        json!({"id": response_id}),
    );
    assert!(matches!(
        validated.unwrap_err(),
        RunError::Fhir(FhirError::Forbidden(_))
    ));
    let document = apply(
        &harness,
        "Composition/$document",
        json!({"id": composition_id}),
    );
    assert!(!bundle_has_type(&document, "QuestionnaireResponse"));
    assert!(bundle_has_type(&document, "Composition"));
    assert!(bundle_has_type(&document, "Patient"));
    let revinclude = apply(
        &harness,
        "Patient/search",
        json!({"_id": "p1", "_revinclude": "QuestionnaireResponse:author"}),
    );
    assert!(!bundle_has_type(&revinclude, "QuestionnaireResponse"));
    let include = apply(
        &harness,
        "Observation/search",
        json!({"derived-from": format!("QuestionnaireResponse/{response_id}"), "_include": "Observation:derived-from"}),
    );
    assert!(!bundle_has_type(&include, "QuestionnaireResponse"));
    assert!(bundle_has_type(&include, "Observation"));
}

fn store_composition(harness: &Harness, id: &str, response_id: &str) {
    let mut tx = harness.db.begin();
    tx.stage_put(Put {
        tablet: TabletId::new("Composition"),
        id: id.to_string(),
        json: json!({
            "resourceType": "Composition",
            "id": id,
            "status": "final",
            "type": {"text": "form"},
            "date": "2020-01-01",
            "author": [{"reference": "Patient/p1"}],
            "title": "answers",
            "section": [{
                "entry": [{"reference": format!("QuestionnaireResponse/{response_id}")}]
            }]
        }),
        new_keys: vec![(IndexId::new("Composition/by_id"), exact_key(id))],
        old_keys: Vec::new(),
        prev_ts: None,
    })
    .unwrap();
    harness.db.commit(tx).unwrap();
}

fn bundle_has_type(bundle: &Value, type_name: &str) -> bool {
    bundle["entry"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["resource"]["resourceType"] == type_name)
}

/// Store resources as they are, in one commit, with no validation: for tests that need a shape
/// the profiles in the harness would refuse (a Composition, a loop of references).
fn store_resources(harness: &Harness, resources: Vec<Value>) {
    let mut tx = harness.db.begin();
    for resource in resources {
        let type_name = resource["resourceType"].as_str().unwrap().to_string();
        let id = resource["id"].as_str().unwrap().to_string();
        tx.stage_put(Put {
            tablet: TabletId::new(type_name.clone()),
            id: id.clone(),
            json: resource,
            new_keys: vec![(IndexId::new(format!("{type_name}/by_id")), exact_key(&id))],
            old_keys: Vec::new(),
            prev_ts: None,
        })
        .unwrap();
    }
    harness.db.commit(tx).unwrap();
}

fn composition_of(id: &str, entries: &[&str]) -> Value {
    json!({
        "resourceType": "Composition",
        "id": id,
        "status": "final",
        "type": {"text": "summary"},
        "date": "2020-01-01",
        "title": "summary",
        "section": [{
            "entry": entries.iter().map(|reference| json!({"reference": reference})).collect::<Vec<_>>()
        }]
    })
}

/// The whole R4 release, loaded once and shared by the tests that only read from it. Loading it is
/// most of what such a test costs; a test that writes builds its own with `r4_ready`.
fn shared_r4() -> &'static Harness {
    static SHARED: std::sync::OnceLock<Harness> = std::sync::OnceLock::new();
    SHARED.get_or_init(r4_ready)
}

/// A harness holding the whole R4 release, for tests that need types the small one lacks.
fn r4_ready() -> Harness {
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();
    harness
}

#[test]
fn a_document_bundle_has_what_fhir_asks_of_one_and_its_references_resolve() {
    let harness = r4_ready();
    // The Patient is reached only through the Observation, which the Composition names in three
    // forms: with a version, plain, and a contained one that is not a stored resource.
    store_resources(
        &harness,
        vec![
            patient("p1", true, "Ann"),
            json!({
                "resourceType": "Observation",
                "id": "o1",
                "status": "final",
                "code": {"text": "pressure"},
                "subject": {"reference": "Patient/p1"}
            }),
            composition_of(
                "comp1",
                &["Observation/o1/_history/1", "Observation/o1", "#note"],
            ),
        ],
    );
    let document = apply(&harness, "Composition/$document", json!({"id": "comp1"}));

    assert_eq!(document["type"], "document");
    assert!(document["identifier"]["value"]
        .as_str()
        .unwrap()
        .starts_with("urn:uuid:"));
    assert!(document["timestamp"].as_str().unwrap().ends_with('Z'));
    let entries = document["entry"].as_array().unwrap();
    let types: Vec<_> = entries
        .iter()
        .map(|entry| entry["resource"]["resourceType"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["Composition", "Observation", "Patient"]);
    let urls: Vec<_> = entries
        .iter()
        .map(|entry| entry["fullUrl"].as_str().unwrap())
        .collect();
    assert!(urls.iter().all(|url| url.starts_with("urn:uuid:")));

    let section_entries = &entries[0]["resource"]["section"][0]["entry"];
    assert_eq!(section_entries[0]["reference"], urls[1]);
    assert_eq!(section_entries[1]["reference"], urls[1]);
    assert_eq!(section_entries[2]["reference"], "#note");
    assert_eq!(entries[1]["resource"]["subject"]["reference"], urls[2]);
}

#[test]
fn a_document_holds_only_the_types_the_callers_scopes_let_them_read() {
    let harness = r4_ready();
    store_resources(
        &harness,
        vec![
            patient("p1", true, "Ann"),
            json!({"resourceType": "Observation", "id": "o1", "status": "final",
                "code": {"text": "pressure"}, "subject": {"reference": "Patient/p1"}}),
            composition_of("comp1", &["Observation/o1"]),
        ],
    );
    let types_of = |args: Value| -> Vec<String> {
        apply(&harness, "Composition/$document", args)["entry"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                entry["resource"]["resourceType"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    };
    assert_eq!(
        types_of(json!({"id": "comp1"})),
        ["Composition", "Observation", "Patient"]
    );
    assert_eq!(
        types_of(
            json!({"id": "comp1", "_readScopes": ["user/Composition.read", "user/Observation.read"]})
        ),
        ["Composition", "Observation"]
    );
    assert_eq!(
        types_of(json!({"id": "comp1", "_readScopes": ["user/*.read"]})),
        ["Composition", "Observation", "Patient"]
    );
}

#[test]
fn a_document_reads_a_loop_of_references_once() {
    let harness = r4_ready();
    let linked = |id: &str, other: &str| {
        json!({
            "resourceType": "Patient",
            "id": id,
            "link": [{"other": {"reference": format!("Patient/{other}")}, "type": "seealso"}]
        })
    };
    store_resources(
        &harness,
        vec![
            linked("a", "b"),
            linked("b", "a"),
            composition_of("comp1", &["Patient/a"]),
        ],
    );
    let document = apply(&harness, "Composition/$document", json!({"id": "comp1"}));
    assert_eq!(document["entry"].as_array().unwrap().len(), 3);
}

#[test]
fn a_document_that_would_hold_too_many_resources_is_refused_not_cut_short() {
    let harness = r4_ready();
    let mut resources = vec![composition_of("comp1", &["Patient/n0"])];
    for number in 0..crate::document::MAX_RESOURCES + 5 {
        resources.push(json!({
            "resourceType": "Patient",
            "id": format!("n{number}"),
            "link": [{"other": {"reference": format!("Patient/n{}", number + 1)}, "type": "seealso"}]
        }));
    }
    store_resources(&harness, resources);
    let refused = unprocessable(run_op(
        &harness,
        "Composition/$document",
        &json!({"id": "comp1"}),
    ));
    assert_eq!(refused["issue"][0]["code"], "too-costly");
}

#[test]
fn unindexed_populate_query_does_not_return_every_patient() {
    let harness = ready();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, sdc_dir()).unwrap();
    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ada")}),
    );
    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p2", true, "Grace")}),
    );
    let expression =
        "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-initialExpression";
    let form = json!({
        "resourceType": "Questionnaire",
        "status": "active",
        "item": [
            {
                "linkId": "named",
                "type": "reference",
                "extension": [{
                    "url": expression,
                    "valueExpression": {"language": "text/fhirpath", "expression": "Patient?identifier=http://hospital.example|mrn-p1"}
                }]
            },
            {
                "linkId": "born",
                "type": "reference",
                "extension": [{
                    "url": expression,
                    "valueExpression": {"language": "text/fhirpath", "expression": "Patient?birthdate=1980"}
                }]
            }
        ]
    });
    let populated = apply(
        &harness,
        "Questionnaire/$populate",
        json!({"resource": form}),
    );
    assert_eq!(
        populated["item"][0]["answer"][0]["valueReference"]["reference"],
        "Patient/p1"
    );
    let born = &populated["item"][1];
    assert!(
        born.get("answer").is_none()
            || born["answer"]
                .as_array()
                .is_some_and(|items| items.is_empty()),
        "birthdate is not an indexed parameter: {born}"
    );
}

#[test]
fn structure_map_translate_uses_the_installed_concept_map() {
    let harness = ready();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, sdc_dir()).unwrap();
    let map = json!({
        "resourceType": "StructureMap",
        "id": "gender",
        "status": "active",
        "group": [{
            "name": "main",
            "input": [
                {"name": "src", "mode": "source"},
                {"name": "out", "type": "QuestionnaireResponse", "mode": "target"}
            ],
            "rule": [{
                "name": "gender",
                "source": [{"context": "src", "element": "entry.resource.gender", "variable": "g"}],
                "target": [{
                    "context": "out",
                    "element": "status",
                    "transform": "translate",
                    "parameter": [
                        {"valueId": "g"},
                        {"valueString": "http://example.org/ConceptMap/gender"}
                    ]
                }]
            }]
        }]
    });
    let form = json!({
        "resourceType": "Questionnaire",
        "status": "active",
        "contained": [map],
        "extension": [{
            "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-sourceStructureMap",
            "valueCanonical": "#gender"
        }]
    });
    let ambiguous = apply(
        &harness,
        "Questionnaire/$populate",
        json!({"resource": form.clone(), "content": [{"resourceType": "Patient", "gender": "M"}]}),
    );
    assert_eq!(ambiguous["status"], "M");
    let translated = apply(
        &harness,
        "Questionnaire/$populate",
        json!({
            "resource": form,
            "content": [{
                "resourceType": "Patient",
                "gender": {"system": "http://example.org/gender-a", "code": "M"}
            }]
        }),
    );
    assert_eq!(translated["status"], "male");
}

// ---- the writer changes: a takeover, and what other processes see of a backfill ----

mod takeover {
    use super::*;
    use refhir_persistence as rp;
    use refhir_persistence::Persistence as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A store that behaves like the real one until it has been asked to move a backfill's
    /// cursor `updates` times, then fails every later update: the process died there.
    struct DiesAfter {
        inner: SqlitePersistence,
        updates_left: AtomicUsize,
    }

    impl rp::ConformanceStore for DiesAfter {
        fn catalog_version(&self) -> Result<u64, rp::PersistError> {
            self.inner.catalog_version()
        }
        fn bump_catalog_version(&self) -> Result<u64, rp::PersistError> {
            self.inner.bump_catalog_version()
        }
        fn catalog_revision(&self) -> Result<u64, rp::PersistError> {
            self.inner.catalog_revision()
        }
        fn bump_catalog_revision(&self) -> Result<u64, rp::PersistError> {
            self.inner.bump_catalog_revision()
        }
        fn terminology_version(&self) -> Result<u64, rp::PersistError> {
            self.inner.terminology_version()
        }
        fn bump_terminology_version(&self) -> Result<u64, rp::PersistError> {
            self.inner.bump_terminology_version()
        }
        fn claim_backfill(&self, id: i64) -> Result<bool, rp::PersistError> {
            self.inner.claim_backfill(id)
        }
        fn load_packages(&self) -> Result<Vec<rp::StoredPackage>, rp::PersistError> {
            self.inner.load_packages()
        }
        fn load_structure_definitions(
            &self,
        ) -> Result<Vec<rp::StoredStructureDefinition>, rp::PersistError> {
            self.inner.load_structure_definitions()
        }
        fn load_search_parameters(
            &self,
        ) -> Result<Vec<rp::StoredSearchParameter>, rp::PersistError> {
            self.inner.load_search_parameters()
        }
        fn load_terminology(&self) -> Result<Vec<rp::StoredTerminology>, rp::PersistError> {
            self.inner.load_terminology()
        }
        fn load_expansions(&self) -> Result<Vec<rp::StoredExpansion>, rp::PersistError> {
            self.inner.load_expansions()
        }
        fn ensure_local_package(&self, fhir_version: &str) -> Result<i64, rp::PersistError> {
            self.inner.ensure_local_package(fhir_version)
        }
        fn upsert_search_parameter(
            &self,
            package_id: i64,
            param: &rp::NewSearchParameter,
        ) -> Result<bool, rp::PersistError> {
            self.inner.upsert_search_parameter(package_id, param)
        }
        fn delete_search_parameter(
            &self,
            package_id: i64,
            url: &str,
        ) -> Result<(), rp::PersistError> {
            self.inner.delete_search_parameter(package_id, url)
        }
        fn upsert_terminology(
            &self,
            package_id: i64,
            row: &rp::NewTerminology,
        ) -> Result<(), rp::PersistError> {
            self.inner.upsert_terminology(package_id, row)
        }
        fn delete_terminology(&self, package_id: i64, url: &str) -> Result<(), rp::PersistError> {
            self.inner.delete_terminology(package_id, url)
        }
        fn replace_expansions(
            &self,
            package_id: i64,
            expansions: &[rp::NewExpansion],
        ) -> Result<(), rp::PersistError> {
            self.inner.replace_expansions(package_id, expansions)
        }
        fn load_terminology_for(
            &self,
            urls: &[String],
        ) -> Result<Vec<rp::StoredTerminology>, rp::PersistError> {
            self.inner.load_terminology_for(urls)
        }
        fn apply_expansions(
            &self,
            changes: &[rp::ExpansionChange],
        ) -> Result<(), rp::PersistError> {
            self.inner.apply_expansions(changes)
        }
        fn install_package(
            &self,
            package: &rp::NewPackage,
        ) -> Result<rp::InstallOutcome, rp::InstallError> {
            self.inner.install_package(package)
        }
        fn mark_base_ready(&self, package_id: i64, base: &str) -> Result<(), rp::PersistError> {
            self.inner.mark_base_ready(package_id, base)
        }
        fn complete_backfill(
            &self,
            backfill_id: i64,
            package_id: i64,
            base: &str,
        ) -> Result<(), rp::PersistError> {
            self.inner.complete_backfill(backfill_id, package_id, base)
        }
        fn set_search_parameter_status(
            &self,
            package_id: i64,
            url: &str,
            base: &str,
            status: rp::ParamStatus,
        ) -> Result<(), rp::PersistError> {
            self.inner
                .set_search_parameter_status(package_id, url, base, status)
        }
        fn insert_backfill(
            &self,
            package_id: i64,
            watermark_ts: i64,
            tablet: &str,
            urls: &[String],
        ) -> Result<i64, rp::PersistError> {
            self.inner
                .insert_backfill(package_id, watermark_ts, tablet, urls)
        }
        fn update_backfill(
            &self,
            id: i64,
            cursor_id: Option<&str>,
            done: bool,
        ) -> Result<(), rp::PersistError> {
            if self.updates_left.load(Ordering::SeqCst) == 0 {
                return Err(rp::PersistError::Definite("the process died".into()));
            }
            self.updates_left.fetch_sub(1, Ordering::SeqCst);
            self.inner.update_backfill(id, cursor_id, done)
        }
        fn load_backfills(&self) -> Result<Vec<rp::StoredBackfill>, rp::PersistError> {
            self.inner.load_backfills()
        }
    }

    fn temp_file(tag: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "refhir-takeover-{tag}-{}-{}.sqlite",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ))
    }

    fn remove(path: &Path) {
        for suffix in ["", "-wal", "-shm", ".lock"] {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            let _ = std::fs::remove_file(name);
        }
    }

    fn with_birth_date(id: &str) -> Value {
        let mut body = patient(id, true, "Ann");
        body["birthDate"] = json!("1990-01-01");
        body
    }

    fn search_total(db: &Database, catalog: &Catalog, args: Value) -> Result<u64, RunError> {
        run(db, catalog, "Patient/search", &args)
            .map(|result| result.value["total"].as_u64().unwrap())
    }

    #[test]
    fn a_takeover_finishes_the_backfill_the_old_primary_left_half_done() {
        let path = temp_file("resume");
        let patients = 40;
        let job_id = {
            // The first primary: queues a backfill over 40 patients, runs two batches of it,
            // then dies before the third.
            let store = SqlitePersistence::open(&path).unwrap();
            assert!(matches!(
                store.try_acquire_writer_lease().unwrap(),
                rp::Lease::Held { epoch: 1 }
            ));
            let db = Database::open(Arc::new(store.clone())).unwrap();
            let catalog = std::sync::RwLock::new(Catalog::empty());
            install_package_dir(&store, &db, &catalog, minir4_dir()).unwrap();
            for n in 0..patients {
                // Ids that sort in creation order, which a client's create (it assigns the id)
                // would not give: the test reads the backfill cursor as a position among them.
                let id = format!("p{n:03}");
                let loaded = catalog.read().unwrap();
                crate::import_resource(&db, &loaded, &with_birth_date(&id)).unwrap();
            }
            let pending = stage_package_deferred(&store, &db, &catalog, extra_dir())
                .unwrap()
                .expect("the package has pending search parameters");
            publish(&store, &catalog).unwrap();
            pending.create(&store, &db).unwrap();
            let crashing = DiesAfter {
                inner: store.clone(),
                updates_left: AtomicUsize::new(2),
            };
            let died = run_backfills(&crashing, &db, &catalog);
            assert!(died.is_err(), "the backfill was meant to be interrupted");
            let jobs: Vec<_> = store
                .load_backfills()
                .unwrap()
                .into_iter()
                .filter(|job| !job.done)
                .collect();
            assert_eq!(jobs.len(), 1, "{jobs:?}");
            let cursor = jobs[0]
                .cursor_id
                .clone()
                .expect("two batches were recorded");
            assert!(
                cursor.as_str() > "p000" && cursor.as_str() < "p039",
                "half way: {cursor}"
            );
            jobs[0].id
            // Dropping the store, database and catalog here ends this process.
        };

        // The standby takes the lease and starts up as the primary: it builds its own catalog
        // and picks the unfinished job up from the cursor.
        let store = SqlitePersistence::open(&path).unwrap();
        assert!(matches!(
            store.try_acquire_writer_lease().unwrap(),
            rp::Lease::Held { epoch: 2 }
        ));
        let db = Database::open(Arc::new(store.clone())).unwrap();
        let (loaded, _) = Catalog::open_consistent(&store, crate::CatalogClock::Latest).unwrap();
        let catalog = std::sync::RwLock::new(loaded);
        let before = search_total(
            &db,
            &catalog.read().unwrap(),
            json!({"birthdate": "1990-01-01"}),
        );
        assert!(
            before.is_err(),
            "the parameter is not ready while the backfill is unfinished"
        );

        run_backfills(&store, &db, &catalog).unwrap();
        let job = store
            .load_backfills()
            .unwrap()
            .into_iter()
            .find(|job| job.id == job_id)
            .unwrap();
        assert!(job.done);
        assert_eq!(
            job.claimed_epoch,
            Some(2),
            "the claim moved to the writer that finished the job"
        );
        let found = search_total(
            &db,
            &catalog.read().unwrap(),
            json!({"birthdate": "1990-01-01"}),
        )
        .unwrap();
        assert_eq!(found, patients, "every patient is indexed, none twice");
        remove(&path);
    }

    #[test]
    fn a_standby_that_loaded_its_catalog_while_waiting_is_current_when_it_takes_over() {
        let primary = ready();
        let store: Arc<dyn rp::ConformanceStore> = Arc::new(primary.store.clone());

        // The standby loads the catalog while it waits, and watches it. The watcher is slow
        // here on purpose, so it has not yet noticed what the primary does next.
        let (loaded, version) =
            Catalog::open_consistent(store.as_ref(), crate::CatalogClock::Latest).unwrap();
        let warm = Arc::new(std::sync::RwLock::new(loaded));
        let watcher = crate::package::watch_catalog(
            store.clone(),
            &warm,
            version,
            std::time::Duration::from_secs(3600),
            crate::CatalogClock::Latest,
        );
        let types = |catalog: &std::sync::RwLock<Catalog>| {
            catalog.read().unwrap().capability_statement()["rest"][0]["resource"]
                .as_array()
                .unwrap()
                .len()
        };
        let before = types(&warm);

        // The primary installs another package and then dies.
        install_package_dir(&primary.store, &primary.db, &primary.catalog, extra_dir()).unwrap();
        assert!(types(&primary.catalog) > before);
        assert_eq!(types(&warm), before, "the standby has not seen it yet");

        // Taking over: stop watching, then catch up once.
        let seen = watcher.stop();
        assert_eq!(seen, version, "the slow watcher never reloaded");
        let now = crate::package::reload_catalog(
            store.as_ref(),
            &warm,
            seen,
            crate::CatalogClock::Latest,
        )
        .unwrap();
        assert!(now.clock > version.clock);
        assert_eq!(types(&warm), types(&primary.catalog));

        // When nothing changed there is nothing to do: the same version comes back and the
        // catalog is not rebuilt.
        let kept = warm.read().unwrap().capability_statement();
        let again =
            crate::package::reload_catalog(store.as_ref(), &warm, now, crate::CatalogClock::Latest)
                .unwrap();
        assert_eq!(again, now);
        assert_eq!(warm.read().unwrap().capability_statement(), kept);
    }

    #[test]
    fn the_version_replicas_follow_moves_only_after_a_backfill_completes() {
        use crate::CatalogClock::{Latest, Stable};
        let db = ready();
        for n in 0..5 {
            apply(
                &db,
                "Patient/create",
                json!({"resource": with_birth_date(&format!("p{n}"))}),
            );
        }
        let version = || db.store.catalog_version().unwrap();
        let revision = || db.store.catalog_revision().unwrap();
        let (version_before, revision_before) = (version(), revision());

        // What a replica loaded before the install: it follows the stable version.
        let (replica_catalog, replica_at) = Catalog::open_consistent(&db.store, Stable).unwrap();
        let replica_catalog = std::sync::RwLock::new(replica_catalog);
        assert_eq!(replica_at.clock, version_before);

        // The package is installed and published, but its backfill has not run.
        let pending = stage_package_deferred(&db.store, &db.db, &db.catalog, extra_dir())
            .unwrap()
            .expect("the package has pending search parameters");
        publish(&db.store, &db.catalog).unwrap();
        pending.create(&db.store, &db.db).unwrap();

        // Writers are told: the revision moved. Replicas are not: the version did not, so a
        // replica keeps its catalog and never loads a parameter whose index is incomplete.
        assert!(revision() > revision_before, "a standby sees every change");
        assert_eq!(version(), version_before, "a replica is not told yet");
        let kept = crate::package::reload_catalog(&db.store, &replica_catalog, replica_at, Stable)
            .unwrap();
        assert_eq!(kept, replica_at, "nothing to reload for a replica");
        assert!(
            search_total(
                &db.db,
                &replica_catalog.read().unwrap(),
                json!({"birthdate": "1990-01-01"})
            )
            .is_err(),
            "the replica's catalog has no such parameter"
        );
        // A standby loading the latest sees the pending parameter, but cannot search it either.
        let (standby, _) = Catalog::open_consistent(&db.store, Latest).unwrap();
        assert!(standby.has_pending_parameters());
        assert!(search_total(&db.db, &standby, json!({"birthdate": "1990-01-01"})).is_err());

        // Completing the backfill publishes again, now with nothing pending, and that is what
        // moves the version.
        run_backfills(&db.store, &db.db, &db.catalog).unwrap();
        assert!(
            version() > version_before,
            "the version moves on completion"
        );
        let now = crate::package::reload_catalog(&db.store, &replica_catalog, replica_at, Stable)
            .unwrap();
        assert_eq!(now.clock, version());
        assert!(!replica_catalog.read().unwrap().has_pending_parameters());
        assert_eq!(
            search_total(
                &db.db,
                &replica_catalog.read().unwrap(),
                json!({"birthdate": "1990-01-01"})
            )
            .unwrap(),
            5,
            "the replica can search it, and its index is complete"
        );
    }

    #[test]
    fn a_change_with_nothing_to_backfill_moves_the_version_at_once() {
        let db = ready();
        let before = db.store.catalog_version().unwrap();
        let revision = db.store.catalog_revision().unwrap();
        // A package with no search parameters has nothing pending, so it is stable at once.
        let plain = dep_package("example.plain", "1.0.0", &[]);
        install_package_dir(&db.store, &db.db, &db.catalog, &plain).unwrap();
        assert!(db.store.catalog_version().unwrap() > before);
        assert!(db.store.catalog_revision().unwrap() > revision);
    }

    const PETS: &str = "http://example.org/pets";
    const ANIMALS: &str = "http://example.org/animals";

    /// What a follower had to do to catch up.
    #[derive(Debug, PartialEq)]
    enum Reload {
        Nothing,
        Terminology,
        Everything,
    }

    /// A process that follows one of the clocks, as a replica (`Stable`) or a standby (`Latest`)
    /// does: the catalog it serves from, and what it loaded it at.
    struct Follower {
        clock: crate::CatalogClock,
        catalog: std::sync::RwLock<Catalog>,
        at: crate::CatalogStamp,
    }

    impl Follower {
        fn start(store: &dyn rp::ConformanceStore, clock: crate::CatalogClock) -> Self {
            let (catalog, at) = Catalog::open_consistent(store, clock).unwrap();
            Self {
                clock,
                catalog: std::sync::RwLock::new(catalog),
                at,
            }
        }

        /// What the watcher does when it sees the counters differ from the ones it loaded at.
        /// The `Reload` is what the stamps say moved, not what was read: a test that cares counts
        /// the reads with a `WatchedStore`.
        fn catch_up(&mut self, store: &dyn rp::ConformanceStore) -> Reload {
            let now =
                crate::package::reload_catalog(store, &self.catalog, self.at, self.clock).unwrap();
            let reload = if now == self.at {
                Reload::Nothing
            } else if now.clock == self.at.clock {
                Reload::Terminology
            } else {
                Reload::Everything
            };
            self.at = now;
            reload
        }

        fn pets(&self) -> Option<Vec<String>> {
            codes_of_pets(&self.catalog)
        }
    }

    /// The codes `catalog` expands the pets value set to, or `None` when it has no such value set.
    fn codes_of_pets(catalog: &std::sync::RwLock<Catalog>) -> Option<Vec<String>> {
        let expanded = catalog.read().unwrap().terms().expand(PETS, None)?;
        let mut codes: Vec<String> = expanded["expansion"]["contains"]
            .as_array()?
            .iter()
            .map(|entry| entry["code"].as_str().unwrap().to_string())
            .collect();
        codes.sort();
        Some(codes)
    }

    fn animals(codes: &[&str]) -> Value {
        json!({
            "resourceType": "CodeSystem", "url": ANIMALS, "status": "active",
            "content": "complete",
            "concept": codes.iter().map(|code| json!({"code": code})).collect::<Vec<_>>()
        })
    }

    fn pets() -> Value {
        json!({
            "resourceType": "ValueSet", "url": PETS, "status": "active",
            "compose": {"include": [{"system": ANIMALS}]}
        })
    }

    /// Write a code system and a value set over it through the API, as a client would, on the
    /// primary `db` and through `store`. Returns the ids they were given.
    fn write_pets_through(
        db: &Harness,
        store: &dyn rp::ConformanceStore,
        codes: &[&str],
    ) -> Result<(String, String), RunError> {
        let id = |value: Value| value["id"].as_str().unwrap().to_string();
        let write = |path: &str, resource: Value| {
            commit_operation(
                store,
                &db.db,
                &db.catalog,
                path,
                &json!({"resource": resource}),
            )
            .map(|result| id(result.value))
        };
        Ok((
            write("CodeSystem/create", animals(codes))?,
            write("ValueSet/create", pets())?,
        ))
    }

    fn write_pets(db: &Harness, codes: &[&str]) -> (String, String) {
        write_pets_through(db, &db.store, codes).unwrap()
    }

    fn with_resource_types() -> Harness {
        let db = ready();
        install_resource_types(&db);
        db
    }

    fn counters(db: &Harness) -> (u64, u64, u64) {
        (
            db.store.catalog_version().unwrap(),
            db.store.catalog_revision().unwrap(),
            db.store.terminology_version().unwrap(),
        )
    }

    #[test]
    fn a_replica_hears_of_terminology_written_through_the_api() {
        let db = with_resource_types();
        let mut replica = Follower::start(&db.store, crate::CatalogClock::Stable);
        assert_eq!(replica.pets(), None);

        write_pets(&db, &["dog", "cat"]);
        assert_eq!(
            codes_of_pets(&db.catalog),
            Some(vec!["cat".into(), "dog".into()])
        );

        // The primary changed its own catalog in place and rewrote the expansions it touched, and
        // that is all the replica could see of it: it follows counters, and nothing moved them.
        // Only the terminology changed, so it loads only that.
        assert_eq!(replica.catch_up(&db.store), Reload::Terminology);
        assert_eq!(replica.pets(), codes_of_pets(&db.catalog));
        assert_eq!(replica.catch_up(&db.store), Reload::Nothing);
    }

    #[test]
    fn a_standby_hears_of_terminology_written_through_the_api() {
        let db = with_resource_types();
        let mut standby = Follower::start(&db.store, crate::CatalogClock::Latest);
        assert_eq!(standby.pets(), None);

        write_pets(&db, &["dog", "cat"]);
        assert_eq!(standby.catch_up(&db.store), Reload::Terminology);
        assert_eq!(standby.pets(), codes_of_pets(&db.catalog));
    }

    #[test]
    fn an_edit_or_a_deletion_of_terminology_reaches_replicas_and_standbys_too() {
        let db = with_resource_types();
        let (system, set) = write_pets(&db, &["dog"]);
        let mut replica = Follower::start(&db.store, crate::CatalogClock::Stable);
        let mut standby = Follower::start(&db.store, crate::CatalogClock::Latest);
        assert_eq!(replica.pets(), Some(vec!["dog".into()]));

        // A code added to a code system changes the expansion of the value set over it.
        let mut edited = animals(&["dog", "cat"]);
        edited["id"] = json!(system);
        commit(
            &db,
            "CodeSystem/update",
            json!({"id": system, "resource": edited}),
        )
        .unwrap();
        for follower in [&mut replica, &mut standby] {
            assert_eq!(follower.catch_up(&db.store), Reload::Terminology);
            assert_eq!(follower.pets(), Some(vec!["cat".into(), "dog".into()]));
        }

        // A value set that is deleted is gone from them too.
        commit(&db, "ValueSet/delete", json!({"id": set})).unwrap();
        assert_eq!(codes_of_pets(&db.catalog), None);
        for follower in [&mut replica, &mut standby] {
            assert_eq!(follower.catch_up(&db.store), Reload::Terminology);
            assert_eq!(follower.pets(), None);
        }
    }

    #[test]
    fn a_terminology_write_moves_the_terminology_counter_and_a_rebuild_moves_the_catalog_ones() {
        let db = with_resource_types();
        let watched = WatchedStore::new(db.store.clone());
        let write = |resource: Value| {
            watched_commit(
                &db,
                &watched,
                "ValueSet/create",
                json!({"resource": resource}),
            )
            .unwrap()
        };
        let colors = |id: &str| color_value_set(id, &["a"], "http://example.org/s");

        // Only what the change touches is recomputed and the live catalog is changed in place, so
        // the others are told the terminology changed, and nothing else.
        let (version, revision, terminology) = counters(&db);
        write(colors("incremental"));
        assert_eq!(counters(&db), (version, revision, terminology + 1));

        // If that fails the store is rebuilt and the catalog loaded again, which is a publish:
        // everything is told, once.
        watched.fail_apply_expansions.store(1, Ordering::SeqCst);
        let (version, revision, terminology) = counters(&db);
        write(colors("fallback"));
        assert_eq!(counters(&db), (version + 1, revision + 1, terminology));
    }

    #[test]
    fn a_terminology_reload_reads_the_terminology_and_nothing_else() {
        let db = with_resource_types();
        let watched = WatchedStore::new(db.store.clone());
        let mut replica = Follower::start(&watched, crate::CatalogClock::Stable);
        let kept = replica.catalog.read().unwrap().capability_statement();

        write_pets_through(&db, &watched, &["dog"]).unwrap();
        watched.reset();
        assert_eq!(replica.catch_up(&watched), Reload::Terminology);
        assert_eq!(replica.pets(), Some(vec!["dog".into()]));
        assert!(WatchedStore::count(&watched.full_terminology_loads) > 0);
        assert!(WatchedStore::count(&watched.full_expansion_loads) > 0);
        assert_eq!(
            WatchedStore::count(&watched.definition_loads)
                + WatchedStore::count(&watched.full_search_parameter_loads),
            0,
            "no structure definition or search parameter was read"
        );
        assert_eq!(replica.catalog.read().unwrap().capability_statement(), kept);

        // A publish moves the clock, and then the whole catalog is read.
        publish(&watched, &db.catalog).unwrap();
        watched.reset();
        assert_eq!(replica.catch_up(&watched), Reload::Everything);
        assert!(WatchedStore::count(&watched.definition_loads) > 0);
        assert!(WatchedStore::count(&watched.full_search_parameter_loads) > 0);
        assert_eq!(replica.pets(), Some(vec!["dog".into()]));
    }

    #[test]
    fn a_transaction_of_terminology_tells_replicas_and_standbys_once() {
        let db = with_resource_types();
        let mut replica = Follower::start(&db.store, crate::CatalogClock::Stable);
        let mut standby = Follower::start(&db.store, crate::CatalogClock::Latest);
        let (version, revision, terminology) = counters(&db);

        // A code system and ten value sets in one request: one commit, so one announcement and
        // one reload, however many resources it holds. Writing them one by one is eleven.
        let mut entries = vec![json!({
            "request": {"method": "POST", "url": "CodeSystem"},
            "resource": animals(&["dog", "cat"])
        })];
        let mut set = pets();
        for n in 0..10 {
            set["url"] = json!(format!("{PETS}-{n}"));
            entries.push(json!({
                "request": {"method": "POST", "url": "ValueSet"},
                "resource": set.clone()
            }));
        }
        let bundle = json!({"resourceType": "Bundle", "type": "transaction", "entry": entries});
        commit(&db, "Bundle/transaction", json!({"resource": bundle})).unwrap();

        assert_eq!(counters(&db), (version, revision, terminology + 1));
        for follower in [&mut replica, &mut standby] {
            assert_eq!(follower.catch_up(&db.store), Reload::Terminology);
            let expanded = follower
                .catalog
                .read()
                .unwrap()
                .terms()
                .expand(&format!("{PETS}-9"), None);
            assert!(
                expanded.is_some(),
                "{:?} has the last value set",
                follower.clock
            );
        }
    }

    #[test]
    fn a_terminology_write_while_a_backfill_is_pending_reaches_both_and_loads_no_parameter() {
        let db = with_resource_types();
        for n in 0..3 {
            apply(
                &db,
                "Patient/create",
                json!({"resource": with_birth_date(&format!("p{n}"))}),
            );
        }
        // What a replica loaded before the install.
        let mut replica = Follower::start(&db.store, crate::CatalogClock::Stable);
        let pending = stage_package_deferred(&db.store, &db.db, &db.catalog, extra_dir())
            .unwrap()
            .expect("the package has pending search parameters");
        publish(&db.store, &db.catalog).unwrap();
        pending.create(&db.store, &db.db).unwrap();
        let mut standby = Follower::start(&db.store, crate::CatalogClock::Latest);
        assert!(standby.catalog.read().unwrap().has_pending_parameters());
        let (version, revision, _) = counters(&db);

        write_pets(&db, &["dog"]);

        // Nothing waits for the backfill, because nothing about a search parameter is loaded:
        // both are told at once and load the terminology, and the replica still has no parameter
        // whose index is incomplete.
        assert_eq!(counters(&db).0, version);
        assert_eq!(counters(&db).1, revision);
        assert_eq!(replica.catch_up(&db.store), Reload::Terminology);
        assert_eq!(replica.pets(), Some(vec!["dog".into()]));
        assert!(!replica.catalog.read().unwrap().has_pending_parameters());
        assert_eq!(standby.catch_up(&db.store), Reload::Terminology);
        assert_eq!(standby.pets(), Some(vec!["dog".into()]));
        assert!(standby.catalog.read().unwrap().has_pending_parameters());

        // Completing the backfill moves the version, and the replica loads the parameter.
        run_backfills(&db.store, &db.db, &db.catalog).unwrap();
        assert_eq!(replica.catch_up(&db.store), Reload::Everything);
        assert!(!replica.catalog.read().unwrap().has_pending_parameters());
        assert_eq!(replica.pets(), Some(vec!["dog".into()]));
    }

    #[test]
    fn an_announcement_that_failed_is_made_again_when_the_write_is_repaired() {
        let db = with_resource_types();
        let watched = WatchedStore::new(db.store.clone());
        let mut replica = Follower::start(&db.store, crate::CatalogClock::Stable);
        let mut standby = Follower::start(&db.store, crate::CatalogClock::Latest);

        // The write and its projection go through and the counter write after them fails. The
        // repair finds the tables in order and used to leave it there: the request succeeded and
        // nobody was told.
        watched.fail_bump_terminology.store(1, Ordering::SeqCst);
        let (system, _) = write_pets_through(&db, &watched, &["dog"]).unwrap();
        assert!(!system.is_empty());
        assert_eq!(codes_of_pets(&db.catalog), Some(vec!["dog".into()]));
        for follower in [&mut replica, &mut standby] {
            assert_ne!(
                follower.catch_up(&db.store),
                Reload::Nothing,
                "{:?} was never told",
                follower.clock
            );
            assert_eq!(follower.pets(), Some(vec!["dog".into()]));
        }

        // The same when the write fell back to a rebuild and it was publishing that failed.
        watched.fail_apply_expansions.store(1, Ordering::SeqCst);
        watched.fail_bump_catalog.store(1, Ordering::SeqCst);
        watched_commit(
            &db,
            &watched,
            "ValueSet/create",
            json!({"resource": color_value_set("rebuilt", &["a"], "http://example.org/s")}),
        )
        .unwrap();
        for follower in [&mut replica, &mut standby] {
            assert_ne!(
                follower.catch_up(&db.store),
                Reload::Nothing,
                "{:?} was never told",
                follower.clock
            );
            let rebuilt = follower
                .catalog
                .read()
                .unwrap()
                .terms()
                .expand("http://example.org/rebuilt", None);
            assert!(rebuilt.is_some(), "{:?}", follower.clock);
        }
    }

    #[test]
    fn an_announcement_that_keeps_failing_does_not_fail_the_write() {
        let db = with_resource_types();
        let watched = WatchedStore::new(db.store.clone());
        let mut replica = Follower::start(&db.store, crate::CatalogClock::Stable);

        commit(
            &db,
            "CodeSystem/create",
            json!({"resource": animals(&["dog"])}),
        )
        .unwrap();
        assert_eq!(replica.catch_up(&db.store), Reload::Terminology);

        // Nothing can be announced, not even by the repair. The write is committed and right on
        // the primary, so the request succeeds: a client that retried a failed one would write
        // it twice. Only the replica is not told.
        watched.fail_bump_terminology.store(1, Ordering::SeqCst);
        watched.fail_bump_catalog.store(100, Ordering::SeqCst);
        watched_commit(
            &db,
            &watched,
            "ValueSet/create",
            json!({"resource": pets()}),
        )
        .unwrap();
        assert_eq!(codes_of_pets(&db.catalog), Some(vec!["dog".into()]));
        assert_eq!(replica.catch_up(&db.store), Reload::Nothing);
        assert_eq!(replica.pets(), None);

        // Any later announcement brings the replica to everything written since.
        watched.fail_bump_catalog.store(0, Ordering::SeqCst);
        watched_commit(
            &db,
            &watched,
            "ValueSet/create",
            json!({"resource": color_value_set("later", &["a"], "http://example.org/s")}),
        )
        .unwrap();
        assert_eq!(replica.catch_up(&db.store), Reload::Terminology);
        assert_eq!(replica.pets(), Some(vec!["dog".into()]));
    }

    #[test]
    fn a_repair_that_publishes_announces_once() {
        let db = with_resource_types();
        let watched = WatchedStore::new(db.store.clone());
        let (version, revision, terminology) = counters(&db);

        // The projection fails after the commit, so the repair finds a row missing, rebuilds and
        // publishes. That is the announcement: the repair does not make a second one.
        watched.fail_upsert_terminology.store(1, Ordering::SeqCst);
        watched_commit(
            &db,
            &watched,
            "ValueSet/create",
            json!({"resource": color_value_set("repaired", &["a"], "http://example.org/s")}),
        )
        .unwrap();
        assert_eq!(counters(&db), (version + 1, revision + 1, terminology));
    }

    #[test]
    fn a_repair_that_finds_nothing_to_fix_still_announces() {
        let db = with_resource_types();
        write_pets(&db, &["dog"]);
        let mut replica = Follower::start(&db.store, crate::CatalogClock::Stable);
        let mut standby = Follower::start(&db.store, crate::CatalogClock::Latest);
        let (version, revision, terminology) = counters(&db);

        // What a primary does as it starts: the process before it may have stopped after writing
        // and before telling anyone, and the tables alone cannot say whether it did.
        repair_conformance(&db.store, &db.db, &db.catalog).unwrap();
        assert_eq!(counters(&db), (version + 1, revision + 1, terminology));
        for follower in [&mut replica, &mut standby] {
            assert_eq!(follower.catch_up(&db.store), Reload::Everything);
        }
    }

    #[test]
    fn a_publish_that_lands_while_terminology_is_loading_is_loaded_afterwards() {
        let db = with_resource_types();
        let watched = WatchedStore::new(db.store.clone());
        let mut replica = Follower::start(&watched, crate::CatalogClock::Stable);
        write_pets(&db, &["dog"]);

        // A publish lands while the replica reads the terminology: here only its version bump.
        let store = db.store.clone();
        *watched.on_terminology_load.lock().unwrap() = Some(Box::new(move || {
            store.bump_catalog_version().unwrap();
        }));
        watched.reset();
        assert_eq!(replica.catch_up(&watched), Reload::Terminology);
        assert_eq!(WatchedStore::count(&watched.definition_loads), 0);

        // The clock that moved was not read into the catalog, so it is not recorded as loaded:
        // the next call loads everything, and the one after finds nothing to do.
        watched.reset();
        assert_eq!(replica.catch_up(&watched), Reload::Everything);
        assert!(WatchedStore::count(&watched.definition_loads) > 0);
        assert_eq!(replica.catch_up(&watched), Reload::Nothing);
    }

    #[test]
    fn the_watcher_loads_the_terminology_for_a_terminology_write_and_everything_for_a_package() {
        let db = with_resource_types();
        let watched = Arc::new(WatchedStore::new(db.store.clone()));
        let (loaded, started) =
            Catalog::open_consistent(watched.as_ref(), crate::CatalogClock::Stable).unwrap();
        let catalog = Arc::new(std::sync::RwLock::new(loaded));
        let store: Arc<dyn rp::ConformanceStore> = watched.clone();
        let watcher = crate::package::watch_catalog(
            store,
            &catalog,
            started,
            std::time::Duration::from_millis(20),
            crate::CatalogClock::Stable,
        );
        let resource_types = || {
            catalog.read().unwrap().capability_statement()["rest"][0]["resource"]
                .as_array()
                .unwrap()
                .len()
        };
        let until = |what: &str, done: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !done() {
                assert!(std::time::Instant::now() < deadline, "never saw {what}");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        // A terminology write: the watcher reads the terminology and nothing else.
        watched.reset();
        write_pets(&db, &["dog"]);
        until("the terminology", &|| {
            codes_of_pets(&catalog) == Some(vec!["dog".into()])
        });
        assert_eq!(WatchedStore::count(&watched.definition_loads), 0);
        assert_eq!(WatchedStore::count(&watched.full_search_parameter_loads), 0);

        // A package moves the clock: it reads everything.
        let types = resource_types();
        install_package_dir(&db.store, &db.db, &db.catalog, extra_dir()).unwrap();
        until("the package", &|| resource_types() > types);
        assert!(WatchedStore::count(&watched.definition_loads) > 0);

        let stopped = watcher.stop();
        assert_eq!(stopped.clock, db.store.catalog_version().unwrap());
        assert_eq!(stopped.terminology, db.store.terminology_version().unwrap());
        assert!(stopped.clock > started.clock && stopped.terminology > started.terminology);
    }
}

/// Search builds its page lazily: only the ids of the index range, only the page's documents, no
/// second check of what the index already decided, and no copy of every match. These tests hold
/// that to the old behaviour (the whole result, byte for byte, and the reads it records) and to an
/// oracle that does not use either implementation, and then count the work.
mod search_paging {
    use super::*;
    use crate::extract::compile_parameter;
    use crate::search::{search_counted, search_reference, Handling, Work};

    const FAMILIES: [&str; 8] = [
        "Smith",
        "Smythe",
        "smith",
        "Jones",
        "O'Neil",
        "Müller",
        "Smith-Jones",
        "Nguyen",
    ];

    fn person(n: usize, family: Option<&str>) -> Value {
        let gender = ["male", "female", "other", "unknown"][n % 4];
        let given = ["Ann", "Bob", "Cy"][n % 3];
        let mut resource = json!({
            "resourceType": "Patient",
            "id": format!("p{n}"),
            "active": n % 3 != 0,
            "gender": gender,
            "birthDate": format!("{}-{:02}-{:02}", 1950 + (n % 60), 1 + (n % 12), 1 + (n % 28)),
            "identifier": [{"system": "http://hospital.example", "value": format!("mrn-{n}")}]
        });
        if let Some(family) = family {
            let mut names = vec![json!({"family": family, "given": [given]})];
            if n % 7 == 0 {
                // Two names that both start with "Sm": the document is in the range twice.
                names.push(json!({"family": "Smithers", "given": ["Zed"]}));
            }
            resource["name"] = json!(names);
        }
        resource
    }

    /// Patients p0..p95, some then changed or deleted, and observations about the first forty.
    /// Returns the harness and what the patients should look like now, for the oracle.
    fn seeded() -> (Harness, BTreeMap<String, Value>) {
        let harness = r4();
        let mut now = BTreeMap::new();
        for n in 0..96 {
            let family = (n % 11 != 0).then(|| FAMILIES[n % FAMILIES.len()]);
            let resource = person(n, family);
            apply(&harness, "Patient/create", json!({"resource": resource}));
            now.insert(format!("p{n}"), resource);
        }
        // Rows an index range is wider than the match for: a date on the boundary of a bound, and a
        // family that is a prefix of a query containing a NUL, in a document whose id continues it.
        for (id, birth, family) in [
            ("p96", "1960-06-01", "Smith"),
            ("p97", "1960-06-02", "Smith"),
            ("b1", "1970-01-01", "a"),
        ] {
            let resource = json!({
                "resourceType": "Patient", "id": id, "birthDate": birth,
                "name": [{"family": family}]
            });
            apply(&harness, "Patient/create", json!({"resource": resource}));
            now.insert(id.to_string(), resource);
        }
        // The index has to follow a change of name and a delete, not keep the old rows.
        for (n, family) in [(1, "Smith"), (9, "Smith"), (0, "Brown"), (5, "Smith")] {
            let resource = person(n, Some(family));
            apply(
                &harness,
                "Patient/update",
                json!({"id": format!("p{n}"), "resource": resource}),
            );
            now.insert(format!("p{n}"), resource);
        }
        for id in ["p8", "p16"] {
            apply(&harness, "Patient/delete", json!({"id": id}));
            now.remove(id);
        }
        for n in (0..40).filter(|n| now.contains_key(&format!("p{n}"))) {
            let code = ["8867-4", "29463-7"][n % 2];
            apply(
                &harness,
                "Observation/create",
                json!({"resource": {
                    "resourceType": "Observation",
                    "id": format!("o{n}"),
                    "status": if n % 2 == 0 { "final" } else { "amended" },
                    "code": {"coding": [{"system": "http://loinc.org", "code": code}]},
                    "subject": {"reference": format!("Patient/p{n}")},
                    "effectiveDateTime": format!("2024-{:02}-10T08:00:00Z", 1 + (n % 12))
                }}),
            );
        }
        (harness, now)
    }

    fn params(query: &[(&str, &str)]) -> BTreeMap<String, String> {
        query
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    /// Runs the query with the current search and with the reference one on the same snapshot.
    /// They must agree on the bundle (or on the error) and on what they read.
    fn both(harness: &Harness, type_name: &str, query: &[(&str, &str)]) -> Option<(Value, Work)> {
        let catalog = harness.catalog.read().expect("catalog");
        let model = catalog.get(type_name).expect("model");
        let params = params(query);
        let mut current_tx = harness.db.begin();
        let current = search_counted(&mut current_tx, &catalog, model, &params, Handling::Strict);
        let mut reference_tx = harness.db.begin();
        let reference = search_reference(&mut reference_tx, &catalog, model, &params);
        match (current, reference) {
            (Ok((bundle, work)), Ok(expected)) => {
                assert_eq!(
                    bundle, expected,
                    "the bundle differs for {type_name} {query:?}"
                );
                assert_eq!(
                    current_tx.read_set(),
                    reference_tx.read_set(),
                    "the reads differ for {type_name} {query:?}"
                );
                Some((bundle, work))
            }
            (Err(current), Err(reference)) => {
                assert_eq!(
                    current.to_string(),
                    reference.to_string(),
                    "the error differs for {type_name} {query:?}"
                );
                None
            }
            (current, reference) => panic!(
                "one succeeded and one failed for {type_name} {query:?}: {:?} / {:?}",
                current.map(|(bundle, _)| bundle),
                reference
            ),
        }
    }

    const PATIENT_QUERIES: &[&[(&str, &str)]] = &[
        &[],
        &[("family", "Smith")],
        &[("family", "sm")],
        &[("family", "SMITH")],
        &[("family", "")],
        &[("family", "zzz")],
        &[("family", "Müller")],
        &[("family", "o'n")],
        &[("family", "a\0b")],
        &[("given", "Ann")],
        &[("name", "an")],
        &[("name", "smithers")],
        &[("gender", "male")],
        &[("active", "true")],
        &[("active", "false")],
        &[("identifier", "http://hospital.example|mrn-7")],
        &[("identifier", "mrn-7")],
        &[("identifier", "|mrn-7")],
        &[("identifier", "http://hospital.example|")],
        &[("identifier", "nope")],
        &[("family", "Sm"), ("gender", "male")],
        &[("family", "Sm"), ("given", "Ann"), ("active", "true")],
        &[("gender", "female"), ("birthdate", "ge1980-01-01")],
        &[("birthdate", "ge1990-01-01")],
        &[("birthdate", "1985")],
        &[("birthdate", "lt1960-06-01")],
        &[("birthdate", "le1960-06-01")],
        &[("birthdate", "gt1960-06-01")],
        &[("birthdate", "ge1960-06-02")],
        &[("birthdate", "sa1960-06-01")],
        &[("birthdate", "eb1960-06-02")],
        &[("birthdate", "ne1960-06-01")],
        &[("birthdate", "1960-06-01")],
        &[("family", "a\0b")],
        &[("family", "a")],
        &[("family:exact", "Smith")],
        &[("family:contains", "mit")],
        &[("gender:not", "male")],
        &[("family:missing", "true")],
        &[("family:missing", "false")],
        &[("_id", "p1")],
        &[("_id", "p8")],
        &[("_id", "nobody")],
        // A comma is OR: values that find different documents, overlapping ranges that find the
        // same ones, one that finds nothing, an alternative no index range can narrow, an
        // escaped comma, and an alternative empty on both sides.
        &[("family", "Smith,Jones")],
        &[("family", "sm,smith,smithers")],
        &[("family", "zzz,Nguyen")],
        &[("family", "O'Neil\\,x")],
        &[("family", "Smith,,Jones,")],
        &[("family:exact", "Smith,smith")],
        &[("family:contains", "mit,jon")],
        &[("gender", "male,female")],
        &[("gender", "other,unknown,nothing")],
        &[("gender:not", "male,female")],
        &[("active", "true,false")],
        &[("identifier", "http://hospital.example|mrn-7,mrn-9")],
        &[("birthdate", "1985,ge1990-01-01")],
        &[("birthdate", "ne1960-06-01,1985")],
        &[("_id", "p1,p2,nobody")],
        &[("_id", "p8,p1")],
        &[("family", "Smith,Jones"), ("gender", "male,female")],
        &[("family", "Sm,Jo"), ("birthdate", "ge1980-01-01")],
        // A repeated parameter is AND.
        &[("birthdate", "ge1960-01-01\u{1f}lt1970-01-01")],
        &[("gender", "male\u{1f}female")],
        &[("family", "sm\u{1f}smith,jones")],
    ];

    const PAGING: &[&[(&str, &str)]] = &[
        &[],
        &[("_count", "0")],
        &[("_count", "1")],
        &[("_count", "10")],
        &[("_count", "1000")],
        &[("_count", "7"), ("_offset", "3")],
        &[("_count", "7"), ("_offset", "1000")],
        &[("_offset", "5")],
        &[("_count", "10"), ("_total", "none")],
        &[("_count", "10"), ("_total", "estimate")],
        &[("_total", "none")],
        &[("_summary", "count")],
        &[("_summary", "count"), ("_count", "5")],
        &[("_count", "10"), ("_summary", "true")],
        &[("_count", "5"), ("_elements", "name")],
        &[("_sort", "family")],
        &[("_sort", "-family"), ("_count", "10")],
        &[
            ("_sort", "birthdate,family"),
            ("_count", "8"),
            ("_offset", "2"),
        ],
        &[("_revinclude", "Observation:subject"), ("_count", "5")],
    ];

    #[test]
    fn patient_searches_return_what_the_whole_result_search_returned() {
        let (harness, _) = seeded();
        let mut compared = 0;
        let mut with_matches = 0;
        for base in PATIENT_QUERIES {
            for paging in PAGING {
                let query: Vec<(&str, &str)> = base.iter().chain(paging.iter()).copied().collect();
                if let Some((bundle, _)) = both(&harness, "Patient", &query) {
                    compared += 1;
                    if bundle["total"].as_u64().unwrap_or(0) > 0
                        || !bundle["entry"].as_array().unwrap().is_empty()
                    {
                        with_matches += 1;
                    }
                }
            }
        }
        // The comparison is only worth something if most of it compared real answers.
        assert!(compared > 400, "only {compared} queries ran");
        assert!(
            with_matches > 250,
            "only {with_matches} queries found anything"
        );
    }

    #[test]
    fn observation_searches_return_what_the_whole_result_search_returned() {
        let (harness, _) = seeded();
        let bases: &[&[(&str, &str)]] = &[
            &[],
            &[("subject", "Patient/p3")],
            &[("subject", "p3")],
            &[("code", "http://loinc.org|8867-4")],
            &[("code", "8867-4")],
            &[("code", "|8867-4")],
            &[("status", "final")],
            &[("status", "amended"), ("subject", "Patient/p1")],
            &[("code", "http://loinc.org|8867-4"), ("status", "final")],
            &[("date", "ge2024-06-01")],
            &[("_compartment", "Patient/p3")],
            &[("subject:missing", "false")],
        ];
        let pagings: &[&[(&str, &str)]] = &[
            &[],
            &[("_count", "5")],
            &[("_count", "5"), ("_offset", "2"), ("_total", "none")],
            &[("_summary", "count")],
            &[("_include", "Observation:subject"), ("_count", "4")],
            &[("_sort", "date"), ("_count", "6")],
        ];
        let mut compared = 0;
        for base in bases {
            for paging in pagings {
                let query: Vec<(&str, &str)> = base.iter().chain(paging.iter()).copied().collect();
                if both(&harness, "Observation", &query).is_some() {
                    compared += 1;
                }
            }
        }
        assert!(compared > 50, "only {compared} queries ran");
    }

    /// The expected ids, worked out from the data the test wrote and nothing else.
    fn expected(now: &BTreeMap<String, Value>, wanted: impl Fn(&Value) -> bool) -> Vec<String> {
        let mut ids: Vec<String> = now
            .iter()
            .filter(|(_, resource)| wanted(resource))
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    fn found(harness: &Harness, query: &[(&str, &str)]) -> (Vec<String>, u64) {
        let mut query = query.to_vec();
        query.push(("_count", "1000"));
        let (bundle, _) = both(harness, "Patient", &query).expect("the search works");
        let mut ids: Vec<String> = bundle["entry"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        (ids, bundle["total"].as_u64().unwrap())
    }

    fn families(resource: &Value) -> Vec<String> {
        resource["name"]
            .as_array()
            .map(|names| {
                names
                    .iter()
                    .filter_map(|name| name["family"].as_str())
                    .map(str::to_lowercase)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn the_matches_are_the_patients_the_data_says_they_are() {
        let (harness, now) = seeded();

        // A prefix of a family name, case folded; a document with two matching names counts once;
        // a renamed patient follows its new name; a deleted one is gone.
        for prefix in [
            "smith", "sm", "s", "jones", "müller", "nguyen", "brown", "zzz",
        ] {
            let want = expected(&now, |resource| {
                families(resource)
                    .iter()
                    .any(|family| family.starts_with(prefix))
            });
            let (ids, total) = found(&harness, &[("family", prefix)]);
            assert_eq!(ids, want, "family={prefix}");
            assert_eq!(total as usize, want.len(), "total for family={prefix}");
        }

        // A token.
        let want = expected(&now, |resource| resource["gender"] == "male");
        assert_eq!(found(&harness, &[("gender", "male")]).0, want);
        let want = expected(&now, |resource| resource["active"] == false);
        assert_eq!(found(&harness, &[("active", "false")]).0, want);
        let want = expected(&now, |resource| resource["id"] == "p7");
        assert_eq!(
            found(&harness, &[("identifier", "http://hospital.example|mrn-7")]).0,
            want
        );

        // Two clauses: the index decides one and the other is checked.
        let want = expected(&now, |resource| {
            resource["gender"] == "male" && families(resource).iter().any(|f| f.starts_with("sm"))
        });
        assert_eq!(
            found(&harness, &[("family", "sm"), ("gender", "male")]).0,
            want
        );
        assert!(!want.is_empty());
    }

    #[test]
    fn a_comma_finds_what_any_of_its_values_finds_and_a_document_once() {
        let (harness, now) = seeded();
        let starts = |resource: &Value, prefixes: &[&str]| {
            families(resource)
                .iter()
                .any(|family| prefixes.iter().any(|prefix| family.starts_with(prefix)))
        };
        // Tokens: the union of the values.
        let want = expected(&now, |resource| {
            resource["gender"] == "male" || resource["gender"] == "female"
        });
        let (ids, total) = found(&harness, &[("gender", "male,female")]);
        assert_eq!(ids, want);
        assert_eq!(total as usize, want.len());
        // Strings: separate prefixes, and prefixes of each other. A patient with two names that
        // both match is one match, and is not counted twice in the total.
        for query in ["jones,nguyen", "sm,smith,smithers", "smith,zzz", "zzz,yyy"] {
            let prefixes: Vec<&str> = query.split(',').collect();
            let want = expected(&now, |resource| starts(resource, &prefixes));
            let (ids, total) = found(&harness, &[("family", query)]);
            assert_eq!(ids, want, "family={query}");
            assert_eq!(total as usize, want.len(), "total for family={query}");
        }
        // Ids; a deleted patient has none.
        let want = expected(&now, |resource| {
            ["p1", "p2"].contains(&resource["id"].as_str().unwrap())
        });
        assert_eq!(found(&harness, &[("_id", "p1,p2,nobody")]).0, want);
        assert_eq!(want.len(), 2);
        let want = expected(&now, |resource| resource["id"] == "p3");
        assert_eq!(found(&harness, &[("_id", "p8,p3")]).0, want);
        // `:not` leaves out every value, and keeps a patient that has no value at all.
        let want = expected(&now, |resource| {
            resource["gender"] != "male" && resource["gender"] != "female"
        });
        assert_eq!(found(&harness, &[("gender:not", "male,female")]).0, want);
        assert!(want.iter().any(|id| id == "p96"), "no gender: {want:?}");
        // Both clauses of two lists have to hold.
        let want = expected(&now, |resource| {
            starts(resource, &["jones", "nguyen"])
                && (resource["gender"] == "male" || resource["gender"] == "other")
        });
        assert_eq!(
            found(
                &harness,
                &[("family", "Jones,Nguyen"), ("gender", "male,other")]
            )
            .0,
            want
        );
        // A repeated parameter is AND: a range written as two bounds.
        let want = expected(&now, |resource| {
            resource["birthDate"]
                .as_str()
                .is_some_and(|date| ("1960-01-01".."1970-01-01").contains(&date))
        });
        let range = crate::join_repeated("birthdate", "ge1960-01-01", "lt1970-01-01");
        assert_eq!(found(&harness, &[("birthdate", &range)]).0, want);
        assert!(!want.is_empty());
    }

    fn work(harness: &Harness, query: &[(&str, &str)]) -> (Value, Work) {
        both(harness, "Patient", query).expect("the search works")
    }

    #[test]
    fn a_page_of_a_comma_search_reads_the_page_and_not_every_match() {
        let harness = many_smiths(300);
        // Two ranges, one of them empty; two that are the same range; both orders.
        for family in ["Smith,Jones", "Jones,Smith", "Smith,smith"] {
            let (bundle, work) = self::work(
                &harness,
                &[("family", family), ("_count", "10"), ("_total", "none")],
            );
            assert_eq!(bundle["entry"].as_array().unwrap().len(), 10, "{family}");
            assert_eq!(work.rows, 11, "{family}: the page and one row more");
            assert_eq!((work.fetched, work.decoded), (10, 10), "{family}");
            assert!(bundle["link"]
                .as_array()
                .unwrap()
                .iter()
                .any(|link| link["relation"] == "next"));
            let (bundle, work) = self::work(
                &harness,
                &[
                    ("family", family),
                    ("_count", "10"),
                    ("_offset", "200"),
                    ("_total", "none"),
                ],
            );
            assert_eq!(bundle["entry"].as_array().unwrap().len(), 10, "{family}");
            assert_eq!(
                work.rows, 211,
                "{family}: the offset, the page and one more"
            );
        }
        // With a total every row is counted, as for one value.
        let (bundle, _) = self::work(&harness, &[("family", "Smith,Jones"), ("_count", "10")]);
        assert_eq!(bundle["total"], 300);
    }

    fn many_smiths(count: usize) -> Harness {
        let harness = r4();
        for n in 0..count {
            let resource = json!({
                "resourceType": "Patient",
                "id": format!("s{n}"),
                "gender": if n % 2 == 0 { "male" } else { "female" },
                "birthDate": if n % 2 == 0 { "1990-05-05" } else { "1950-05-05" },
                "name": [{"family": "Smith"}]
            });
            apply(&harness, "Patient/create", json!({"resource": resource}));
        }
        harness
    }

    #[test]
    fn a_page_of_ten_among_many_matches_reads_ten_documents() {
        let harness = many_smiths(300);
        let (bundle, work) = work(&harness, &[("family", "Smith"), ("_count", "10")]);
        assert_eq!(bundle["total"], 300, "the total still counts every match");
        assert_eq!(bundle["entry"].as_array().unwrap().len(), 10);
        assert_eq!(work.rows, 300, "the range lists every match");
        assert_eq!(work.fetched, 10, "but only the page's documents are read");
        assert_eq!(work.decoded, 10);
        assert_eq!(work.checked, 0, "and the index already decided the match");

        let (bundle, work) = self::work(&harness, &[("family", "Smith"), ("_summary", "count")]);
        assert_eq!(bundle["total"], 300);
        assert_eq!(
            (work.fetched, work.decoded),
            (0, 0),
            "a count reads no document"
        );

        // Later pages cost the same.
        let (_, work) = self::work(
            &harness,
            &[("family", "Smith"), ("_count", "10"), ("_offset", "200")],
        );
        assert_eq!((work.fetched, work.decoded), (10, 10));

        // Without a total the range is not even listed to the end: the page and one row more,
        // which is what it takes to know there is a next page.
        let (bundle, work) = self::work(
            &harness,
            &[("family", "Smith"), ("_count", "10"), ("_total", "none")],
        );
        assert!(bundle.get("total").is_none());
        assert_eq!(
            work.rows, 11,
            "ten for the page and one to show there is more"
        );
        assert_eq!((work.fetched, work.decoded), (10, 10));
        assert!(bundle["link"]
            .as_array()
            .unwrap()
            .iter()
            .any(|link| link["relation"] == "next"));

        let (bundle, work) = self::work(
            &harness,
            &[
                ("family", "Smith"),
                ("_count", "10"),
                ("_offset", "200"),
                ("_total", "none"),
            ],
        );
        assert_eq!(work.rows, 211, "the offset, the page and one more");
        assert_eq!(bundle["entry"].as_array().unwrap().len(), 10);

        // A page that is most of the range is not read a document at a time: with no _count every
        // match is on the page, and one joined scan beats 300 queries of one row.
        for query in [
            vec![("family", "Smith")],
            vec![("family", "Smith"), ("_count", "1000")],
        ] {
            let (bundle, work) = self::work(&harness, &query);
            assert_eq!(bundle["entry"].as_array().unwrap().len(), 300, "{query:?}");
            assert_eq!(work.fetched, 0, "no point reads for {query:?}");
            assert_eq!(work.decoded, 300);
        }

        // The last page has nothing after it, and says so.
        let (bundle, work) = self::work(
            &harness,
            &[
                ("family", "Smith"),
                ("_count", "10"),
                ("_offset", "295"),
                ("_total", "none"),
            ],
        );
        assert_eq!(work.rows, 300, "the range ran out");
        assert_eq!(bundle["entry"].as_array().unwrap().len(), 5);
        assert!(!bundle["link"]
            .as_array()
            .unwrap()
            .iter()
            .any(|link| link["relation"] == "next"));
    }

    #[test]
    fn a_second_clause_is_checked_and_the_scan_stops_after_the_page_when_no_total_is_wanted() {
        // The family decides the range (a string prefix ranks above a date bound); the birth date,
        // which an index can only narrow, is checked on every row of it.
        let harness = many_smiths(300);
        let query = [
            ("family", "Smith"),
            ("birthdate", "ge1980-01-01"),
            ("_count", "10"),
        ];

        let (bundle, work) = work(&harness, &query);
        assert_eq!(bundle["total"], 150);
        assert_eq!(work.rows, 300);
        assert_eq!(work.checked, 300, "an exact total needs every row checked");
        assert_eq!(work.decoded, 300);

        let mut query = query.to_vec();
        query.push(("_total", "none"));
        let (bundle, work) = self::work(&harness, &query);
        assert!(bundle.get("total").is_none());
        assert_eq!(bundle["entry"].as_array().unwrap().len(), 10);
        assert!(
            bundle["link"]
                .as_array()
                .unwrap()
                .iter()
                .any(|link| link["relation"] == "next"),
            "it still knows there is a next page"
        );
        assert!(
            work.checked <= 25,
            "ten matches and one more, about every other row: {work:?}"
        );
        assert!(work.decoded < work.rows, "and the rest are never decoded");
    }

    #[test]
    fn only_a_plain_string_token_reference_or_uri_parameter_read_from_a_path_is_proven_by_the_index(
    ) {
        let proves = |kind: &str, expression: &str, query: &str| {
            compile_parameter(
                &json!({"type": kind, "code": "x", "expression": expression, "base": ["Patient"]}),
                &BTreeMap::new(),
            )
            .map(|extractor| extractor.index_proves(query))
        };
        assert_eq!(proves("string", "Patient.name.family", "sm"), Some(true));
        assert_eq!(proves("token", "Patient.gender", "male"), Some(true));
        assert_eq!(proves("token", "Patient.identifier", "a|b"), Some(true));
        assert_eq!(proves("uri", "Patient.name.family", "urn:x"), Some(true));
        assert_eq!(
            proves("reference", "Patient.generalPractitioner", "Practitioner/1"),
            Some(true)
        );

        // A NUL would match the one that ends every string key.
        assert_eq!(proves("string", "Patient.name.family", "a\0b"), Some(false));
        // Other kinds keep a second check: their range is only a coarse filter.
        assert_eq!(proves("date", "Patient.birthDate", "ge1990"), Some(false));
        assert_eq!(
            proves("number", "Patient.multipleBirthInteger", "2"),
            Some(false)
        );
        // Anything asked of the world when it is read can disagree with the keys saved earlier.
        for expression in [
            "Patient.generalPractitioner.resolve().id",
            "Patient.gender.where(now().toString() != '')",
            "Patient.name.family.where(today().toString() != '')",
            "Patient.name.family.where(timeOfDay().toString() != '')",
            "Patient.gender.where(conformsTo('http://example.org/p'))",
        ] {
            assert_eq!(
                proves("token", expression, "x"),
                Some(false),
                "{expression}"
            );
        }
    }
}

/// A search value that cannot be read as the type of its parameter (a date that is not one, a
/// number that is not a number) is refused with a message that names it, or, when the client asked
/// for that, left out of the search. It was once read as matching nothing, which an empty result
/// cannot be told from.
mod unreadable_values {
    use super::*;
    use crate::search::{search_handling, Handling};

    /// The harnesses the read-only tests share, since installing the R4 package is most of what
    /// each would cost. A test that writes builds its own with `seeded()`.
    fn shared() -> &'static Harness {
        static SEEDED: std::sync::OnceLock<Harness> = std::sync::OnceLock::new();
        SEEDED.get_or_init(seeded)
    }

    fn blank() -> &'static Harness {
        static BLANK: std::sync::OnceLock<Harness> = std::sync::OnceLock::new();
        BLANK.get_or_init(r4)
    }

    /// Two patients, and an observation of the first, so that a clause set aside has a visible
    /// effect: it would have matched both patients.
    fn seeded() -> Harness {
        let harness = r4();
        for (id, gender, born) in [("p1", "female", "1990-01-01"), ("p2", "male", "1985-05-05")] {
            let resource = json!({
                "resourceType": "Patient", "id": id, "gender": gender, "birthDate": born,
                "name": [{"family": "Smith"}]
            });
            apply(&harness, "Patient/create", json!({"resource": resource}));
        }
        let observation = json!({
            "resourceType": "Observation", "id": "o1", "status": "final",
            "code": {"coding": [{"system": "http://loinc.org", "code": "29463-7"}]},
            "subject": {"reference": "Patient/p1"},
            "effectiveDateTime": "2024-01-10T08:00:00Z",
            "valueQuantity": {"value": 5.4, "system": "http://unitsofmeasure.org", "code": "mg"}
        });
        apply(
            &harness,
            "Observation/create",
            json!({"resource": observation}),
        );
        harness
    }

    fn find(
        harness: &Harness,
        type_name: &str,
        query: &[(&str, &str)],
        handling: Handling,
    ) -> Result<Value, FhirError> {
        let catalog = harness.catalog.read().expect("catalog");
        let model = catalog.get(type_name).expect("model");
        let params: BTreeMap<String, String> = query
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        let mut tx = harness.db.begin();
        search_handling(&mut tx, &catalog, model, &params, handling)
    }

    fn refusal(harness: &Harness, type_name: &str, query: &[(&str, &str)]) -> String {
        match find(harness, type_name, query, Handling::Strict) {
            Err(FhirError::Invalid(message)) => message,
            other => panic!("{type_name} {query:?} was not refused: {other:?}"),
        }
    }

    fn ids(bundle: &Value) -> Vec<String> {
        bundle["entry"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["search"]["mode"] == "match")
            .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string())
            .collect()
    }

    fn outcomes(bundle: &Value) -> Vec<&Value> {
        bundle["entry"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["search"]["mode"] == "outcome")
            .map(|entry| &entry["resource"])
            .collect()
    }

    const UNREADABLE: [(&str, &str, &str); 10] = [
        ("Patient", "birthdate", "notadate"),
        ("Patient", "birthdate", "1990-02-30"),
        ("Observation", "date", "gtlater"),
        ("Observation", "value-quantity", "abc"),
        ("Observation", "code-value-quantity", "29463-7"),
        ("RiskAssessment", "probability", "nan"),
        ("Patient", "name:missing", "maybe"),
        ("Patient", "identifier:of-type", "mrn|1"),
        ("Observation", "subject:Patient.birthdate", "bad"),
        ("Patient", "_has:Observation:patient:date", "bad"),
    ];

    #[test]
    fn a_value_that_cannot_be_read_is_refused_by_name_whatever_is_stored() {
        let (empty, full) = (blank(), shared());
        for (type_name, name, value) in UNREADABLE {
            let message = refusal(empty, type_name, &[(name, value)]);
            assert!(
                message.contains(&format!("{value:?}"))
                    && message.contains(&format!("search parameter {name} on {type_name}")),
                "{message}"
            );
            assert_eq!(
                refusal(full, type_name, &[(name, value)]),
                message,
                "the answer follows the value, not the rows: {type_name} {name}"
            );
        }
        // The case that started this, word for word.
        let message = refusal(empty, "Patient", &[("birthdate", "notadate")]);
        assert!(message.starts_with("invalid value \"notadate\" for search parameter birthdate"));
        assert!(message.contains("expected a date"), "{message}");
    }

    #[test]
    fn a_value_that_can_be_read_still_searches() {
        let harness = shared();
        let run = |type_name: &str, query: &[(&str, &str)]| {
            let found = find(harness, type_name, query, Handling::Strict).unwrap();
            (ids(&found), found["total"].as_u64().unwrap())
        };
        assert_eq!(run("Patient", &[("birthdate", "ge1990-01-01")]).0, ["p1"]);
        assert_eq!(run("Patient", &[("birthdate", "1985")]).0, ["p2"]);
        assert_eq!(run("Patient", &[("birthdate", "lt1990-01-01")]).0, ["p2"]);
        assert_eq!(run("Patient", &[("name:missing", "false")]).1, 2);
        assert_eq!(run("Observation", &[("date", "2024-01")]).0, ["o1"]);
        assert_eq!(
            run("Observation", &[("date", "ge2024-01-10T08:00:00+00:00")]).0,
            ["o1"]
        );
        assert_eq!(
            run("Observation", &[("value-quantity", "5.4||mg")]).0,
            ["o1"]
        );
        assert_eq!(run("Observation", &[("value-quantity", "gt5")]).0, ["o1"]);
        assert_eq!(run("Observation", &[("value-quantity", "gt6")]).1, 0);
        assert_eq!(
            run("Observation", &[("code-value-quantity", "29463-7$5.4||mg")]).0,
            ["o1"]
        );
        assert_eq!(
            run("Patient", &[("_has:Observation:patient:date", "2024")]).0,
            ["p1"]
        );
        assert_eq!(
            run("Observation", &[("subject:Patient.birthdate", "1990")]).0,
            ["o1"]
        );
    }

    #[test]
    fn a_lenient_search_leaves_the_parameter_out_and_says_so() {
        let harness = shared();
        let query = [("gender", "female"), ("birthdate", "notadate")];
        assert!(find(harness, "Patient", &query, Handling::Strict).is_err());

        let found = find(harness, "Patient", &query, Handling::Lenient).unwrap();
        assert_eq!(ids(&found), ["p1"], "the rest of the search still holds");
        assert_eq!(found["total"], 1, "the outcome is not a match");
        let issued = outcomes(&found);
        assert_eq!(issued.len(), 1, "{found}");
        let issue = &issued[0]["issue"][0];
        assert_eq!(issue["severity"], "warning");
        assert_eq!(issue["expression"][0], "birthdate");
        let said = issue["diagnostics"].as_str().unwrap();
        assert!(
            said.contains("\"notadate\"") && said.contains("ignored"),
            "{said}"
        );
        // The self link is the search that ran.
        let links = found["link"].as_array().unwrap();
        assert_eq!(links[0]["url"], "Patient?gender=female", "{found}");

        // With every parameter read, lenient and strict are the same search.
        let clean = [("gender", "female"), ("birthdate", "1990")];
        assert_eq!(
            find(harness, "Patient", &clean, Handling::Lenient).unwrap(),
            find(harness, "Patient", &clean, Handling::Strict).unwrap()
        );
        // Every unreadable parameter is reported, and a search left with none is the whole type.
        let all = find(
            harness,
            "Patient",
            &[("birthdate", "notadate"), ("name:missing", "maybe")],
            Handling::Lenient,
        )
        .unwrap();
        assert_eq!(ids(&all), ["p1", "p2"]);
        assert_eq!(outcomes(&all)[0]["issue"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_lenient_search_leaves_out_a_chain_and_a_has_clause_it_cannot_read() {
        let harness = shared();
        let found = find(
            harness,
            "Observation",
            &[("subject:Patient.birthdate", "bad"), ("code", "29463-7")],
            Handling::Lenient,
        )
        .unwrap();
        assert_eq!(ids(&found), ["o1"]);
        assert_eq!(outcomes(&found).len(), 1);
        let found = find(
            harness,
            "Patient",
            &[("_has:Observation:patient:date", "bad"), ("gender", "male")],
            Handling::Lenient,
        )
        .unwrap();
        assert_eq!(ids(&found), ["p2"]);
        assert_eq!(outcomes(&found).len(), 1);
    }

    #[test]
    fn lenient_does_not_reach_what_is_not_a_value_to_read() {
        let harness = shared();
        for (query, said) in [
            (vec![("_count", "abc")], "_count must be an integer"),
            (
                vec![("bogus", "1")],
                "unsupported search parameter bogus on Patient",
            ),
            (vec![("_filter", "gender eq")], "invalid _filter"),
            (vec![("_cursor", "!")], "invalid _cursor"),
        ] {
            let err = find(harness, "Patient", &query, Handling::Lenient).unwrap_err();
            assert_eq!(err.to_string(), said, "{query:?}");
        }
    }

    #[test]
    fn only_the_search_a_client_asked_for_can_be_lenient() {
        let harness = shared();
        let unreadable = json!({"birthdate": "notadate"});
        // The operation a `GET` runs takes the handling from the server's own argument.
        let lenient = apply(
            harness,
            "Patient/search",
            json!({"birthdate": "notadate", "_handling": "lenient"}),
        );
        assert_eq!(ids(&lenient), ["p1", "p2"]);
        assert_eq!(outcomes(&lenient).len(), 1);
        assert!(run_op(harness, "Patient/search", &unreadable).is_err());
        let other = json!({"birthdate": "notadate", "_handling": "other"});
        assert!(run_op(harness, "Patient/search", &other).is_err());

        // Nothing else a client can send is lenient, however it asks. A conditional delete that
        // set a clause aside would match every patient, and delete the only one there is.
        let single = seeded();
        apply(&single, "Patient/delete", json!({"id": "p2"}));
        for query in ["birthdate=notadate", "_handling=lenient&birthdate=notadate"] {
            let err = run_op(
                &single,
                "Patient/conditional-delete",
                &json!({"query": query, "_handling": "lenient"}),
            )
            .unwrap_err();
            assert!(
                matches!(err, RunError::Fhir(FhirError::Invalid(_))),
                "{err}"
            );
        }
        assert_eq!(apply(&single, "Patient/search", json!({}))["total"], 1);

        // A batch entry's URL is the client's, `_handling` included.
        let bundle = json!({"resourceType": "Bundle", "type": "batch", "entry": [
            {"request": {"method": "GET", "url": "Patient?birthdate=notadate&_handling=lenient"}}
        ]});
        let answered = apply(harness, "Bundle/batch", json!({"resource": bundle}));
        let status = answered["entry"][0]["response"]["status"].as_str().unwrap();
        assert!(status.starts_with("400"), "{answered}");
    }

    #[test]
    fn a_filter_reads_its_values_like_any_other_clause() {
        let harness = shared();
        for (type_name, filter, in_param) in [
            ("Patient", "birthdate gt notadate", "in birthdate"),
            ("Patient", "birthdate pr maybe", "in birthdate"),
            (
                "Patient",
                "gender eq female and birthdate lt 1990-13-01",
                "in birthdate",
            ),
            ("Observation", "value-quantity gt abc", "in value-quantity"),
        ] {
            let message = refusal(harness, type_name, &[("_filter", filter)]);
            assert!(
                message.contains(&format!("search parameter _filter on {type_name}"))
                    && message.contains(in_param),
                "{message}"
            );
        }
        // A parameter that does not exist was once a comparison that matched nothing.
        let unknown = refusal(harness, "Patient", &[("_filter", "nosuch eq 1")]);
        assert_eq!(unknown, "unsupported search parameter nosuch on Patient");

        let readable = find(
            harness,
            "Patient",
            &[("_filter", "birthdate gt 1986-01-01")],
            Handling::Strict,
        )
        .unwrap();
        assert_eq!(ids(&readable), ["p1"]);

        // Lenient sets the whole `_filter` aside, as it does any parameter it cannot read.
        let found = find(
            harness,
            "Patient",
            &[("_filter", "birthdate gt notadate")],
            Handling::Lenient,
        )
        .unwrap();
        assert_eq!(ids(&found), ["p1", "p2"]);
        assert_eq!(outcomes(&found)[0]["issue"][0]["expression"][0], "_filter");
        assert_eq!(found["link"][0]["url"], "Patient");
    }

    #[test]
    fn each_alternative_and_each_occurrence_is_read_on_its_own() {
        use crate::search::AND;
        let harness = shared();
        let strict = |value: &str| {
            find(
                harness,
                "Patient",
                &[("birthdate", value)],
                Handling::Strict,
            )
        };
        let lenient = |value: &str| {
            find(
                harness,
                "Patient",
                &[("birthdate", value)],
                Handling::Lenient,
            )
        };

        // A comma is OR: every alternative is read, and the one that cannot be is named.
        // In the order of the index it read, which is by date here, so compare as a set.
        let mut either = ids(&strict("1990,1985").unwrap());
        either.sort();
        assert_eq!(either, ["p1", "p2"]);
        let message = strict("1990,notadate").unwrap_err().to_string();
        assert!(
            message.starts_with("invalid value \"notadate\" for search parameter birthdate"),
            "{message}"
        );
        // A parameter given twice is AND, and each occurrence is read.
        assert_eq!(ids(&strict(&format!("ge1980{AND}le1989")).unwrap()), ["p2"]);
        let message = strict(&format!("ge1980{AND}notadate"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("\"notadate\""), "{message}");

        // Lenient sets aside a value with an alternative it cannot read, all of it.
        let found = lenient("1985,notadate").unwrap();
        let mut all = ids(&found);
        all.sort();
        assert_eq!(all, ["p1", "p2"]);
        assert_eq!(found["link"][0]["url"], "Patient");
        // Of a parameter given twice it sets aside only the occurrence that cannot be read.
        let found = lenient(&format!("le1989{AND}notadate")).unwrap();
        assert_eq!(ids(&found), ["p2"]);
        assert_eq!(found["link"][0]["url"], "Patient?birthdate=le1989");
        assert_eq!(outcomes(&found).len(), 1);
    }

    #[test]
    fn a_lenient_search_pages_like_a_strict_one() {
        let harness = shared();
        let query = [
            ("name", "Smith"),
            ("birthdate", "notadate"),
            ("_count", "1"),
        ];
        let first = find(harness, "Patient", &query, Handling::Lenient).unwrap();
        assert_eq!(ids(&first).len(), 1);
        let links = first["link"].as_array().unwrap();
        let next = links
            .iter()
            .find(|link| link["relation"] == "next")
            .expect("a next link")["url"]
            .as_str()
            .unwrap();
        assert!(!next.contains("birthdate"), "{next}");
        let cursor = next.split("_cursor=").nth(1).expect("a cursor");

        // The link as it is, and the client's own query with the cursor added, give the same page.
        let mut again = query.to_vec();
        again.push(("_cursor", cursor));
        let second = find(harness, "Patient", &again, Handling::Lenient).unwrap();
        let by_link = find(
            harness,
            "Patient",
            &[("name", "Smith"), ("_count", "1"), ("_cursor", cursor)],
            Handling::Lenient,
        )
        .unwrap();
        assert_eq!(ids(&second).len(), 1);
        assert_ne!(ids(&second), ids(&first));
        assert_eq!(ids(&second), ids(&by_link));
    }

    #[test]
    fn a_lenient_count_has_no_entries_and_its_link_shows_what_ran() {
        let found = find(
            shared(),
            "Patient",
            &[("birthdate", "notadate"), ("_summary", "count")],
            Handling::Lenient,
        )
        .unwrap();
        assert_eq!(found["total"], 2);
        assert!(found["entry"].as_array().unwrap().is_empty(), "{found}");
        assert_eq!(found["link"][0]["url"], "Patient?_summary=count");
    }

    #[test]
    fn a_long_value_is_not_echoed_whole() {
        let long = "x".repeat(5000);
        let message = refusal(blank(), "Patient", &[("birthdate", long.as_str())]);
        assert!(message.len() < 400, "{} characters", message.len());
        assert!(
            message.contains(&format!("{}…", "x".repeat(64))),
            "{message}"
        );
        // A value at the limit is echoed as it is.
        let exact = "y".repeat(64);
        let message = refusal(blank(), "Patient", &[("birthdate", exact.as_str())]);
        assert!(message.contains(&format!("\"{exact}\"")), "{message}");
    }
}
#[test]
fn value_sets_that_cannot_expand_say_why() {
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();

    // The role ValueSet includes a SNOMED hierarchy; the R4 package ships SNOMED as a stub with
    // no concepts, so the filter cannot be evaluated and the HL7 roles alone would be a
    // silently partial answer.
    let roles = unprocessable(run_op(
        &harness,
        "ValueSet/$expand",
        &json!({"url": "http://hl7.org/fhir/ValueSet/practitioner-role"}),
    ));
    let issues = roles["issue"].as_array().unwrap();
    assert_eq!(issues.len(), 1, "{roles}");
    assert_eq!(issues[0]["severity"], "error");
    assert_eq!(issues[0]["expression"][0], "ValueSet.compose.include[1]");
    let text = issues[0]["diagnostics"].as_str().unwrap();
    for part in [
        "http://hl7.org/fhir/ValueSet/practitioner-role expansion is not supported",
        "http://snomed.info/sct",
        "concept is-a 223366009",
        "none of its concepts are installed",
    ] {
        assert!(text.contains(part), "{part} missing from {text}");
    }

    // The insurance plan types live in a CodeSystem whose content is `fragment`: there is no
    // telling which concepts it lacks, so including it whole is refused.
    let plans = unprocessable(run_op(
        &harness,
        "ValueSet/$expand",
        &json!({"url": "http://hl7.org/fhir/ValueSet/insuranceplan-type"}),
    ));
    let text = plans["issue"][0]["diagnostics"].as_str().unwrap();
    for part in [
        "http://hl7.org/fhir/ValueSet/insuranceplan-type expansion is not supported",
        "http://terminology.hl7.org/CodeSystem/insurance-plan-type",
        "'fragment'",
        "cannot be included whole",
    ] {
        assert!(text.contains(part), "{part} missing from {text}");
    }
    assert_eq!(
        plans["issue"][0]["expression"][0],
        "ValueSet.compose.include[0]"
    );

    // BCP-47 is not installed, so there is nothing to list.
    let languages = unprocessable(run_op(
        &harness,
        "ValueSet/$expand",
        &json!({"url": "http://hl7.org/fhir/ValueSet/all-languages"}),
    ));
    let text = languages["issue"][0]["diagnostics"].as_str().unwrap();
    for part in [
        "http://hl7.org/fhir/ValueSet/all-languages expansion is not supported",
        "code system urn:ietf:bcp:47 is not installed",
    ] {
        assert!(text.contains(part), "{part} missing from {text}");
    }

    // Expansion that works is untouched.
    let gender = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://hl7.org/fhir/ValueSet/administrative-gender"}),
    );
    assert_eq!(gender["resourceType"], "ValueSet");

    // The deployment check lists both before any client asks for them.
    let catalog = harness.catalog.read().unwrap();
    let report = catalog.terms().unexpandable();
    let urls: Vec<&str> = report.iter().map(|(url, _)| url.as_str()).collect();
    assert!(urls.contains(&"http://hl7.org/fhir/ValueSet/practitioner-role"));
    assert!(urls.contains(&"http://hl7.org/fhir/ValueSet/insuranceplan-type"));
    assert!(urls.contains(&"http://hl7.org/fhir/ValueSet/all-languages"));
    // The service categories expand, flagged, so they are not among those that cannot.
    assert!(!urls.contains(&"http://hl7.org/fhir/ValueSet/service-category"));
}

/// The codes of an expansion as `system|code`, and the `expansion.parameter` entries that say a code
/// system was drawn on in part, as `name = value`. (The entries that say what was asked of the
/// expansion are not these.)
fn expansion_summary(expanded: &Value) -> (Vec<String>, Vec<String>) {
    let codes = expanded["expansion"]["contains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            format!(
                "{}|{}",
                entry["system"].as_str().unwrap_or(""),
                entry["code"].as_str().unwrap()
            )
        })
        .collect();
    let parameters = expanded["expansion"]["parameter"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    item["name"]
                        .as_str()
                        .is_some_and(|name| name.ends_with("#CodeSystem.content"))
                })
                .map(|item| {
                    format!(
                        "{} = {}",
                        item["name"].as_str().unwrap(),
                        item["valueCode"].as_str().unwrap()
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    (codes, parameters)
}

fn validated(harness: &Harness, args: Value) -> (bool, Option<String>) {
    let checked = apply(harness, "ValueSet/$validate-code", args);
    let parameter = |name: &str| {
        checked["parameter"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == name)
            .cloned()
    };
    (
        parameter("result").unwrap()["valueBoolean"]
            .as_bool()
            .unwrap(),
        parameter("message").map(|item| item["valueString"].as_str().unwrap().to_string()),
    )
}

#[test]
fn a_value_set_over_a_whole_example_system_expands_and_says_it_is_not_complete() {
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();
    let categories = "http://terminology.hl7.org/CodeSystem/service-category";
    let types = "http://terminology.hl7.org/CodeSystem/service-type";

    // HL7's service categories hold the 38 concepts the code system carries, and the expansion
    // names the system that is only a sample, with the content mode that says so.
    let expanded = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://hl7.org/fhir/ValueSet/service-category"}),
    );
    let (codes, parameters) = expansion_summary(&expanded);
    assert_eq!(codes.len(), 38);
    assert_eq!(expanded["expansion"]["total"], 38);
    // R4 requires a timestamp: when the expansion was produced, here a moment ago.
    let timestamp = expanded["expansion"]["timestamp"].as_str().unwrap();
    let now = || {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        refhir_types::rfc3339(ms)
    };
    assert!(
        timestamp.len() == 24 && timestamp.as_bytes()[10] == b'T' && timestamp.ends_with('Z'),
        "{timestamp}"
    );
    // RFC 3339 instants of one length and zone sort as time does.
    assert!(timestamp <= now().as_str(), "{timestamp} is in the future");
    let earlier = refhir_types::rfc3339(
        (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64)
            .saturating_sub(10 * 60 * 1000),
    );
    assert!(timestamp >= earlier.as_str(), "{timestamp} is not recent");
    assert_eq!(
        parameters,
        [format!("{categories}|4.0.1#CodeSystem.content = example")]
    );
    let display = |code: &str| {
        expanded["expansion"]["contains"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["code"] == code)
            .map(|entry| entry["display"].as_str().unwrap().to_string())
    };
    assert_eq!(display("17").as_deref(), Some("General Practice"));
    assert_eq!(display("22").as_deref(), Some("Mental Health"));
    assert!(codes.iter().all(|code| code.starts_with(categories)));

    // So do the service types, and the other value sets that are only such an include.
    let expanded = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://hl7.org/fhir/ValueSet/service-type"}),
    );
    let (codes, parameters) = expansion_summary(&expanded);
    assert_eq!(codes.len(), 596);
    assert!(codes.iter().all(|code| code.starts_with(types)));
    assert_eq!(
        parameters,
        [format!("{types}|4.0.1#CodeSystem.content = example")]
    );
    for url in [
        "http://hl7.org/fhir/ValueSet/program",
        "http://hl7.org/fhir/ValueSet/appointment-cancellation-reason",
    ] {
        let (codes, parameters) =
            expansion_summary(&apply(&harness, "ValueSet/$expand", json!({"url": url})));
        assert!(!codes.is_empty(), "{url}");
        assert_eq!(parameters.len(), 1, "{url}");
    }

    // A page of it carries the flag too, and a filter narrows what is listed.
    let page = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://hl7.org/fhir/ValueSet/service-category", "filter": "health", "count": 2}),
    );
    assert_eq!(page["expansion"]["contains"].as_array().unwrap().len(), 2);
    assert_eq!(page["expansion"]["total"], 3);
    assert_eq!(expansion_summary(&page).1.len(), 1);

    // A value set whose code systems are complete carries no such flag.
    let complete = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://hl7.org/fhir/ValueSet/organization-type"}),
    );
    assert_eq!(complete["expansion"]["total"], 12);
    assert!(complete["expansion"].get("parameter").is_none());

    // `$validate-code` agrees with `$expand`: a code it lists is in, and one it does not is out.
    let in_set = |code: &str| {
        validated(
            &harness,
            json!({"url": "http://hl7.org/fhir/ValueSet/service-category", "system": categories, "code": code}),
        )
    };
    assert_eq!(in_set("17"), (true, None));
    let (found, message) = in_set("999");
    assert!(!found);
    // It says that the sample is the reason, not that 999 is known to be wrong.
    let message = message.unwrap();
    assert!(
        message.contains("not among the concepts listed") && message.contains("only some"),
        "{message}"
    );
    // A code that is in another system is not among its codes either.
    let (found, _) = validated(
        &harness,
        json!({"url": "http://hl7.org/fhir/ValueSet/service-category", "system": types, "code": "17"}),
    );
    assert!(!found);
    // A value set that is complete says nothing of the kind about a code outside it.
    let (found, message) = validated(
        &harness,
        json!({"url": "http://hl7.org/fhir/ValueSet/organization-type",
               "system": "http://terminology.hl7.org/CodeSystem/organization-type", "code": "nope"}),
    );
    assert!(!found);
    assert_eq!(message.as_deref(), Some("code is not in the value set"));

    // The expansion was stored with its flag, so a server that reads it back says the same.
    let before = expansion_summary(&apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://hl7.org/fhir/ValueSet/service-category"}),
    ));
    publish(&harness.store, &harness.catalog).unwrap();
    let after = expansion_summary(&apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://hl7.org/fhir/ValueSet/service-category"}),
    ));
    assert_eq!(before, after);
    assert_eq!(after.1.len(), 1);
}

#[test]
fn create_ignores_the_id_and_version_a_client_supplies() {
    let db = ready();
    let mut body = patient("mine", true, "Ann");
    body["meta"] = json!({
        "versionId": "999",
        "lastUpdated": "2000-01-01T00:00:00Z",
        "tag": [{"system": "http://example.org/tags", "code": "kept"}]
    });
    let created = run_op(&db, "Patient/create", &json!({"resource": body})).unwrap();
    assert_eq!(created.status, 201);
    let id = created.value["id"].as_str().unwrap().to_string();
    assert_ne!(id, "mine");
    assert_ne!(created.value["meta"]["versionId"], "999");
    // The time is the server's: the one the client sent is not kept.
    let written = created.value["meta"]["lastUpdated"].as_str().unwrap();
    assert_ne!(written, "2000-01-01T00:00:00Z");
    assert!(crate::instant::parse(written).unwrap() > crate::instant::parse("2020-01-01").unwrap());
    // Everything else the client said about meta stays.
    assert_eq!(created.value["meta"]["tag"][0]["code"], "kept");
    let missing = run_op(&db, "Patient/read", &json!({"id": "mine"})).unwrap_err();
    assert!(matches!(missing, RunError::Fhir(FhirError::NotFound(_))));
    // A second create with the same body is a second resource, not a conflict.
    let again = run_op(
        &db,
        "Patient/create",
        &json!({"resource": patient("mine", true, "Ann")}),
    )
    .unwrap();
    assert_ne!(again.value["id"], created.value["id"]);
}

#[test]
fn put_to_an_id_that_does_not_exist_is_not_allowed() {
    let db = ready();
    let err = run_op(
        &db,
        "Patient/update",
        &json!({"id": "nope", "resource": patient("nope", true, "Ann")}),
    )
    .unwrap_err();
    let RunError::Fhir(FhirError::MethodNotAllowed(message)) = err else {
        panic!("expected 405, got {err}");
    };
    assert!(message.contains("Patient/nope does not exist"), "{message}");
    assert!(message.contains("update-as-create"), "{message}");
    assert!(message.contains("POST to Patient"), "{message}");

    // The same PUT in a transaction names its entry and undoes the rest.
    let bundle = json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": [
            {"request": {"method": "POST", "url": "Patient"}, "resource": patient("a", true, "Ann")},
            {"request": {"method": "PUT", "url": "Patient/nope"}, "resource": patient("nope", true, "Bea")}
        ]
    });
    let err = run_op(&db, "Bundle/transaction", &json!({"resource": bundle})).unwrap_err();
    let RunError::Fhir(FhirError::MethodNotAllowed(message)) = err else {
        panic!("expected 405, got {err}");
    };
    assert!(message.starts_with("entry 1:"), "{message}");
    assert_eq!(patient_count(&db), 0);

    // A PUT to a resource that exists is an ordinary update. Once it is deleted its id is like
    // any other that does not exist, so a PUT to it is a 405 too.
    let created = run_op(
        &db,
        "Patient/create",
        &json!({"resource": patient("x", true, "Ann")}),
    )
    .unwrap()
    .value;
    let id = created["id"].as_str().unwrap().to_string();
    let mut changed = created.clone();
    changed["active"] = json!(false);
    let updated = run_op(
        &db,
        "Patient/update",
        &json!({"id": id, "resource": changed}),
    )
    .unwrap();
    assert_eq!(updated.value["active"], false);
    run_op(&db, "Patient/delete", &json!({"id": id})).unwrap();
    let gone = run_op(
        &db,
        "Patient/update",
        &json!({"id": id, "resource": created}),
    )
    .unwrap_err();
    assert!(
        matches!(gone, RunError::Fhir(FhirError::MethodNotAllowed(_))),
        "{gone}"
    );
    // PATCH has no such case: it reads first, so a missing resource is a plain 404.
    let patched = run_op(
        &db,
        "Patient/patch",
        &json!({"id": "nope", "patch": [{"op": "add", "path": "/active", "value": true}]}),
    )
    .unwrap_err();
    assert!(
        matches!(patched, RunError::Fhir(FhirError::NotFound(_))),
        "{patched}"
    );
}

#[test]
fn conditional_update_creates_only_when_the_body_names_no_id() {
    let db = ready();
    let query = "identifier=http://hospital.example|mrn-z";
    let without_id = |family: &str| {
        let mut body = patient("z", true, family);
        body.as_object_mut().unwrap().remove("id");
        body
    };
    // Nothing matches and the body names no id: the create interaction, so a new id.
    let created = run_op(
        &db,
        "Patient/conditional-update",
        &json!({"resource": without_id("Ann"), "query": query}),
    )
    .unwrap();
    assert_eq!(created.status, 201);
    let id = created.value["id"].as_str().unwrap().to_string();

    // One match: an update of that resource.
    let updated = run_op(
        &db,
        "Patient/conditional-update",
        &json!({"resource": without_id("Bea"), "query": query}),
    )
    .unwrap();
    assert_eq!(updated.status, 200);
    assert_eq!(updated.value["id"], json!(id));
    assert_eq!(updated.value["name"][0]["family"], "Bea");
    // ...and the body may repeat the id it matched, but not name another.
    let mut same = without_id("Cat");
    same["id"] = json!(id);
    assert_eq!(
        run_op(
            &db,
            "Patient/conditional-update",
            &json!({"resource": same, "query": query})
        )
        .unwrap()
        .status,
        200
    );
    let mut other = without_id("Dee");
    other["id"] = json!("someone-else");
    let err = run_op(
        &db,
        "Patient/conditional-update",
        &json!({"resource": other, "query": query}),
    )
    .unwrap_err();
    assert!(
        matches!(err, RunError::Fhir(FhirError::Invalid(_))),
        "{err}"
    );

    // Nothing matches and the body names an id: that is update-as-create.
    let mut named = patient("fresh", true, "Eve");
    named["identifier"] = json!([{"system": "http://hospital.example", "value": "mrn-new"}]);
    let err = run_op(
        &db,
        "Patient/conditional-update",
        &json!({"resource": named, "query": "identifier=http://hospital.example|mrn-new"}),
    )
    .unwrap_err();
    assert!(
        matches!(err, RunError::Fhir(FhirError::MethodNotAllowed(_))),
        "{err}"
    );
    assert_eq!(patient_count(&db), 1);
}

#[test]
fn capability_statement_says_what_put_and_the_conditional_interactions_do() {
    let db = ready();
    let statement = db.catalog.read().unwrap().capability_statement();
    let resources = statement["rest"][0]["resource"].as_array().unwrap();
    assert!(!resources.is_empty());
    for resource in resources {
        assert_eq!(resource["updateCreate"], false, "{resource}");
        assert_eq!(resource["conditionalCreate"], true, "{resource}");
        assert_eq!(resource["conditionalUpdate"], true, "{resource}");
        assert_eq!(resource["conditionalDelete"], "single", "{resource}");
    }
}

// A conformance store that counts what a write reads and rewrites, and can be made to fail. ----

use std::sync::atomic::{AtomicUsize, Ordering};

use refhir_persistence::{
    ExpansionChange, InstallError, InstallOutcome, NewExpansion, NewPackage, NewSearchParameter,
    NewTerminology, ParamStatus, PersistError, StoredBackfill, StoredExpansion, StoredPackage,
    StoredSearchParameter, StoredStructureDefinition, StoredTerminology,
};

struct WatchedStore {
    inner: SqlitePersistence,
    /// Reads of every terminology row, or every expansion row.
    full_terminology_loads: AtomicUsize,
    full_expansion_loads: AtomicUsize,
    full_search_parameter_loads: AtomicUsize,
    definition_loads: AtomicUsize,
    /// Run once, as the next terminology read begins: a change another process makes meanwhile.
    on_terminology_load: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Rewrites of a whole package's expansions.
    package_expansion_rewrites: AtomicUsize,
    expansion_changes: AtomicUsize,
    /// How many of the next calls to fail.
    fail_upsert_terminology: AtomicUsize,
    fail_apply_expansions: AtomicUsize,
    /// How many of the next reads of every terminology row to fail.
    fail_load_terminology: AtomicUsize,
    /// Counter writes that announce a change to the other processes.
    fail_bump_catalog: AtomicUsize,
    fail_bump_terminology: AtomicUsize,
    /// How many of the next reads of one stored row (a definition, a code system, an expansion)
    /// to fail.
    fail_row_reads: AtomicUsize,
}

impl WatchedStore {
    fn new(inner: SqlitePersistence) -> Self {
        Self {
            inner,
            full_terminology_loads: AtomicUsize::new(0),
            full_expansion_loads: AtomicUsize::new(0),
            full_search_parameter_loads: AtomicUsize::new(0),
            definition_loads: AtomicUsize::new(0),
            on_terminology_load: std::sync::Mutex::new(None),
            package_expansion_rewrites: AtomicUsize::new(0),
            expansion_changes: AtomicUsize::new(0),
            fail_upsert_terminology: AtomicUsize::new(0),
            fail_apply_expansions: AtomicUsize::new(0),
            fail_load_terminology: AtomicUsize::new(0),
            fail_bump_catalog: AtomicUsize::new(0),
            fail_bump_terminology: AtomicUsize::new(0),
            fail_row_reads: AtomicUsize::new(0),
        }
    }

    fn reset(&self) {
        for counter in [
            &self.full_terminology_loads,
            &self.full_expansion_loads,
            &self.full_search_parameter_loads,
            &self.definition_loads,
            &self.package_expansion_rewrites,
            &self.expansion_changes,
        ] {
            counter.store(0, Ordering::SeqCst);
        }
    }

    fn count(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::SeqCst)
    }

    fn should_fail(counter: &AtomicUsize) -> bool {
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
    }
}

impl ConformanceStore for WatchedStore {
    fn catalog_version(&self) -> Result<u64, PersistError> {
        self.inner.catalog_version()
    }
    fn bump_catalog_version(&self) -> Result<u64, PersistError> {
        if Self::should_fail(&self.fail_bump_catalog) {
            return Err(PersistError::Definite("injected: catalog bump".into()));
        }
        self.inner.bump_catalog_version()
    }
    fn catalog_revision(&self) -> Result<u64, PersistError> {
        self.inner.catalog_revision()
    }
    fn bump_catalog_revision(&self) -> Result<u64, PersistError> {
        if Self::should_fail(&self.fail_bump_catalog) {
            return Err(PersistError::Definite("injected: catalog bump".into()));
        }
        self.inner.bump_catalog_revision()
    }
    fn terminology_version(&self) -> Result<u64, PersistError> {
        self.inner.terminology_version()
    }
    fn bump_terminology_version(&self) -> Result<u64, PersistError> {
        if Self::should_fail(&self.fail_bump_terminology) {
            return Err(PersistError::Definite("injected: terminology bump".into()));
        }
        self.inner.bump_terminology_version()
    }
    fn claim_backfill(&self, id: i64) -> Result<bool, PersistError> {
        self.inner.claim_backfill(id)
    }
    fn load_packages(&self) -> Result<Vec<StoredPackage>, PersistError> {
        self.inner.load_packages()
    }
    fn load_structure_definitions(&self) -> Result<Vec<StoredStructureDefinition>, PersistError> {
        self.definition_loads.fetch_add(1, Ordering::SeqCst);
        self.inner.load_structure_definitions()
    }
    fn load_search_parameters(&self) -> Result<Vec<StoredSearchParameter>, PersistError> {
        self.full_search_parameter_loads
            .fetch_add(1, Ordering::SeqCst);
        self.inner.load_search_parameters()
    }
    fn load_structure_definition_json(
        &self,
        package_id: i64,
        url: &str,
    ) -> Result<Option<String>, PersistError> {
        if Self::should_fail(&self.fail_row_reads) {
            return Err(PersistError::Definite("injected: row read".into()));
        }
        self.inner.load_structure_definition_json(package_id, url)
    }
    fn load_terminology_json(
        &self,
        package_id: i64,
        url: &str,
    ) -> Result<Option<String>, PersistError> {
        if Self::should_fail(&self.fail_row_reads) {
            return Err(PersistError::Definite("injected: row read".into()));
        }
        self.inner.load_terminology_json(package_id, url)
    }
    fn load_expansion_json(
        &self,
        package_id: i64,
        url: &str,
    ) -> Result<Option<String>, PersistError> {
        if Self::should_fail(&self.fail_row_reads) {
            return Err(PersistError::Definite("injected: row read".into()));
        }
        self.inner.load_expansion_json(package_id, url)
    }
    fn load_terminology(&self) -> Result<Vec<StoredTerminology>, PersistError> {
        self.full_terminology_loads.fetch_add(1, Ordering::SeqCst);
        if Self::should_fail(&self.fail_load_terminology) {
            return Err(PersistError::Definite("injected failure".into()));
        }
        let hook = self.on_terminology_load.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        self.inner.load_terminology()
    }
    fn load_expansions(&self) -> Result<Vec<StoredExpansion>, PersistError> {
        self.full_expansion_loads.fetch_add(1, Ordering::SeqCst);
        self.inner.load_expansions()
    }
    fn load_terminology_for(
        &self,
        urls: &[String],
    ) -> Result<Vec<StoredTerminology>, PersistError> {
        self.inner.load_terminology_for(urls)
    }
    fn apply_expansions(&self, changes: &[ExpansionChange]) -> Result<(), PersistError> {
        if Self::should_fail(&self.fail_apply_expansions) {
            return Err(PersistError::Definite(
                "injected apply_expansions failure".into(),
            ));
        }
        self.expansion_changes
            .fetch_add(changes.len(), Ordering::SeqCst);
        self.inner.apply_expansions(changes)
    }
    fn ensure_local_package(&self, fhir_version: &str) -> Result<i64, PersistError> {
        self.inner.ensure_local_package(fhir_version)
    }
    fn upsert_search_parameter(
        &self,
        package_id: i64,
        param: &NewSearchParameter,
    ) -> Result<bool, PersistError> {
        self.inner.upsert_search_parameter(package_id, param)
    }
    fn delete_search_parameter(&self, package_id: i64, url: &str) -> Result<(), PersistError> {
        self.inner.delete_search_parameter(package_id, url)
    }
    fn upsert_terminology(
        &self,
        package_id: i64,
        row: &NewTerminology,
    ) -> Result<(), PersistError> {
        if Self::should_fail(&self.fail_upsert_terminology) {
            return Err(PersistError::Definite(
                "injected upsert_terminology failure".into(),
            ));
        }
        self.inner.upsert_terminology(package_id, row)
    }
    fn delete_terminology(&self, package_id: i64, url: &str) -> Result<(), PersistError> {
        self.inner.delete_terminology(package_id, url)
    }
    fn replace_expansions(
        &self,
        package_id: i64,
        expansions: &[NewExpansion],
    ) -> Result<(), PersistError> {
        self.package_expansion_rewrites
            .fetch_add(1, Ordering::SeqCst);
        self.inner.replace_expansions(package_id, expansions)
    }
    fn install_package(&self, package: &NewPackage) -> Result<InstallOutcome, InstallError> {
        self.inner.install_package(package)
    }
    fn mark_base_ready(&self, package_id: i64, base: &str) -> Result<(), PersistError> {
        self.inner.mark_base_ready(package_id, base)
    }
    fn complete_backfill(
        &self,
        backfill_id: i64,
        package_id: i64,
        base: &str,
    ) -> Result<(), PersistError> {
        self.inner.complete_backfill(backfill_id, package_id, base)
    }
    fn set_search_parameter_status(
        &self,
        package_id: i64,
        url: &str,
        base: &str,
        status: ParamStatus,
    ) -> Result<(), PersistError> {
        self.inner
            .set_search_parameter_status(package_id, url, base, status)
    }
    fn insert_backfill(
        &self,
        package_id: i64,
        watermark_ts: i64,
        tablet: &str,
        urls: &[String],
    ) -> Result<i64, PersistError> {
        self.inner
            .insert_backfill(package_id, watermark_ts, tablet, urls)
    }
    fn update_backfill(
        &self,
        id: i64,
        cursor_id: Option<&str>,
        done: bool,
    ) -> Result<(), PersistError> {
        self.inner.update_backfill(id, cursor_id, done)
    }
    fn load_backfills(&self) -> Result<Vec<StoredBackfill>, PersistError> {
        self.inner.load_backfills()
    }
}

fn watched_r4() -> (Harness, WatchedStore) {
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();
    let watched = WatchedStore::new(harness.store.clone());
    (harness, watched)
}

fn watched_commit(
    harness: &Harness,
    watched: &WatchedStore,
    path: &str,
    args: Value,
) -> Result<Value, RunError> {
    commit_operation(watched, &harness.db, &harness.catalog, path, &args).map(|result| result.value)
}

fn color_value_set(id: &str, concepts: &[&str], system: &str) -> Value {
    json!({
        "resourceType": "ValueSet", "id": id, "status": "active",
        "url": format!("http://example.org/{id}"),
        "compose": {"include": [{"system": system, "concept":
            concepts.iter().map(|code| json!({"code": code})).collect::<Vec<_>>()}]}
    })
}

/// url -> expansion document, as stored.
fn stored_expansions(store: &dyn ConformanceStore) -> BTreeMap<String, Value> {
    store
        .load_expansions()
        .unwrap()
        .into_iter()
        .map(|row| (row.url, serde_json::from_str::<Value>(&row.json).unwrap()))
        .collect()
}

/// What differs between two sets of expansions, in a line a failure can print.
fn expansion_diff(left: &BTreeMap<String, Value>, right: &BTreeMap<String, Value>) -> String {
    let mut lines = Vec::new();
    for (url, document) in left {
        match right.get(url) {
            None => lines.push(format!("only in stored: {url}")),
            Some(other) if other != document => {
                let codes = |doc: &Value| -> std::collections::BTreeSet<String> {
                    doc["contains"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|entry| {
                            format!(
                                "{}|{}",
                                entry["system"].as_str().unwrap_or(""),
                                entry["code"].as_str().unwrap_or("")
                            )
                        })
                        .collect()
                };
                let (stored, rebuilt) = (codes(document), codes(other));
                lines.push(format!(
                    "differs: {url} (stored-only {:?}, rebuild-only {:?}, systems {} vs {})",
                    stored.difference(&rebuilt).collect::<Vec<_>>(),
                    rebuilt.difference(&stored).collect::<Vec<_>>(),
                    document["systems"],
                    other["systems"]
                ));
            }
            _ => {}
        }
    }
    for url in right.keys().filter(|url| !left.contains_key(*url)) {
        lines.push(format!("only in rebuild: {url}"));
    }
    lines.truncate(12);
    lines.join("; ")
}

/// What is stored is what a full rebuild of everything stored would give. This holds for the
/// whole R4 package: its code systems that come in several versions (the v2 tables carry theirs
/// in the URL, `.../v2-0006|2.1`) are all kept, so what a value set expands to does not depend
/// on the order they were loaded in.
fn assert_expansions_match(store: &dyn ConformanceStore, when: &str) {
    let (stored, rebuilt) = (stored_expansions(store), rebuilt_expansions(store));
    assert!(
        stored == rebuilt,
        "{when}: {}",
        expansion_diff(&stored, &rebuilt)
    );
}

/// Every expansion that was stored before is exactly what it was.
fn assert_untouched(store: &dyn ConformanceStore, before: &BTreeMap<String, Value>) {
    let after = stored_expansions(store);
    for (url, document) in before {
        assert_eq!(after.get(url), Some(document), "{url} changed");
    }
}

/// url -> expansion document, as a full rebuild of everything stored would make it.
fn rebuilt_expansions(store: &dyn ConformanceStore) -> BTreeMap<String, Value> {
    let rows = store.load_terminology().unwrap();
    let resources: Vec<Value> = rows
        .iter()
        .map(|row| serde_json::from_str::<Value>(&row.json).unwrap())
        .collect();
    // Each value set is read with the code systems of its own package first, as the server's
    // rebuild reads it.
    let scoped: Vec<(i64, &Value)> = rows
        .iter()
        .map(|row| row.package_id)
        .zip(&resources)
        .collect();
    // A rebuild reads an unversioned reference as the server's release does.
    let release = store
        .load_packages()
        .unwrap()
        .first()
        .map(|package| package.fhir_version.clone());
    refhir_validate::MemoryTerminology::expansion_documents_scoped(&scoped, release.as_deref())
        .into_iter()
        .map(|document| (document["url"].as_str().unwrap().to_string(), document))
        .collect()
}

/// The `contains` entries of the expansion of `url`.
fn expansion_of(harness: &Harness, url: &str) -> Vec<Value> {
    let expanded = apply(harness, "ValueSet/$expand", json!({"url": url}));
    expanded["expansion"]["contains"]
        .as_array()
        .unwrap()
        .clone()
}

/// Write `resource` with the incremental update made to fail, which falls back to rebuilding every
/// stored expansion from all the packages at once.
fn force_full_rebuild(harness: &Harness, watched: &WatchedStore, resource: Value) {
    watched.fail_apply_expansions.store(1, Ordering::SeqCst);
    watched.reset();
    watched_commit(
        harness,
        watched,
        "ValueSet/create",
        json!({ "resource": resource }),
    )
    .unwrap();
    assert!(
        WatchedStore::count(&watched.full_terminology_loads) > 0,
        "the rebuild ran"
    );
}

#[test]
fn a_cross_version_package_does_not_change_what_the_release_value_sets_mean() {
    // `hl7.fhir.uv.xver-r5.r4` is a default dependency of SDC. It carries 5.0.0 of R4's own
    // `encounter-status` code system, under the same URL as the one in the R4 core package.
    let (harness, watched) = watched_r4();
    install_package_dir(
        &harness.store,
        &harness.db,
        &harness.catalog,
        cross_version_dir(),
    )
    .unwrap();
    let status_codes = || -> Vec<String> {
        expansion_of(&harness, "http://hl7.org/fhir/ValueSet/encounter-status")
            .iter()
            .filter_map(|item| item["code"].as_str().map(str::to_string))
            .collect()
    };
    let is_r4 = |codes: &[String]| {
        let has = |code: &str| codes.iter().any(|item| item == code);
        has("arrived") && has("finished") && !has("completed")
    };
    let check_in = |status: &str| {
        run_op(
            &harness,
            "Encounter/create",
            &json!({"resource": encounter(json!({"status": status}))}),
        )
    };

    // As installed, an Encounter can arrive and finish.
    assert!(is_r4(&status_codes()), "{:?}", status_codes());
    check_in("arrived").unwrap();
    check_in("finished").unwrap();

    // A rebuild of every stored expansion must read the same code system version install did.
    force_full_rebuild(
        &harness,
        &watched,
        color_value_set("ops-colors", &["a"], "http://example.org/s"),
    );
    assert!(is_r4(&status_codes()), "{:?}", status_codes());
    check_in("arrived").unwrap();
    check_in("finished").unwrap();
    assert_expansions_match(&watched.inner, "after the rebuild");
}

#[test]
fn a_package_that_carries_another_releases_copy_of_a_value_set_does_not_replace_the_releases() {
    // The R4 package's value set is at 4.0.1 over `terminology.hl7.org`'s code system.
    // `hl7.fhir.uv.extensions.r4`, installed later, has a 5.3.0-ballot-tc1 copy over R5's
    // `hl7.org/fhir` one. A server for R4 means the first, whichever package was installed last.
    let (harness, watched) = watched_r4();
    install_package_dir(
        &harness.store,
        &harness.db,
        &harness.catalog,
        extension_copies_dir(),
    )
    .unwrap();
    let url = "http://hl7.org/fhir/ValueSet/allerg-intol-substance-exp-risk";
    let r4_system = "http://terminology.hl7.org/CodeSystem/allerg-intol-substance-exp-risk";
    let r5_system = "http://hl7.org/fhir/allerg-intol-substance-exp-risk";
    let systems = || -> std::collections::BTreeSet<String> {
        expansion_of(&harness, url)
            .iter()
            .filter_map(|item| item["system"].as_str().map(str::to_string))
            .collect()
    };
    let accepts = |system: &str| -> bool {
        let answer = apply(
            &harness,
            "ValueSet/$validate-code",
            json!({"url": url, "system": system, "code": "known-reaction-risk"}),
        );
        answer["parameter"]
            .as_array()
            .unwrap()
            .iter()
            .find(|parameter| parameter["name"] == "result")
            .unwrap()["valueBoolean"]
            .as_bool()
            .unwrap()
    };
    let r4_only = std::collections::BTreeSet::from([r4_system.to_string()]);

    // As installed: the start reads the R4 package's expansion, not the later package's.
    assert_eq!(systems(), r4_only);
    assert!(accepts(r4_system) && !accepts(r5_system));

    force_full_rebuild(
        &harness,
        &watched,
        color_value_set("ops-colors", &["a"], "http://example.org/s"),
    );
    assert_eq!(
        systems(),
        r4_only,
        "the rebuild changed the value set in use"
    );
    assert!(accepts(r4_system) && !accepts(r5_system));
    assert_expansions_match(&watched.inner, "after the rebuild");

    // The rebuild stores the expansion under the package whose definition is in use, so a start
    // reads it from there.
    let core = package_id(&harness, "hl7.fhir.r4.core");
    let rows = stored_rows(&harness, url);
    assert_eq!(rows.len(), 1, "one expansion is stored for it");
    assert_eq!(rows[0].package_id, core);
    // And a start from what is stored reads the same.
    publish(&harness.store, &harness.catalog).unwrap();
    assert_eq!(systems(), r4_only, "after a restart");
    assert!(accepts(r4_system) && !accepts(r5_system));
}

fn package_id(harness: &Harness, name: &str) -> i64 {
    harness
        .store
        .load_packages()
        .unwrap()
        .into_iter()
        .find(|package| package.name == name)
        .unwrap()
        .id
}

/// The stored expansion rows of `url`, one per package that holds one.
fn stored_rows(harness: &Harness, url: &str) -> Vec<StoredExpansion> {
    harness
        .store
        .load_expansions()
        .unwrap()
        .into_iter()
        .filter(|row| row.url == url)
        .collect()
}

#[test]
fn a_rebuild_gives_each_package_the_expansions_installing_it_gave() {
    // The R4 package's `marital-status` includes its own copy of v3-MaritalStatus (10 codes).
    // `hl7.terminology.r4` carries a newer copy that adds `C`. Installing the first expanded
    // the value set with its own copy, and a rebuild over both packages must do the same.
    let (harness, watched) = watched_r4();
    install_package_dir(
        &harness.store,
        &harness.db,
        &harness.catalog,
        newer_terminology_dir(),
    )
    .unwrap();
    let marital = "http://hl7.org/fhir/ValueSet/marital-status";
    let codes = |url: &str| -> Vec<String> {
        expansion_of(&harness, url)
            .iter()
            .filter_map(|item| item["code"].as_str().map(str::to_string))
            .collect()
    };
    let before = codes(marital);
    assert_eq!(before.len(), 11, "ten statuses and UNK: {before:?}");
    assert!(!before.contains(&"C".to_string()), "{before:?}");

    // A value set written through the API belongs to no package, so it reads the newest copy of
    // a system it names without a version.
    force_full_rebuild(
        &harness,
        &watched,
        including(
            "http://example.org/ops-marital",
            "http://terminology.hl7.org/CodeSystem/v3-MaritalStatus",
            None,
        ),
    );
    // The package's value set is what it was; the local one reads the newer copy.
    assert_eq!(
        codes(marital),
        before,
        "the rebuild changed what installing gave"
    );
    assert!(codes("http://example.org/ops-marital").contains(&"C".to_string()));
    assert_expansions_match(&watched.inner, "after the rebuild");
}

#[test]
fn expansions_stored_under_other_rules_are_rebuilt_once_at_the_next_start() {
    // A database the previous version rebuilt holds one row per URL under the first package that
    // defines it, read from one pool: here the extensions package's R5-style expansion, under
    // core, with no rules number. Nothing but this rebuild would ever replace it.
    let (harness, _watched) = watched_r4();
    install_package_dir(
        &harness.store,
        &harness.db,
        &harness.catalog,
        extension_copies_dir(),
    )
    .unwrap();
    let url = "http://hl7.org/fhir/ValueSet/allerg-intol-substance-exp-risk";
    let r4_system = "http://terminology.hl7.org/CodeSystem/allerg-intol-substance-exp-risk";
    let r5_system = "http://hl7.org/fhir/allerg-intol-substance-exp-risk";
    let (core, ext) = (
        package_id(&harness, "hl7.fhir.r4.core"),
        package_id(&harness, "hl7.fhir.uv.extensions.r4"),
    );
    let mut legacy: Value = serde_json::from_str(
        &stored_rows(&harness, url)
            .into_iter()
            .find(|row| row.package_id == ext)
            .unwrap()
            .json,
    )
    .unwrap();
    legacy.as_object_mut().unwrap().remove("rules");
    let change = |package_id, json: Option<String>| refhir_persistence::ExpansionChange {
        package_id,
        url: url.to_string(),
        json,
    };
    harness
        .store
        .apply_expansions(&[change(core, Some(legacy.to_string())), change(ext, None)])
        .unwrap();
    let systems = || -> std::collections::BTreeSet<String> {
        expansion_of(&harness, url)
            .iter()
            .filter_map(|item| item["system"].as_str().map(str::to_string))
            .collect()
    };

    // A start reads what is stored, and the database says its expansions are out of date.
    publish(&harness.store, &harness.catalog).unwrap();
    assert!(harness.catalog.read().unwrap().expansions_stale());
    assert_eq!(
        systems(),
        std::collections::BTreeSet::from([r5_system.to_string()])
    );

    // The repair that runs at every start rebuilds them once, under the current rules.
    repair_conformance(&harness.store, &harness.db, &harness.catalog).unwrap();
    assert!(!harness.catalog.read().unwrap().expansions_stale());
    assert_eq!(
        systems(),
        std::collections::BTreeSet::from([r4_system.to_string()])
    );
    let rows = stored_rows(&harness, url);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].package_id, core);
    assert_eq!(
        serde_json::from_str::<Value>(&rows[0].json).unwrap()["rules"],
        4
    );
    assert_expansions_match(&harness.store, "after the repair");
    // A second start finds nothing to do: the repair is not repeated.
    publish(&harness.store, &harness.catalog).unwrap();
    assert!(!harness.catalog.read().unwrap().expansions_stale());
}

/// The error issues `Invoice/$validate` finds in an invoice whose net total is `value` in `currency`.
fn currency_errors(harness: &Harness, currency: &str) -> Vec<String> {
    let checked = apply(
        harness,
        "Invoice/$validate",
        json!({"resource": {"resourceType": "Invoice", "status": "draft",
                            "totalNet": {"value": 15000, "currency": currency}}}),
    );
    checked["issue"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|issue| issue["severity"] == "error")
        .map(|issue| issue["diagnostics"].as_str().unwrap_or("").to_string())
        .collect()
}

/// The entries of an expansion as `code -> abstract`, for those that have a code.
fn abstract_flags(expanded: &Value) -> std::collections::BTreeMap<String, bool> {
    expanded["expansion"]["contains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["code"].as_str().unwrap().to_string(),
                entry["abstract"].as_bool().unwrap_or(false),
            )
        })
        .collect()
}

#[test]
fn an_expansion_marks_the_codes_its_system_does_not_let_you_select_as_abstract() {
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();
    let coverage = "http://hl7.org/fhir/ValueSet/coverage-type";
    let act_code = "http://terminology.hl7.org/CodeSystem/v3-ActCode";

    // The v3 grouping concepts are in the expansion, flagged, and the codes that are meant to be
    // chosen are not. ActCode says so with the `notSelectable` property, which is what R4's
    // `abstract` on an expansion entry means.
    let expanded = apply(&harness, "ValueSet/$expand", json!({"url": coverage}));
    assert_eq!(expanded["expansion"]["total"], 58);
    let flags = abstract_flags(&expanded);
    assert_eq!(flags.len(), 58);
    let abstract_codes: Vec<&str> = flags
        .iter()
        .filter(|(_, flagged)| **flagged)
        .map(|(code, _)| code.as_str())
        .collect();
    assert_eq!(
        abstract_codes,
        [
            "_ActCoverageTypeCode",
            "_ActHealthInsuranceTypeCode",
            "_ActInsurancePolicyCode",
            "_ActInsuranceTypeCode",
            "_ActProgramTypeCode"
        ]
    );
    for code in ["HMO", "PPO", "PUBLICPOL", "pay"] {
        assert_eq!(flags.get(code), Some(&false), "{code}");
    }
    // The flag is on the entry, as R4 puts it, and an entry that is not abstract says nothing.
    let entries = expanded["expansion"]["contains"].as_array().unwrap();
    let entry = |code: &str| entries.iter().find(|entry| entry["code"] == code).unwrap();
    assert_eq!(entry("_ActCoverageTypeCode")["abstract"], true);
    assert_eq!(entry("_ActCoverageTypeCode")["system"], act_code);
    assert!(entry("HMO").get("abstract").is_none());

    // A filter and a page still carry it.
    let filtered = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": coverage, "filter": "ActCoverage"}),
    );
    assert_eq!(
        abstract_flags(&filtered).get("_ActCoverageTypeCode"),
        Some(&true)
    );
    let paged = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": coverage, "count": 1000}),
    );
    assert_eq!(abstract_flags(&paged), flags);

    // A value set whose codes carry no such property flags nothing.
    let types = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://hl7.org/fhir/ValueSet/organization-type"}),
    );
    assert!(abstract_flags(&types).values().all(|flagged| !flagged));
    assert!(!types.to_string().contains("abstract"));

    // R4 lets a client say whether it is where an abstract code may be used (`abstract`). The
    // code is a member of the value set either way; what changes is whether it is valid.
    let check = |extra: Value| {
        let mut args = json!({"url": coverage, "system": act_code, "code": "_ActCoverageTypeCode"});
        for (name, value) in extra.as_object().unwrap() {
            args[name] = value.clone();
        }
        validated(&harness, args)
    };
    // Said nothing: valid, with a hint that it is a grouping concept.
    let (valid, message) = check(json!({}));
    assert!(valid);
    assert!(message.unwrap().contains("is abstract"));
    // Said it is where such a code belongs: valid, nothing to add.
    assert_eq!(check(json!({"abstract": true})), (true, None));
    // Said it is not: not valid, and why.
    let (valid, message) = check(json!({"abstract": false}));
    assert!(!valid);
    assert!(message.unwrap().contains("is abstract"));
    // As a Parameters resource the same input is `valueBoolean`.
    let as_parameters = apply(
        &harness,
        "ValueSet/$validate-code",
        json!({"resourceType": "Parameters", "parameter": [
            {"name": "url", "valueUri": coverage},
            {"name": "system", "valueUri": act_code},
            {"name": "code", "valueCode": "_ActCoverageTypeCode"},
            {"name": "abstract", "valueBoolean": false}
        ]}),
    );
    assert_eq!(as_parameters["parameter"][0]["valueBoolean"], false);
    // A code that is selectable is valid whatever is said.
    for extra in [
        json!({}),
        json!({"abstract": false}),
        json!({"abstract": true}),
    ] {
        let mut args = json!({"url": coverage, "system": act_code, "code": "HMO"});
        for (name, value) in extra.as_object().unwrap() {
            args[name] = value.clone();
        }
        assert_eq!(validated(&harness, args), (true, None), "{extra}");
    }

    // The flag is stored with the expansion, so a server that reads it back says the same, and a
    // rebuild from the stored resources makes the same documents.
    publish(&harness.store, &harness.catalog).unwrap();
    let reread = apply(&harness, "ValueSet/$expand", json!({"url": coverage}));
    assert_eq!(abstract_flags(&reread), flags);
    assert_expansions_match(&harness.store, "with abstract codes");
}

#[test]
fn an_expansion_says_which_value_set_status_it_has_which_expansion_it_is_and_what_was_asked() {
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();
    let url = "http://hl7.org/fhir/ValueSet/organization-type";
    let defined: Value = serde_json::from_str(
        &std::fs::read_to_string(r4_dir().join("ValueSet-organization-type.json")).unwrap(),
    )
    .unwrap();

    // R4 requires a status of the ValueSet an expansion is in: the definition's own.
    let plain = apply(&harness, "ValueSet/$expand", json!({"url": url}));
    assert_eq!(plain["status"], defined["status"]);
    assert!(defined["status"].is_string());
    // The identifier names what the expansion says, a urn:uuid: the same for an answer that says
    // the same, and another for one that says something else (a filtered one, a page).
    let identifier = plain["expansion"]["identifier"].as_str().unwrap();
    assert!(
        identifier.starts_with("urn:uuid:") && identifier.len() == "urn:uuid:".len() + 36,
        "{identifier}"
    );
    let again = apply(&harness, "ValueSet/$expand", json!({"url": url}));
    assert_eq!(
        again["expansion"]["identifier"],
        plain["expansion"]["identifier"]
    );
    let narrower = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": url, "filter": "hospital"}),
    );
    assert_ne!(
        narrower["expansion"]["identifier"],
        plain["expansion"]["identifier"]
    );
    // Nothing was asked, so nothing is listed as asked.
    assert!(plain["expansion"].get("parameter").is_none());

    // What was asked that shaped the expansion is listed, as R4 says it should be, and a page
    // says its offset and count.
    let asked = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": url, "filter": "hospital", "count": 1, "offset": 0, "activeOnly": true}),
    );
    let parameters = asked["expansion"]["parameter"].as_array().unwrap();
    for expected in [
        json!({"name": "filter", "valueString": "hospital"}),
        json!({"name": "activeOnly", "valueBoolean": true}),
        json!({"name": "count", "valueInteger": 1}),
        json!({"name": "offset", "valueInteger": 0}),
    ] {
        assert!(
            parameters.contains(&expected),
            "{expected} in {parameters:?}"
        );
    }
    assert_eq!(parameters.len(), 4);
    // `activeOnly` only says that inactive codes were left out, and none of these is inactive.
    let all = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": url, "activeOnly": "true"}),
    );
    assert_eq!(all["expansion"]["total"], 12);
    assert!(all["expansion"]["parameter"]
        .as_array()
        .unwrap()
        .contains(&json!({"name": "activeOnly", "valueBoolean": true})));

    // A value set that is not expanded from a definition with a status is `active`.
    let bare = refhir_validate::MemoryTerminology::from_resources(&[
        json!({"resourceType": "CodeSystem", "url": "http://example.org/s", "content": "complete",
               "concept": [{"code": "a"}]}),
        json!({"resourceType": "ValueSet", "url": "http://example.org/v",
               "compose": {"include": [{"system": "http://example.org/s"}]}}),
    ]);
    assert_eq!(
        bare.expand("http://example.org/v", None).unwrap()["status"],
        "active"
    );
}

#[test]
fn an_answer_outside_a_sample_value_set_is_not_refused_but_one_outside_a_complete_one_is() {
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();
    let questionnaire = |id: &str, value_set: &str| {
        json!({"resource": {"resourceType": "Questionnaire", "id": id,
               "url": format!("http://example.org/Questionnaire/{id}"), "status": "active",
               "item": [{"linkId": "a", "text": "A", "type": "choice",
                         "answerValueSet": value_set}]}})
    };
    let answer = |id: &str, system: &str, code: &str| {
        run_op(
            &harness,
            "QuestionnaireResponse/create",
            &json!({"resource": {"resourceType": "QuestionnaireResponse", "status": "completed",
                "questionnaire": format!("http://example.org/Questionnaire/{id}"),
                "item": [{"linkId": "a", "answer": [{"valueCoding":
                    {"system": system, "code": code}}]}]}}),
        )
    };
    apply(
        &harness,
        "Questionnaire/create",
        questionnaire("sample", "http://hl7.org/fhir/ValueSet/service-type"),
    );
    apply(
        &harness,
        "Questionnaire/create",
        questionnaire("complete", "http://hl7.org/fhir/ValueSet/organization-type"),
    );

    // A value set that lists only a sample of its code system cannot rule a code out, so a code
    // that is not in the sample is not an answer outside the value set.
    let stored = answer(
        "sample",
        "http://terminology.hl7.org/CodeSystem/service-type",
        "not-in-the-sample",
    );
    assert!(stored.is_ok(), "{stored:?}");
    // One that lists all of it can, and the answer is refused.
    let refused = answer(
        "complete",
        "http://terminology.hl7.org/CodeSystem/organization-type",
        "not-a-type",
    );
    assert!(refused.is_err(), "{refused:?}");
    // A code that is in the sample, and one in the complete set, are both fine.
    assert!(answer(
        "sample",
        "http://terminology.hl7.org/CodeSystem/service-type",
        "1"
    )
    .is_ok());
    assert!(answer(
        "complete",
        "http://terminology.hl7.org/CodeSystem/organization-type",
        "prov"
    )
    .is_ok());
}

#[test]
fn the_currencies_value_set_expands_over_the_iso_4217_codes_the_server_carries() {
    let currencies = "http://hl7.org/fhir/ValueSet/currencies";
    let iso = "urn:iso:std:iso:4217";
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();

    // HL7 publishes no CodeSystem for ISO 4217, so with only its packages the value set cannot
    // expand, and a Money cannot be checked.
    let refused = unprocessable(run_op(
        &harness,
        "ValueSet/$expand",
        &json!({"url": currencies}),
    ));
    let text = refused["issue"][0]["diagnostics"].as_str().unwrap();
    assert!(
        text.contains("code system urn:iso:std:iso:4217 is not installed"),
        "{text}"
    );
    assert!(
        currency_errors(&harness, "ZZZ").is_empty(),
        "an unchecked code is not an error"
    );

    // The server installs the code system itself, and the value set that was waiting for it
    // expands without any other package being touched.
    crate::install_builtin_terminology(&harness.store, &harness.db, &harness.catalog).unwrap();
    let expanded = apply(&harness, "ValueSet/$expand", json!({"url": currencies}));
    let (codes, parameters) = expansion_summary(&expanded);
    assert_eq!(codes.len(), 178);
    assert_eq!(expanded["expansion"]["total"], 178);
    let unique: std::collections::BTreeSet<&String> = codes.iter().collect();
    assert_eq!(unique.len(), codes.len(), "no duplicates");
    for code in ["NGN", "USD", "GBP", "EUR", "GHS", "KES", "ZAR"] {
        assert!(codes.contains(&format!("{iso}|{code}")), "{code}");
    }
    // The system is complete: nothing says the list is a sample.
    assert!(parameters.is_empty(), "{parameters:?}");
    let display = |expanded: &Value, code: &str| {
        expanded["expansion"]["contains"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["code"] == code)
            .map(|entry| entry["display"].as_str().unwrap().to_string())
    };
    assert_eq!(display(&expanded, "NGN").as_deref(), Some("Nigerian Naira"));
    assert_eq!(display(&expanded, "USD").as_deref(), Some("US Dollar"));

    // A filter finds a currency by what it is called, and by its code.
    let naira = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": currencies, "filter": "naira"}),
    );
    assert_eq!(expansion_summary(&naira).0, [format!("{iso}|NGN")]);
    let by_code = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": currencies, "filter": "ngn"}),
    );
    assert_eq!(expansion_summary(&by_code).0, [format!("{iso}|NGN")]);

    // A real code is in the value set and an unknown one is not.
    let check = |code: &str| {
        validated(
            &harness,
            json!({"url": currencies, "system": iso, "code": code}),
        )
    };
    assert_eq!(check("NGN"), (true, None));
    assert_eq!(
        check("ZZZ"),
        (false, Some("code is not in the value set".to_string()))
    );
    let looked = apply(
        &harness,
        "CodeSystem/$validate-code",
        json!({"url": iso, "code": "GHS"}),
    );
    assert_eq!(looked["parameter"][0]["valueBoolean"], true);

    // So a Money that carries a currency is checked: a real one passes, an unknown one is refused.
    assert!(currency_errors(&harness, "NGN").is_empty());
    let refused = currency_errors(&harness, "ZZZ");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(
        refused[0].contains("ZZZ") && refused[0].contains("currencies"),
        "{refused:?}"
    );

    // A write is checked the same way.
    let invoice = |currency: &str| {
        json!({"resource": {"resourceType": "Invoice", "status": "draft",
                            "totalNet": {"value": 15000, "currency": currency}}})
    };
    assert!(run_op(&harness, "Invoice/create", &invoice("NGN")).is_ok());
    let rejected = run_op(&harness, "Invoice/create", &invoice("ZZZ"));
    assert!(rejected.is_err(), "{rejected:?}");

    // Installing it again is not a second copy, and the answer does not change.
    let packages = harness.store.load_packages().unwrap();
    crate::install_builtin_terminology(&harness.store, &harness.db, &harness.catalog).unwrap();
    assert_eq!(harness.store.load_packages().unwrap().len(), packages.len());
    assert_eq!(
        packages
            .iter()
            .filter(
                |package| format!("{}@{}", package.name, package.version) == crate::BUILTIN_PACKAGE
            )
            .count(),
        1
    );
    let again = apply(&harness, "ValueSet/$expand", json!({"url": currencies}));
    assert_eq!(expansion_summary(&again).0, codes);

    // The expansion is stored, so a server that reads it back says the same.
    publish(&harness.store, &harness.catalog).unwrap();
    let reread = apply(&harness, "ValueSet/$expand", json!({"url": currencies}));
    assert_eq!(expansion_summary(&reread).0, codes);
    assert_expansions_match(&harness.store, "with the built-in terminology");
}

#[test]
fn the_built_in_terminology_waits_for_a_package_that_could_refer_to_it() {
    // A server with no package has nothing that refers to ISO 4217, and a replica that starts
    // serving as soon as any package is stored must not be left with only this one.
    let harness = empty();
    crate::install_builtin_terminology(&harness.store, &harness.db, &harness.catalog).unwrap();
    assert!(harness.store.load_packages().unwrap().is_empty());

    install_package_dir(&harness.store, &harness.db, &harness.catalog, minir4_dir()).unwrap();
    crate::install_builtin_terminology(&harness.store, &harness.db, &harness.catalog).unwrap();
    let names: Vec<String> = harness
        .store
        .load_packages()
        .unwrap()
        .iter()
        .map(|package| format!("{}@{}", package.name, package.version))
        .collect();
    assert!(
        names.contains(&crate::BUILTIN_PACKAGE.to_string()),
        "{names:?}"
    );
    assert_eq!(names.len(), 2, "{names:?}");
}

#[test]
fn a_database_made_under_the_previous_rules_gains_the_value_sets_that_now_expand_at_its_next_start()
{
    // The previous version could not expand HL7's service categories, so it stored no expansion
    // for them, and stored the rest under rules 2. Nothing else would ever add them: a start
    // that finds expansions made under other rules rebuilds them all, once.
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();
    let (categories, types) = (
        "http://hl7.org/fhir/ValueSet/service-category",
        "http://hl7.org/fhir/ValueSet/organization-type",
    );
    let change =
        |package_id, url: &str, json: Option<String>| refhir_persistence::ExpansionChange {
            package_id,
            url: url.to_string(),
            json,
        };
    let mut changes: Vec<_> = stored_rows(&harness, categories)
        .iter()
        .map(|row| change(row.package_id, categories, None))
        .collect();
    assert!(!changes.is_empty());
    for row in stored_rows(&harness, types) {
        let mut previous: Value = serde_json::from_str(&row.json).unwrap();
        previous["rules"] = json!(2);
        changes.push(change(row.package_id, types, Some(previous.to_string())));
    }
    harness.store.apply_expansions(&changes).unwrap();

    publish(&harness.store, &harness.catalog).unwrap();
    assert!(harness.catalog.read().unwrap().expansions_stale());
    unprocessable(run_op(
        &harness,
        "ValueSet/$expand",
        &json!({"url": categories}),
    ));

    repair_conformance(&harness.store, &harness.db, &harness.catalog).unwrap();
    assert!(!harness.catalog.read().unwrap().expansions_stale());
    let expanded = apply(&harness, "ValueSet/$expand", json!({"url": categories}));
    assert_eq!(expanded["expansion"]["total"], 38);
    assert_eq!(expansion_summary(&expanded).1.len(), 1);
    assert_expansions_match(&harness.store, "after the repair");
    // The rows it wrote say so, and a second start finds nothing to repeat.
    let stored: Value = serde_json::from_str(&stored_rows(&harness, categories)[0].json).unwrap();
    assert_eq!(stored["rules"], 4);
    assert_eq!(stored["incomplete"][0]["content"], "example");
    publish(&harness.store, &harness.catalog).unwrap();
    assert!(!harness.catalog.read().unwrap().expansions_stale());
}

/// A package of value sets that include a code system it does not carry (one of them through
/// another value set), and a package of that code system.
fn waiting_packages() -> (PathBuf, PathBuf) {
    let system = "http://example.org/arrived";
    let sets = dep_package("example.waiting.sets", "1.0.0", &[]);
    let value_set = |file: &str, url: &str, include: Value| {
        std::fs::write(
            sets.join(file),
            json!({"resourceType": "ValueSet", "url": url, "status": "active",
                   "compose": {"include": [include]}})
            .to_string(),
        )
        .unwrap();
    };
    value_set(
        "ValueSet-waits.json",
        "http://example.org/ValueSet/waits",
        json!({"system": system}),
    );
    value_set(
        "ValueSet-nested.json",
        "http://example.org/ValueSet/nested",
        json!({"valueSet": ["http://example.org/ValueSet/waits"]}),
    );
    // What no package carries stays as it was.
    value_set(
        "ValueSet-absent.json",
        "http://example.org/ValueSet/absent",
        json!({"system": "http://example.org/nobody-has-this"}),
    );
    let code_system = dep_package("example.waiting.system", "1.0.0", &[]);
    std::fs::write(
        code_system.join("CodeSystem-arrived.json"),
        json!({"resourceType": "CodeSystem", "url": system, "status": "active",
               "content": "complete", "concept": [{"code": "x"}, {"code": "y"}]})
        .to_string(),
    )
    .unwrap();
    (sets, code_system)
}

fn assert_waiting_value_sets_follow_what_arrived(harness: &Harness, when: &str) {
    for url in [
        "http://example.org/ValueSet/waits",
        "http://example.org/ValueSet/nested",
    ] {
        let codes: Vec<_> = expansion_of(harness, url)
            .iter()
            .filter_map(|item| item["code"].as_str().map(str::to_string))
            .collect();
        assert_eq!(codes, ["x", "y"], "{url} {when}");
    }
    unprocessable(run_op(
        harness,
        "ValueSet/$expand",
        &json!({"url": "http://example.org/ValueSet/absent"}),
    ));
}

#[test]
fn a_value_set_waiting_for_another_packages_code_system_is_expanded_when_the_two_are_together() {
    // Installing a package stores the expansions of its own value sets, so the value sets here
    // were stored with none; whichever package came first, the one that came second finds them.
    for code_system_first in [false, true] {
        let harness = empty();
        let (sets, code_system) = waiting_packages();
        let order = if code_system_first {
            [&code_system, &sets]
        } else {
            [&sets, &code_system]
        };
        for dir in order {
            install_package_dir(&harness.store, &harness.db, &harness.catalog, dir).unwrap();
        }
        let order = if code_system_first { "after" } else { "before" };
        assert_waiting_value_sets_follow_what_arrived(
            &harness,
            &format!("the code system came {order} the value sets"),
        );
        // The expansions are stored, under the package whose value set it is, so a catalog loaded
        // again from the store has them, and they are what a full rebuild would make.
        publish(&harness.store, &harness.catalog).unwrap();
        assert_waiting_value_sets_follow_what_arrived(&harness, "after the catalog was loaded");
        let holder = package_id(&harness, "example.waiting.sets");
        let rows = stored_rows(&harness, "http://example.org/ValueSet/waits");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].package_id, holder);
        assert_expansions_match(&harness.store, "with value sets that waited");
        assert!(harness
            .catalog
            .read()
            .unwrap()
            .terms()
            .unexpandable_urls()
            .iter()
            .all(|url| url == "http://example.org/ValueSet/absent"));
    }
}

#[test]
fn packages_installed_together_are_followed_by_the_same_pass() {
    // The install that stores several packages and loads the catalog once (a bundle, the
    // packages an IG depends on) goes through the same pass afterwards.
    let harness = empty();
    let (sets, code_system) = waiting_packages();
    let pending: Vec<_> = [&sets, &code_system]
        .into_iter()
        .filter_map(|dir| {
            stage_package_deferred(&harness.store, &harness.db, &harness.catalog, dir).unwrap()
        })
        .collect();
    finish_install(
        &harness.store,
        &harness.db,
        &harness.catalog,
        &pending,
        Ok(()),
    )
    .unwrap();
    assert_waiting_value_sets_follow_what_arrived(&harness, "installed together");
    assert_expansions_match(&harness.store, "installed together");
}

#[test]
fn a_write_stores_a_waiting_value_set_under_the_package_in_use_and_removing_it_clears_every_holder()
{
    // Two packages define one value set that includes a code system nothing carries, so
    // installing either cannot expand it. The second is the one in use.
    let (harness, _watched) = watched_r4();
    let (set, system) = (
        "http://example.org/ValueSet/shared",
        "http://example.org/shared-system",
    );
    let package = |name: &str, version: &str| {
        let dir = dep_package(name, version, &[]);
        std::fs::write(
            dir.join("ValueSet-shared.json"),
            json!({"resourceType": "ValueSet", "url": set, "version": version, "status": "active",
                   "compose": {"include": [{"system": system}]}})
            .to_string(),
        )
        .unwrap();
        install_package_dir(&harness.store, &harness.db, &harness.catalog, &dir).unwrap();
        dir
    };
    let first = package("example.shared.a", "1.0.0");
    let second = package("example.shared.b", "2.0.0");
    let (a, b) = (
        package_id(&harness, "example.shared.a"),
        package_id(&harness, "example.shared.b"),
    );
    assert!(run_op(&harness, "ValueSet/$expand", &json!({"url": set})).is_err());

    // A code system written through the API is what both were waiting for. The incremental update
    // expands the one in use and stores it under that package, not the first that defined it.
    let written = commit(
        &harness,
        "CodeSystem/create",
        json!({"resource": {"resourceType": "CodeSystem", "url": system, "status": "active",
                            "content": "complete", "concept": [{"code": "x"}, {"code": "y"}]}}),
    )
    .unwrap();
    let codes: Vec<_> = expansion_of(&harness, set)
        .iter()
        .filter_map(|item| item["code"].as_str().map(str::to_string))
        .collect();
    assert_eq!(codes, ["x", "y"]);
    let rows = stored_rows(&harness, set);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].package_id, b);

    // An expansion the other package still holds from before (what an earlier version left) is a
    // candidate a start would fall back to; removing the code system must clear it as well.
    harness
        .store
        .apply_expansions(&[refhir_persistence::ExpansionChange {
            package_id: a,
            url: set.to_string(),
            json: Some(
                json!({"url": set, "systems": [], "contains": [{"code": "stale"}]}).to_string(),
            ),
        }])
        .unwrap();
    commit(&harness, "CodeSystem/delete", json!({"id": written["id"]})).unwrap();
    assert!(stored_rows(&harness, set).is_empty());
    assert!(run_op(&harness, "ValueSet/$expand", &json!({"url": set})).is_err());
    for dir in [first, second] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn a_terminology_write_reads_and_rewrites_only_what_it_touches() {
    let (harness, watched) = watched_r4();
    let before = stored_expansions(&watched.inner);
    assert!(before.len() > 1000, "the R4 catalog is installed");
    watched.reset();

    let created = watched_commit(
        &harness,
        &watched,
        "ValueSet/create",
        json!({"resource": color_value_set("colors", &["red", "blue"], "http://example.org/sys")}),
    )
    .unwrap();
    // The catalog expands it at once.
    let expanded = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://example.org/colors"}),
    );
    assert_eq!(
        expanded["expansion"]["contains"].as_array().unwrap().len(),
        2
    );
    // Nothing that is not the new value set was read in full or rewritten.
    assert_eq!(WatchedStore::count(&watched.full_terminology_loads), 0);
    assert_eq!(WatchedStore::count(&watched.full_expansion_loads), 0);
    assert_eq!(WatchedStore::count(&watched.full_search_parameter_loads), 0);
    assert_eq!(WatchedStore::count(&watched.package_expansion_rewrites), 0);
    assert_eq!(WatchedStore::count(&watched.expansion_changes), 1);
    let after = stored_expansions(&watched.inner);
    assert_eq!(after.len(), before.len() + 1);
    for (url, document) in &before {
        assert_eq!(&after[url], document, "{url} was rewritten");
    }
    assert!(created["id"].is_string());
}

#[test]
fn a_run_of_terminology_writes_leaves_the_store_as_a_full_rebuild_would() {
    let (harness, watched) = watched_r4();
    assert_expansions_match(&watched.inner, "right after installing R4");
    let r4_before = stored_expansions(&watched.inner);
    let write = |path: &str, resource: Value, id: Option<&str>| {
        let mut args = json!({"resource": resource});
        if let Some(id) = id {
            args["id"] = json!(id);
        }
        watched_commit(&harness, &watched, path, args).unwrap()
    };
    // A value set, a code system, a value set over both, an edit that changes the one below, a
    // deletion that leaves the one above unexpandable, and a whole-system include of an
    // `example` system that never could expand.
    let first = write(
        "ValueSet/create",
        color_value_set("first", &["a"], "http://example.org/s"),
        None,
    );
    let first_id = first["id"].as_str().unwrap().to_string();
    let system = write(
        "CodeSystem/create",
        json!({"resourceType": "CodeSystem", "url": "http://example.org/shapes", "status": "active",
               "content": "complete", "concept": [{"code": "round"}, {"code": "flat"}]}),
        None,
    );
    let system_id = system["id"].as_str().unwrap().to_string();
    write(
        "ValueSet/create",
        json!({"resourceType": "ValueSet", "url": "http://example.org/over", "status": "active",
               "compose": {"include": [
                   {"valueSet": ["http://example.org/first"]},
                   {"system": "http://example.org/shapes"},
                   {"valueSet": ["http://hl7.org/fhir/ValueSet/administrative-gender"]}]}}),
        None,
    );
    let mut edited = color_value_set("first", &["a", "b", "c"], "http://example.org/s");
    edited["id"] = json!(first_id);
    write("ValueSet/update", edited, Some(&first_id));
    write(
        "ValueSet/create",
        json!({"resourceType": "ValueSet", "url": "http://example.org/never", "status": "active",
               "compose": {"include": [{"system": "http://terminology.hl7.org/CodeSystem/service-category"}]}}),
        None,
    );
    let mut changed_system = json!({"resourceType": "CodeSystem", "url": "http://example.org/shapes",
        "status": "active", "content": "complete", "id": system_id,
        "concept": [{"code": "round"}, {"code": "flat"}, {"code": "tall"}]});
    changed_system["id"] = json!(system_id);
    write("CodeSystem/update", changed_system, Some(&system_id));

    let over = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://example.org/over"}),
    );
    let codes: Vec<&str> = over["expansion"]["contains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["code"].as_str().unwrap())
        .collect();
    for expected in ["a", "b", "c", "round", "flat", "tall", "male"] {
        assert!(
            codes.contains(&expected),
            "{expected} missing from {codes:?}"
        );
    }
    assert_expansions_match(&watched.inner, "after the run of writes");
    assert_untouched(&watched.inner, &r4_before);

    // Removing a value set that another includes: the one above can no longer expand.
    watched_commit(
        &harness,
        &watched,
        "ValueSet/delete",
        json!({"id": first_id}),
    )
    .unwrap();
    let outcome = unprocessable(run_op(
        &harness,
        "ValueSet/$expand",
        &json!({"url": "http://example.org/over"}),
    ));
    assert!(
        outcome.to_string().contains("http://example.org/first"),
        "{outcome}"
    );
    assert_expansions_match(&watched.inner, "after deleting a value set");
    assert_untouched(&watched.inner, &r4_before);
    // Gone from the catalog and from the store.
    assert!(!stored_expansions(&watched.inner).contains_key("http://example.org/first"));

    // The catalog in memory says the same as one reopened from the store.
    let reopened = Catalog::open(&watched.inner).unwrap();
    let live = harness.catalog.read().unwrap();
    for url in [
        "http://example.org/over",
        "http://example.org/never",
        "http://hl7.org/fhir/ValueSet/administrative-gender",
    ] {
        assert_eq!(
            live.terms().why_not_expanded(url),
            reopened.terms().why_not_expanded(url),
            "{url}"
        );
        assert_eq!(
            live.terms().expand(url, None),
            reopened.terms().expand(url, None),
            "{url}"
        );
    }
}

/// What a request of a common kind needs is in memory when the catalog is built: it reads nothing
/// from the store, the first time or after. What is not common (a profile) is read once.
#[test]
fn a_request_of_a_common_kind_reads_nothing_from_the_store() {
    use refhir_fhirpath::Host;
    struct NoHost;
    impl Host for NoHost {}

    let harness = r4();
    let source = Arc::new(crate::lazy::StoreSource::new(Arc::new(
        harness.store.clone(),
    )));
    let catalog = Catalog::open_from(&harness.store, Some(source.clone())).unwrap();
    let after_open = source.reads();
    assert!(after_open > 0, "what a base type binds is read to be kept");
    let validate = |resource: &Value| {
        refhir_validate::validate_resource(resource, catalog.schema(), catalog.terms(), &NoHost)
            .operation_outcome()
    };
    let common = [
        json!({"resourceType": "Patient", "gender": "female", "birthDate": "1980-05-01",
               "name": [{"family": "Okafor", "given": ["Ada"]}],
               "maritalStatus": {"coding": [{
                   "system": "http://terminology.hl7.org/CodeSystem/v3-MaritalStatus",
                   "code": "M"}]},
               "communication": [{"language": {"coding": [{
                   "system": "urn:ietf:bcp:47", "code": "en"}]}}]}),
        json!({"resourceType": "Observation", "status": "final",
               "category": [{"coding": [{
                   "system": "http://terminology.hl7.org/CodeSystem/observation-category",
                   "code": "vital-signs"}]}],
               "code": {"coding": [{"system": "http://loinc.org", "code": "8480-6"}]},
               "valueQuantity": {"value": 120, "unit": "mmHg",
                                 "system": "http://unitsofmeasure.org", "code": "mm[Hg]"}}),
        json!({"resourceType": "Encounter", "status": "finished",
               "class": {"system": "http://terminology.hl7.org/CodeSystem/v3-ActCode",
                         "code": "AMB", "display": "ambulatory"}}),
        json!({"resourceType": "MedicationRequest", "status": "active", "intent": "order",
               "medicationCodeableConcept": {"text": "aspirin"}}),
        json!({"resourceType": "Encounter", "status": "wrong"}),
    ];
    let answers: Vec<Value> = common.iter().map(&validate).collect();
    for resource in &common {
        validate(resource);
    }
    assert_eq!(source.reads(), after_open, "a common request reads nothing");

    // A profile is not held: it is read the first time a request names it, and not again.
    let profiled = json!({"resourceType": "Observation", "status": "final",
        "meta": {"profile": ["http://hl7.org/fhir/StructureDefinition/vitalsigns"]},
        "code": {"coding": [{"system": "http://loinc.org", "code": "8480-6"}]}});
    let first = validate(&profiled);
    let read = source.reads();
    assert!(read > after_open);
    assert_eq!(validate(&profiled), first);
    assert_eq!(source.reads(), read);

    // Reading nothing is not answering differently.
    let whole = Catalog::open_whole(&harness.store).unwrap();
    for (resource, answer) in common.iter().zip(&answers) {
        let held =
            refhir_validate::validate_resource(resource, whole.schema(), whole.terms(), &NoHost)
                .operation_outcome();
        assert_eq!(&held, answer);
    }
}

/// A definition that cannot be read from the store is not a definition that does not exist: the
/// request that needed it fails, and a write is not committed on what was left to check it by.
#[test]
fn a_request_that_could_not_read_a_definition_fails_and_commits_nothing() {
    let harness = r4();
    let watched = Arc::new(WatchedStore::new(harness.store.clone()));
    let source = Arc::new(crate::lazy::StoreSource::new(watched.clone()));
    let catalog = Catalog::open_from(watched.as_ref(), Some(source)).unwrap();
    let profile = "http://hl7.org/fhir/StructureDefinition/vitalsigns";
    let observation = json!({"resourceType": "Observation", "status": "final",
        "meta": {"profile": [profile]},
        "code": {"coding": [{"system": "http://loinc.org", "code": "8480-6"}]}});
    let validate = || {
        run(
            &harness.db,
            &catalog,
            "Observation/$validate",
            &json!({"resource": observation}),
        )
    };

    // The profile is read when it is first used. If that read fails the request fails, and says
    // what failed, where it would have been told the profile is unknown.
    watched.fail_row_reads.store(usize::MAX, Ordering::SeqCst);
    match validate() {
        Err(RunError::Fhir(FhirError::Storage(message))) => {
            assert!(
                message.contains("could not read structure definition"),
                "{message}"
            );
            assert!(message.contains(profile), "{message}");
        }
        other => panic!("expected a storage failure, got {other:?}"),
    }
    let create = run(
        &harness.db,
        &catalog,
        "Observation/create",
        &json!({"resource": observation}),
    );
    assert!(
        matches!(create, Err(RunError::Fhir(FhirError::Storage(_)))),
        "{create:?}"
    );

    // Nothing was kept of the failure: the same request, once the store answers, is answered.
    watched.fail_row_reads.store(0, Ordering::SeqCst);
    let outcome = validate().expect("the store answers now").value;
    assert_eq!(outcome["resourceType"], "OperationOutcome");
    let created = run(
        &harness.db,
        &catalog,
        "Observation/create",
        &json!({"resource": observation}),
    );
    // Whatever the profile says of it, it is the profile that answered: no read failed.
    assert!(!matches!(
        created,
        Err(RunError::Fhir(FhirError::Storage(_)))
    ));
}

/// A catalog that takes the place of another has already read what the requests that were being
/// answered used, so the swap does not send them back to waiting for the store.
#[test]
fn a_catalog_that_replaces_another_has_read_what_the_old_one_had() {
    use refhir_fhirpath::Host;
    struct NoHost;
    impl Host for NoHost {}

    let harness = r4();
    let profile = "http://hl7.org/fhir/StructureDefinition/vitalsigns";
    let observation = json!({"resourceType": "Observation", "status": "final",
        "meta": {"profile": [profile]},
        "code": {"coding": [{"system": "http://loinc.org", "code": "8480-6"}]}});
    let validate = |catalog: &std::sync::RwLock<Catalog>| {
        let catalog = catalog.read().unwrap();
        refhir_validate::validate_resource(&observation, catalog.schema(), catalog.terms(), &NoHost)
            .operation_outcome()
    };
    let resident = |catalog: &std::sync::RwLock<Catalog>| {
        catalog
            .read()
            .unwrap()
            .schema()
            .resident()
            .contains(&profile.to_string())
    };
    let (replica, replica_at) =
        Catalog::open_consistent(&harness.store, crate::CatalogClock::Stable).unwrap();
    let replica = std::sync::RwLock::new(replica);
    assert!(!resident(&harness.catalog) && !resident(&replica));
    let answer = validate(&harness.catalog);
    validate(&replica);
    assert!(resident(&harness.catalog) && resident(&replica));

    // A publish builds a new catalog, which has read the profile before it is put in place.
    publish(&harness.store, &harness.catalog).unwrap();
    assert!(resident(&harness.catalog));
    assert_eq!(validate(&harness.catalog), answer);

    // So does a replica's reload when the version it follows has moved.
    let now = crate::package::reload_catalog(
        &harness.store,
        &replica,
        replica_at,
        crate::CatalogClock::Stable,
    )
    .unwrap();
    assert!(now.clock > replica_at.clock);
    assert!(resident(&replica));
    assert_eq!(validate(&replica), answer);
}

/// A catalog that reads definitions and terminology when they are first used, kept current by
/// writes as it is by a rebuild, and by a reload on a replica.
#[test]
fn a_catalog_read_when_used_follows_terminology_writes_and_reloads() {
    let harness = r4();
    let store = &harness.store;
    // Read before the writes: what a replica holds, and what it has read so far.
    let (replica, replica_at) =
        Catalog::open_consistent(store, crate::CatalogClock::Stable).unwrap();
    let replica = std::sync::RwLock::new(replica);
    let gender = "http://hl7.org/fhir/ValueSet/administrative-gender";
    let genders = |catalog: &std::sync::RwLock<Catalog>| -> Vec<String> {
        let expanded = catalog
            .read()
            .unwrap()
            .terms()
            .expand(gender, None)
            .unwrap();
        let mut codes: Vec<String> = expanded["expansion"]["contains"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["code"].as_str().unwrap().to_string())
            .collect();
        codes.sort();
        codes
    };
    assert_eq!(genders(&replica), ["female", "male", "other", "unknown"]);

    let write = |path: &str, resource: Value, id: Option<&str>| {
        let mut args = json!({"resource": resource});
        if let Some(id) = id {
            args["id"] = json!(id);
        }
        commit_operation(store, &harness.db, &harness.catalog, path, &args)
            .unwrap()
            .value
    };
    let shapes = |codes: &[&str], id: Option<&str>| {
        let mut system = json!({"resourceType": "CodeSystem", "url": "http://example.org/shapes",
            "status": "active", "content": "complete",
            "concept": codes.iter().map(|code| json!({"code": code})).collect::<Vec<_>>()});
        if let Some(id) = id {
            system["id"] = json!(id);
        }
        system
    };
    let first = write(
        "ValueSet/create",
        color_value_set("first", &["a"], "http://example.org/s"),
        None,
    );
    let first_id = first["id"].as_str().unwrap().to_string();
    let system = write("CodeSystem/create", shapes(&["round", "flat"], None), None);
    let system_id = system["id"].as_str().unwrap().to_string();
    write(
        "ValueSet/create",
        json!({"resourceType": "ValueSet", "url": "http://example.org/over", "status": "active",
               "compose": {"include": [
                   {"valueSet": ["http://example.org/first"]},
                   {"system": "http://example.org/shapes"},
                   {"valueSet": [gender]}]}}),
        None,
    );
    write(
        "CodeSystem/update",
        shapes(&["round", "flat", "tall"], Some(&system_id)),
        Some(&system_id),
    );
    let mut edited = color_value_set("first", &["a", "b"], "http://example.org/s");
    edited["id"] = json!(first_id);
    write("ValueSet/update", edited, Some(&first_id));
    assert_expansions_match(store, "after the writes");

    // What the live catalog says is what one that holds everything, opened from the store, says.
    let whole = Catalog::open_whole(store).unwrap();
    let live = harness.catalog.read().unwrap();
    for url in [
        "http://example.org/over",
        "http://example.org/first",
        gender,
    ] {
        assert_eq!(
            live.terms().expand(url, None),
            whole.terms().expand(url, None),
            "{url}"
        );
    }
    let codes = |catalog: &Catalog| -> Vec<String> {
        let expanded = catalog
            .terms()
            .expand("http://example.org/over", None)
            .unwrap();
        expanded["expansion"]["contains"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["code"].as_str().unwrap().to_string())
            .collect()
    };
    for expected in ["a", "b", "round", "flat", "tall", "male"] {
        assert!(codes(&live).contains(&expected.to_string()), "{expected}");
    }
    assert_eq!(
        live.terms()
            .lookup("http://example.org/shapes", None, "tall"),
        whole
            .terms()
            .lookup("http://example.org/shapes", None, "tall")
    );
    assert!(live
        .terms()
        .lookup("http://example.org/shapes", None, "tall")
        .is_some());
    drop(live);

    // The replica has not seen any of it. A change that publishes the catalog (a package, a
    // search parameter) moves the version it follows, and it reloads: what it had read does not
    // outlive the catalog it was read for.
    assert!(replica
        .read()
        .unwrap()
        .terms()
        .expand("http://example.org/over", None)
        .is_none());
    publish(store, &harness.catalog).unwrap();
    let now =
        crate::package::reload_catalog(store, &replica, replica_at, crate::CatalogClock::Stable)
            .unwrap();
    assert!(now.clock > replica_at.clock);
    assert_eq!(genders(&replica), ["female", "male", "other", "unknown"]);
    let replicated = replica.read().unwrap();
    assert_eq!(
        replicated.terms().expand("http://example.org/over", None),
        whole.terms().expand("http://example.org/over", None)
    );
    assert_eq!(
        replicated
            .terms()
            .lookup("http://example.org/shapes", None, "tall"),
        whole
            .terms()
            .lookup("http://example.org/shapes", None, "tall")
    );
    drop(replicated);

    // Deleting a code system leaves what includes it unable to expand, in the live catalog and
    // in one reloaded from the store.
    write_delete(&harness, "CodeSystem/delete", &system_id);
    let whole = Catalog::open_whole(store).unwrap();
    let live = harness.catalog.read().unwrap();
    assert_eq!(
        live.terms().why_not_expanded("http://example.org/over"),
        whole.terms().why_not_expanded("http://example.org/over")
    );
    assert!(live
        .terms()
        .why_not_expanded("http://example.org/over")
        .is_some());
    assert!(live
        .terms()
        .lookup("http://example.org/shapes", None, "tall")
        .is_none());
}

fn write_delete(harness: &Harness, path: &str, id: &str) {
    commit_operation(
        &harness.store,
        &harness.db,
        &harness.catalog,
        path,
        &json!({"id": id}),
    )
    .unwrap();
}

#[test]
fn a_failed_incremental_update_falls_back_to_a_full_rebuild() {
    let (harness, watched) = watched_r4();
    watched.fail_apply_expansions.store(1, Ordering::SeqCst);
    watched.reset();
    watched_commit(
        &harness,
        &watched,
        "ValueSet/create",
        json!({"resource": color_value_set("fallback", &["a"], "http://example.org/s")}),
    )
    .unwrap();
    // The write succeeded and the catalog is right, by the long way.
    assert!(WatchedStore::count(&watched.full_terminology_loads) > 0);
    let expanded = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://example.org/fallback"}),
    );
    assert_eq!(expanded["expansion"]["contains"][0]["code"], "a");
    assert_expansions_match(&watched.inner, "after the fallback rebuild");
}

#[test]
fn a_store_failure_after_commit_is_repaired_or_says_the_write_happened() {
    let harness = ready();
    install_resource_types(&harness);
    let watched = WatchedStore::new(harness.store.clone());

    // One failure: the repair that follows succeeds, so the request succeeds.
    watched.fail_upsert_terminology.store(1, Ordering::SeqCst);
    let ok = watched_commit(
        &harness,
        &watched,
        "ValueSet/create",
        json!({"resource": color_value_set("repaired", &["a"], "http://example.org/s")}),
    )
    .unwrap();
    assert!(ok["id"].is_string());
    let expanded = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://example.org/repaired"}),
    );
    assert_eq!(expanded["expansion"]["contains"][0]["code"], "a");

    // Two: the repair fails as well. The error says the document was saved, and a later repair
    // finishes the job.
    watched.fail_upsert_terminology.store(2, Ordering::SeqCst);
    let err = watched_commit(
        &harness,
        &watched,
        "ValueSet/create",
        json!({"resource": color_value_set("kept", &["b"], "http://example.org/s")}),
    )
    .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("the write was committed"), "{message}");
    assert!(message.contains("repair"), "{message}");
    let found = apply(&harness, "ValueSet/search", json!({}));
    assert_eq!(found["total"], 2, "both documents are stored");
    repair_conformance(&watched, &harness.db, &harness.catalog).unwrap();
    let expanded = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://example.org/kept"}),
    );
    assert_eq!(expanded["expansion"]["contains"][0]["code"], "b");
}

fn encounter(extra: Value) -> Value {
    let mut encounter =
        json!({"resourceType": "Encounter", "status": "planned", "class": {"code": "AMB"}});
    for (key, value) in extra.as_object().unwrap() {
        encounter[key] = value.clone();
    }
    encounter
}

#[test]
fn references_inside_repeating_elements_are_checked_like_the_rest() {
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();
    apply(
        &harness,
        "Practitioner/create",
        json!({"resource": {"resourceType": "Practitioner", "id": "there"}}),
    );
    let create =
        |resource: Value| run_op(&harness, "Encounter/create", &json!({"resource": resource}));

    // One that exists is fine, however deep it sits.
    create(encounter(
        json!({"participant": [{"individual": {"reference": "Practitioner/there"}}]}),
    ))
    .unwrap();
    // One that does not exist is refused, and the message says where.
    let missing = create(encounter(json!({"participant": [
        {"individual": {"reference": "Practitioner/there"}},
        {"individual": {"reference": "Practitioner/nobody"}}
    ]})))
    .unwrap_err();
    assert!(
        missing
            .to_string()
            .contains("participant.individual Practitioner/nobody does not exist"),
        "{missing}"
    );
    // The type has to be one the element allows. That was already true of the validator; the
    // write path now agrees.
    let wrong = create(encounter(
        json!({"participant": [{"individual": {"reference": "Organization/there"}}]}),
    ))
    .unwrap_err();
    assert!(
        matches!(wrong, RunError::Fhir(FhirError::Invalid(_))),
        "{wrong}"
    );
    // An element that is absent needs nothing: its minimum applies only if its parent is there.
    create(encounter(json!({}))).unwrap();
    create(encounter(
        json!({"participant": [{"type": [{"text": "x"}]}]}),
    ))
    .unwrap();

    // Under the `fhir` policy a reference may point at something this server does not hold.
    harness
        .catalog
        .write()
        .unwrap()
        .set_reference_policy(ReferencePolicy::Fhir);
    create(encounter(
        json!({"participant": [{"individual": {"reference": "Practitioner/nobody"}}]}),
    ))
    .unwrap();
}

#[test]
fn a_transaction_creates_what_a_nested_reference_names_before_the_resource_that_names_it() {
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();
    let practitioner = "urn:uuid:33333333-3333-4333-8333-333333333333";
    let bundle = json!({
        "resourceType": "Bundle", "type": "transaction",
        "entry": [
            // The Encounter comes first and names a Practitioner two levels down.
            {"request": {"method": "POST", "url": "Encounter"},
             "resource": encounter(json!({"participant": [{"individual": {"reference": practitioner}}]}))},
            {"fullUrl": practitioner, "request": {"method": "POST", "url": "Practitioner"},
             "resource": {"resourceType": "Practitioner"}}
        ]
    });
    let response = apply(&harness, "Bundle/transaction", json!({"resource": bundle}));
    let practitioner_id = response["entry"][1]["resource"]["id"].as_str().unwrap();
    assert_eq!(
        response["entry"][0]["resource"]["participant"][0]["individual"]["reference"],
        format!("Practitioner/{practitioner_id}")
    );
}

#[test]
fn the_versions_of_a_code_system_in_a_package_each_expand_their_own_value_sets() {
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();
    let codes = |url: &str| -> usize {
        let expanded = apply(&harness, "ValueSet/$expand", json!({"url": url}));
        expanded["expansion"]["contains"].as_array().unwrap().len()
    };
    // HL7's v2 tables: one URL, several versions, and the value set for a version names it.
    assert_eq!(codes("http://terminology.hl7.org/ValueSet/v2-2.1-0006"), 9);
    assert_eq!(codes("http://terminology.hl7.org/ValueSet/v2-2.4-0006"), 86);
    // A value set that pins a version the package does not carry is refused, and the message says
    // which versions it does: task-code asks for 3.6.0 and the package has 4.0.1.
    let pinned = unprocessable(run_op(
        &harness,
        "ValueSet/$expand",
        &json!({"url": "http://hl7.org/fhir/ValueSet/task-code"}),
    ));
    let text = pinned["issue"][0]["diagnostics"].as_str().unwrap();
    assert!(
        text.contains("not at version 3.6.0") && text.contains("installed: 4.0.1"),
        "{text}"
    );
    // Nothing the stored rows hold depends on load order.
    assert_expansions_match(&harness.store, "after installing R4");
}

fn code_system_version(url: &str, version: &str, codes: &[&str]) -> Value {
    json!({
        "resourceType": "CodeSystem", "url": url, "version": version, "status": "active",
        "content": "complete",
        "concept": codes.iter().map(|code| json!({"code": code})).collect::<Vec<_>>()
    })
}

fn including(url: &str, system: &str, version: Option<&str>) -> Value {
    let mut include = json!({"system": system});
    if let Some(version) = version {
        include["version"] = json!(version);
    }
    json!({"resourceType": "ValueSet", "url": url, "status": "active", "compose": {"include": [include]}})
}

#[test]
fn versions_of_a_code_system_written_through_the_api_are_kept_side_by_side() {
    let harness = ready();
    install_resource_types(&harness);
    let system = "http://example.org/versions";
    let create = |resource: Value| {
        let path = format!("{}/create", resource["resourceType"].as_str().unwrap());
        commit(&harness, &path, json!({"resource": resource})).unwrap()
    };
    let codes_of = |url: &str| -> Option<Vec<String>> {
        let result = commit(&harness, "ValueSet/$expand", json!({"url": url})).ok()?;
        Some(
            result["expansion"]["contains"]
                .as_array()?
                .iter()
                .map(|entry| entry["code"].as_str().unwrap().to_string())
                .collect(),
        )
    };
    let rows = |store: &SqlitePersistence| -> Vec<String> {
        store
            .load_terminology_for(&[system.to_string()])
            .unwrap()
            .into_iter()
            .map(|row| row.url)
            .collect()
    };

    let first = create(code_system_version(system, "1", &["a"]));
    let second = create(code_system_version(system, "2", &["a", "b"]));
    create(including("http://example.org/pinned-1", system, Some("1")));
    create(including("http://example.org/pinned-2", system, Some("2")));
    create(including("http://example.org/latest", system, None));
    create(including(
        "http://example.org/by-suffix",
        &format!("{system}|1"),
        None,
    ));
    // Both versions are stored, as two rows, and each value set got the one it names.
    assert_eq!(
        rows(&harness.store),
        vec![format!("{system}|1"), format!("{system}|2")]
    );
    assert_eq!(
        codes_of("http://example.org/pinned-1"),
        Some(vec!["a".into()])
    );
    assert_eq!(
        codes_of("http://example.org/pinned-2"),
        Some(vec!["a".into(), "b".into()])
    );
    assert_eq!(
        codes_of("http://example.org/by-suffix"),
        Some(vec!["a".into()])
    );
    assert_eq!(
        codes_of("http://example.org/latest"),
        Some(vec!["a".into(), "b".into()])
    );

    // Writing a new content for one version changes what names it, and only that.
    let mut changed = code_system_version(system, "1", &["a", "z"]);
    changed["id"] = first["id"].clone();
    commit(
        &harness,
        "CodeSystem/update",
        json!({"id": first["id"], "resource": changed}),
    )
    .unwrap();
    assert_eq!(
        codes_of("http://example.org/pinned-1"),
        Some(vec!["a".into(), "z".into()])
    );
    assert_eq!(
        codes_of("http://example.org/pinned-2"),
        Some(vec!["a".into(), "b".into()])
    );

    // An update that gives the resource a new version is that version now, not an extra one.
    let mut moved = code_system_version(system, "3", &["q"]);
    moved["id"] = first["id"].clone();
    commit(
        &harness,
        "CodeSystem/update",
        json!({"id": first["id"], "resource": moved}),
    )
    .unwrap();
    assert_eq!(
        rows(&harness.store),
        vec![format!("{system}|2"), format!("{system}|3")]
    );
    let gone = unprocessable(run_op(
        &harness,
        "ValueSet/$expand",
        &json!({"url": "http://example.org/pinned-1"}),
    ));
    assert!(gone.to_string().contains("not at version 1"), "{gone}");
    assert_eq!(
        codes_of("http://example.org/latest"),
        Some(vec!["q".into()]),
        "3 is the latest"
    );

    // Deleting a version takes only that one.
    commit(&harness, "CodeSystem/delete", json!({"id": second["id"]})).unwrap();
    assert_eq!(rows(&harness.store), vec![format!("{system}|3")]);
    assert!(!has_stored_expansion(
        &harness,
        "http://example.org/pinned-2"
    ));

    // What the store holds is what a rebuild from it gives, in memory and on disk, and a repair
    // finds nothing to do.
    assert_expansions_match(&harness.store, "after the writes");
    let before = stored_expansions(&harness.store);
    repair_conformance(&harness.store, &harness.db, &harness.catalog).unwrap();
    assert_eq!(stored_expansions(&harness.store), before);
    assert_eq!(rows(&harness.store), vec![format!("{system}|3")]);
    let reopened = Catalog::open(&harness.store).unwrap();
    let live = harness.catalog.read().unwrap();
    for url in [
        "http://example.org/latest",
        "http://example.org/pinned-1",
        "http://example.org/pinned-2",
    ] {
        assert_eq!(
            live.terms().expand(url, None),
            reopened.terms().expand(url, None),
            "{url}"
        );
        assert_eq!(
            live.terms().why_not_expanded(url),
            reopened.terms().why_not_expanded(url),
            "{url}"
        );
    }
}

fn has_stored_expansion(harness: &Harness, url: &str) -> bool {
    stored_expansions(&harness.store).contains_key(url)
}

#[test]
fn a_code_system_stored_before_versions_had_rows_moves_to_its_own_row_when_written_again() {
    let harness = ready();
    install_resource_types(&harness);
    let system = "http://example.org/legacy";
    // As it was written then: a code system with a version, under its bare URL.
    let mut legacy = code_system_version(system, "5", &["a"]);
    legacy["id"] = json!("legacy");
    apply(
        &harness,
        "CodeSystem/create",
        json!({"resource": legacy.clone()}),
    );
    let local = harness.store.ensure_local_package("4.0.1").unwrap();
    harness
        .store
        .upsert_terminology(
            local,
            &NewTerminology {
                resource_type: "CodeSystem".into(),
                url: system.into(),
                json: legacy.to_string(),
            },
        )
        .unwrap();
    let urls = |harness: &Harness| -> Vec<String> {
        harness
            .store
            .load_terminology_for(&[system.to_string()])
            .unwrap()
            .into_iter()
            .map(|row| row.url)
            .collect()
    };
    assert_eq!(urls(&harness), vec![system.to_string()]);
    // Repair does the same as a write would.
    repair_conformance(&harness.store, &harness.db, &harness.catalog).unwrap();
    assert_eq!(
        urls(&harness),
        vec![format!("{system}|5")],
        "one row, under its version"
    );
}

#[test]
fn a_conditional_create_that_matches_is_what_other_entries_refer_to() {
    let harness = ready();
    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    );
    let urn = "urn:uuid:44444444-4444-4444-8444-444444444444";
    let bundle = json!({
        "resourceType": "Bundle", "type": "transaction",
        "entry": [
            {"fullUrl": urn,
             "request": {"method": "POST", "url": "Patient", "ifNoneExist": "identifier=http://hospital.example|mrn-p1"},
             "resource": patient("ignored", true, "Ann")},
            {"request": {"method": "POST", "url": "Observation"},
             "resource": {"resourceType": "Observation", "subject": {"reference": urn}, "effectiveDateTime": "2020-05-01"}}
        ]
    });
    let response = apply(&harness, "Bundle/transaction", json!({"resource": bundle}));
    assert_eq!(response["entry"][0]["response"]["status"], "200 OK");
    assert_eq!(response["entry"][0]["resource"]["id"], "p1");
    assert_eq!(
        response["entry"][1]["resource"]["subject"]["reference"],
        "Patient/p1"
    );
}

#[test]
fn deleting_a_local_value_set_leaves_a_package_holding_that_url_in_charge() {
    let harness = ready();
    install_resource_types(&harness);
    let url = "http://example.org/shared";
    let local = json!({
        "resourceType": "ValueSet", "status": "active", "url": url,
        "compose": {"include": [{"system": "http://example.org/s", "concept": [{"code": "local"}]}]}
    });
    let created = commit_operation(
        &harness.store,
        &harness.db,
        &harness.catalog,
        "ValueSet/create",
        &json!({"resource": local}),
    )
    .unwrap();
    let id = created.value["id"].as_str().unwrap().to_string();
    // A package installed later brings a value set with the same canonical.
    let package = json!({
        "resourceType": "ValueSet", "status": "active", "url": url,
        "expansion": {"contains": [{"system": "http://example.org/s", "code": "package"}]}
    });
    harness
        .store
        .install_package(&NewPackage {
            name: "shared-ig".into(),
            version: "1.0.0".into(),
            fhir_version: "4.0.1".into(),
            structure_definitions: vec![],
            search_parameters: vec![],
            terminology: vec![NewTerminology {
                resource_type: "ValueSet".into(),
                url: url.into(),
                json: package.to_string(),
            }],
            expansions: vec![],
        })
        .unwrap();
    commit_operation(
        &harness.store,
        &harness.db,
        &harness.catalog,
        "ValueSet/delete",
        &json!({"id": id}),
    )
    .unwrap();
    let expanded = run_op(&harness, "ValueSet/$expand", &json!({"url": url})).unwrap();
    let codes: Vec<&str> = expanded.value["expansion"]["contains"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["code"].as_str())
        .collect();
    assert_eq!(codes, vec!["package"]);
}

/// What the server hands an operation beside the caller's own arguments: the scopes of the token,
/// and whether the server is an archiver. They are not search parameters, and an array among the
/// arguments of a search is not a reason to refuse it.
fn server_controls() -> Value {
    json!({"_readScopes": ["user/Patient.read"], "_sdcArchiver": true})
}

#[test]
fn the_server_can_ask_for_a_page_when_a_search_names_no_count() {
    let harness = ready();
    for n in 0..5 {
        apply(
            &harness,
            "Patient/create",
            json!({"resource": patient(&format!("p{n}"), true, "Ann")}),
        );
    }
    let search = |args: Value| apply(&harness, "Patient/search", args);
    let link = |bundle: &Value, relation: &str| -> Option<String> {
        bundle["link"]
            .as_array()
            .unwrap()
            .iter()
            .find(|link| link["relation"] == relation)
            .map(|link| link["url"].as_str().unwrap().to_string())
    };
    // The page applies as if the request had said `_count`, and `total` counts every match.
    let found = search(json!({"_defaultCount": "2"}));
    assert_eq!(found["entry"].as_array().unwrap().len(), 2);
    assert_eq!(found["total"], 5);
    assert!(link(&found, "self").unwrap().contains("_count=2"));
    let next = link(&found, "next").expect("a next link");
    assert!(
        next.contains("_count=2") && next.contains("_cursor="),
        "{next}"
    );
    assert!(!next.contains("_defaultCount"), "{next}");
    // The request's own `_count` is used, `_count=0` included.
    let found = search(json!({"_defaultCount": "2", "_count": "4"}));
    assert_eq!(found["entry"].as_array().unwrap().len(), 4);
    let found = search(json!({"_defaultCount": "2", "_count": "0"}));
    assert_eq!(found["entry"].as_array().unwrap().len(), 0);
    assert_eq!(found["total"], 5);
    // Nothing asked for, nothing paged; so for a size of 0.
    for args in [json!({}), json!({"_defaultCount": "0"})] {
        let found = search(args);
        assert_eq!(found["entry"].as_array().unwrap().len(), 5);
        assert!(link(&found, "next").is_none());
    }
    // The server's word is a number.
    for bad in [json!("many"), json!(3), json!(["2"])] {
        let refused =
            run_op(&harness, "Patient/search", &json!({"_defaultCount": bad})).unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("_defaultCount must be a number"),
            "{refused}"
        );
    }
}

#[test]
fn a_control_that_takes_one_value_is_refused_as_an_array_not_misread() {
    let harness = ready();
    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    );
    for name in [
        "_count",
        "_summary",
        "_total",
        "_offset",
        "_cursor",
        "_compartment",
    ] {
        let refused = run_op(&harness, "Patient/search", &json!({name: ["a", "b"]})).unwrap_err();
        assert!(
            refused
                .to_string()
                .contains(&format!("{name} takes one value")),
            "{name}: {refused}"
        );
    }
    // The controls that are lists, and the parameters a repeat means something for, still take one.
    let found = apply(
        &harness,
        "Patient/search",
        json!({"_sort": ["name", "-_id"], "active": ["true", "true"], "_include": ["Patient:link"]}),
    );
    assert_eq!(found["total"], 1);
}

#[test]
fn a_search_is_not_refused_for_the_controls_the_server_adds() {
    let harness = ready();
    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    );
    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p2", false, "Bea")}),
    );
    let mut args = json!({"active": "true"});
    for (name, value) in server_controls().as_object().unwrap() {
        args[name] = value.clone();
    }
    let found = apply(&harness, "Patient/search", args);
    assert_eq!(
        found["total"], 1,
        "the controls did not become search parameters"
    );
    assert_eq!(found["entry"][0]["resource"]["id"], "p1");
    // Nothing else about a search changed: an argument that is not a string (or, for a parameter
    // given more than once, an array of them) is still refused.
    for bad in [
        json!(true),
        json!(1),
        json!([]),
        json!(["true", 1]),
        json!({}),
    ] {
        let refused = run_op(&harness, "Patient/search", &json!({"active": bad})).unwrap_err();
        assert!(
            refused.to_string().contains("must be a string"),
            "{bad}: {refused}"
        );
    }
}

#[test]
fn a_search_in_a_batch_is_not_refused_for_the_scopes_every_entry_carries() {
    let harness = ready();
    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    );
    let mut args = json!({"resource": {
        "resourceType": "Bundle",
        "type": "batch",
        "entry": [
            {"request": {"method": "GET", "url": "Patient?active=true"}},
            {"request": {"method": "GET", "url": "Patient/p1"}}
        ]
    }});
    for (name, value) in server_controls().as_object().unwrap() {
        args[name] = value.clone();
    }
    let batch = apply(&harness, "Bundle/batch", args);
    assert_eq!(batch["entry"][0]["response"]["status"], "200 OK", "{batch}");
    assert_eq!(batch["entry"][0]["resource"]["total"], 1);
    assert_eq!(batch["entry"][1]["resource"]["id"], "p1");
}

/// A comma between the values of a parameter is OR, a repeated parameter is AND, and `:iterate`
/// follows references from the resources `_include` and `_revinclude` have added. The data is the
/// shape of a booking system: slots on schedules, whose actors are a healthcare service and a
/// location.
mod or_and_iterate {
    use super::*;

    fn create(harness: &Harness, resource: Value) {
        let path = format!("{}/create", resource["resourceType"].as_str().unwrap());
        apply(harness, &path, json!({"resource": resource}));
    }

    fn r4() -> Harness {
        let harness = empty();
        install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();
        harness
    }

    fn slot(id: &str, schedule: &str, status: &str, hour: u8, types: &[&str]) -> Value {
        let mut slot = json!({
            "resourceType": "Slot",
            "id": id,
            "schedule": {"reference": format!("Schedule/{schedule}")},
            "status": status,
            "start": format!("2026-11-01T{hour:02}:00:00Z"),
            "end": format!("2026-11-01T{hour:02}:30:00Z")
        });
        if !types.is_empty() {
            slot["serviceType"] = types
                .iter()
                .map(|code| json!({"coding": [{"system": "http://example.org/st", "code": code}]}))
                .collect();
        }
        slot
    }

    /// The booking data the tests that only search share: seeded once for the module, not once
    /// per test.
    struct Booking(&'static Harness);

    impl std::ops::Deref for Booking {
        type Target = Harness;

        fn deref(&self) -> &Harness {
            self.0
        }
    }

    fn booking() -> Booking {
        static SEEDED: std::sync::OnceLock<Harness> = std::sync::OnceLock::new();
        Booking(SEEDED.get_or_init(seed))
    }

    /// Two healthcare services, four locations (`l2` is part of `l1`; `l3` and `l4` are part of
    /// each other), two schedules (`s1` has `hs1` and `l1` as actors, `s2` has `hs2`) and four
    /// slots: `sl1` and `sl3` are free, `sl2` is busy and `sl4` is busy-unavailable.
    fn seed() -> Harness {
        let harness = r4();
        create(
            &harness,
            json!({"resourceType": "HealthcareService", "id": "hs1", "name": "Cardiology"}),
        );
        create(
            &harness,
            json!({"resourceType": "HealthcareService", "id": "hs2", "name": "Dermatology"}),
        );
        create(
            &harness,
            json!({"resourceType": "Location", "id": "l1", "name": "Wing A, North"}),
        );
        create(
            &harness,
            json!({
                "resourceType": "Location", "id": "l2", "name": "North Annex",
                "partOf": {"reference": "Location/l1"}
            }),
        );
        create(
            &harness,
            json!({"resourceType": "Location", "id": "l3", "name": "Cycle A"}),
        );
        create(
            &harness,
            json!({
                "resourceType": "Location", "id": "l4", "name": "Cycle B",
                "partOf": {"reference": "Location/l3"}
            }),
        );
        apply(
            &harness,
            "Location/update",
            json!({"id": "l3", "resource": {
                "resourceType": "Location", "id": "l3", "name": "Cycle A",
                "partOf": {"reference": "Location/l4"}
            }}),
        );
        create(
            &harness,
            json!({
                "resourceType": "Schedule", "id": "s1",
                "actor": [
                    {"reference": "HealthcareService/hs1"},
                    {"reference": "Location/l1"}
                ]
            }),
        );
        create(
            &harness,
            json!({
                "resourceType": "Schedule", "id": "s2",
                "actor": [{"reference": "HealthcareService/hs2"}]
            }),
        );
        create(&harness, slot("sl1", "s1", "free", 9, &["general", "xray"]));
        create(&harness, slot("sl2", "s1", "busy", 10, &["general"]));
        create(&harness, slot("sl3", "s2", "free", 11, &["xray"]));
        create(&harness, slot("sl4", "s2", "busy-unavailable", 12, &[]));
        harness
    }

    fn search(harness: &Harness, type_name: &str, args: Value) -> Value {
        apply(harness, &format!("{type_name}/search"), args)
    }

    /// The `Type/id` of the entries with this search mode, sorted.
    fn with_mode(bundle: &Value, mode: &str) -> Vec<String> {
        let mut found: Vec<String> = bundle["entry"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["search"]["mode"] == mode)
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

    fn total(harness: &Harness, type_name: &str, args: Value) -> u64 {
        search(harness, type_name, args)["total"].as_u64().unwrap()
    }

    #[test]
    fn a_comma_between_token_values_is_or() {
        let harness = booking();
        assert_eq!(total(&harness, "Slot", json!({"status": "free"})), 2);
        assert_eq!(total(&harness, "Slot", json!({"status": "free,busy"})), 3);
        assert_eq!(total(&harness, "Slot", json!({"status": "free,zzz"})), 2);
        assert_eq!(total(&harness, "Slot", json!({"status": "zzz,yyy"})), 0);
        assert_eq!(
            total(
                &harness,
                "Slot",
                json!({"status": "free,busy,busy-unavailable"})
            ),
            4
        );
        // A repeated alternative, or an empty one, changes nothing.
        assert_eq!(
            total(&harness, "Slot", json!({"status": "free,,busy,free,"})),
            3
        );
        let found = search(&harness, "Slot", json!({"status": "free,busy"}));
        assert_eq!(
            with_mode(&found, "match"),
            ["Slot/sl1", "Slot/sl2", "Slot/sl3"]
        );
    }

    #[test]
    fn a_page_of_an_or_search_counts_every_match_and_can_be_followed() {
        let harness = booking();
        let next = |bundle: &Value| {
            bundle["link"]
                .as_array()
                .unwrap()
                .iter()
                .find(|link| link["relation"] == "next")
                .map(|link| link["url"].as_str().unwrap().to_string())
        };
        let mut query = json!({"status": "free,busy", "_count": "2"});
        let first = search(&harness, "Slot", query.clone());
        assert_eq!(first["total"], 3);
        let mut found = with_mode(&first, "match");
        assert_eq!(found.len(), 2);
        let url = next(&first).expect("a next page");
        query["_cursor"] = json!(url.split("_cursor=").nth(1).unwrap());
        let second = search(&harness, "Slot", query);
        assert_eq!(second["total"], 3);
        found.extend(with_mode(&second, "match"));
        found.sort();
        assert_eq!(found, ["Slot/sl1", "Slot/sl2", "Slot/sl3"]);
        assert!(next(&second).is_none());
        // No total asked for: the same pages.
        let quick = search(
            &harness,
            "Slot",
            json!({"status": "free,busy", "_count": "2", "_total": "none"}),
        );
        assert_eq!(with_mode(&quick, "match").len(), 2);
        assert!(next(&quick).is_some());
    }

    #[test]
    fn a_comma_between_ids_is_or() {
        let harness = booking();
        let ids = |value: &str| total(&harness, "HealthcareService", json!({"_id": value}));
        assert_eq!(ids("hs1"), 1);
        assert_eq!(ids("hs1,zzz"), 1);
        assert_eq!(ids("zzz,hs1"), 1);
        assert_eq!(ids("hs1,hs2"), 2);
        assert_eq!(ids("hs2,hs2,hs1"), 2);
        assert_eq!(ids("zzz,yyy"), 0);
        let found = search(
            &harness,
            "HealthcareService",
            json!({"_id": "hs1,zzz", "_include": "HealthcareService:location"}),
        );
        assert_eq!(with_mode(&found, "match"), ["HealthcareService/hs1"]);
    }

    #[test]
    fn a_comma_is_or_for_every_kind_of_parameter() {
        let harness = booking();
        // A document that several of the values find is listed once.
        assert_eq!(
            total(&harness, "Slot", json!({"service-type": "general,xray"})),
            3
        );
        assert_eq!(
            total(
                &harness,
                "Slot",
                json!({"service-type": "http://example.org/st|general,http://example.org/st|xray"})
            ),
            3
        );
        // A reference.
        assert_eq!(
            total(
                &harness,
                "Schedule",
                json!({"actor": "HealthcareService/hs1,Location/l1"})
            ),
            1
        );
        assert_eq!(
            total(
                &harness,
                "Schedule",
                json!({"actor": "HealthcareService/hs1,HealthcareService/hs2"})
            ),
            2
        );
        // A string: two prefixes that do not overlap, and two that do.
        assert_eq!(
            total(&harness, "HealthcareService", json!({"name": "Card,Derm"})),
            2
        );
        assert_eq!(
            total(&harness, "HealthcareService", json!({"name": "C,Cardio"})),
            1
        );
        // A date: a prefix and a bound.
        assert_eq!(
            total(
                &harness,
                "Slot",
                json!({"start": "2026-11-01T09,ge2026-11-01T12:00:00Z"})
            ),
            2
        );
        // `:exact` and `:contains` are matched against each value.
        assert_eq!(
            total(
                &harness,
                "HealthcareService",
                json!({"name:exact": "Cardiology,dermatology"})
            ),
            1
        );
        assert_eq!(
            total(
                &harness,
                "HealthcareService",
                json!({"name:contains": "ology,matol"})
            ),
            2
        );
    }

    #[test]
    fn not_excludes_every_value_a_comma_lists() {
        let harness = booking();
        let found = search(&harness, "Slot", json!({"status:not": "free,busy"}));
        assert_eq!(with_mode(&found, "match"), ["Slot/sl4"]);
        let found = search(&harness, "Slot", json!({"status:not": "free"}));
        assert_eq!(with_mode(&found, "match"), ["Slot/sl2", "Slot/sl4"]);
    }

    #[test]
    fn an_escaped_comma_is_part_of_the_value() {
        let harness = booking();
        // Two alternatives: "Wing A" (the first location) and "North" (the second).
        assert_eq!(
            total(&harness, "Location", json!({"name": "Wing A,North"})),
            2
        );
        // One value, with a comma in it.
        let found = search(&harness, "Location", json!({"name": r"Wing A\, North"}));
        assert_eq!(with_mode(&found, "match"), ["Location/l1"]);
        let found = search(&harness, "Location", json!({"name": r"Wing A\, North,Zzz"}));
        assert_eq!(with_mode(&found, "match"), ["Location/l1"]);
    }

    #[test]
    fn a_repeated_parameter_is_and() {
        let harness = booking();
        let range =
            crate::join_repeated("start", "ge2026-11-01T10:00:00Z", "lt2026-11-01T12:00:00Z");
        let found = search(&harness, "Slot", json!({"start": range}));
        assert_eq!(with_mode(&found, "match"), ["Slot/sl2", "Slot/sl3"]);
        // AND of OR lists.
        let found = search(
            &harness,
            "Slot",
            json!({"start": range, "status": "free,busy"}),
        );
        assert_eq!(with_mode(&found, "match"), ["Slot/sl2", "Slot/sl3"]);
        let found = search(
            &harness,
            "Slot",
            json!({"status": crate::join_repeated("status", "free,busy", "free")}),
        );
        assert_eq!(with_mode(&found, "match"), ["Slot/sl1", "Slot/sl3"]);
        // Nothing is both free and busy.
        let none = crate::join_repeated("status", "free", "busy");
        assert_eq!(total(&harness, "Slot", json!({"status": none})), 0);
        // The links of a page give a repeated parameter back as it was sent, and follow.
        let first = search(&harness, "Slot", json!({"start": range, "_count": "1"}));
        let next = first["link"]
            .as_array()
            .unwrap()
            .iter()
            .find(|link| link["relation"] == "next")
            .map(|link| link["url"].as_str().unwrap().to_string())
            .expect("a next page");
        assert!(!next.contains('\u{1f}'), "{next}");
        assert!(
            next.contains("start=ge2026-11-01T10:00:00Z&start=lt2026-11-01T12:00:00Z"),
            "{next}"
        );
        let own = first["link"][0]["url"].as_str().unwrap();
        assert!(
            !own.contains('\u{1f}') && own.contains("&start=lt"),
            "{own}"
        );
        let cursor = next.split("_cursor=").nth(1).unwrap();
        let second = search(
            &harness,
            "Slot",
            json!({"start": range, "_count": "1", "_cursor": cursor}),
        );
        let mut paged = with_mode(&first, "match");
        paged.extend(with_mode(&second, "match"));
        paged.sort();
        assert_eq!(paged, ["Slot/sl2", "Slot/sl3"]);
        // A query string repeats a parameter as a client does.
        let params =
            crate::patch::parse_query("status=free,busy&status=free&_include=a&_include=b");
        assert_eq!(params["status"], "free,busy\u{1f}free");
        assert_eq!(params["_include"], "a,b");
    }

    fn entries(harness: &Harness, type_name: &str, args: Value) -> Vec<(String, String)> {
        search(harness, type_name, args)["entry"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["search"]["mode"].as_str().unwrap().to_string(),
                    format!(
                        "{}/{}",
                        entry["resource"]["resourceType"].as_str().unwrap(),
                        entry["resource"]["id"].as_str().unwrap()
                    ),
                )
            })
            .collect()
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(mode, id)| (mode.to_string(), id.to_string()))
            .collect()
    }

    #[test]
    fn include_iterate_follows_a_link_from_an_included_resource() {
        let harness = booking();
        // One hop: the schedule of the slot.
        assert_eq!(
            entries(
                &harness,
                "Slot",
                json!({"_id": "sl1", "_include": "Slot:schedule"})
            ),
            pairs(&[("match", "Slot/sl1"), ("include", "Schedule/s1")])
        );
        // Plain rules apply to the matches only, so the schedule's actors are not followed.
        assert_eq!(
            entries(
                &harness,
                "Slot",
                json!({"_id": "sl1", "_include": "Slot:schedule,Schedule:actor"})
            ),
            pairs(&[("match", "Slot/sl1"), ("include", "Schedule/s1")])
        );
        // The second hop: from the included schedule to its healthcare service and location.
        let found = search(
            &harness,
            "Slot",
            json!({
                "_id": "sl1",
                "_include": "Slot:schedule",
                "_include:iterate": "Schedule:actor"
            }),
        );
        assert_eq!(with_mode(&found, "match"), ["Slot/sl1"]);
        assert_eq!(
            with_mode(&found, "include"),
            ["HealthcareService/hs1", "Location/l1", "Schedule/s1"]
        );
        assert_eq!(found["total"], 1);
        // A third segment names the type the reference has to point at.
        let found = search(
            &harness,
            "Slot",
            json!({
                "_id": "sl1",
                "_include": "Slot:schedule",
                "_include:iterate": "Schedule:actor:Location"
            }),
        );
        assert_eq!(with_mode(&found, "include"), ["Location/l1", "Schedule/s1"]);
        // An iterate rule needs no plain one: it applies to the matches, and then to what it adds.
        let found = search(
            &harness,
            "Slot",
            json!({"_id": "sl1", "_include:iterate": "Slot:schedule,Schedule:actor"}),
        );
        assert_eq!(
            with_mode(&found, "include"),
            ["HealthcareService/hs1", "Location/l1", "Schedule/s1"]
        );
        // The same, marked on the value.
        let found = search(
            &harness,
            "Slot",
            json!({"_id": "sl1", "_include": "Slot:schedule,Schedule:actor:iterate"}),
        );
        assert_eq!(
            with_mode(&found, "include"),
            ["HealthcareService/hs1", "Location/l1", "Schedule/s1"]
        );
    }

    #[test]
    fn include_iterate_with_a_page_of_slots_and_a_comma_search() {
        let harness = booking();
        // The request that found both gaps: free slots, their schedules, and the actors of those.
        let found = search(
            &harness,
            "Slot",
            json!({
                "status": "free,busy",
                "_count": "2",
                "_include": "Slot:schedule",
                "_include:iterate": "Schedule:actor"
            }),
        );
        assert_eq!(found["total"], 3);
        let matched = with_mode(&found, "match");
        assert_eq!(matched.len(), 2);
        let included = with_mode(&found, "include");
        // Every include is reached from the two matches only, and none is listed twice.
        assert!(
            included.contains(&"Schedule/s1".to_string()),
            "{included:?}"
        );
        let mut unique = included.clone();
        unique.dedup();
        assert_eq!(unique, included);
        assert!(included.iter().all(|id| !matched.contains(id)));
    }

    #[test]
    fn include_iterate_stops_when_references_go_round() {
        let harness = booking();
        // `l3` is part of `l4`, which is part of `l3`.
        assert_eq!(
            entries(
                &harness,
                "Location",
                json!({"_id": "l3", "_include:iterate": "Location:partof"})
            ),
            pairs(&[("match", "Location/l3"), ("include", "Location/l4")])
        );
    }

    #[test]
    fn revinclude_iterate_follows_a_link_to_an_included_resource() {
        let harness = booking();
        // One hop: the schedules that name the healthcare service as an actor.
        let found = search(
            &harness,
            "HealthcareService",
            json!({"_id": "hs1", "_revinclude": "Schedule:actor"}),
        );
        assert_eq!(with_mode(&found, "include"), ["Schedule/s1"]);
        // Two hops: and the slots of those schedules.
        let found = search(
            &harness,
            "HealthcareService",
            json!({
                "_id": "hs1",
                "_revinclude": "Schedule:actor",
                "_revinclude:iterate": "Slot:schedule"
            }),
        );
        assert_eq!(with_mode(&found, "match"), ["HealthcareService/hs1"]);
        assert_eq!(
            with_mode(&found, "include"),
            ["Schedule/s1", "Slot/sl1", "Slot/sl2"]
        );
        // An iterate rule applies to the matches as well.
        let found = search(
            &harness,
            "Schedule",
            json!({"_id": "s1", "_revinclude:iterate": "Slot:schedule"}),
        );
        assert_eq!(with_mode(&found, "include"), ["Slot/sl1", "Slot/sl2"]);
        // A third segment names the type that is pointed at.
        let found = search(
            &harness,
            "Schedule",
            json!({"_id": "s1", "_revinclude": "Slot:schedule:Schedule"}),
        );
        assert_eq!(with_mode(&found, "include"), ["Slot/sl1", "Slot/sl2"]);
        let found = search(
            &harness,
            "Schedule",
            json!({"_id": "s1", "_revinclude": "Slot:schedule:Location"}),
        );
        assert!(with_mode(&found, "include").is_empty());
    }

    #[test]
    fn include_and_revinclude_iterate_work_together() {
        let harness = booking();
        // From a slot to its schedule and the actors, and from the schedule to its other slots.
        let found = search(
            &harness,
            "Slot",
            json!({
                "_id": "sl1",
                "_include": "Slot:schedule",
                "_include:iterate": "Schedule:actor",
                "_revinclude:iterate": "Slot:schedule"
            }),
        );
        assert_eq!(
            with_mode(&found, "include"),
            [
                "HealthcareService/hs1",
                "Location/l1",
                "Schedule/s1",
                "Slot/sl2"
            ]
        );
    }

    #[test]
    fn a_reverse_include_cannot_take_a_wildcard() {
        let harness = booking();
        for args in [
            json!({"_revinclude": "*"}),
            json!({"_revinclude": "Slot:*"}),
            json!({"_revinclude:iterate": "*"}),
        ] {
            let refused = run_op(&harness, "Schedule/search", &args).unwrap_err();
            assert!(
                matches!(&refused, RunError::Fhir(FhirError::Invalid(message))
                    if message.contains("not supported")),
                "{args}: {refused:?}"
            );
        }
        // A forward include still takes one.
        let found = search(&harness, "Slot", json!({"_id": "sl1", "_include": "*"}));
        assert_eq!(with_mode(&found, "include"), ["Schedule/s1"]);
    }

    #[test]
    fn iterating_rules_are_capped_where_they_read() {
        let harness = booking();
        // `s2` has two slots. One more than the cap in a single range is refused when the range
        // is read, and a cap of two lets it through.
        let reverse = json!({"_id": "s2", "_revinclude:iterate": "Slot:schedule"});
        {
            let _cap = crate::search::LoweredCap::to(1);
            let refused = run_op(&harness, "Schedule/search", &reverse).unwrap_err();
            assert!(
                matches!(&refused, RunError::Fhir(FhirError::Invalid(message))
                    if message.contains("more than 1 resources")),
                "{refused:?}"
            );
            // A plain rule has the page to bound it, not this cap.
            let found = search(
                &harness,
                "Schedule",
                json!({"_id": "s2", "_revinclude": "Slot:schedule"}),
            );
            assert_eq!(with_mode(&found, "include").len(), 2);
        }
        {
            let _cap = crate::search::LoweredCap::to(2);
            let found = search(&harness, "Schedule", reverse);
            assert_eq!(with_mode(&found, "include").len(), 2);
        }
        // The cap is for everything the rules add over all the rounds: the schedule is the
        // first, its healthcare service the second.
        let forward = json!({"_id": "sl3", "_include:iterate": "Slot:schedule,Schedule:actor"});
        {
            let _cap = crate::search::LoweredCap::to(1);
            assert!(run_op(&harness, "Slot/search", &forward).is_err());
        }
        {
            let _cap = crate::search::LoweredCap::to(2);
            let found = search(&harness, "Slot", forward);
            assert_eq!(
                with_mode(&found, "include"),
                ["HealthcareService/hs2", "Schedule/s2"]
            );
        }
    }

    #[test]
    fn a_count_follows_no_include() {
        let harness = booking();
        // Nothing is shown, so nothing is followed, and no cap is reached.
        let _cap = crate::search::LoweredCap::to(0);
        let found = search(
            &harness,
            "Slot",
            json!({
                "_summary": "count",
                "_include": "Slot:schedule",
                "_include:iterate": "Schedule:actor",
                "_revinclude:iterate": "Slot:schedule"
            }),
        );
        assert_eq!(found["total"], 4);
        assert!(found["entry"].as_array().unwrap().is_empty());
    }

    #[test]
    fn comma_and_repeated_values_reach_chains_and_reverse_chains() {
        let harness = booking();
        let actor_name = "actor:HealthcareService.name";
        // A chain: a comma is OR for the parameter of the referenced resource.
        assert_eq!(
            total(&harness, "Schedule", json!({actor_name: "Cardio"})),
            1
        );
        assert_eq!(
            total(&harness, "Schedule", json!({actor_name: "Cardio,Derm"})),
            2
        );
        assert_eq!(
            total(&harness, "Schedule", json!({actor_name: "Zzz,Derm"})),
            1
        );
        // A repeated chained parameter is AND: no schedule has both services as actors.
        let both = crate::join_repeated(actor_name, "Cardio", "Derm");
        assert_eq!(total(&harness, "Schedule", json!({actor_name: both})), 0);
        let either_and_one = crate::join_repeated(actor_name, "Cardio,Derm", "Derm");
        assert_eq!(
            total(&harness, "Schedule", json!({actor_name: either_and_one})),
            1
        );
        // A reverse chain.
        let has = "_has:Slot:schedule:status";
        assert_eq!(total(&harness, "Schedule", json!({has: "free,busy"})), 2);
        assert_eq!(
            total(&harness, "Schedule", json!({has: "busy-unavailable"})),
            1
        );
        assert_eq!(
            total(&harness, "Schedule", json!({has: "busy,busy-unavailable"})),
            2
        );
        let and = crate::join_repeated(has, "free,busy", "busy-unavailable");
        let found = search(&harness, "Schedule", json!({has: and}));
        assert_eq!(with_mode(&found, "match"), ["Schedule/s2"]);
    }

    #[test]
    fn the_capability_statement_says_what_search_does_not_do() {
        let harness = ready();
        let statement = harness.catalog.read().unwrap().capability_statement();
        let notes = statement["rest"][0]["documentation"].as_str().unwrap();
        for expected in [
            "`status=free,busy`",
            r"`\,` is a literal comma",
            "at most 1000 alternatives",
            "more than once is AND",
            "an array of strings",
            "one value per name",
            "names no `_count` returns one page",
            "`_include:iterate` and `_revinclude:iterate`",
            "more than 1000 resources",
            "`_revinclude=*`",
            r"`\$`",
            r"`\|`",
        ] {
            assert!(notes.contains(expected), "{expected} is not in: {notes}");
        }
    }

    #[test]
    fn the_patient_limit_holds_for_what_iterate_reaches() {
        let harness = r4();
        for id in ["p1", "p2"] {
            create(&harness, json!({"resourceType": "Patient", "id": id}));
        }
        create(
            &harness,
            json!({"resourceType": "Practitioner", "id": "pr1"}),
        );
        // `o1` has the other patient's `o2` as a member.
        for (id, subject, member) in [("o2", "p2", None), ("o1", "p1", Some("o2"))] {
            let mut observation = json!({
                "resourceType": "Observation", "id": id, "status": "final",
                "code": {"coding": [{"system": "http://loinc.org", "code": "8480-6"}]},
                "subject": {"reference": format!("Patient/{subject}")},
                "performer": [{"reference": "Practitioner/pr1"}]
            });
            if let Some(member) = member {
                observation["hasMember"] = json!([{"reference": format!("Observation/{member}")}]);
            }
            create(&harness, observation);
        }
        let query = json!({
            "_id": "o1",
            "_include": "Observation:performer",
            "_revinclude:iterate": "Observation:performer"
        });
        // Without a limit the practitioner leads to the other patient's observation...
        let found = search(&harness, "Observation", query.clone());
        assert_eq!(
            with_mode(&found, "include"),
            ["Observation/o2", "Practitioner/pr1"]
        );
        // ...and with the server's limit it does not (the reverse rule here, the forward one below).
        let mut limited = query.clone();
        limited["_patientCompartment"] = json!("Patient/p1");
        let found = search(&harness, "Observation", limited);
        assert_eq!(with_mode(&found, "match"), ["Observation/o1"]);
        assert_eq!(with_mode(&found, "include"), ["Practitioner/pr1"]);
        // A forward include: the member of `o1` is another patient's observation.
        let member = json!({"_id": "o1", "_include": "Observation:has-member"});
        let found = search(&harness, "Observation", member.clone());
        assert_eq!(with_mode(&found, "include"), ["Observation/o2"]);
        let mut limited = member;
        limited["_patientCompartment"] = json!("Patient/p1");
        let found = search(&harness, "Observation", limited);
        assert!(with_mode(&found, "include").is_empty(), "{found}");
        // A reference that only ends in the patient's id is not the patient: the subject of `o3`
        // is a Group that happens to be called `p1`.
        create(
            &harness,
            json!({"resourceType": "Group", "id": "p1", "type": "person", "actual": true}),
        );
        create(
            &harness,
            json!({
                "resourceType": "Observation", "id": "o3", "status": "final",
                "code": {"coding": [{"system": "http://loinc.org", "code": "8480-6"}]},
                "subject": {"reference": "Group/p1"}
            }),
        );
        let found = search(
            &harness,
            "Observation",
            json!({"_patientCompartment": "Patient/p1"}),
        );
        assert_eq!(with_mode(&found, "match"), ["Observation/o1"]);
        // The patient's own resources are still included.
        let found = search(
            &harness,
            "Observation",
            json!({
                "_id": "o1", "_patientCompartment": "Patient/p1",
                "_include": "Observation:subject", "_revinclude:iterate": "Observation:subject"
            }),
        );
        assert_eq!(with_mode(&found, "include"), ["Patient/p1"]);
    }
}

#[test]
fn an_examples_package_that_is_only_a_dependency_is_not_installed() {
    // SDC lists `hl7.fhir.r4.examples`, which holds examples and a second copy of the core
    // definitions. The registry below does not have it, so installing the app would fail if it
    // were fetched.
    let app = dep_package(
        "example.app",
        "1.0.0",
        &[("hl7.fhir.r4.examples", "4.0.1"), ("example.lib", "1.0.0")],
    );
    let lib = dep_package("example.lib", "1.0.0", &[]);
    let fetcher = registry(&[("example.lib", "1.0.0", &lib)]);
    let db = empty();
    let cache = temp_dir("examples-cache");
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        app.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap();
    assert_eq!(
        installed_packages(&db),
        ["example.app@1.0.0", "example.lib@1.0.0"]
    );

    // Asked for by name, it is installed like any other.
    let examples = dep_package("hl7.fhir.r4.examples", "4.0.1", &[]);
    let fetcher = registry(&[("hl7.fhir.r4.examples", "4.0.1", &examples)]);
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        "hl7.fhir.r4.examples@4.0.1",
        &fetcher,
        &cache,
    )
    .unwrap();
    assert!(installed_packages(&db).contains(&"hl7.fhir.r4.examples@4.0.1".to_string()));
    for dir in [app, lib, examples, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// A prepared file of the two fixture packages, and the `name@version` of each.
fn prepared_fixtures(scratch: &Path) -> (PathBuf, Vec<String>) {
    let file = scratch.join("packages.bundle");
    let specs = [minir4_dir(), extra_dir()].map(|dir| dir.to_str().unwrap().to_string());
    let names = crate::write_prepared(&file, &specs, &NoNetwork, &scratch.join("cache")).unwrap();
    (file, names)
}

fn as_strs(names: &[String]) -> Vec<&str> {
    names.iter().map(String::as_str).collect()
}

/// Installing the packages in `file` stores every table as installing them from their directories
/// does, and publishes the catalog.
fn assert_a_prepared_file_installs_as_its_directories_do(file: &Path, names: &[String]) {
    let from_directories = empty();
    install_package_dir(
        &from_directories.store,
        &from_directories.db,
        &from_directories.catalog,
        minir4_dir(),
    )
    .unwrap();
    install_package_dir(
        &from_directories.store,
        &from_directories.db,
        &from_directories.catalog,
        extra_dir(),
    )
    .unwrap();

    let from_file = empty();
    let installed = crate::install_prepared(
        &from_file.store,
        &from_file.db,
        &from_file.catalog,
        file,
        &as_strs(names),
    )
    .unwrap();
    assert_eq!(installed, Some(2));

    // Every table holds what it holds when the packages are read from their directories.
    let (a, b) = (&from_directories.store, &from_file.store);
    assert_eq!(a.load_packages().unwrap(), b.load_packages().unwrap());
    assert_eq!(
        a.load_structure_definitions().unwrap(),
        b.load_structure_definitions().unwrap()
    );
    assert_eq!(
        a.load_search_parameters().unwrap(),
        b.load_search_parameters().unwrap()
    );
    assert_eq!(a.load_terminology().unwrap(), b.load_terminology().unwrap());
    assert_eq!(a.load_expansions().unwrap(), b.load_expansions().unwrap());
    assert!(!b.load_search_parameters().unwrap().is_empty());
    // And the catalog is published: the types and search parameters are there to use.
    let catalog = from_file.catalog.read().expect("catalog");
    assert!(catalog.contains("Patient"));
    assert!(catalog.contains("Encounter"));
}

#[test]
fn a_package_installed_from_a_prepared_file_is_stored_as_one_installed_from_its_directory() {
    let scratch = temp_dir("prepared");
    let (file, names) = prepared_fixtures(&scratch);
    assert_eq!(names.len(), 2);
    assert_a_prepared_file_installs_as_its_directories_do(&file, &names);
    let _ = std::fs::remove_dir_all(scratch);
}

#[test]
fn a_package_cut_into_many_values_is_stored_as_one_installed_from_its_directory() {
    let scratch = temp_dir("prepared-pieces");
    let file = scratch.join("packages.bundle");
    let specs = [minir4_dir(), extra_dir()].map(|dir| dir.to_str().unwrap().to_string());
    // Values of a few hundred bytes: each kind of row is cut into several, and a row is larger
    // than a value.
    let names = crate::prepared::write_prepared_in_pieces(
        &file,
        &specs,
        &NoNetwork,
        &scratch.join("cache"),
        700,
    )
    .unwrap();
    assert_a_prepared_file_installs_as_its_directories_do(&file, &names);
    // Read whole, each package is what its directory makes.
    let mut bundle = crate::PreparedBundle::open(&file).unwrap();
    let first = bundle.next().unwrap().unwrap();
    let from_directory = crate::package::read_package(minir4_dir()).unwrap();
    assert_eq!(first.name, from_directory.name);
    assert_eq!(
        first.structure_definitions.len(),
        from_directory.structure_definitions.len()
    );
    assert_eq!(first.terminology.len(), from_directory.terminology.len());
    assert_eq!(first.expansions.len(), from_directory.expansions.len());
    let _ = std::fs::remove_dir_all(scratch);
}

#[test]
fn a_prepared_file_leaves_alone_what_is_already_installed() {
    let scratch = temp_dir("prepared-installed");
    let (file, names) = prepared_fixtures(&scratch);
    let db = empty();
    install_package_dir(&db.store, &db.db, &db.catalog, minir4_dir()).unwrap();
    let before = db.store.load_structure_definitions().unwrap();

    let installed =
        crate::install_prepared(&db.store, &db.db, &db.catalog, &file, &as_strs(&names)).unwrap();
    assert_eq!(installed, Some(1), "only the one that was missing");
    assert_eq!(installed_packages(&db).len(), 2);
    assert!(db
        .store
        .load_structure_definitions()
        .unwrap()
        .starts_with(&before));

    // With every package there, nothing is done.
    let again =
        crate::install_prepared(&db.store, &db.db, &db.catalog, &file, &as_strs(&names)).unwrap();
    assert_eq!(again, Some(0));
    let _ = std::fs::remove_dir_all(scratch);
}

#[test]
fn a_prepared_file_that_lacks_a_package_asked_for_is_not_used() {
    let scratch = temp_dir("prepared-lacks");
    let (file, names) = prepared_fixtures(&scratch);
    let db = empty();
    let mut asked = as_strs(&names);
    asked.push("example.missing@1.0.0");
    let installed = crate::install_prepared(&db.store, &db.db, &db.catalog, &file, &asked).unwrap();
    assert_eq!(installed, None);
    assert!(installed_packages(&db).is_empty());
    let _ = std::fs::remove_dir_all(scratch);
}

#[test]
fn a_damaged_prepared_file_installs_what_came_before_the_damage() {
    let scratch = temp_dir("prepared-damaged");
    let (file, names) = prepared_fixtures(&scratch);
    let bytes = std::fs::read(&file).unwrap();
    std::fs::write(&file, &bytes[..bytes.len() - 40]).unwrap();

    let db = empty();
    let err = crate::install_prepared(&db.store, &db.db, &db.catalog, &file, &as_strs(&names))
        .unwrap_err();
    assert!(err.to_string().contains("prepared packages"), "{err}");
    // The first package was whole: it is installed and published.
    assert_eq!(installed_packages(&db), [names[0].clone()]);
    assert!(db.catalog.read().expect("catalog").contains("Patient"));
    let _ = std::fs::remove_dir_all(scratch);
}

#[test]
fn a_prepared_file_in_a_format_this_server_cannot_read_is_refused() {
    use std::io::Write;
    let scratch = temp_dir("prepared-format");
    let file = scratch.join("packages.bundle");
    let mut encoder = flate2::write::GzEncoder::new(
        std::fs::File::create(&file).unwrap(),
        flate2::Compression::default(),
    );
    encoder
        .write_all(br#"{"format":99,"refhir":"9.9.9","packages":[]}"#)
        .unwrap();
    encoder.finish().unwrap();
    let err = crate::PreparedBundle::open(&file).err().expect("refused");
    assert!(err.to_string().contains("format 99"), "{err}");

    // A file that is not a prepared file at all is refused as well, not mistaken for one.
    std::fs::write(&file, b"not a bundle").unwrap();
    assert!(crate::PreparedBundle::open(&file).is_err());
    let _ = std::fs::remove_dir_all(scratch);
}

#[test]
fn a_registry_that_may_not_download_downloads_nothing() {
    let cache = temp_dir("no-download-cache");
    let registry = crate::NpmRegistry::new("http://127.0.0.1:1", cache.clone())
        .unwrap()
        .allowing_download(false);
    let dest = cache.join("example.lib").join("1.0.0");
    std::fs::create_dir_all(&dest).unwrap();
    let err = registry.fetch("example.lib", "1.0.0", &dest).unwrap_err();
    assert!(matches!(err, FhirError::Forbidden(_)), "{err:?}");
    assert!(err.to_string().contains("package_download = true"), "{err}");
    assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 0);

    // Installing by name through it is refused and installs nothing.
    let db = empty();
    let err = install_with(
        &db.store,
        &db.db,
        &db.catalog,
        "example.lib@1.0.0",
        &registry,
        &cache,
    )
    .unwrap_err();
    assert!(matches!(err, FhirError::Forbidden(_)), "{err:?}");
    assert!(installed_packages(&db).is_empty());

    // A package already in the cache needs no download, so it installs.
    let lib = dep_package("example.lib", "1.0.0", &[]);
    let _ = std::fs::remove_dir_all(&dest);
    copy_tree(&lib, &dest).unwrap();
    std::fs::write(dest.join(".complete"), "ok").unwrap();
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        "example.lib@1.0.0",
        &registry,
        &cache,
    )
    .unwrap();
    assert_eq!(installed_packages(&db), ["example.lib@1.0.0"]);
    for dir in [lib, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn a_catalog_rebuilt_while_starting_does_not_keep_the_old_one_alive() {
    let harness = ready();
    let watched = WatchedStore::new(harness.store.clone());
    assert!(harness.catalog.read().expect("catalog").contains("Patient"));

    // Serving, which is the default: the old catalog stays until the new one is built, so a
    // rebuild that fails leaves requests with the catalog they had.
    watched.fail_load_terminology.store(1, Ordering::SeqCst);
    assert!(publish(&watched, &harness.catalog).is_err());
    assert!(harness.catalog.read().expect("catalog").contains("Patient"));

    // Starting: nothing is served from it, so the old one is let go first, to build the new one
    // in its place instead of beside it. A rebuild that fails leaves none. The setting belongs to
    // this catalog alone, so no other catalog in the process is affected by it.
    harness
        .catalog
        .write()
        .expect("catalog")
        .set_rebuild_in_place(true);
    watched.fail_load_terminology.store(1, Ordering::SeqCst);
    let failed = publish(&watched, &harness.catalog);
    assert!(failed.is_err());
    assert!(
        !harness.catalog.read().expect("catalog").contains("Patient"),
        "the old catalog was kept alive under the rebuild"
    );

    // And one that works gives the same catalog either way, and keeps the setting.
    harness
        .catalog
        .write()
        .expect("catalog")
        .set_rebuild_in_place(true);
    publish(&watched, &harness.catalog).unwrap();
    let catalog = harness.catalog.read().expect("catalog");
    assert!(catalog.contains("Patient"));
    assert!(catalog.rebuild_in_place());
}

#[test]
fn a_prepared_file_that_holds_fewer_packages_than_its_header_lists_is_refused() {
    use std::io::Write;
    let scratch = temp_dir("prepared-short");
    let file = scratch.join("packages.bundle");
    // A package with no rows is one value that names it.
    let package = |name: &str| {
        format!(r#"{{"Package":{{"name":"{name}","version":"1.0.0","fhir_version":"4.0.1"}}}}"#)
    };
    // The header promises two packages, and the stream ends cleanly after one: nothing says the
    // second is missing except the count.
    let mut encoder = flate2::write::GzEncoder::new(
        std::fs::File::create(&file).unwrap(),
        flate2::Compression::default(),
    );
    writeln!(
        encoder,
        r#"{{"format":2,"refhir":"0.0.1","packages":["example.a@1.0.0","example.b@1.0.0"]}}"#
    )
    .unwrap();
    writeln!(encoder, "{}", package("example.a")).unwrap();
    encoder.finish().unwrap();

    let db = empty();
    let err = crate::install_prepared(
        &db.store,
        &db.db,
        &db.catalog,
        &file,
        &["example.a@1.0.0", "example.b@1.0.0"],
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("lists 2 packages and it holds 1"),
        "{err}"
    );
    // What was there was whole: it is installed.
    assert_eq!(installed_packages(&db), ["example.a@1.0.0"]);
    let _ = std::fs::remove_dir_all(scratch);
}

#[test]
fn an_implementation_guides_own_examples_package_is_installed_as_a_dependency() {
    // Only the examples packages of the FHIR releases are left out; a guide's own may hold
    // profiles it needs.
    let guide = dep_package(
        "example.guide",
        "1.0.0",
        &[("hl7.fhir.us.example.examples", "1.0.0")],
    );
    let examples = dep_package("hl7.fhir.us.example.examples", "1.0.0", &[]);
    let fetcher = registry(&[("hl7.fhir.us.example.examples", "1.0.0", &examples)]);
    let db = empty();
    let cache = temp_dir("guide-examples-cache");
    install_with(
        &db.store,
        &db.db,
        &db.catalog,
        guide.to_str().unwrap(),
        &fetcher,
        &cache,
    )
    .unwrap();
    assert_eq!(
        installed_packages(&db),
        ["example.guide@1.0.0", "hl7.fhir.us.example.examples@1.0.0"]
    );
    for dir in [guide, examples, cache] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn a_prepared_file_that_could_not_be_written_leaves_nothing_behind() {
    let scratch = temp_dir("prepared-failed");
    let broken = scratch.join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(
        broken.join("package.json"),
        r#"{"name":"example.broken","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    std::fs::write(broken.join("StructureDefinition-bad.json"), "{ not json").unwrap();
    let out = scratch.join("out").join("packages.bundle");
    let err = crate::write_prepared(
        &out,
        &[broken.to_str().unwrap().to_string()],
        &NoNetwork,
        &scratch.join("cache"),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("StructureDefinition-bad.json"),
        "{err}"
    );
    assert!(!out.exists());
    let left: Vec<_> = std::fs::read_dir(scratch.join("out"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
    let _ = std::fs::remove_dir_all(scratch);
}
/// A page of sorted matches among thousands of Slots, the shape of a booking agent's question:
/// `Slot?status=free&start=ge{now}&_sort=start&_count=3`.
///
/// This used to cost about a millisecond for every match, because each comparison of the sort
/// worked out both sort keys again from the FHIRPath expression, and every match was kept until the
/// sort was done. The counters say what a search did, and do not depend on the machine, so they, and
/// not a clock, are what the tests hold it to; the benchmark prints the times.
mod sorted_pages {
    use super::*;
    use crate::search::{search_counted, search_reference, Handling, Work};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    const SCHEDULES: usize = 5;

    struct Slot {
        id: String,
        schedule: String,
        status: &'static str,
        start: String,
    }
    const FROM: &str = "2030-02-10T00:00:00Z";

    struct Seeded {
        harness: Harness,
        /// What the Slots are, for the answer the search is held to.
        slots: Vec<Slot>,
    }

    /// `count` Slots on five schedules. Every fifth is busy, and the starts repeat, so several
    /// Slots tie on the sort key and the id has to decide.
    fn seed(count: usize) -> Seeded {
        let harness = r4();
        for n in 0..SCHEDULES {
            let create = |resource: Value| {
                let path = format!("{}/create", resource["resourceType"].as_str().unwrap());
                apply(&harness, &path, json!({"resource": resource}));
            };
            create(json!({"resourceType": "HealthcareService", "id": format!("h{n}")}));
            create(json!({"resourceType": "Location", "id": format!("l{n}")}));
            create(
                json!({"resourceType": "Schedule", "id": format!("s{n}"), "actor": [
                {"reference": format!("HealthcareService/h{n}")},
                {"reference": format!("Location/l{}", n % 2)}]}),
            );
        }
        let mut slots = Vec::new();
        for n in 0..count {
            let day = 1 + (n * 7) % 28;
            let hour = (n * 3) % 24;
            let status = if n % 5 == 4 { "busy" } else { "free" };
            let start = format!("2030-02-{day:02}T{hour:02}:00:00Z");
            let id = format!("sl{n:05}");
            apply(
                &harness,
                "Slot/create",
                json!({"resource": {
                    "resourceType": "Slot", "id": id,
                    "schedule": {"reference": format!("Schedule/s{}", n % SCHEDULES)},
                    "status": status, "start": start,
                    "end": format!("2030-02-{day:02}T{hour:02}:30:00Z")
                }}),
            );
            slots.push(Slot {
                id,
                schedule: format!("Schedule/s{}", n % SCHEDULES),
                status,
                start,
            });
        }
        Seeded { harness, slots }
    }

    /// Four thousand Slots, seeded once for the module: 3,200 of them free, and 1,600 of those
    /// from `FROM` on.
    fn seeded() -> &'static Seeded {
        static SEEDED: OnceLock<Seeded> = OnceLock::new();
        SEEDED.get_or_init(|| seed(4000))
    }

    fn params(query: &[(&str, &str)]) -> BTreeMap<String, String> {
        query
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    fn search(harness: &Harness, query: &[(&str, &str)]) -> (Value, Work, Duration) {
        let catalog = harness.catalog.read().unwrap();
        let model = catalog.get("Slot").unwrap();
        let params = params(query);
        let mut tx = harness.db.begin();
        let started = Instant::now();
        let (bundle, work) =
            search_counted(&mut tx, &catalog, model, &params, Handling::Strict).unwrap();
        (bundle, work, started.elapsed())
    }

    /// The bundle of the search as it was before pages were built lazily: every match kept and
    /// sorted by comparing sort keys worked out afresh each time. Slow, so only a few searches ask.
    fn reference(harness: &Harness, query: &[(&str, &str)]) -> Value {
        let catalog = harness.catalog.read().unwrap();
        let model = catalog.get("Slot").unwrap();
        let mut tx = harness.db.begin();
        search_reference(&mut tx, &catalog, model, &params(query)).unwrap()
    }

    fn ids(bundle: &Value) -> Vec<String> {
        bundle["entry"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string())
            .collect()
    }

    /// The free Slots that start at `since` or later, ordered as `order` says, ties going to the
    /// lower id.
    fn free_from(
        seeded: &Seeded,
        since: &str,
        order: impl Fn(&Slot, &Slot) -> std::cmp::Ordering,
    ) -> Vec<String> {
        let mut found: Vec<&Slot> = seeded
            .slots
            .iter()
            .filter(|slot| slot.status == "free" && slot.start.as_str() >= since)
            .collect();
        found.sort_by(|left, right| order(left, right).then(left.id.cmp(&right.id)));
        found.into_iter().map(|slot| slot.id.clone()).collect()
    }

    fn by_start(left: &Slot, right: &Slot) -> std::cmp::Ordering {
        left.start.cmp(&right.start)
    }

    #[test]
    fn a_sorted_page_among_thousands_of_matches_is_the_right_page() {
        let seeded = seeded();
        let ascending = free_from(seeded, FROM, by_start);
        let descending = free_from(seeded, FROM, |left, right| by_start(right, left));
        assert!(ascending.len() > 1000, "{}", ascending.len());
        for (sort, want) in [("start", &ascending), ("-start", &descending)] {
            for (count, offset) in [(3usize, 0usize), (10, 7), (50, 1500)] {
                let (count_text, offset_text) = (count.to_string(), offset.to_string());
                let (bundle, _, _) = search(
                    &seeded.harness,
                    &[
                        ("status", "free"),
                        ("start", &format!("ge{FROM}")),
                        ("_sort", sort),
                        ("_count", &count_text),
                        ("_offset", &offset_text),
                    ],
                );
                let end = (offset + count).min(want.len());
                assert_eq!(
                    ids(&bundle),
                    want[offset..end],
                    "{sort} {count} from {offset}"
                );
                assert_eq!(bundle["total"], want.len(), "{sort}");
            }
        }
        // Past the last match there is nothing, and the total is still the number of matches.
        let (bundle, _, _) = search(
            &seeded.harness,
            &[
                ("status", "free"),
                ("start", &format!("ge{FROM}")),
                ("_sort", "start"),
                ("_count", "5"),
                ("_offset", "100000"),
            ],
        );
        assert!(ids(&bundle).is_empty());
        assert_eq!(bundle["total"], ascending.len());
        // Two sort terms, the second ordering what the first leaves tied.
        let want = free_from(seeded, "", |left, right| {
            left.schedule
                .to_lowercase()
                .cmp(&right.schedule.to_lowercase())
                .then(by_start(right, left))
        });
        let (bundle, _, _) = search(
            &seeded.harness,
            &[
                ("status", "free"),
                ("_sort", "schedule,-start"),
                ("_count", "25"),
                ("_offset", "10"),
            ],
        );
        assert_eq!(ids(&bundle), want[10..35]);
        // The bundle is what sorting every match gives, which is what this search used to do.
        for sort in ["start", "-start"] {
            let query = [
                ("status", "free"),
                ("start", &*format!("ge{FROM}")),
                ("_sort", sort),
                ("_count", "3"),
                ("_offset", "5"),
            ];
            assert_eq!(
                search(&seeded.harness, &query).0,
                reference(&seeded.harness, &query)
            );
        }
    }

    #[test]
    fn a_sorted_page_works_out_each_sort_key_once_and_holds_only_what_it_may_need() {
        let seeded = seeded();
        let matches = free_from(seeded, FROM, by_start).len();
        let wanted = |count: usize, offset: usize| count + offset;
        for (count, offset) in [(3usize, 0usize), (3, 40), (100, 0)] {
            let (count_text, offset_text) = (count.to_string(), offset.to_string());
            let (_, work, _) = search(
                &seeded.harness,
                &[
                    ("status", "free"),
                    ("start", &format!("ge{FROM}")),
                    ("_sort", "start"),
                    ("_count", &count_text),
                    ("_offset", &offset_text),
                ],
            );
            // Every match is seen, once: the filter is checked on each row of the range, and the
            // sort key, which used to be worked out for every comparison, once for each match.
            assert_eq!(work.rows, 3200);
            assert_eq!(work.checked, 3200);
            assert_eq!(
                work.keyed, matches,
                "{count} from {offset}: one key for each match"
            );
            // And only a page and what sorts before it is held: never more than twice that, or a
            // few dozen, however many matches there are.
            let held = (2 * wanted(count, offset)).max(64);
            assert!(
                work.retained <= held,
                "{count} from {offset}: held {} of {matches} matches, at most {held} expected",
                work.retained
            );
            assert!(work.retained < matches / 4);
        }
        // Without `_sort` there is nothing to rank.
        let (_, work, _) = search(
            &seeded.harness,
            &[
                ("status", "free"),
                ("start", &format!("ge{FROM}")),
                ("_count", "3"),
            ],
        );
        assert_eq!((work.keyed, work.retained), (0, 0));
        // A count of 0 keeps nothing and still counts.
        let (bundle, work, _) = search(
            &seeded.harness,
            &[
                ("status", "free"),
                ("start", &format!("ge{FROM}")),
                ("_sort", "start"),
                ("_count", "0"),
            ],
        );
        assert_eq!(bundle["total"], matches);
        assert_eq!((work.keyed, work.retained), (0, 0));
    }

    /// The best of a few runs, so a pause of the machine does not decide the answer.
    fn fastest(harness: &Harness, query: &[(&str, &str)]) -> Duration {
        (0..3)
            .map(|_| search(harness, query).2)
            .min()
            .expect("a run")
    }

    /// Times for the search a booking agent sends, by the number of matches. Run it with
    /// `cargo test --release -p refhir-fhir sorted_pages::benchmark -- --ignored --nocapture`.
    #[test]
    #[ignore = "a benchmark: prints times, asserts nothing"]
    fn benchmark() {
        for count in [1900usize, 7600] {
            let seeded = seed(count);
            eprintln!("--- {count} Slots, {} free", count - count / 5);
            let from = format!("ge{FROM}");
            let queries: [(&str, Vec<(&str, &str)>); 6] = [
                (
                    "status=free&_count=2",
                    vec![("status", "free"), ("_count", "2")],
                ),
                (
                    "status=free&start=ge&_count=3",
                    vec![("status", "free"), ("start", &from), ("_count", "3")],
                ),
                (
                    "status=free&start=ge&_sort=start&_count=3",
                    vec![
                        ("status", "free"),
                        ("start", &from),
                        ("_sort", "start"),
                        ("_count", "3"),
                    ],
                ),
                (
                    "status=free&_sort=start&_count=3",
                    vec![("status", "free"), ("_sort", "start"), ("_count", "3")],
                ),
                (
                    "status=free&schedule.actor=h1&_sort=start&_count=3",
                    vec![
                        ("status", "free"),
                        ("schedule.actor", "HealthcareService/h1"),
                        ("_sort", "start"),
                        ("_count", "3"),
                    ],
                ),
                ("status=free", vec![("status", "free")]),
            ];
            for (label, query) in queries {
                let (_, work, _) = search(&seeded.harness, &query);
                eprintln!(
                    "{label:<55} {:>9.2?}  {work:?}",
                    fastest(&seeded.harness, &query)
                );
            }
        }
    }
}

/// Every issue of an outcome that makes a resource invalid.
fn errors_of(outcome: &Value) -> Vec<&Value> {
    outcome["issue"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|issue| matches!(issue["severity"].as_str(), Some("error" | "fatal")))
        .collect()
}

fn xhtml(inner: &str) -> String {
    format!("<div xmlns=\"http://www.w3.org/1999/xhtml\">{inner}</div>")
}

#[test]
fn a_narrative_is_checked_by_html_checks_not_refused_for_it() {
    let harness = shared_r4();
    let with = |div: String| {
        json!({"resourceType": "Patient", "active": true,
            "text": {"status": "generated", "div": div}})
    };
    let valid = apply(
        harness,
        "Patient/$validate",
        json!({"resource": with(xhtml("<p>Ann <b>Smith</b></p>"))}),
    );
    assert!(errors_of(&valid).is_empty(), "{valid}");

    // The rules say what a narrative may hold: no script, and some text.
    for (label, div) in [
        ("script", xhtml("<script>alert(1)</script>text")),
        ("an event handler", xhtml("<p onclick=\"go()\">text</p>")),
        ("no text", xhtml("<p></p>")),
        ("no namespace", "<div>text</div>".to_string()),
    ] {
        let outcome = apply(harness, "Patient/$validate", json!({"resource": with(div)}));
        let keys: Vec<_> = errors_of(&outcome)
            .iter()
            .map(|issue| {
                issue["diagnostics"]
                    .as_str()
                    .unwrap()
                    .split(':')
                    .next()
                    .unwrap()
            })
            .collect();
        assert!(
            keys.contains(&"txt-1") || keys.contains(&"txt-2"),
            "{label}: {outcome}"
        );
    }
}

#[test]
fn a_resource_in_a_bundle_entry_is_validated_as_the_resource_it_is() {
    let harness = shared_r4();
    let bundle = |entry: Value| {
        json!({"resourceType": "Bundle", "type": "collection",
            "entry": [{"fullUrl": "urn:uuid:1", "resource": entry}]})
    };
    let valid = apply(
        harness,
        "Bundle/$validate",
        json!({"resource": bundle(json!({"resourceType": "Patient", "active": true}))}),
    );
    assert!(errors_of(&valid).is_empty(), "{valid}");

    // What is wrong with the entry is said at the entry, once.
    let invalid = apply(
        harness,
        "Bundle/$validate",
        json!({"resource": bundle(json!({"resourceType": "Patient", "active": "yes", "shoeSize": 9}))}),
    );
    let at: Vec<_> = errors_of(&invalid)
        .iter()
        .map(|issue| issue["expression"][0].as_str().unwrap().to_string())
        .collect();
    assert!(
        at.contains(&"Bundle.entry[0].resource.shoeSize".to_string()),
        "{at:?}"
    );
    assert!(
        at.contains(&"Bundle.entry[0].resource.active".to_string()),
        "{at:?}"
    );
    let mut unique = at.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), at.len(), "a finding is said once: {at:?}");

    // An entry with no resourceType is not a resource at all.
    let nameless = apply(
        harness,
        "Bundle/$validate",
        json!({"resource": bundle(json!({"active": true}))}),
    );
    assert!(
        errors_of(&nameless)
            .iter()
            .any(|issue| issue["diagnostics"] == "resourceType is required"),
        "{nameless}"
    );
}

#[test]
fn a_complex_extension_on_a_primitive_is_checked_by_the_values_inside_it() {
    let harness = shared_r4();
    let translated = |parts: Value| {
        json!({
            "resourceType": "Observation", "status": "final",
            "code": {"coding": [{"system": "http://loinc.org", "code": "8480-6", "display": "Systolic",
                "_display": {"extension": [{
                    "url": "http://hl7.org/fhir/StructureDefinition/translation",
                    "extension": parts
                }]}}]}
        })
    };
    // Each part holds its value under a key named for the type (`valueCode`), not under `value[x]`.
    let valid = apply(
        harness,
        "Observation/$validate",
        json!({"resource": translated(json!([
            {"url": "lang", "valueCode": "nl"},
            {"url": "content", "valueString": "Systolisch"}
        ]))}),
    );
    assert!(errors_of(&valid).is_empty(), "{valid}");
    // A part that has none is still refused.
    let missing = apply(
        harness,
        "Observation/$validate",
        json!({"resource": translated(json!([
            {"url": "lang"},
            {"url": "content", "valueString": "Systolisch"}
        ]))}),
    );
    assert!(!errors_of(&missing).is_empty(), "{missing}");
}

/// The IPS package unpacked into a scratch directory, and the folder it unpacked to.
fn unpacked_ips() -> (PathBuf, PathBuf) {
    let scratch = temp_dir("ips");
    let archive = std::fs::File::open(ips_package()).unwrap();
    tar::Archive::new(flate2::read::GzDecoder::new(archive))
        .unpack(&scratch)
        .unwrap();
    let package = scratch.join("package");
    (scratch, package)
}

const IPS_BUNDLE_PROFILE: &str = "http://hl7.org/fhir/uv/ips/StructureDefinition/Bundle-uv-ips";

#[test]
fn hl7s_own_example_summaries_are_valid_against_the_ips_profiles() {
    let harness = summary_ready();
    let (scratch, package) = unpacked_ips();
    let examples = [
        "Bundle-bundle-minimal",
        "Bundle-bundle-ips-all-sections",
        "Bundle-IPS-examples-Bundle-01",
        "Bundle-bundle-no-info-required-sections",
        "Bundle-IPS-examples-Bundle-with-immunization",
    ];
    for name in examples {
        let text =
            std::fs::read_to_string(package.join("example").join(format!("{name}.json"))).unwrap();
        let bundle: Value = serde_json::from_str(&text).unwrap();
        let outcome = apply(
            harness,
            "Bundle/$validate",
            json!({"resource": bundle, "profile": IPS_BUNDLE_PROFILE}),
        );
        assert!(errors_of(&outcome).is_empty(), "{name}: {outcome}");
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn an_ips_summary_that_breaks_the_profile_is_refused() {
    let harness = summary_ready();
    let (scratch, package) = unpacked_ips();
    let text = std::fs::read_to_string(package.join("example/Bundle-bundle-minimal.json")).unwrap();
    let mut bundle: Value = serde_json::from_str(&text).unwrap();
    // The Composition leads a document. Without it the Bundle is not a summary.
    bundle["entry"].as_array_mut().unwrap().remove(0);
    let outcome = apply(
        harness,
        "Bundle/$validate",
        json!({"resource": bundle, "profile": IPS_BUNDLE_PROFILE}),
    );
    assert!(!errors_of(&outcome).is_empty(), "{outcome}");
    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn the_same_finding_in_each_item_of_a_list_is_reported_for_each_item() {
    // The validator says a finding once when it reaches it twice, and an item of a list is not the
    // same place as the one before it.
    let harness = summary_ready();
    let outcome = apply(
        harness,
        "Patient/$validate",
        json!({"resource": {
            "resourceType": "Patient",
            "telecom": [
                {"system": "bogus", "value": "1"},
                {"system": "bogus", "value": "1"},
                {"system": "bogus", "value": "1"}
            ]
        }}),
    );
    let places: Vec<String> = errors_of(&outcome)
        .iter()
        .map(|issue| issue["expression"][0].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        places,
        [
            "Patient.telecom[0].system",
            "Patient.telecom[1].system",
            "Patient.telecom[2].system"
        ],
        "{outcome}"
    );
}

#[test]
fn an_ips_summary_without_a_section_the_profile_requires_is_refused() {
    const COMPOSITION_PROFILE: &str =
        "http://hl7.org/fhir/uv/ips/StructureDefinition/Composition-uv-ips";
    let harness = summary_ready();
    let (scratch, package) = unpacked_ips();
    let text = std::fs::read_to_string(package.join("example/Bundle-bundle-minimal.json")).unwrap();
    let mut bundle: Value = serde_json::from_str(&text).unwrap();
    // The IPS Composition requires its problems section (11450-4), whatever else it has.
    let mut composition = bundle["entry"][0]["resource"].clone();
    let sections = composition["section"].as_array().unwrap().clone();
    assert_eq!(
        sections.len(),
        3,
        "the example has the three required sections"
    );
    let without = |codes: &[&str]| -> Vec<Value> {
        sections
            .iter()
            .filter(|section| {
                let code = section["code"]["coding"][0]["code"].as_str().unwrap_or("");
                !codes.contains(&code)
            })
            .cloned()
            .collect()
    };
    let missing_slice = |outcome: &Value, name: &str| {
        errors_of(outcome).iter().any(|issue| {
            issue["diagnostics"]
                .as_str()
                .is_some_and(|text| text.contains(&format!("slice {name} requires at least 1")))
        })
    };

    // Checked on its own against the profile, a Composition is refused for each required section it
    // lacks, and by name.
    composition["section"] = Value::Array(without(&["11450-4"]));
    let outcome = apply(
        harness,
        "Composition/$validate",
        json!({"resource": composition, "profile": COMPOSITION_PROFILE}),
    );
    assert!(missing_slice(&outcome, "sectionProblems"), "{outcome}");
    assert!(!missing_slice(&outcome, "sectionAllergies"), "{outcome}");
    composition["section"] = Value::Array(without(&["11450-4", "10160-0"]));
    let outcome = apply(
        harness,
        "Composition/$validate",
        json!({"resource": composition, "profile": COMPOSITION_PROFILE}),
    );
    assert!(missing_slice(&outcome, "sectionProblems"), "{outcome}");
    assert!(missing_slice(&outcome, "sectionMedications"), "{outcome}");

    // A slice that was there is not missing: the example's own Composition is whole.
    let whole = apply(
        harness,
        "Composition/$validate",
        json!({"resource": bundle["entry"][0]["resource"], "profile": COMPOSITION_PROFILE}),
    );
    assert!(errors_of(&whole).is_empty(), "{whole}");

    // And in a summary that says it is one, as a Bundle entry.
    composition["section"] = Value::Array(without(&["11450-4"]));
    composition["meta"] = json!({"profile": [COMPOSITION_PROFILE]});
    bundle["entry"][0]["resource"] = composition;
    let outcome = apply(
        harness,
        "Bundle/$validate",
        json!({"resource": bundle, "profile": IPS_BUNDLE_PROFILE}),
    );
    assert!(missing_slice(&outcome, "sectionProblems"), "{outcome}");
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A harness with the whole R4 release and the IPS package, holding a patient with a record, a
/// second patient whose record must stay out of the first's summary, and one with no record. It is
/// built once and shared, so a test that uses it must only read.
fn summary_ready() -> &'static Harness {
    static READY: std::sync::OnceLock<Harness> = std::sync::OnceLock::new();
    READY.get_or_init(build_summary_harness)
}

fn build_summary_harness() -> Harness {
    let harness = r4_ready();
    let (scratch, package) = unpacked_ips();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, &package).unwrap();
    let _ = std::fs::remove_dir_all(&scratch);
    let create = |resource: Value| {
        apply(
            &harness,
            &format!("{}/create", resource["resourceType"].as_str().unwrap()),
            json!({"resource": resource}),
        );
    };
    let patient_ref = |id: &str| json!({"reference": format!("Patient/{id}")});
    let clinical = |system: &str, code: &str| json!({"coding": [{"system": system, "code": code}]});
    create(json!({"resourceType": "Organization", "id": "org1", "name": "General Hospital"}));
    create(json!({"resourceType": "Patient", "id": "p2", "name": [{"family": "Other"}]}));
    create(json!({
        "resourceType": "Patient", "id": "p1",
        "identifier": [{"system": "http://hospital.example", "value": "mrn-p1"}],
        "name": [{"family": "Lovelace", "given": ["Ada"]}],
        "gender": "female", "birthDate": "1815-12-10",
        "managingOrganization": {"reference": "Organization/org1"},
        "link": [{"other": {"reference": "Patient/p2"}, "type": "seealso"}]
    }));
    create(
        json!({"resourceType": "Patient", "id": "p3", "name": [{"family": "Empty"}], "gender": "unknown", "birthDate": "2000-01-01"}),
    );
    create(
        json!({"resourceType": "Medication", "id": "med1", "code": {"text": "Salbutamol 100 micrograms inhaler"}}),
    );
    let condition_clinical = "http://terminology.hl7.org/CodeSystem/condition-clinical";
    create(
        json!({"resourceType": "Condition", "id": "c1", "clinicalStatus": clinical(condition_clinical, "active"),
        "code": {"text": "Asthma"}, "subject": patient_ref("p1"), "onsetDateTime": "2010-05-01"}),
    );
    create(
        json!({"resourceType": "Condition", "id": "c2", "clinicalStatus": clinical(condition_clinical, "resolved"),
        "code": {"text": "Fractured wrist"}, "subject": patient_ref("p1")}),
    );
    create(
        json!({"resourceType": "Condition", "id": "c3", "clinicalStatus": clinical(condition_clinical, "active"),
        "code": {"text": "Someone else's diabetes"}, "subject": patient_ref("p2")}),
    );
    create(json!({"resourceType": "AllergyIntolerance", "id": "a1",
        "clinicalStatus": clinical("http://terminology.hl7.org/CodeSystem/allergyintolerance-clinical", "active"),
        "code": {"text": "Penicillin"}, "patient": patient_ref("p1"), "criticality": "high",
        "reaction": [{"manifestation": [{"text": "Hives"}]}]}));
    create(
        json!({"resourceType": "MedicationStatement", "id": "m1", "status": "active",
        "medicationReference": {"reference": "Medication/med1"}, "subject": patient_ref("p1"),
        "effectiveDateTime": "2024-01-01"}),
    );
    create(
        json!({"resourceType": "MedicationStatement", "id": "m2", "status": "stopped",
        "medicationCodeableConcept": {"text": "Amoxicillin"}, "subject": patient_ref("p1")}),
    );
    create(
        json!({"resourceType": "Immunization", "id": "i1", "status": "completed",
        "vaccineCode": {"text": "Influenza"}, "patient": patient_ref("p1"), "occurrenceDateTime": "2023-10-01"}),
    );
    create(
        json!({"resourceType": "Observation", "id": "o1", "status": "final",
        "category": [clinical("http://terminology.hl7.org/CodeSystem/observation-category", "laboratory")],
        "code": {"text": "Hemoglobin"}, "subject": patient_ref("p1"), "effectiveDateTime": "2024-02-02",
        "valueQuantity": {"value": 13.5, "unit": "g/dL"}}),
    );
    create(
        json!({"resourceType": "Procedure", "id": "pr1", "status": "completed",
        "code": {"text": "Appendectomy"}, "subject": patient_ref("p1"), "performedDateTime": "2000-03-04"}),
    );
    harness
}

fn summary_of(harness: &Harness, patient: &str) -> Value {
    apply(harness, "Patient/$summary", json!({"id": patient}))
}

fn section_titles(summary: &Value) -> Vec<String> {
    summary["entry"][0]["resource"]["section"]
        .as_array()
        .unwrap()
        .iter()
        .map(|section| section["title"].as_str().unwrap().to_string())
        .collect()
}

fn entry_ids(summary: &Value) -> Vec<String> {
    summary["entry"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| {
            let resource = &entry["resource"];
            Some(format!(
                "{}/{}",
                resource["resourceType"].as_str()?,
                resource["id"].as_str()?
            ))
        })
        .collect()
}

#[test]
fn a_summary_is_an_ips_document_of_that_patients_record_and_nothing_else() {
    let harness = summary_ready();
    let summary = summary_of(harness, "p1");

    assert_eq!(summary["type"], "document");
    assert_eq!(summary["meta"]["profile"][0], IPS_BUNDLE_PROFILE);
    assert!(summary["identifier"]["value"]
        .as_str()
        .unwrap()
        .starts_with("urn:uuid:"));
    let composition = &summary["entry"][0]["resource"];
    assert_eq!(composition["resourceType"], "Composition");
    assert_eq!(composition["type"]["coding"][0]["code"], "60591-5");
    assert_eq!(
        section_titles(&summary),
        [
            "Allergies and Intolerances",
            "Problem List",
            "Medication Summary",
            "Immunizations",
            "Results",
            "History of Procedures"
        ]
    );

    // What the sections took: the active problem and the current medicine, not the resolved
    // problem, the stopped medicine, or what belongs to another patient.
    let ids = entry_ids(&summary);
    for held in [
        "Patient/p1",
        "Condition/c1",
        "AllergyIntolerance/a1",
        "MedicationStatement/m1",
        "Immunization/i1",
        "Observation/o1",
        "Procedure/pr1",
        "Organization/org1",
        "Medication/med1",
    ] {
        assert!(
            ids.contains(&held.to_string()),
            "{held} is in the summary: {ids:?}"
        );
    }
    for left_out in [
        "Condition/c2",
        "Condition/c3",
        "MedicationStatement/m2",
        "Patient/p2",
    ] {
        assert!(
            !ids.contains(&left_out.to_string()),
            "{left_out} is not: {ids:?}"
        );
    }
    let patient = summary["entry"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["resource"]["id"] == "p1")
        .unwrap();
    assert!(
        patient["resource"].get("link").is_none(),
        "no link to another patient's record"
    );

    // Each section says in words what it holds.
    let narrative = |title: &str| {
        let sections = composition["section"].as_array().unwrap();
        let section = sections
            .iter()
            .find(|section| section["title"] == title)
            .unwrap();
        section["text"]["div"].as_str().unwrap().to_string()
    };
    let problems = narrative("Problem List");
    assert!(
        problems.contains("Asthma") && problems.contains("2010-05-01"),
        "{problems}"
    );
    assert!(!problems.contains("Fractured wrist"), "{problems}");
    let medicines = narrative("Medication Summary");
    assert!(
        medicines.contains("Salbutamol 100 micrograms inhaler"),
        "the Medication it points at is read: {medicines}"
    );
    assert!(narrative("Allergies and Intolerances").contains("Hives"));
    assert!(narrative("Results").contains("13.5 g/dL"));
    for title in section_titles(&summary) {
        assert!(
            refhir_fhirpath::html_checks(&narrative(&title)),
            "{title} is valid narrative"
        );
    }

    // The references in the document resolve inside it.
    let entries = summary["entry"].as_array().unwrap();
    let url_of = |type_name: &str, id: &str| {
        entries
            .iter()
            .find(|entry| {
                entry["resource"]["resourceType"] == type_name && entry["resource"]["id"] == id
            })
            .map(|entry| entry["fullUrl"].clone())
            .unwrap()
    };
    assert_eq!(composition["subject"]["reference"], url_of("Patient", "p1"));
    assert_eq!(
        composition["author"][0]["reference"],
        url_of("Organization", "org1")
    );
    assert_eq!(
        composition["section"][1]["entry"][0]["reference"],
        url_of("Condition", "c1")
    );

    // The summary names its own profile, so it is checked against IPS as it stands: the way to
    // validate it over HTTP, where a type-level validate takes no profile of its own.
    let outcome = apply(harness, "Bundle/$validate", json!({"resource": summary}));
    assert!(
        errors_of(&outcome).is_empty(),
        "the summary is a valid IPS document: {outcome}"
    );
    let mut headless = summary.clone();
    headless["entry"].as_array_mut().unwrap().remove(0);
    let outcome = apply(harness, "Bundle/$validate", json!({"resource": headless}));
    assert!(
        !errors_of(&outcome).is_empty(),
        "it was held to the IPS profile, which wants its Composition: {outcome}"
    );
}

#[test]
fn a_required_section_with_nothing_in_it_says_so_and_an_optional_one_is_left_out() {
    let harness = summary_ready();
    let summary = summary_of(harness, "p3");
    assert_eq!(
        section_titles(&summary),
        [
            "Allergies and Intolerances",
            "Problem List",
            "Medication Summary"
        ]
    );
    for section in summary["entry"][0]["resource"]["section"]
        .as_array()
        .unwrap()
    {
        assert_eq!(section["emptyReason"]["coding"][0]["code"], "unavailable");
        assert!(section.get("entry").is_none());
        assert!(section["text"]["div"]
            .as_str()
            .unwrap()
            .contains("no information available"));
    }
    // The Composition has no id of its own, so the patient is the only entry that has one.
    assert_eq!(entry_ids(&summary), ["Patient/p3"]);
    let outcome = apply(
        harness,
        "Bundle/$validate",
        json!({"resource": summary, "profile": IPS_BUNDLE_PROFILE}),
    );
    assert!(errors_of(&outcome).is_empty(), "{outcome}");
}

#[test]
fn a_summary_names_its_patient_by_id_or_by_identifier() {
    let harness = summary_ready();
    let patient_of = |summary: &Value| {
        summary["entry"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["resource"]["resourceType"] == "Patient")
            .unwrap()["resource"]["id"]
            .clone()
    };
    let by_query = apply(
        harness,
        "Patient/$summary",
        json!({"identifier": "http://hospital.example|mrn-p1"}),
    );
    assert_eq!(patient_of(&by_query), "p1");
    let by_parameters = apply(
        harness,
        "Patient/$summary",
        json!({"resource": {
            "resourceType": "Parameters",
            "parameter": [{"name": "identifier", "valueString": "http://hospital.example|mrn-p1"}]
        }}),
    );
    assert_eq!(patient_of(&by_parameters), "p1");

    let unknown = run_op(
        harness,
        "Patient/$summary",
        &json!({"identifier": "http://hospital.example|nobody"}),
    )
    .unwrap_err();
    assert!(
        matches!(unknown, RunError::Fhir(FhirError::NotFound(_))),
        "{unknown:?}"
    );
    let unnamed = run_op(harness, "Patient/$summary", &json!({})).unwrap_err();
    assert!(
        matches!(unnamed, RunError::Fhir(FhirError::Invalid(_))),
        "{unnamed:?}"
    );
    let missing = run_op(harness, "Patient/$summary", &json!({"id": "nobody"})).unwrap_err();
    assert!(
        matches!(missing, RunError::Fhir(FhirError::NotFound(_))),
        "{missing:?}"
    );
    let wrong_type = run_op(harness, "Observation/$summary", &json!({"id": "o1"})).unwrap_err();
    assert!(
        matches!(wrong_type, RunError::Fhir(FhirError::Unsupported(_))),
        "{wrong_type:?}"
    );
}

#[test]
fn a_summary_holds_only_what_the_callers_scopes_let_them_read() {
    let harness = summary_ready();
    let narrow = apply(
        harness,
        "Patient/$summary",
        json!({
            "id": "p1",
            "_readScopes": ["patient/Patient.rs", "patient/Condition.rs"]
        }),
    );
    let ids = entry_ids(&narrow);
    assert!(ids.contains(&"Condition/c1".to_string()), "{ids:?}");
    for unreadable in [
        "AllergyIntolerance/a1",
        "MedicationStatement/m1",
        "Immunization/i1",
        "Organization/org1",
        "Medication/med1",
    ] {
        assert!(
            !ids.contains(&unreadable.to_string()),
            "{unreadable} is not readable: {ids:?}"
        );
    }
    // A required section the token cannot read is not filled, and says nothing is available.
    let allergies = &narrow["entry"][0]["resource"]["section"][0];
    assert_eq!(allergies["title"], "Allergies and Intolerances");
    assert_eq!(allergies["emptyReason"]["coding"][0]["code"], "unavailable");

    let everything = apply(
        harness,
        "Patient/$everything",
        json!({
            "id": "p1",
            "_readScopes": ["patient/Patient.rs", "patient/Condition.rs"]
        }),
    );
    let types: std::collections::BTreeSet<_> = everything["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            entry["resource"]["resourceType"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        types,
        ["Condition", "Patient"]
            .map(String::from)
            .into_iter()
            .collect()
    );
}

fn card_request(parameters: Value) -> Value {
    json!({"resource": {"resourceType": "Parameters", "parameter": parameters}})
}

fn card_args(patient: &str, parameters: Value) -> Value {
    let mut args = card_request(parameters);
    args["id"] = json!(patient);
    args
}

fn credential_type(uri: &str) -> Value {
    json!({"name": "credentialType", "valueUri": uri})
}

const IMMUNIZATION_TYPE: &str = "https://smarthealth.cards#immunization";
const LABORATORY_TYPE: &str = "https://smarthealth.cards#laboratory";

#[test]
fn a_card_holds_who_it_is_about_and_what_was_asked_and_nothing_more() {
    let harness = summary_ready();
    let issued = apply(
        harness,
        "Patient/$health-cards-issue",
        card_args("p1", json!([credential_type("Immunization")])),
    );
    assert_eq!(issued["fhirVersion"], "4.0.1");
    let credentials = issued["credentials"].as_array().unwrap();
    assert_eq!(credentials.len(), 1);
    let credential = &credentials[0];
    assert_eq!(credential["types"], json!([IMMUNIZATION_TYPE]));

    let bundle = &credential["bundle"];
    assert_eq!(bundle["resourceType"], "Bundle");
    assert_eq!(bundle["type"], "collection");
    let entries = bundle["entry"].as_array().unwrap();
    assert_eq!(
        entries.len(),
        2,
        "the patient and the immunization: {bundle}"
    );
    assert_eq!(entries[0]["fullUrl"], "resource:0");
    assert_eq!(entries[1]["fullUrl"], "resource:1");

    // Who it is about: a name and a birth date, no more than the format asks.
    assert_eq!(
        entries[0]["resource"],
        json!({
            "resourceType": "Patient",
            "name": [{"family": "Lovelace", "given": ["Ada"]}],
            "birthDate": "1815-12-10"
        })
    );
    // The immunization is as small as it can be, and points at the patient in the card.
    let immunization = &entries[1]["resource"];
    assert_eq!(immunization["resourceType"], "Immunization");
    assert!(immunization.get("id").is_none() && immunization.get("meta").is_none());
    assert!(immunization.get("text").is_none());
    assert_eq!(immunization["patient"]["reference"], "resource:0");
    // A concept that is only words keeps them: with them gone nothing would say what the vaccine was.
    assert_eq!(immunization["vaccineCode"]["text"], "Influenza");
    assert_eq!(immunization["occurrenceDateTime"], "2023-10-01");

    // Where each resource came from, for the response that goes with the card.
    assert_eq!(
        credential["links"],
        json!([
            {"bundledResource": "resource:0", "hostedResource": "Patient/p1"},
            {"bundledResource": "resource:1", "hostedResource": "Immunization/i1"}
        ])
    );
    // What is in the card is a valid FHIR Bundle.
    let outcome = apply(harness, "Bundle/$validate", json!({"resource": bundle}));
    assert!(errors_of(&outcome).is_empty(), "{outcome}");
}

#[test]
fn a_card_takes_the_kinds_named_by_type_or_by_the_legacy_uris_and_every_one_of_them() {
    let harness = summary_ready();
    let types_of = |parameters: Value| {
        let issued = apply(
            harness,
            "Patient/$health-cards-issue",
            card_args("p1", parameters),
        );
        let credentials = issued["credentials"].as_array().unwrap().clone();
        credentials
            .first()
            .map(|credential| {
                credential["bundle"]["entry"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|entry| {
                        entry["resource"]["resourceType"]
                            .as_str()
                            .unwrap()
                            .to_string()
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    assert_eq!(
        types_of(json!([credential_type(LABORATORY_TYPE)])),
        ["Patient", "Observation"]
    );
    // Two kinds are "and": one card holds both.
    assert_eq!(
        types_of(json!([
            credential_type("Immunization"),
            credential_type("Procedure")
        ])),
        ["Patient", "Immunization", "Procedure"]
    );
    // A kind with nothing in the record means there is no card, not a card with less in it.
    assert!(types_of(json!([
        credential_type("Immunization"),
        credential_type("DiagnosticReport")
    ]))
    .is_empty());
    // A patient with no record has no card for any kind.
    let none = apply(
        harness,
        "Patient/$health-cards-issue",
        card_args("p3", json!([credential_type("Immunization")])),
    );
    assert_eq!(none["credentials"], json!([]));
    // The legacy laboratory type carries its own credential type.
    let lab = apply(
        harness,
        "Patient/$health-cards-issue",
        card_args("p1", json!([credential_type(LABORATORY_TYPE)])),
    );
    assert_eq!(lab["credentials"][0]["types"], json!([LABORATORY_TYPE]));
}

#[test]
fn a_card_says_of_the_patient_what_the_request_names() {
    let harness = summary_ready();
    let issued = apply(
        harness,
        "Patient/$health-cards-issue",
        card_args(
            "p1",
            json!([
                credential_type("Immunization"),
                {"name": "includeIdentityClaim", "valueString": "Patient.gender"},
                {"name": "includeIdentityClaim", "valueString": "Patient.identifier"}
            ]),
        ),
    );
    assert_eq!(
        issued["credentials"][0]["bundle"]["entry"][0]["resource"],
        json!({
            "resourceType": "Patient",
            "gender": "female",
            "identifier": [{"system": "http://hospital.example", "value": "mrn-p1"}]
        })
    );
}

#[test]
fn a_card_request_that_cannot_be_met_is_refused_and_says_why() {
    let harness = summary_ready();
    let refused = |args: Value| match run_op(harness, "Patient/$health-cards-issue", &args) {
        Err(RunError::Fhir(err)) => err,
        other => panic!("expected a refusal, got {other:?}"),
    };
    assert!(
        matches!(refused(card_args("p1", json!([]))), FhirError::Invalid(message) if message.contains("credentialType is required"))
    );
    assert!(matches!(
        refused(card_args("p1", json!([credential_type("https://smarthealth.cards#covid19")]))),
        FhirError::Invalid(message) if message.contains("covid19")
    ));
    assert!(matches!(
        refused(card_args("p1", json!([credential_type("Patient")]))),
        FhirError::Invalid(_)
    ));
    assert!(matches!(
        refused(card_args(
            "p1",
            json!([credential_type("Immunization"), {"name": "_since", "valueString": "last week"}])
        )),
        FhirError::Invalid(message) if message.contains("_since is not a date")
    ));
    assert!(matches!(
        refused(card_args(
            "p1",
            json!([credential_type("Immunization"), {"name": "_since", "valueDateTime": "2024-01-01"}, {"name": "_since", "valueDateTime": "2025-01-01"}])
        )),
        FhirError::Invalid(message) if message.contains("given once")
    ));
    assert!(matches!(
        refused(card_args(
            "p1",
            json!([credential_type("Immunization"), {"name": "includeIdentityClaim", "valueString": "Patient.photo"}])
        )),
        FhirError::Invalid(_)
    ));
    assert!(matches!(
        refused(card_args(
            "nobody",
            json!([credential_type("Immunization")])
        )),
        FhirError::NotFound(_)
    ));
    assert!(matches!(
        refused(json!({"id": "o1", "resource": {"resourceType": "Parameters", "parameter": []}})),
        FhirError::Invalid(_)
    ));
    assert!(matches!(
        run_op(
            harness,
            "Observation/$health-cards-issue",
            &card_args("o1", json!([credential_type("Immunization")]))
        ),
        Err(RunError::Fhir(FhirError::Unsupported(_)))
    ));
}

#[test]
fn a_card_holds_only_what_the_callers_scopes_let_them_read() {
    let harness = summary_ready();
    let mut args = card_args("p1", json!([credential_type("Immunization")]));
    args["_readScopes"] = json!(["patient/Patient.rs"]);
    let issued = apply(harness, "Patient/$health-cards-issue", args);
    assert_eq!(issued["credentials"], json!([]));
}

fn pause() {
    // Times are in milliseconds, so two writes that a test wants apart are this far apart.
    std::thread::sleep(std::time::Duration::from_millis(15));
}

fn moment() -> String {
    crate::instant::format(crate::instant::now_ms())
}

fn written_at(resource: &Value) -> i64 {
    crate::instant::parse(
        resource["meta"]["lastUpdated"]
            .as_str()
            .expect("lastUpdated"),
    )
    .unwrap()
}

#[test]
fn every_version_says_when_it_was_written_whatever_wrote_it_and_the_clients_word_is_not_kept() {
    let harness = ready();
    let before = crate::instant::now_ms();
    let mut body = patient("p1", true, "Ann");
    body["meta"] = json!({"lastUpdated": "1999-01-01T00:00:00Z"});
    let created = apply(&harness, "Patient/create", json!({"resource": body}));
    let first = written_at(&created);
    assert!(
        first >= before && first <= crate::instant::now_ms(),
        "the server's clock, not the client's"
    );

    let read = apply(&harness, "Patient/read", json!({"id": "p1"}));
    assert_eq!(
        read["meta"]["lastUpdated"], created["meta"]["lastUpdated"],
        "what is kept is what was said"
    );

    // An update carries the time it was read with, and a client may claim another; neither is kept.
    pause();
    let mut changed = read.clone();
    changed["active"] = json!(false);
    let updated = apply(
        &harness,
        "Patient/update",
        json!({"id": "p1", "resource": changed}),
    );
    let second = written_at(&updated);
    assert!(second > first, "{second} after {first}");
    let mut claimed = apply(&harness, "Patient/read", json!({"id": "p1"}));
    claimed["meta"]["lastUpdated"] = json!("1999-01-01T00:00:00Z");
    pause();
    let third = written_at(&apply(
        &harness,
        "Patient/update",
        json!({"id": "p1", "resource": claimed}),
    ));
    assert!(third > second);

    // A patch, and a change of tags, are writes too.
    pause();
    let patched = apply(
        &harness,
        "Patient/patch",
        json!({"id": "p1", "patch": [{"op": "replace", "path": "/active", "value": true}]}),
    );
    let fourth = written_at(&patched);
    assert!(fourth > third);
    pause();
    apply(
        &harness,
        "Patient/$meta-add",
        json!({"id": "p1", "resource": {"meta": {"tag": [{"system": "http://example.org/tags", "code": "vip"}]}}}),
    );
    let tagged = apply(&harness, "Patient/read", json!({"id": "p1"}));
    assert!(written_at(&tagged) > fourth);
    assert_eq!(
        tagged["meta"]["tag"][0]["code"], "vip",
        "the tag is kept beside the time"
    );

    // A transaction writes with the same clock.
    pause();
    apply(
        &harness,
        "Bundle/transaction",
        json!({"resource": {"resourceType": "Bundle", "type": "transaction", "entry": [
            {"resource": {"resourceType": "Patient", "name": [{"family": "Bee"}]}, "request": {"method": "POST", "url": "Patient"}}
        ]}}),
    );
    let all = apply(&harness, "Patient/search", json!({}));
    for entry in all["entry"].as_array().unwrap() {
        assert!(written_at(&entry["resource"]) >= first);
    }
    assert_eq!(all["entry"].as_array().unwrap().len(), 2);

    // Each version keeps its own time.
    let first_version = created["meta"]["versionId"].as_str().unwrap().to_string();
    let old = apply(
        &harness,
        "Patient/vread",
        json!({"id": "p1", "versionId": first_version}),
    );
    assert_eq!(written_at(&old), first);
    let history = apply(&harness, "Patient/history", json!({"id": "p1"}));
    let times: Vec<i64> = history["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| written_at(&entry["resource"]))
        .collect();
    assert_eq!(times.len(), 5);
    assert!(
        times.windows(2).all(|pair| pair[0] > pair[1]),
        "newest first, each its own time: {times:?}"
    );
}

#[test]
fn a_search_by_when_a_resource_was_written_finds_what_it_should_and_follows_an_update() {
    let harness = r4_ready();
    let ids = |query: Value| -> Vec<String> {
        let found = apply(&harness, "Patient/search", query);
        let mut ids: Vec<String> = found["entry"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    };
    let create = |id: &str| {
        apply(
            &harness,
            "Patient/create",
            json!({"resource": patient(id, true, "Ann")}),
        )
    };
    let before = moment();
    pause();
    create("p1");
    pause();
    let between = moment();
    pause();
    create("p2");
    pause();
    let after = moment();

    assert_eq!(
        ids(json!({"_lastUpdated": format!("ge{before}")})),
        ["p1", "p2"]
    );
    assert_eq!(ids(json!({"_lastUpdated": format!("ge{between}")})), ["p2"]);
    assert_eq!(ids(json!({"_lastUpdated": format!("lt{between}")})), ["p1"]);
    assert_eq!(
        ids(json!({"_lastUpdated": format!("gt{after}")})),
        Vec::<String>::new()
    );
    assert_eq!(
        ids(json!({"_lastUpdated": format!("le{after}")})),
        ["p1", "p2"]
    );
    // Twice is "and": a window.
    assert_eq!(
        ids(json!({"_lastUpdated": [format!("ge{before}"), format!("lt{between}")]})),
        ["p1"]
    );
    // A date is the whole day.
    assert_eq!(ids(json!({"_lastUpdated": "ge2000-01-01"})), ["p1", "p2"]);
    assert_eq!(
        ids(json!({"_lastUpdated": "le1999-12-31"})),
        Vec::<String>::new()
    );
    assert_eq!(
        ids(json!({"_lastUpdated": "gt2999-01-01"})),
        Vec::<String>::new()
    );
    // With another parameter.
    assert_eq!(
        ids(json!({"_lastUpdated": format!("ge{before}"), "_id": "p2"})),
        ["p2"]
    );

    // An update moves a resource to when it was updated, and it is no longer found where it was.
    let mut changed = apply(&harness, "Patient/read", json!({"id": "p1"}));
    changed["active"] = json!(false);
    apply(
        &harness,
        "Patient/update",
        json!({"id": "p1", "resource": changed}),
    );
    assert_eq!(ids(json!({"_lastUpdated": format!("ge{after}")})), ["p1"]);
    assert_eq!(
        ids(json!({"_lastUpdated": format!("lt{between}")})),
        Vec::<String>::new()
    );
}

/// A moment as a time in a zone, to the millisecond, second or minute: `2026-10-18T05:45:36+01:00`.
/// Also where that time starts and ends, in milliseconds.
fn written_in(ms: i64, offset_minutes: i64, width: &str) -> (String, i64, i64) {
    let (start, size) = match width {
        "ms" => (ms, 1),
        "second" => (ms.div_euclid(1000) * 1000, 1000),
        _ => (ms.div_euclid(60_000) * 60_000, 60_000),
    };
    let local = crate::instant::format(start + offset_minutes * 60_000);
    let clock = match width {
        "ms" => &local[..23],
        "second" => &local[..19],
        _ => &local[..16],
    };
    let zone = if offset_minutes == 0 {
        "Z".to_string()
    } else {
        let sign = if offset_minutes < 0 { '-' } else { '+' };
        let minutes = offset_minutes.abs();
        format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
    };
    (format!("{clock}{zone}"), start, start + size)
}

#[test]
fn a_time_asked_for_to_the_second_or_in_another_zone_finds_a_time_stored_to_the_millisecond() {
    // `lastUpdated` is kept to the millisecond, in UTC, and a search for it can say a minute, a
    // second or a millisecond and can be in any zone. Dates are indexed as text, in text order,
    // so a search that gets this wrong finds nothing where the data is, and does it quietly.
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, cards_dir()).unwrap();
    let mut written = Vec::new();
    for id in ["p1", "p2", "p3"] {
        apply(
            &harness,
            "Patient/create",
            json!({"resource": {
                "resourceType": "Patient",
                "id": id,
                "name": [{"family": "Ann"}]
            }}),
        );
        let read = apply(&harness, "Patient/read", json!({"id": id}));
        written.push((id, written_at(&read)));
        pause();
    }
    // Times that do not fall on a second, so that a second and a millisecond are not the same.
    assert!(written.iter().any(|(_, at)| at % 1000 != 0));

    let found = |query: &str| -> Vec<String> {
        let result = apply(&harness, "Patient/search", json!({"_lastUpdated": query}));
        let mut ids: Vec<String> = result["entry"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    };
    let mut asked = 0;
    let anchor = written[1].1;
    for offset in [0, 60, -300, 330, 840, -720] {
        for width in ["ms", "second", "minute"] {
            for shift in [-61_000, -1_500, -1, 0, 1, 1_500, 61_000] {
                let (text, from, to) = written_in(anchor + shift, offset, width);
                for prefix in ["eq", "ge", "gt", "le", "lt"] {
                    // What each comparison means, from the specification and not from the code: a
                    // time written to the millisecond is the one millisecond from `at`.
                    let holds = |at: i64| {
                        let end = at + 1;
                        match prefix {
                            "eq" => at < to && end > from,
                            "ge" => end > from,
                            "gt" => at >= to,
                            "le" => at < to,
                            _ => end <= from,
                        }
                    };
                    let expected: Vec<&str> = written
                        .iter()
                        .filter(|(_, at)| holds(*at))
                        .map(|(id, _)| *id)
                        .collect();
                    assert_eq!(
                        found(&format!("{prefix}{text}")),
                        expected,
                        "{prefix}{text}: the writes are at {written:?}"
                    );
                    asked += 1;
                }
            }
        }
    }
    assert_eq!(asked, 6 * 3 * 7 * 5);
}

#[test]
fn a_date_to_the_year_or_the_month_is_found_by_a_search_for_a_day_inside_it_or_around_it() {
    let harness = ready();
    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    );
    let dates = [
        ("a", "2025-12"),
        ("b", "2026"),
        ("c", "2026-10"),
        ("d", "2026-10-18"),
        ("e", "2026-10-19"),
        ("f", "2026-10-18T23:30:00-05:00"),
        ("g", "2026-10-19T03:30:00.5Z"),
        ("h", "2027"),
    ];
    for (id, date) in dates {
        apply(
            &harness,
            "Observation/create",
            json!({"resource": observation(id, "p1", date, "8480-6")}),
        );
    }
    let found = |query: &str| -> Vec<String> {
        let result = apply(&harness, "Observation/search", json!({"date": query}));
        let mut ids: Vec<String> = result["entry"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    };
    // A year holds every day of it, so it is on both sides of a day inside it. `f` and `g` are the
    // same moment, 2026-10-19T04:30:00Z and 2026-10-19T03:30:00.5Z, a minute apart in the 19th.
    assert_eq!(found("2026-10-18"), ["b", "c", "d"]);
    assert_eq!(found("eq2026-10-19"), ["b", "c", "e", "f", "g"]);
    assert_eq!(found("ge2026-10-19"), ["b", "c", "e", "f", "g", "h"]);
    // `gt` and `lt` take what reaches above or below the day, so a year or a month that holds days
    // after it (or before it) is in; `sa` and `eb` take only what lies wholly beyond it.
    assert_eq!(found("gt2026-10-18"), ["b", "c", "e", "f", "g", "h"]);
    assert_eq!(found("sa2026-10-18"), ["e", "f", "g", "h"]);
    assert_eq!(found("le2026-10-18"), ["a", "b", "c", "d"]);
    assert_eq!(found("lt2026-10-18"), ["a", "b", "c"]);
    assert_eq!(found("eb2026-10-18"), ["a"]);
    assert_eq!(
        found("lt2026-10-19T04:00:00Z"),
        ["a", "b", "c", "d", "e", "g"]
    );
    assert_eq!(found("ge2026-10-19T04:00:00Z"), ["b", "c", "e", "f", "h"]);
    assert_eq!(found("2026-10-19T03:30:00Z"), ["b", "c", "e", "g"]);
    assert_eq!(found("2026-10-18T23:30:00-05:00"), ["b", "c", "e", "f"]);
}

fn cards_harness() -> Harness {
    let harness = empty();
    install_package_dir(&harness.store, &harness.db, &harness.catalog, cards_dir()).unwrap();
    harness
}

fn seeded(harness: &Harness, resource: Value) {
    apply(
        harness,
        &format!("{}/create", resource["resourceType"].as_str().unwrap()),
        json!({"resource": resource}),
    );
}

fn resource_keys(bundle: &Value) -> Vec<String> {
    let mut keys: Vec<String> = bundle["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            format!(
                "{}/{}",
                entry["resource"]["resourceType"].as_str().unwrap(),
                entry["resource"]["id"].as_str().unwrap_or("-")
            )
        })
        .collect();
    keys.sort();
    keys
}

#[test]
fn a_document_for_a_caller_limited_to_one_patient_leaves_other_patients_out() {
    let harness = cards_harness();
    // A links to B, as a merged or duplicate record does. B is made first: a reference has a target.
    seeded(
        &harness,
        json!({"resourceType": "Patient", "id": "pb", "name": [{"family": "Bee"}]}),
    );
    seeded(
        &harness,
        json!({"resourceType": "Patient", "id": "pa", "name": [{"family": "A"}],
            "link": [{"other": {"reference": "Patient/pb"}, "type": "seealso"}]}),
    );
    seeded(
        &harness,
        json!({"resourceType": "Composition", "id": "ca", "status": "final",
            "type": {"text": "Summary"}, "subject": {"reference": "Patient/pa"},
            "date": "2026-10-18", "title": "A"}),
    );
    let everyone = apply(&harness, "Composition/$document", json!({"id": "ca"}));
    assert_eq!(
        resource_keys(&everyone),
        ["Composition/ca", "Patient/pa", "Patient/pb"],
        "without a limit the document follows the link"
    );
    let limited = apply(
        &harness,
        "Composition/$document",
        json!({"id": "ca", "_patientCompartment": "Patient/pa"}),
    );
    assert_eq!(
        resource_keys(&limited),
        ["Composition/ca", "Patient/pa"],
        "a caller limited to A does not get B"
    );
    assert!(
        !limited.to_string().contains("Bee"),
        "nothing of B is in it: {limited}"
    );
}

#[test]
fn a_card_is_asked_for_a_clinical_type_and_for_nothing_else_the_compartment_lists() {
    // This catalog's Patient compartment lists Patient (by `link`), Immunization and Composition, as
    // R4's does Patient, and much besides.
    let harness = cards_harness();
    seeded(
        &harness,
        json!({"resourceType": "Patient", "id": "p1", "name": [{"family": "One"}]}),
    );
    seeded(
        &harness,
        json!({"resourceType": "Patient", "id": "p2", "name": [{"family": "Two"}],
            "link": [{"other": {"reference": "Patient/p1"}, "type": "seealso"}]}),
    );
    let asked = |kind: &str| {
        run_op(
            &harness,
            "Patient/$health-cards-issue",
            &card_args("p1", json!([credential_type(kind)])),
        )
    };
    for kind in ["Patient", "Composition"] {
        match asked(kind) {
            Err(RunError::Fhir(FhirError::Invalid(message))) => {
                assert!(
                    message.contains(kind) && message.contains("name one of"),
                    "{message}"
                );
                assert!(
                    message.contains("Immunization"),
                    "it says what is allowed: {message}"
                );
            }
            other => panic!("{kind} is not a thing a card holds: {other:?}"),
        }
    }
    let none = asked("Immunization").expect("a clinical type is taken");
    assert_eq!(
        none.value,
        json!({"credentials": []}),
        "and with none of it, there is no card"
    );
}

#[test]
fn a_section_holds_the_latest_of_what_there_is_and_a_read_is_bounded_and_newest_first() {
    let harness = cards_harness();
    seeded(
        &harness,
        json!({"resourceType": "Patient", "id": "p1", "name": [{"family": "One"}]}),
    );
    let vaccine = |id: &str| {
        json!({"resourceType": "Immunization", "id": id, "status": "completed",
            "vaccineCode": {"coding": [{"system": "http://hl7.org/fhir/sid/cvx", "code": "140"}]},
            "patient": {"reference": "Patient/p1"}, "occurrenceDateTime": "2024-01-01"})
    };
    for number in 0..105 {
        seeded(&harness, vaccine(&format!("i{number:03}")));
    }
    pause();
    seeded(&harness, vaccine("newest"));

    let summary = apply(&harness, "Patient/$summary", json!({"id": "p1"}));
    let shots: Vec<String> = resource_keys(&summary)
        .into_iter()
        .filter(|key| key.starts_with("Immunization/"))
        .collect();
    assert_eq!(
        shots.len(),
        100,
        "a long record is a summary, and not an error"
    );
    assert!(
        shots.contains(&"Immunization/newest".to_string()),
        "the latest is kept"
    );

    let catalog = harness.catalog.read().unwrap();
    let mut tx = harness.db.begin();
    let three = crate::summary::compartment_resources(
        &mut tx,
        &catalog,
        "p1",
        "Immunization",
        &|_| true,
        3,
    )
    .unwrap();
    assert_eq!(three.len(), 3, "no more than the limit is read");
    assert_eq!(three[0]["id"], "newest", "and the newest come first");
}

/// `method url` of each entry of a history, newest first.
fn history_of(harness: &Harness, path: &str, args: Value) -> Vec<String> {
    apply(harness, path, args)["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            format!(
                "{} {}",
                entry["request"]["method"].as_str().unwrap(),
                entry["request"]["url"].as_str().unwrap()
            )
        })
        .collect()
}

#[test]
fn history_by_date_lists_what_was_written_after_it_and_deletions_since_the_last_change_before() {
    let harness = ready();
    let create = |id: &str| {
        apply(
            &harness,
            "Patient/create",
            json!({"resource": patient(id, true, "Ann")}),
        )
    };
    // A resource made and deleted long before the moment, with writes after it and before the
    // moment: its deletion is older than the last change before the moment, and is not listed.
    create("z");
    apply(&harness, "Patient/delete", json!({"id": "z"}));
    pause();
    create("a");
    pause();
    let b = create("b");
    pause();
    let at = moment();
    pause();
    create("c");
    apply(&harness, "Patient/delete", json!({"id": "b"}));
    pause();
    let mut changed = apply(&harness, "Patient/read", json!({"id": "c"}));
    changed["active"] = json!(false);
    apply(
        &harness,
        "Patient/update",
        json!({"id": "c", "resource": changed}),
    );

    // Newest first: the update of `c`, the deletion of `b`, the creation of `c`, then the first two.
    assert_eq!(
        history_of(&harness, "Patient/history", json!({})),
        [
            "PUT Patient/c",
            "DELETE Patient/b",
            "POST Patient/c",
            "POST Patient/b",
            "POST Patient/a",
            "DELETE Patient/z",
            "POST Patient/z"
        ]
    );
    // After the moment: the two writes of `c`, and the deletion of `b`, which came in between.
    assert_eq!(
        history_of(&harness, "Patient/history", json!({"_since": at})),
        ["PUT Patient/c", "DELETE Patient/b", "POST Patient/c"]
    );
    // The same for the whole server, and with the offset a client may write the moment with.
    assert_eq!(
        history_of(&harness, "system/history", json!({"_since": at})),
        ["PUT Patient/c", "DELETE Patient/b", "POST Patient/c"]
    );
    // Before everything is everything; after everything is nothing.
    assert_eq!(
        history_of(
            &harness,
            "Patient/history",
            json!({"_since": "2000-01-01T00:00:00Z"})
        )
        .len(),
        7
    );
    assert_eq!(
        history_of(&harness, "Patient/history", json!({"_since": "2000-01-01"})).len(),
        7
    );
    assert!(history_of(
        &harness,
        "Patient/history",
        json!({"_since": "2999-01-01T00:00:00Z"})
    )
    .is_empty());
    // A version id, which the server's own clients give, still means what it did: inclusive.
    let b_version = b["meta"]["versionId"].as_str().unwrap();
    assert_eq!(
        history_of(&harness, "Patient/history", json!({"_since": b_version})),
        [
            "PUT Patient/c",
            "DELETE Patient/b",
            "POST Patient/c",
            "POST Patient/b"
        ]
    );
    // What is not a moment is refused, and says what is.
    for bad in ["yesterday", "2026-13-40", "2026-10-18T25:00:00Z", ""] {
        match run_op(&harness, "Patient/history", &json!({"_since": bad})) {
            Err(RunError::Fhir(FhirError::Invalid(message))) => {
                assert!(message.contains("invalid _since"), "{message}")
            }
            other => panic!("{bad:?} should be refused, got {other:?}"),
        }
    }
}

#[test]
fn history_by_date_lists_a_deletion_that_came_just_before_it_rather_than_miss_one_that_came_after()
{
    let harness = ready();
    let create = |id: &str| {
        apply(
            &harness,
            "Patient/create",
            json!({"resource": patient(id, true, "Ann")}),
        )
    };
    create("a");
    apply(&harness, "Patient/delete", json!({"id": "a"}));
    pause();
    let at = moment();
    pause();
    create("b");
    // A deletion carries no time, so it cannot be told from one after the moment: the log is read
    // from the last change that came before the moment, and the deletion is after that change.
    assert_eq!(
        history_of(&harness, "Patient/history", json!({"_since": at})),
        ["POST Patient/b", "DELETE Patient/a"]
    );
}

#[test]
fn a_card_for_what_is_new_holds_what_was_written_since_and_nothing_else() {
    let harness = r4_ready();
    apply(
        &harness,
        "Patient/create",
        json!({"resource": {"resourceType": "Patient", "id": "p1", "name": [{"family": "Ann"}], "birthDate": "1990-01-01"}}),
    );
    let shot = |id: &str, date: &str| {
        apply(
            &harness,
            "Immunization/create",
            json!({"resource": {"resourceType": "Immunization", "id": id, "status": "completed",
                "vaccineCode": {"coding": [{"system": "http://hl7.org/fhir/sid/cvx", "code": "140"}]},
                "patient": {"reference": "Patient/p1"}, "occurrenceDateTime": date}}),
        )
    };
    shot("old", "2020-01-01");
    pause();
    let at = moment();
    pause();
    shot("new", "2024-05-05");
    let dates = |since: Option<&str>| -> Vec<String> {
        let mut parameters = vec![credential_type("Immunization")];
        if let Some(since) = since {
            parameters.push(json!({"name": "_since", "valueDateTime": since}));
        }
        let issued = apply(
            &harness,
            "Patient/$health-cards-issue",
            card_args("p1", json!(parameters)),
        );
        issued["credentials"][0]["bundle"]["entry"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .skip(1)
                    .map(|entry| {
                        entry["resource"]["occurrenceDateTime"]
                            .as_str()
                            .unwrap()
                            .to_string()
                    })
                    .collect()
            })
            .map(|mut found: Vec<String>| {
                found.sort();
                found
            })
            .unwrap_or_default()
    };
    assert_eq!(dates(None), ["2020-01-01", "2024-05-05"]);
    assert_eq!(dates(Some(&at)), ["2024-05-05"]);
    assert_eq!(
        dates(Some("2000-01-01")),
        ["2020-01-01", "2024-05-05"],
        "a date alone is where it starts"
    );
    assert!(
        dates(Some("2999-01-01T00:00:00Z")).is_empty(),
        "nothing new is no card"
    );
}
