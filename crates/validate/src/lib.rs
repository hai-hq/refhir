mod outcome;
mod primitive;
mod schema;
mod slice;
mod terminology;
mod walk;

pub use outcome::{Issue, Outcome, Severity};
pub use schema::Schema;
pub use terminology::{MemoryTerminology, TermAnswer, Terminology};
pub use walk::validate;

use refhir_fhirpath::Host;

pub fn validate_resource(
    instance: &serde_json::Value,
    schema: &Schema,
    terms: &dyn Terminology,
    host: &dyn Host,
) -> Outcome {
    validate(instance, &[], schema, terms, host)
}

#[cfg(test)]
mod tests {
    use refhir_fhirpath::Host;
    use serde_json::json;

    use super::*;

    struct NoHost;
    impl Host for NoHost {}

    fn patient_schema() -> Schema {
        Schema::from_definitions(&[json!({
            "resourceType": "StructureDefinition",
            "url": "http://hl7.org/fhir/StructureDefinition/Patient",
            "name": "Patient",
            "type": "Patient",
            "kind": "resource",
            "derivation": "specialization",
            "snapshot": { "element": [
                {"path": "Patient", "min": 0, "max": "*"},
                {"path": "Patient.active", "min": 0, "max": "1", "type": [{"code": "boolean"}]},
                {"path": "Patient.birthDate", "min": 0, "max": "1", "type": [{"code": "date"}]},
                {"path": "Patient.gender", "min": 0, "max": "1", "type": [{"code": "code"}], "binding": {"strength": "required", "valueSet": "http://hl7.org/fhir/ValueSet/administrative-gender"}},
                {"path": "Patient.deceased[x]", "min": 0, "max": "1", "type": [{"code": "boolean"}, {"code": "dateTime"}]},
                {"path": "Patient.contact", "min": 0, "max": "*", "type": [{"code": "BackboneElement"}], "constraint": [{"key": "pat-1", "severity": "error", "human": "contact needs a name or organization", "expression": "name.exists() or organization.exists()"}]},
                {"path": "Patient.contact.name", "min": 0, "max": "1", "type": [{"code": "HumanName"}]},
                {"path": "Patient.contact.organization", "min": 0, "max": "1", "type": [{"code": "Reference", "targetProfile": ["http://hl7.org/fhir/StructureDefinition/Organization"]}]},
                {"path": "Patient.generalPractitioner", "min": 1, "max": "1", "type": [{"code": "Reference", "targetProfile": ["http://hl7.org/fhir/StructureDefinition/Organization"]}]}
            ]}
        })])
    }

    fn gender_terms() -> MemoryTerminology {
        MemoryTerminology::from_resources(&[json!({
            "resourceType": "ValueSet",
            "url": "http://hl7.org/fhir/ValueSet/administrative-gender",
            "compose": {"include": [{"system": "http://hl7.org/fhir/administrative-gender", "concept": [{"code": "male"}, {"code": "female"}, {"code": "other"}, {"code": "unknown"}]}]}
        })])
    }

    fn check(instance: serde_json::Value, terms: &MemoryTerminology) -> Outcome {
        validate_resource(&instance, &patient_schema(), terms, &NoHost)
    }

    #[test]
    fn rejects_bad_primitive_choice_and_unknown_element() {
        let outcome = check(json!({
            "resourceType": "Patient",
            "birthDate": "yesterday",
            "deceasedBoolean": true,
            "deceasedDateTime": "2020-01-01T00:00:00Z",
            "notAField": 1,
            "generalPractitioner": {"reference": "Organization/1"}
        }), &MemoryTerminology::default());
        let messages: Vec<_> = outcome.issues.iter().map(|issue| issue.diagnostics.clone()).collect();
        assert!(messages.iter().any(|message| message.contains("invalid date")), "{messages:?}");
        assert!(messages.iter().any(|message| message.contains("choice")), "{messages:?}");
        assert!(messages.iter().any(|message| message.contains("unknown element")), "{messages:?}");
        assert!(outcome.has_errors());
    }

    #[test]
    fn enforces_cardinality_reference_and_contact_invariant() {
        let outcome = check(json!({
            "resourceType": "Patient",
            "contact": [{}],
            "generalPractitioner": {"reference": "Patient/1"}
        }), &MemoryTerminology::default());
        let messages: Vec<_> = outcome.issues.iter().map(|issue| issue.diagnostics.clone()).collect();
        assert!(messages.iter().any(|message| message.contains("pat-1")), "{messages:?}");
        assert!(messages.iter().any(|message| message.contains("generalPractitioner")), "{messages:?}");
        assert!(outcome.has_errors());
    }

    #[test]
    fn required_binding_accepts_known_code_and_rejects_unknown() {
        let terms = gender_terms();
        let bad = check(json!({
            "resourceType": "Patient",
            "gender": "otherish",
            "generalPractitioner": {"reference": "Organization/1"}
        }), &terms);
        assert!(bad.issues.iter().any(|issue| issue.code == "code-invalid"));
        let good = check(json!({
            "resourceType": "Patient",
            "gender": "female",
            "generalPractitioner": {"reference": "Organization/1"}
        }), &terms);
        assert!(!good.has_errors(), "{:?}", good.issues);
    }

    #[test]
    fn missing_value_set_warns_without_rejecting_the_instance() {
        let outcome = check(
            json!({
                "resourceType": "Patient",
                "gender": "female",
                "generalPractitioner": {"reference": "Organization/1"}
            }),
            &MemoryTerminology::default(),
        );
        assert!(!outcome.has_errors(), "{:?}", outcome.issues);
        assert!(outcome.issues.iter().any(|issue| {
            issue.severity == Severity::Warning && issue.diagnostics.contains("administrative-gender")
        }));
    }

    #[test]
    fn closed_slice_rejects_unmatched_repetition() {
        let schema = Schema::from_definitions(&[json!({
            "resourceType": "StructureDefinition",
            "url": "http://example.org/StructureDefinition/Obs",
            "name": "Obs",
            "type": "Observation",
            "kind": "resource",
            "derivation": "specialization",
            "snapshot": { "element": [
                {"path": "Observation", "min": 0, "max": "*"},
                {"path": "Observation.component", "min": 0, "max": "*", "type": [{"code": "BackboneElement"}], "slicing": {"discriminator": [{"type": "value", "path": "code"}], "rules": "closed"}},
                {"path": "Observation.component", "id": "Observation.component:sys", "sliceName": "sys", "min": 0, "max": "1", "type": [{"code": "BackboneElement"}]},
                {"path": "Observation.component.code", "id": "Observation.component:sys.code", "min": 1, "max": "1", "fixedCode": "sys"}
            ]}
        })]);
        let outcome = validate_resource(&json!({
            "resourceType": "Observation",
            "component": [{"code": "other"}]
        }), &schema, &MemoryTerminology::default(), &NoHost);
        assert!(outcome.issues.iter().any(|issue| issue.diagnostics.contains("closed slice")), "{:?}", outcome.issues);

        let matched = validate_resource(&json!({
            "resourceType": "Observation",
            "component": [{"code": "sys"}]
        }), &schema, &MemoryTerminology::default(), &NoHost);
        assert!(!matched.issues.iter().any(|issue| issue.diagnostics.contains("closed slice")), "{:?}", matched.issues);
    }

    #[test]
    fn backbone_and_content_reference_validate_nested_children() {
        let schema = Schema::from_definitions(&[
            json!({
                "resourceType": "StructureDefinition",
                "url": "http://hl7.org/fhir/StructureDefinition/BackboneElement",
                "name": "BackboneElement",
                "type": "BackboneElement",
                "kind": "complex-type",
                "derivation": "specialization",
                "snapshot": { "element": [
                    {"path": "BackboneElement", "min": 0, "max": "*"},
                    {"path": "BackboneElement.modifierExtension", "min": 0, "max": "*", "type": [{"code": "Extension"}]}
                ]}
            }),
            json!({
                "resourceType": "StructureDefinition",
                "url": "http://hl7.org/fhir/StructureDefinition/Questionnaire",
                "name": "Questionnaire",
                "type": "Questionnaire",
                "kind": "resource",
                "derivation": "specialization",
                "snapshot": { "element": [
                    {"path": "Questionnaire", "min": 0, "max": "*"},
                    {"path": "Questionnaire.item", "min": 0, "max": "*", "type": [{"code": "BackboneElement"}], "constraint": [{"key": "que-1", "severity": "error", "human": "item needs a linkId", "expression": "linkId.exists()"}]},
                    {"path": "Questionnaire.item.linkId", "min": 1, "max": "1", "type": [{"code": "string"}]},
                    {"path": "Questionnaire.item.item", "min": 0, "max": "*", "contentReference": "#Questionnaire.item", "constraint": [{"key": "ele-1", "severity": "error", "human": "element", "expression": "true"}]}
                ]}
            }),
        ]);
        let missing = validate_resource(
            &json!({
                "resourceType": "Questionnaire",
                "item": [{"item": [{}]}]
            }),
            &schema,
            &MemoryTerminology::default(),
            &NoHost,
        );
        let messages: Vec<_> = missing.issues.iter().map(|issue| issue.diagnostics.clone()).collect();
        assert!(messages.iter().any(|message| message.contains("linkId")), "{messages:?}");
        assert!(messages.iter().any(|message| message.contains("que-1")), "{messages:?}");
        assert!(!messages.iter().any(|message| message.contains("unknown element")), "{messages:?}");

        let ok = validate_resource(
            &json!({
                "resourceType": "Questionnaire",
                "item": [{"linkId": "1", "item": [{"linkId": "1.1"}]}]
            }),
            &schema,
            &MemoryTerminology::default(),
            &NoHost,
        );
        assert!(!ok.has_errors(), "{:?}", ok.issues);
    }

    #[test]
    fn primitive_null_with_extension_satisfies_element_invariant() {
        let schema = Schema::from_definitions(&[json!({
            "resourceType": "StructureDefinition",
            "url": "http://hl7.org/fhir/StructureDefinition/Patient",
            "name": "Patient",
            "type": "Patient",
            "kind": "resource",
            "derivation": "specialization",
            "snapshot": { "element": [
                {"path": "Patient", "min": 0, "max": "*"},
                {"path": "Patient.name", "min": 0, "max": "*", "type": [{"code": "HumanName"}]},
                {"path": "Patient.name.given", "min": 0, "max": "*", "type": [{"code": "string"}], "constraint": [{"key": "ele-1", "severity": "error", "human": "element must have a value or children", "expression": "hasValue() or (children().count() > id.count())"}]}
            ]}
        })]);
        let outcome = validate_resource(
            &json!({
                "resourceType": "Patient",
                "name": [{
                    "given": [null, "James"],
                    "_given": [{"extension": [{"url": "https://example.org/syllable-count", "valueString": "five"}]}]
                }]
            }),
            &schema,
            &MemoryTerminology::default(),
            &NoHost,
        );
        assert!(!outcome.has_errors(), "{:?}", outcome.issues);
    }

    #[test]
    fn r4_patient_passes_base_structure_validation() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fhir/fixtures/r4");
        let mut definitions = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
            if !name.starts_with("StructureDefinition-") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            definitions.push(serde_json::from_str(&text).unwrap());
        }
        let schema = Schema::from_definitions(&definitions);
        let outcome = validate_resource(
            &json!({
                "resourceType": "Patient",
                "name": [{"family": "Ann", "given": ["Ann"]}]
            }),
            &schema,
            &MemoryTerminology::default(),
            &NoHost,
        );
        let errors: Vec<_> = outcome.issues.iter().filter(|issue| issue.severity.fails()).collect();
        assert!(errors.is_empty(), "{errors:#?}");

        let terms = MemoryTerminology::from_resources(&[json!({
            "resourceType": "ValueSet",
            "url": "http://hl7.org/fhir/ValueSet/name-use",
            "compose": {"include": [{"system": "http://hl7.org/fhir/name-use", "concept": [
                {"code": "usual"}, {"code": "official"}, {"code": "temp"}, {"code": "nickname"},
                {"code": "anonymous"}, {"code": "old"}, {"code": "maiden"}
            ]}]}
        })]);
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../fhirpath/tests/hl7/input/patient-name-extensions.json");
        let instance: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let outcome = validate_resource(&instance, &schema, &terms, &NoHost);
        let errors: Vec<_> = outcome.issues.iter().filter(|issue| issue.severity.fails()).collect();
        assert!(errors.is_empty(), "{errors:#?}");

        let contained = validate_resource(
            &json!({
                "resourceType": "Patient",
                "contained": [{"resourceType": "Organization", "id": "1", "name": "Gastro"}],
                "managingOrganization": {"reference": "#1"}
            }),
            &schema,
            &MemoryTerminology::default(),
            &NoHost,
        );
        let errors: Vec<_> = contained.issues.iter().filter(|issue| issue.severity.fails()).collect();
        assert!(errors.is_empty(), "{errors:#?}");

        let unmatched = validate_resource(
            &json!({
                "resourceType": "Patient",
                "contained": [{"resourceType": "Organization", "id": "1", "name": "Gastro"}]
            }),
            &schema,
            &MemoryTerminology::default(),
            &NoHost,
        );
        assert!(
            unmatched.issues.iter().any(|issue| issue.diagnostics.contains("dom-3")),
            "{:#?}",
            unmatched.issues
        );
    }
}
