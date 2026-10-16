use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use refhir_database::{Database, Put};
use refhir_persistence::ConformanceStore;
use refhir_sqlite::SqlitePersistence;
use refhir_types::{exact_key, token_interval, IndexId, Order, TabletId};
use serde_json::{json, Value};

use refhir_validate::{TermAnswer, Terminology};

use crate::catalog::{Catalog, ReferencePolicy};
use crate::conform::{commit_operation, repair_conformance};
use crate::error::FhirError;
use crate::npm::{install_package, install_with, PackageFetcher};
use crate::package::{
    cross_version_dir, expression_clash_dir, extension_copies_dir, extra_dir, install_package_dir,
    minir4_dir, newer_terminology_dir, publish, r4_dir, run_backfills, sdc_dir,
    stage_package_deferred, stage_package_dir, version_clash_dir,
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
        assert!(now > version);
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
        assert_eq!(replica_at, version_before);

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
        assert_eq!(now, version());
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
}

/// Search builds its page lazily: only the ids of the index range, only the page's documents, no
/// second check of what the index already decided, and no copy of every match. These tests hold
/// that to the old behaviour (the whole result, byte for byte, and the reads it records) and to an
/// oracle that does not use either implementation, and then count the work.
mod search_paging {
    use super::*;
    use crate::extract::compile_parameter;
    use crate::search::{search_counted, search_reference, Work};

    fn r4() -> Harness {
        let harness = empty();
        install_package_dir(&harness.store, &harness.db, &harness.catalog, r4_dir()).unwrap();
        harness
    }

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
        let current = search_counted(&mut current_tx, &catalog, model, &params);
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

    // The service categories live in a CodeSystem whose content is `example`.
    let categories = unprocessable(run_op(
        &harness,
        "ValueSet/$expand",
        &json!({"url": "http://hl7.org/fhir/ValueSet/service-category"}),
    ));
    let text = categories["issue"][0]["diagnostics"].as_str().unwrap();
    for part in [
        "http://terminology.hl7.org/CodeSystem/service-category",
        "'example'",
        "cannot be included whole",
    ] {
        assert!(text.contains(part), "{part} missing from {text}");
    }
    assert_eq!(
        categories["issue"][0]["expression"][0],
        "ValueSet.compose.include[0]"
    );

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
    assert!(urls.contains(&"http://hl7.org/fhir/ValueSet/service-category"));
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
    assert!(created.value["meta"].get("lastUpdated").is_none());
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
    /// Rewrites of a whole package's expansions.
    package_expansion_rewrites: AtomicUsize,
    expansion_changes: AtomicUsize,
    /// How many of the next calls to fail.
    fail_upsert_terminology: AtomicUsize,
    fail_apply_expansions: AtomicUsize,
}

impl WatchedStore {
    fn new(inner: SqlitePersistence) -> Self {
        Self {
            inner,
            full_terminology_loads: AtomicUsize::new(0),
            full_expansion_loads: AtomicUsize::new(0),
            full_search_parameter_loads: AtomicUsize::new(0),
            package_expansion_rewrites: AtomicUsize::new(0),
            expansion_changes: AtomicUsize::new(0),
            fail_upsert_terminology: AtomicUsize::new(0),
            fail_apply_expansions: AtomicUsize::new(0),
        }
    }

    fn reset(&self) {
        for counter in [
            &self.full_terminology_loads,
            &self.full_expansion_loads,
            &self.full_search_parameter_loads,
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
        self.inner.bump_catalog_version()
    }
    fn catalog_revision(&self) -> Result<u64, PersistError> {
        self.inner.catalog_revision()
    }
    fn bump_catalog_revision(&self) -> Result<u64, PersistError> {
        self.inner.bump_catalog_revision()
    }
    fn claim_backfill(&self, id: i64) -> Result<bool, PersistError> {
        self.inner.claim_backfill(id)
    }
    fn load_packages(&self) -> Result<Vec<StoredPackage>, PersistError> {
        self.inner.load_packages()
    }
    fn load_structure_definitions(&self) -> Result<Vec<StoredStructureDefinition>, PersistError> {
        self.inner.load_structure_definitions()
    }
    fn load_search_parameters(&self) -> Result<Vec<StoredSearchParameter>, PersistError> {
        self.full_search_parameter_loads
            .fetch_add(1, Ordering::SeqCst);
        self.inner.load_search_parameters()
    }
    fn load_terminology(&self) -> Result<Vec<StoredTerminology>, PersistError> {
        self.full_terminology_loads.fetch_add(1, Ordering::SeqCst);
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
        2
    );
    assert_expansions_match(&harness.store, "after the repair");
    // A second start finds nothing to do: the repair is not repeated.
    publish(&harness.store, &harness.catalog).unwrap();
    assert!(!harness.catalog.read().unwrap().expansions_stale());
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
    // Nothing else about a search changed: an argument that is not a string is still refused.
    let refused = run_op(&harness, "Patient/search", &json!({"active": ["true"]})).unwrap_err();
    assert!(
        refused.to_string().contains("must be a string"),
        "{refused}"
    );
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
            "one value per name",
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
