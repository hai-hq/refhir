//! A catalog that reads definitions and terminology when they are first used answers what a
//! catalog that holds all of them answers.
//!
//! The R4 definitions are the real thing: 658 definitions, thousands of value sets and code
//! systems, stored as a server stores them, and loaded both ways.

use std::sync::{Arc, RwLock};

use refhir_database::Database;
use refhir_fhir::{install_package_dir, r4_dir, Catalog, ConformanceStore};
use refhir_fhirpath::Host;
use refhir_sqlite::SqlitePersistence;
use refhir_validate::{validate_resource, Terminology};
use serde_json::{json, Value};

struct NoHost;
impl Host for NoHost {}

fn installed() -> SqlitePersistence {
    let store = SqlitePersistence::open_in_memory().unwrap();
    let db = Database::open(Arc::new(store.clone())).unwrap();
    let catalog = RwLock::new(Catalog::empty());
    install_package_dir(&store, &db, &catalog, r4_dir()).unwrap();
    store
}

fn samples() -> Vec<Value> {
    vec![
        json!({"resourceType": "Patient", "active": true, "gender": "female",
               "birthDate": "1980-05-01",
               "name": [{"use": "official", "family": "Okafor", "given": ["Ada"]}],
               "maritalStatus": {"coding": [{
                   "system": "http://terminology.hl7.org/CodeSystem/v3-MaritalStatus",
                   "code": "M"}]},
               "communication": [{"language": {"coding": [{
                   "system": "urn:ietf:bcp:47", "code": "en"}]}}]}),
        json!({"resourceType": "Patient", "gender": "nonsense", "birthDate": "1980-13-45",
               "name": [{"family": 3}],
               "maritalStatus": {"coding": [{
                   "system": "http://terminology.hl7.org/CodeSystem/v3-MaritalStatus",
                   "code": "ZZ"}]}}),
        json!({"resourceType": "Observation", "status": "final",
               "category": [{"coding": [{
                   "system": "http://terminology.hl7.org/CodeSystem/observation-category",
                   "code": "vital-signs"}]}],
               "code": {"coding": [{"system": "http://loinc.org", "code": "8480-6"}]},
               "subject": {"reference": "Patient/1"},
               "valueQuantity": {"value": 120, "unit": "mmHg",
                                 "system": "http://unitsofmeasure.org", "code": "mm[Hg]"}}),
        json!({"resourceType": "Encounter", "status": "wrong",
               "class": {"system": "http://terminology.hl7.org/CodeSystem/v3-ActCode",
                         "code": "AMB"}}),
        json!({"resourceType": "Questionnaire", "status": "active",
               "item": [{"linkId": "1", "type": "choice",
                         "answerValueSet": "http://hl7.org/fhir/ValueSet/administrative-gender"}]}),
        json!({"resourceType": "Bundle", "type": "collection", "entry": [
            {"resource": {"resourceType": "Patient", "gender": "male"}}]}),
    ]
}

#[test]
fn a_catalog_that_reads_when_used_answers_what_one_that_holds_everything_answers() {
    let store = installed();
    let whole = Catalog::open_whole(&store).unwrap();
    let lazy = Catalog::open(&store).unwrap();

    // The resource types, their references and search parameters.
    assert_eq!(
        format!("{:?}", whole.resource_models().collect::<Vec<_>>()),
        format!("{:?}", lazy.resource_models().collect::<Vec<_>>())
    );
    assert_eq!(whole.fhir_version(), lazy.fhir_version());

    // Every structure, by its URL, and every type by its name.
    let definitions = store.load_structure_definitions().unwrap();
    assert!(definitions.len() > 600);
    for row in &definitions {
        assert_eq!(
            format!("{:?}", whole.schema().structure(&row.url)),
            format!("{:?}", lazy.schema().structure(&row.url)),
            "{}",
            row.url
        );
        assert_eq!(
            format!("{:?}", whole.schema().base_type(&row.type_name)),
            format!("{:?}", lazy.schema().base_type(&row.type_name)),
            "{}",
            row.type_name
        );
    }

    // Every expansion, a code from it, and why the value sets that have none have none.
    let expansions = store.load_expansions().unwrap();
    assert!(expansions.len() > 1000);
    for row in &expansions {
        assert_eq!(
            whole.terms().expand(&row.url, None),
            lazy.terms().expand(&row.url, None),
            "{}",
            row.url
        );
        let document: Value = serde_json::from_str(&row.json).unwrap();
        if let Some(first) = document.pointer("/contains/0") {
            let system = first.get("system").and_then(Value::as_str);
            let code = first["code"].as_str().unwrap();
            assert_eq!(
                whole.terms().contains(&row.url, system, code),
                lazy.terms().contains(&row.url, system, code)
            );
            assert_eq!(
                whole
                    .terms()
                    .validate_code(Some(&row.url), system, None, code, None),
                lazy.terms()
                    .validate_code(Some(&row.url), system, None, code, None)
            );
            assert_eq!(
                format!("{:?}", whole.terms().describe_bound(&row.url, system, code)),
                format!("{:?}", lazy.terms().describe_bound(&row.url, system, code))
            );
            if let Some(system) = system {
                assert_eq!(
                    whole.terms().lookup(system, None, code),
                    lazy.terms().lookup(system, None, code)
                );
            }
        }
    }
    let unexpandable = |catalog: &Catalog| -> Vec<(String, Vec<String>)> {
        catalog
            .terms()
            .unexpandable()
            .into_iter()
            .map(|(url, failures)| (url, failures.iter().map(|f| f.message()).collect()))
            .collect()
    };
    assert!(!unexpandable(&whole).is_empty());
    assert_eq!(unexpandable(&whole), unexpandable(&lazy));
    assert_eq!(
        whole.terms().unexpandable_urls(),
        lazy.terms().unexpandable_urls()
    );

    // What a request is told about a resource.
    for sample in samples() {
        let held = validate_resource(&sample, whole.schema(), whole.terms(), &NoHost);
        let read = validate_resource(&sample, lazy.schema(), lazy.terms(), &NoHost);
        assert_eq!(
            held.operation_outcome(),
            read.operation_outcome(),
            "{sample}"
        );
    }
}
