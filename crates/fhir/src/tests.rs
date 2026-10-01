use std::sync::Arc;

use refhir_database::Database;
use refhir_persistence::ConformanceStore;
use refhir_sqlite::SqlitePersistence;
use refhir_types::{token_interval, IndexId, Order};
use serde_json::{json, Value};

use refhir_validate::{TermAnswer, Terminology};

use crate::catalog::{Catalog, ReferencePolicy};
use crate::conform::{commit_operation, repair_conformance};
use crate::error::FhirError;
use crate::package::{
    expression_clash_dir, extra_dir, install_package_dir, minir4_dir, r4_dir, run_backfills,
    stage_package_dir, version_clash_dir,
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

fn apply(harness: &Harness, path: &str, args: Value) -> Value {
    let catalog = harness.catalog.read().expect("catalog");
    run(&harness.db, &catalog, path, &args).expect(path).value
}

fn run_op(harness: &Harness, path: &str, args: &Value) -> Result<crate::run::RunResult, RunError> {
    let catalog = harness.catalog.read().expect("catalog");
    run(&harness.db, &catalog, path, args)
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
    let bundle = json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": [
            {
                "request": {"method": "POST", "url": "Patient"},
                "resource": patient("p1", true, "Ann")
            },
            {
                "request": {"method": "POST", "url": "Observation"},
                "resource": {
                    "resourceType": "Observation",
                    "id": "o1",
                    "subject": {"reference": "Patient/p1"},
                    "effectiveDateTime": "2020-05-01"
                }
            },
            {
                "request": {"method": "GET", "url": "Patient/p1"}
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
    assert_eq!(response["entry"][2]["resource"]["id"], "p1");
    assert_eq!(
        response["entry"][0]["resource"]["meta"]["versionId"],
        response["entry"][1]["resource"]["meta"]["versionId"]
    );

    let failed = json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": [
            {
                "request": {"method": "POST", "url": "Patient/p2"},
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
    let missing = run_op(&db, "Patient/read", &json!({"id": "p2"})).unwrap_err();
    assert!(matches!(missing, RunError::Fhir(FhirError::NotFound(_))));

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
    let missing = run_op(&db, "Patient/read", &json!({"id": "p3"})).unwrap_err();
    assert!(matches!(missing, RunError::Fhir(FhirError::NotFound(_))));
}

#[test]
fn transaction_creates_a_referenced_patient_before_the_observation() {
    let db = ready();
    let bundle = json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": [
            {
                "request": {"method": "POST", "url": "Observation"},
                "resource": {
                    "resourceType": "Observation",
                    "id": "o1",
                    "subject": {"reference": "Patient/p9"},
                    "effectiveDateTime": "2020-05-01"
                }
            },
            {
                "request": {"method": "GET", "url": "Patient/p9"}
            },
            {
                "request": {"method": "POST", "url": "Patient"},
                "resource": patient("p9", true, "Ann")
            }
        ]
    });
    let response = apply(&db, "Bundle/transaction", json!({"resource": bundle}));
    assert_eq!(response["entry"][0]["response"]["status"], "201 Created");
    assert_eq!(
        response["entry"][0]["resource"]["subject"]["reference"],
        "Patient/p9"
    );
    assert_eq!(response["entry"][1]["resource"]["id"], "p9");
    assert_eq!(response["entry"][2]["response"]["status"], "201 Created");
    assert_eq!(
        response["entry"][0]["resource"]["meta"]["versionId"],
        response["entry"][2]["resource"]["meta"]["versionId"]
    );
    let stored = apply(&db, "Observation/read", json!({"id": "o1"}));
    assert_eq!(stored["subject"]["reference"], "Patient/p9");
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
    let missing = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://example.org/missing"}),
    );
    assert_eq!(missing["resourceType"], "OperationOutcome");
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
    commit(
        &harness,
        "SearchParameter/create",
        json!({"resource": label_parameter("Patient.name")}),
    )
    .unwrap();
    commit(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    )
    .unwrap();
    let found = commit(&harness, "Patient/search", json!({"label": "Ann"})).unwrap();
    assert_eq!(found["total"], 1);
    assert_eq!(found["entry"][0]["resource"]["id"], "p1");

    commit(
        &harness,
        "SearchParameter/update",
        json!({"id": "label", "resource": label_parameter("Patient.birthDate")}),
    )
    .unwrap();
    let missed = commit(&harness, "Patient/search", json!({"label": "Ann"})).unwrap();
    assert_eq!(missed["total"], 0);

    commit(&harness, "SearchParameter/delete", json!({"id": "label"})).unwrap();
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
    commit(&harness, "ValueSet/create", json!({"resource": colors})).unwrap();
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
        json!({"id": "colors", "resource": colors}),
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

    commit(&harness, "ValueSet/delete", json!({"id": "colors"})).unwrap();
    let gone = commit(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://example.org/colors"}),
    )
    .unwrap();
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
    let before = apply(
        &harness,
        "ValueSet/$expand",
        json!({"url": "http://example.org/repair"}),
    );
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
    apply(&harness, "Patient/create", json!({"resource": patient("p1", true, "Ann")}));
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
    apply(&harness, "Patient/create", json!({"resource": patient("p1", true, "Ann")}));
    apply(&harness, "Patient/create", json!({"resource": patient("p2", false, "Bee")}));
    apply(&harness, "Observation/create", json!({"resource": observation("o1", "p1", "2020-01-02", "8480-6")}));
    apply(&harness, "Observation/create", json!({"resource": observation("o2", "p2", "2018-01-02", "8480-6")}));

    let exact = apply(&harness, "Patient/search", json!({"name:exact": "Bee"}));
    assert_eq!(exact["total"], 1);
    let loose = apply(&harness, "Patient/search", json!({"name:exact": "bee"}));
    assert_eq!(loose["total"], 0);
    let contains = apply(&harness, "Patient/search", json!({"name:contains": "ee"}));
    assert_eq!(contains["total"], 1);
    let missing = apply(&harness, "Patient/search", json!({"active:missing": "false"}));
    assert_eq!(missing["total"], 2);
    let negated = apply(&harness, "Patient/search", json!({"active:not": "true"}));
    assert_eq!(negated["total"], 1);
    assert_eq!(negated["entry"][0]["resource"]["id"], "p2");

    let later = apply(&harness, "Observation/search", json!({"date": "gt2019"}));
    assert_eq!(later["total"], 1);
    assert_eq!(later["entry"][0]["resource"]["id"], "o1");
    let earlier = apply(&harness, "Observation/search", json!({"date": "lt2019"}));
    assert_eq!(earlier["total"], 1);

    let page = apply(&harness, "Patient/search", json!({"_count": "1", "_sort": "name"}));
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
    let full_page = apply(&harness, "Patient/search", json!({"_count": "2", "_sort": "name"}));
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

    let chained = apply(&harness, "Observation/search", json!({"subject:Patient.name": "Bee"}));
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
    apply(&harness, "Patient/create", json!({"resource": patient("p1", true, "Ann")}));
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

    apply(&harness, "Patient/create", json!({"resource": patient("p2", true, "Ann")}));
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
                {"request": {"method": "GET", "url": "Patient/b1"}}
            ]
        }}),
    );
    assert_eq!(batch["type"], "batch-response");
    assert_eq!(batch["entry"][0]["response"]["status"], "201 Created");
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
    assert_eq!(failed["entry"][0]["response"]["outcome"]["issue"][0]["code"], "not-found");
    assert_eq!(failed["entry"][1]["response"]["status"], "409 Conflict");
    assert_eq!(failed["entry"][1]["response"]["outcome"]["issue"][0]["code"], "conflict");
    assert_eq!(failed["entry"][2]["response"]["status"], "412 Precondition Failed");
    assert_eq!(failed["entry"][2]["response"]["outcome"]["issue"][0]["code"], "conflict");
    let stored = apply(&harness, "Patient/read", json!({"id": "b1"}));
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
                        "id": "urn-obs",
                        "subject": {"reference": "urn:uuid:11111111-1111-1111-1111-111111111111"}
                    }
                }
            ]
        }}),
    );
    assert_eq!(urn["type"], "transaction-response");
    let obs = apply(&harness, "Observation/read", json!({"id": "urn-obs"}));
    assert!(obs["subject"]["reference"].as_str().unwrap().starts_with("Patient/"));
}

#[test]
fn meta_everything_and_instance_validate() {
    let harness = ready();
    apply(&harness, "Patient/create", json!({"resource": patient("p1", true, "Ann")}));
    apply(&harness, "Observation/create", json!({"resource": observation("o1", "p1", "2020-01-02", "8480-6")}));
    let added = apply(
        &harness,
        "Patient/$meta-add",
        json!({"id": "p1", "resource": {"meta": {"tag": [{"system": "http://example.org/tags", "code": "vip"}]}}}),
    );
    assert_eq!(added["parameter"][0]["valueMeta"]["tag"][0]["code"], "vip");
    let current = apply(&harness, "Patient/$meta", json!({"id": "p1"}));
    assert_eq!(current["parameter"][0]["valueMeta"]["tag"][0]["code"], "vip");
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
    assert!(matches!(document, RunError::Fhir(FhirError::Unsupported(_))));
}
