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
    expression_clash_dir, extra_dir, install_package_dir, minir4_dir, publish, r4_dir,
    run_backfills, sdc_dir, stage_package_deferred, stage_package_dir, version_clash_dir,
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
    let path = std::env::temp_dir().join(format!(
        "refhir-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
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
    let resources: Vec<Value> = store
        .load_terminology()
        .unwrap()
        .into_iter()
        .map(|row| serde_json::from_str::<Value>(&row.json).unwrap())
        .collect();
    refhir_validate::MemoryTerminology::expansion_documents(&resources)
        .into_iter()
        .map(|document| (document["url"].as_str().unwrap().to_string(), document))
        .collect()
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
