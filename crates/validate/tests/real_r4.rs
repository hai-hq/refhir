//! Validation against the real R4 StructureDefinitions that `refhir-fhir` ships as fixtures.
//!
//! The unit tests in `src/` build tiny hand-written schemas. A snapshot copies every invariant
//! of an element's type into the element, and R4 uses `Resource` as a reference target, so
//! behaviour that depends on the shape of the real definitions is pinned here.

use refhir_fhirpath::Host;
use refhir_validate::{validate_resource, Issue, MemoryTerminology, Schema, Severity};
use serde_json::{json, Value};

struct NoHost;
impl Host for NoHost {}

const EXT: &str = "https://boolbyte.health/fhir/StructureDefinition/onboarding-json";

fn r4() -> Schema {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../fhir/fixtures/r4");
    let mut definitions: Vec<Value> = Vec::new();
    for entry in std::fs::read_dir(dir).expect("r4 fixtures") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name.starts_with("StructureDefinition-") {
            let text = std::fs::read_to_string(&path).unwrap();
            definitions.push(serde_json::from_str(&text).unwrap());
        }
    }
    Schema::from_definitions(&definitions)
}

fn errors(schema: &Schema, instance: &Value) -> Vec<Issue> {
    validate_resource(instance, schema, &MemoryTerminology::default(), &NoHost)
        .issues
        .into_iter()
        .filter(|issue| issue.severity == Severity::Error)
        .collect()
}

fn basic(extension: Value) -> Value {
    json!({"resourceType": "Basic", "code": {"text": "x"}, "extension": extension})
}

fn expressions(issues: &[Issue]) -> Vec<&str> {
    issues
        .iter()
        .map(|issue| issue.expression.as_str())
        .collect()
}

// R1 --------------------------------------------------------------------------------------

#[test]
fn generic_resource_targets_accept_concrete_types_but_specific_targets_do_not() {
    let schema = r4();
    let accepted = [
        // Basic.subject and CommunicationRequest.about are Reference(Resource).
        json!({"resourceType": "Basic", "code": {"text": "x"}, "subject": {"reference": "Organization/org-1"}}),
        json!({"resourceType": "CommunicationRequest", "status": "active", "about": [{"reference": "Appointment/a1"}]}),
        json!({"resourceType": "Observation", "status": "final", "code": {"text": "x"}, "subject": {"reference": "Patient/p1"}}),
        // A versioned reference is still a reference to a Patient.
        json!({"resourceType": "Observation", "status": "final", "code": {"text": "x"}, "subject": {"reference": "Patient/1/_history/2"}}),
    ];
    for instance in &accepted {
        let found = errors(&schema, instance);
        assert!(found.is_empty(), "{instance}: {found:?}");
    }
    let rejected = [
        json!({"resourceType": "Observation", "status": "final", "code": {"text": "x"}, "subject": {"reference": "Organization/o1"}}),
        // Nested element, so the check cannot be limited to top-level references.
        json!({"resourceType": "Encounter", "status": "planned", "class": {"code": "x"}, "participant": [{"individual": {"reference": "Organization/o1"}}]}),
    ];
    for instance in &rejected {
        let found = errors(&schema, instance);
        assert_eq!(found.len(), 1, "{instance}: {found:?}");
        assert!(found[0].diagnostics.contains("is not one of"), "{found:?}");
    }
}

fn observation_schema(subject_targets: &[&str], extra: &[Value]) -> Schema {
    let mut definitions = vec![json!({
        "resourceType": "StructureDefinition",
        "url": "http://hl7.org/fhir/StructureDefinition/Observation",
        "name": "Observation", "type": "Observation", "kind": "resource", "derivation": "specialization",
        "snapshot": {"element": [
            {"path": "Observation", "min": 0, "max": "*"},
            {"path": "Observation.subject", "min": 0, "max": "1", "type": [{"code": "Reference", "targetProfile": subject_targets}]}
        ]}
    })];
    definitions.extend(extra.iter().cloned());
    Schema::from_definitions(&definitions)
}

#[test]
fn a_profile_target_constrains_its_base_type_not_its_id() {
    let target = "http://example.org/fhir/StructureDefinition/my-patient";
    let profile = json!({
        "resourceType": "StructureDefinition",
        "url": target, "name": "MyPatient", "type": "Patient", "kind": "resource", "derivation": "constraint",
        "snapshot": {"element": [{"path": "Patient", "min": 0, "max": "*"}]}
    });
    // Installed: the target is a Patient, whatever the profile is called.
    let installed = observation_schema(&[target], &[profile]);
    let patient = json!({"resourceType": "Observation", "subject": {"reference": "Patient/1"}});
    assert!(errors(&installed, &patient).is_empty());
    let organization =
        json!({"resourceType": "Observation", "subject": {"reference": "Organization/1"}});
    let found = errors(&installed, &organization);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].diagnostics.contains("Patient"), "{found:?}");
    // Not installed: the type is unknown, so nothing is rejected on a guess from the id.
    let missing = observation_schema(&[target], &[]);
    assert!(errors(&missing, &patient).is_empty());
    assert!(errors(&missing, &organization).is_empty());
}

// R2 --------------------------------------------------------------------------------------

#[test]
fn extension_needs_a_value_or_nested_extensions_but_not_both() {
    let schema = r4();
    let valid = [
        basic(json!([{"url": EXT, "valueString": "{}"}])),
        basic(json!([{"url": EXT, "extension": [{"url": "a", "valueString": "x"}]}])),
        // A falsy value is still a value.
        basic(json!([{"url": EXT, "valueBoolean": false}])),
        basic(json!([{"url": EXT, "valueInteger": 0}])),
        basic(json!([{"url": EXT, "valueCodeableConcept": {"text": "x"}}])),
        basic(json!([{"url": EXT, "valueReference": {"reference": "Organization/o"}}])),
        json!({"resourceType": "Basic", "code": {"text": "x"}, "modifierExtension": [{"url": EXT, "valueString": "x"}]}),
        json!({"resourceType": "Basic", "code": {"text": "x", "extension": [{"url": EXT, "valueString": "x"}]}}),
        json!({"resourceType": "Patient", "birthDate": "2000-01-01", "_birthDate": {"extension": [{"url": EXT, "valueString": "x"}]}}),
        json!({"resourceType": "Basic", "code": {"text": "x"}, "subject": {"reference": "#c"},
               "contained": [{"resourceType": "Basic", "id": "c", "code": {"text": "y"}, "extension": [{"url": EXT, "valueString": "x"}]}]}),
    ];
    for instance in &valid {
        let found = errors(&schema, instance);
        assert!(found.is_empty(), "{instance}: {found:?}");
    }
    let neither = basic(json!([{"url": EXT}]));
    let both = basic(
        json!([{"url": EXT, "valueString": "x", "extension": [{"url": "a", "valueString": "y"}]}]),
    );
    for instance in [neither, both] {
        let found = errors(&schema, &instance);
        assert_eq!(found.len(), 1, "{instance}: {found:?}");
        assert!(found[0].diagnostics.starts_with("ext-1:"), "{found:?}");
    }
}

#[test]
fn extension_failures_name_the_node_that_failed() {
    let schema = r4();
    let cases = [
        // The second of two extensions is the bad one.
        (
            basic(json!([{"url": EXT, "valueString": "x"}, {"url": EXT}])),
            "Basic.extension[1]",
        ),
        // A nested extension, two levels down.
        (
            basic(json!([{"url": EXT, "extension": [{"url": "child"}]}])),
            "Basic.extension[0].extension[0]",
        ),
        // An extension on a datatype inside the resource.
        (
            json!({"resourceType": "Basic", "code": {"text": "x", "extension": [{"url": EXT}]}}),
            "Basic.code.extension[0]",
        ),
        // An extension on a primitive.
        (
            json!({"resourceType": "Patient", "birthDate": "2000-01-01", "_birthDate": {"extension": [{"url": EXT}]}}),
            "Patient.birthDate.extension[0]",
        ),
        // An extension inside a contained resource.
        (
            json!({"resourceType": "Basic", "code": {"text": "x"}, "subject": {"reference": "#c"},
                "contained": [{"resourceType": "Basic", "id": "c", "code": {"text": "y"}, "extension": [{"url": EXT}]}]}),
            "Basic.contained[0].extension[0]",
        ),
    ];
    for (instance, expected) in cases {
        let found = errors(&schema, &instance);
        // One issue per failure: the invariant lives on the element and on its datatype, and is
        // evaluated once.
        assert_eq!(expressions(&found), vec![expected], "{instance}: {found:?}");
    }
}

#[test]
fn missing_and_misplaced_elements_are_reported_at_their_indexed_location() {
    let schema = r4();
    let instance = json!({
        "resourceType": "Patient",
        "name": [{"family": "A"}, {"family": "B", "nope": 1}],
        "contact": [{"gender": "female"}, {"relationship": [{"text": "x"}], "extra": true}]
    });
    let found = errors(&schema, &instance);
    assert!(
        expressions(&found).contains(&"Patient.name[1].nope"),
        "{found:?}"
    );
    assert!(
        expressions(&found).contains(&"Patient.contact[1].extra"),
        "{found:?}"
    );
}
