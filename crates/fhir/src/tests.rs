use std::sync::Arc;

use haifhir_database::Database;
use haifhir_sqlite::SqlitePersistence;
use haifhir_types::{token_interval, Order};
use serde_json::{json, Value};

use crate::catalog::ResourceType;
use crate::error::FhirError;
use crate::run::{run, RunError};

fn db() -> Database {
    Database::open(Arc::new(SqlitePersistence::open_in_memory().unwrap())).unwrap()
}

fn apply(db: &Database, path: &str, args: Value) -> Value {
    run(db, path, &args).expect(path).value
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
    assert!(ResourceType::parse("Encounter").is_err());
    let err = run(&db(), "Encounter/read", &json!({"id": "x"})).unwrap_err();
    assert!(matches!(err, RunError::Fhir(FhirError::UnknownResource(_))));
}

#[test]
fn patient_crud_history_and_search() {
    let db = db();
    let created = apply(
        &db,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    );
    assert_eq!(created["meta"]["versionId"], "1");
    assert_eq!(created["id"], "p1");

    let by_id = db
        .reader()
        .index_scan(
            &ResourceType::Patient.by_id(),
            db.latest_ts(),
            &haifhir_types::Interval::all(),
            Order::Asc,
        )
        .unwrap();
    assert_eq!(by_id.len(), 1);

    let read = apply(&db, "Patient/read", json!({"id": "p1"}));
    assert_eq!(read["meta"]["versionId"], "1");

    let missing = run(&db, "Patient/read", &json!({"id": "nope"})).unwrap_err();
    assert!(matches!(missing, RunError::Fhir(FhirError::NotFound(_))));

    let mut updated_body = patient("p1", true, "Ann");
    updated_body["birthDate"] = json!("1990-01-01");
    updated_body["meta"] = json!({"versionId": "9"});
    let wrong = run(
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
    let after = run(&db, "Patient/read", &json!({"id": "p1"})).unwrap_err();
    assert!(matches!(after, RunError::Fhir(FhirError::NotFound(_))));
    let history = apply(&db, "Patient/history", json!({"id": "p1"}));
    assert_eq!(history["entry"][0]["request"]["method"], "DELETE");
    assert!(history["entry"][0].get("resource").is_none());
    let gone = run(&db, "Patient/vread", &json!({"id": "p1", "versionId": "3"})).unwrap_err();
    assert!(matches!(gone, RunError::Fhir(FhirError::Gone(_))));
    let still = apply(&db, "Patient/vread", json!({"id": "p1", "versionId": "1"}));
    assert_eq!(still["meta"]["versionId"], "1");

    let hidden = db
        .reader()
        .index_scan(
            &ResourceType::Patient.by_id(),
            db.latest_ts(),
            &haifhir_types::Interval::all(),
            Order::Asc,
        )
        .unwrap();
    assert!(hidden.is_empty());
}

#[test]
fn search_conflicts_with_a_matching_update() {
    let db = db();
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

    let mut reader = db.begin();
    crate::dispatch(&mut reader, "Patient/search", &json!({"active": "true"})).unwrap();
    let mut p2 = patient("p2", false, "Bea");
    p2["birthDate"] = json!("1980-01-01");
    crate::dispatch(
        &mut reader,
        "Patient/update",
        &json!({"id": "p2", "resource": p2}),
    )
    .unwrap();

    let mut writer = db.begin();
    let mut p1 = patient("p1", true, "Ann");
    p1["birthDate"] = json!("1970-01-01");
    crate::dispatch(
        &mut writer,
        "Patient/update",
        &json!({"id": "p1", "resource": p1}),
    )
    .unwrap();
    db.commit_with(writer, crate::stamp_stored_json).unwrap();
    let err = db
        .commit_with(reader, crate::stamp_stored_json)
        .unwrap_err();
    assert!(err.is_occ(), "{err}");

    let _ = token_interval("true");
}

#[test]
fn observation_requires_a_patient_and_is_searchable() {
    let db = db();
    let missing = run(
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
    let absolute = run(
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
    let db = db();
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
    let err = run(&db, "Bundle/transaction", &json!({"resource": failed})).unwrap_err();
    assert!(matches!(err, RunError::Fhir(FhirError::Invalid(_))));
    let missing = run(&db, "Patient/read", &json!({"id": "p2"})).unwrap_err();
    assert!(matches!(missing, RunError::Fhir(FhirError::NotFound(_))));
}

#[test]
fn transaction_creates_a_referenced_patient_before_the_observation() {
    let db = db();
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
    let statement = crate::capability_statement();
    let resources = statement["rest"][0]["resource"].as_array().unwrap();
    let types: Vec<_> = resources
        .iter()
        .map(|resource| resource["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["Patient", "Observation"]);
    assert!(statement["rest"][0]["interaction"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["code"] == "transaction"));
}
