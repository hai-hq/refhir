//! Structured Data Capture 4.0.0 operations.
//!
//! Handlers borrow a [`Chart`] for the duration of one transaction. The FHIR
//! server supplies that chart from the open `Transaction`. Nothing here opens
//! a database or keeps a SQL transaction.

mod adaptive;
mod assemble;
mod behavior;
mod chart;
mod evalexpr;
mod ext;
mod extract;
mod items;
mod populate;
mod validate;

use serde_json::{json, Value as Json};

pub use adaptive::next_question;
pub use assemble::assemble;
pub use behavior::apply_behavior;
pub use chart::{scope_allows, Chart, MemoryChart, NullChart};
pub use evalexpr::find_canonical;
pub use extract::extract;
pub use populate::populate;
pub use ext::client_extensions;
pub use validate::{has_errors, validate_response};

/// Drop disabled answers and validate a completed response.
/// `Ok` is the response to store. `Err` is an OperationOutcome.
pub fn prepare_completed(
    chart: &mut dyn Chart,
    response: &Json,
) -> Result<Json, Json> {
    if response.get("status").and_then(|value| value.as_str()) != Some("completed") {
        return Ok(response.clone());
    }
    let questionnaire = match questionnaire_of(chart, response) {
        Ok(value) => value,
        Err(message) => return Err(invalid_outcome(&message)),
    };
    let view = apply_behavior(chart, &questionnaire, response).map_err(|err| invalid_outcome(&err))?;
    let enabled = behavior::enabled_map(&view);
    let mut cleaned = response.clone();
    for (link_id, on) in &enabled {
        if !on {
            items::strip_link(&mut cleaned, link_id);
        }
    }
    let outcome = validate_response(chart, &questionnaire, &cleaned).map_err(|err| invalid_outcome(&err))?;
    if has_errors(&outcome) {
        return Err(outcome);
    }
    Ok(cleaned)
}

pub fn behavior_document(
    chart: &mut dyn Chart,
    questionnaire: &Json,
    response: &Json,
) -> Result<Json, String> {
    apply_behavior(chart, questionnaire, response)
}

fn questionnaire_of(chart: &mut dyn Chart, response: &Json) -> Result<Json, String> {
    let canonical = response
        .get("questionnaire")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "QuestionnaireResponse.questionnaire is required".to_string())?;
    if let Some(id) = canonical.strip_prefix('#') {
        return response
            .get("contained")
            .and_then(|value| value.as_array())
            .and_then(|items| {
                items.iter().find(|item| item.get("id").and_then(|value| value.as_str()) == Some(id))
            })
            .cloned()
            .ok_or_else(|| format!("contained questionnaire {canonical} was not found"));
    }
    evalexpr::find_canonical(chart, "Questionnaire", canonical)?
        .ok_or_else(|| format!("questionnaire {canonical} was not found"))
}

fn invalid_outcome(message: &str) -> Json {
    json!({
        "resourceType": "OperationOutcome",
        "issue": [{
            "severity": "error",
            "code": "invalid",
            "diagnostics": message
        }]
    })
}

/// Declare SDC roles, operations, and expression languages on a CapabilityStatement.
pub fn annotate_capability(statement: &mut Json) {
    statement["instantiates"] = json!([
        "http://hl7.org/fhir/uv/sdc/CapabilityStatement/sdc-form-fill-manager",
        "http://hl7.org/fhir/uv/sdc/CapabilityStatement/sdc-form-manager",
        "http://hl7.org/fhir/uv/sdc/CapabilityStatement/sdc-form-response-manager",
        "http://hl7.org/fhir/uv/sdc/CapabilityStatement/sdc-form-receiver",
        "http://hl7.org/fhir/uv/sdc/CapabilityStatement/sdc-form-archiver"
    ]);
    let Some(resources) = statement
        .pointer_mut("/rest/0/resource")
        .and_then(|value| value.as_array_mut())
    else {
        return;
    };
    for resource in resources {
        let type_name = resource.get("type").and_then(|value| value.as_str()).unwrap_or("");
        match type_name {
            "Questionnaire" => {
                resource["operation"] = json!([
                    {"name": "assemble", "definition": "http://hl7.org/fhir/uv/sdc/OperationDefinition/Questionnaire-assemble"},
                    {"name": "populate", "definition": "http://hl7.org/fhir/uv/sdc/OperationDefinition/Questionnaire-populate"},
                    {"name": "next-question", "definition": "http://hl7.org/fhir/uv/sdc/OperationDefinition/Questionnaire-next-question"},
                    {"name": "behavior", "definition": "http://hl7.org/fhir/uv/sdc/OperationDefinition/Questionnaire-behavior"}
                ]);
                add_languages(resource);
            }
            "QuestionnaireResponse" => {
                resource["operation"] = json!([
                    {"name": "extract", "definition": "http://hl7.org/fhir/uv/sdc/OperationDefinition/QuestionnaireResponse-extract"}
                ]);
            }
            "Library" => add_languages(resource),
            _ => {}
        }
    }
}

fn add_languages(resource: &mut Json) {
    let languages = ["text/fhirpath", "text/cql-expression", "text/cql-identifier"];
    let contexts = [
        "behaviorExpressionLanguage",
        "populationExpressionLanguage",
        "extractionExpressionLanguage",
    ];
    let mut extensions = Vec::new();
    for context in contexts {
        for language in languages {
            extensions.push(json!({
                "url": "http://hl7.org/fhir/uv/application-feature/StructureDefinition/feature",
                "extension": [
                    {
                        "url": "code",
                        "valueCodeableConcept": {
                            "coding": [{
                                "system": "http://hl7.org/fhir/uv/sdc/CodeSystem/temp",
                                "code": context
                            }]
                        }
                    },
                    {
                        "url": "value",
                        "valueCodeableConcept": {
                            "coding": [{
                                "system": "urn:ietf:bcp:13",
                                "code": language
                            }]
                        }
                    }
                ]
            }));
        }
    }
    resource["extension"] = json!(extensions);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn patient() -> Json {
        json!({"resourceType": "Patient", "id": "p1", "birthDate": "1980-01-02", "gender": "female"})
    }

    #[test]
    fn enable_when_and_calculated_expression() {
        let questionnaire = json!({
            "resourceType": "Questionnaire",
            "status": "active",
            "item": [
                {"linkId": "gate", "type": "boolean", "text": "Continue"},
                {
                    "linkId": "detail",
                    "type": "string",
                    "text": "Detail",
                    "enableWhen": [{"question": "gate", "operator": "=", "answerBoolean": true}]
                },
                {
                    "linkId": "double",
                    "type": "integer",
                    "text": "Double",
                    "extension": [{
                        "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-calculatedExpression",
                        "valueExpression": {"language": "text/fhirpath", "expression": "1 + 2"}
                    }]
                }
            ]
        });
        let response = json!({
            "resourceType": "QuestionnaireResponse",
            "status": "in-progress",
            "item": [{"linkId": "gate", "answer": [{"valueBoolean": true}]}]
        });
        let view = apply_behavior(&mut NullChart, &questionnaire, &response).unwrap();
        assert_eq!(view["enabled"]["detail"], true);
        let double = view["response"]["item"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["linkId"] == "double")
            .unwrap();
        assert_eq!(double["answer"][0]["valueInteger"], 3);
        let hidden = json!({
            "resourceType": "QuestionnaireResponse",
            "status": "in-progress",
            "item": [{"linkId": "gate", "answer": [{"valueBoolean": false}]}]
        });
        let view = apply_behavior(&mut NullChart, &questionnaire, &hidden).unwrap();
        assert_eq!(view["enabled"]["detail"], false);
    }

    #[test]
    fn completed_response_requires_enabled_answers() {
        let questionnaire = json!({
            "resourceType": "Questionnaire",
            "url": "http://example.org/Questionnaire/required",
            "status": "active",
            "item": [{"linkId": "name", "type": "string", "required": true, "text": "Name"}]
        });
        let mut chart = MemoryChart::new(vec![questionnaire]);
        let missing = json!({
            "resourceType": "QuestionnaireResponse",
            "questionnaire": "http://example.org/Questionnaire/required",
            "status": "completed"
        });
        let err = prepare_completed(&mut chart, &missing).unwrap_err();
        assert_eq!(err["issue"][0]["severity"], "error");
        let present = json!({
            "resourceType": "QuestionnaireResponse",
            "questionnaire": "http://example.org/Questionnaire/required",
            "status": "completed",
            "item": [{"linkId": "name", "answer": [{"valueString": "Ada"}]}]
        });
        assert!(prepare_completed(&mut chart, &present).is_ok());
    }

    #[test]
    fn assembles_a_subquestionnaire_and_prefixes_link_ids() {
        let child = json!({
            "resourceType": "Questionnaire",
            "id": "child",
            "url": "http://example.org/Questionnaire/child",
            "status": "active",
            "item": [{"linkId": "city", "type": "string", "text": "City"}]
        });
        let parent = json!({
            "resourceType": "Questionnaire",
            "url": "http://example.org/Questionnaire/parent",
            "status": "active",
            "item": [{
                "linkId": "slot",
                "type": "display",
                "text": "Module",
                "extension": [
                    {"url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-subQuestionnaire", "valueCanonical": "http://example.org/Questionnaire/child"},
                    {"url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-linkIdPrefix", "valueString": "addr."}
                ]
            }]
        });
        let assembled = assemble(&mut MemoryChart::new(vec![child]), &parent).unwrap();
        assert_eq!(assembled["item"][0]["linkId"], "addr.city");
        assert_eq!(
            assembled["extension"][0]["valueCanonical"],
            "http://example.org/Questionnaire/child"
        );
    }

    #[test]
    fn populates_an_observation_and_records_provenance() {
        let questionnaire = json!({
            "resourceType": "Questionnaire",
            "url": "http://example.org/Questionnaire/weight",
            "status": "active",
            "item": [{
                "linkId": "weight",
                "type": "decimal",
                "text": "Weight",
                "code": [{"system": "http://loinc.org", "code": "29463-7"}],
                "extension": [{
                    "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-observationLinkPeriod",
                    "valueDuration": {"value": 1, "unit": "year", "system": "http://unitsofmeasure.org", "code": "a"}
                }]
            }]
        });
        let observation = json!({
            "resourceType": "Observation",
            "id": "w1",
            "status": "final",
            "code": {"coding": [{"system": "http://loinc.org", "code": "29463-7"}]},
            "subject": {"reference": "Patient/p1"},
            "effectiveDateTime": "2099-01-01",
            "valueQuantity": {"value": 70, "unit": "kg", "system": "http://unitsofmeasure.org", "code": "kg"}
        });
        let response = populate(
            &mut MemoryChart::new(vec![observation]),
            &questionnaire,
            &json!({"subject": "Patient/p1"}),
        )
        .unwrap();
        assert_eq!(response["item"][0]["answer"][0]["valueDecimal"], 70.0);
        assert_eq!(response["contained"][0]["entity"][0]["role"], "source");
        assert_eq!(response["contained"][0]["entity"][0]["what"]["reference"], "Observation/w1");
    }

    #[test]
    fn populate_hides_observations_the_caller_cannot_read() {
        let questionnaire = json!({
            "resourceType": "Questionnaire",
            "status": "active",
            "item": [{
                "linkId": "weight",
                "type": "decimal",
                "code": [{"code": "29463-7"}],
                "extension": [{
                    "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-observationLinkPeriod",
                    "valueDuration": {"value": 200, "code": "a"}
                }]
            }]
        });
        let observation = json!({
            "resourceType": "Observation",
            "id": "w1",
            "code": {"coding": [{"code": "29463-7"}]},
            "subject": {"reference": "Patient/p1"},
            "effectiveDateTime": "2099-01-01",
            "valueQuantity": {"value": 70}
        });
        let mut chart = MemoryChart::new(vec![observation]);
        chart.scopes = Some(vec!["user/Patient.read".into()]);
        let response = populate(&mut chart, &questionnaire, &json!({"subject": "Patient/p1"})).unwrap();
        assert!(response.get("item").is_none());
    }

    #[test]
    fn extracts_a_patient_name_from_a_definition() {
        let questionnaire = json!({
            "resourceType": "Questionnaire",
            "url": "http://example.org/Questionnaire/demo",
            "status": "active",
            "extension": [{
                "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-definitionExtract",
                "extension": [{"url": "definition", "valueCanonical": "http://hl7.org/fhir/StructureDefinition/Patient"}]
            }],
            "item": [{
                "linkId": "family",
                "type": "string",
                "definition": "http://hl7.org/fhir/StructureDefinition/Patient#Patient.name.family"
            }]
        });
        let response = json!({
            "resourceType": "QuestionnaireResponse",
            "questionnaire": "http://example.org/Questionnaire/demo",
            "status": "completed",
            "item": [{"linkId": "family", "answer": [{"valueString": "Lovelace"}]}]
        });
        let bundle = extract(&mut MemoryChart::new(vec![questionnaire]), &response).unwrap();
        assert_eq!(bundle["type"], "transaction");
        assert_eq!(bundle["entry"][0]["resource"]["name"]["family"], "Lovelace");
        assert_eq!(bundle["entry"][0]["request"]["method"], "POST");
    }

    #[test]
    fn structure_map_population_ignores_observation_items() {
        let map = json!({
            "resourceType": "StructureMap",
            "id": "pop",
            "url": "http://example.org/StructureMap/pop",
            "status": "active",
            "group": [{
                "name": "main",
                "input": [
                    {"name": "src", "mode": "source"},
                    {"name": "out", "type": "QuestionnaireResponse", "mode": "target"}
                ],
                "rule": [{
                    "name": "status",
                    "source": [{"context": "src", "variable": "s"}],
                    "target": [{
                        "context": "out",
                        "element": "status",
                        "transform": "copy",
                        "parameter": [{"valueString": "in-progress"}]
                    }]
                }]
            }]
        });
        let questionnaire = json!({
            "resourceType": "Questionnaire",
            "status": "active",
            "contained": [map],
            "extension": [{
                "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-sourceStructureMap",
                "valueCanonical": "#pop"
            }],
            "item": [{
                "linkId": "weight",
                "type": "decimal",
                "code": [{"code": "29463-7"}],
                "extension": [{
                    "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-observationLinkPeriod",
                    "valueDuration": {"value": 1, "code": "a"}
                }]
            }]
        });
        let observation = json!({
            "resourceType": "Observation",
            "id": "w1",
            "code": {"coding": [{"code": "29463-7"}]},
            "subject": {"reference": "Patient/p1"},
            "effectiveDateTime": "2099-01-01",
            "valueQuantity": {"value": 70}
        });
        let response = populate(
            &mut MemoryChart::new(vec![observation]),
            &questionnaire,
            &json!({"subject": "Patient/p1"}),
        )
        .unwrap();
        assert!(response.get("item").is_none());
        assert_eq!(response["status"], "in-progress");
    }

    #[test]
    fn adaptive_form_reveals_one_item_at_a_time() {
        let bank = json!({
            "resourceType": "Questionnaire",
            "id": "bank",
            "url": "http://example.org/Questionnaire/phq",
            "status": "active",
            "extension": [{
                "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-questionnaireAdaptive",
                "valueBoolean": true
            }],
            "item": [
                {"linkId": "q1", "type": "string", "text": "One"},
                {"linkId": "q2", "type": "string", "text": "Two"}
            ]
        });
        let mut chart = MemoryChart::new(vec![bank]);
        let start = json!({
            "resourceType": "QuestionnaireResponse",
            "questionnaire": "http://example.org/Questionnaire/phq",
            "status": "in-progress"
        });
        let first = next_question(&mut chart, &start).unwrap();
        assert_eq!(first["contained"][0]["item"][0]["linkId"], "q1");
        assert_eq!(first["status"], "in-progress");
        let mut second_input = first.clone();
        second_input["item"] = json!([{"linkId": "q1", "answer": [{"valueString": "a"}]}]);
        let second = next_question(&mut chart, &second_input).unwrap();
        assert_eq!(second["contained"][0]["item"][1]["linkId"], "q2");
        let mut done = second.clone();
        done["item"] = json!([
            {"linkId": "q1", "answer": [{"valueString": "a"}]},
            {"linkId": "q2", "answer": [{"valueString": "b"}]}
        ]);
        let finished = next_question(&mut chart, &done).unwrap();
        assert_eq!(finished["status"], "completed");
    }

    #[test]
    fn expression_population_uses_launch_context() {
        let questionnaire = json!({
            "resourceType": "Questionnaire",
            "status": "active",
            "extension": [{
                "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-launchContext",
                "extension": [
                    {"url": "name", "valueCoding": {"code": "patient"}},
                    {"url": "type", "valueCode": "Patient"}
                ]
            }],
            "item": [{
                "linkId": "born",
                "type": "date",
                "extension": [{
                    "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-initialExpression",
                    "valueExpression": {"language": "text/fhirpath", "expression": "%patient.birthDate"}
                }]
            }]
        });
        let response = populate(
            &mut MemoryChart::new(vec![patient()]),
            &questionnaire,
            &json!({"subject": "Patient/p1", "content": [patient()]}),
        )
        .unwrap();
        assert_eq!(response["item"][0]["answer"][0]["valueDate"], "1980-01-02");
    }

    #[test]
    fn observation_and_expression_on_one_item_is_an_error() {
        let questionnaire = json!({
            "resourceType": "Questionnaire",
            "status": "active",
            "item": [{
                "linkId": "weight",
                "type": "string",
                "code": [{"system": "http://loinc.org", "code": "29463-7"}],
                "extension": [
                    {
                        "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-observationLinkPeriod",
                        "valueDuration": {"value": 1, "code": "a"}
                    },
                    {
                        "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-initialExpression",
                        "valueExpression": {"language": "text/fhirpath", "expression": "'70'"}
                    }
                ]
            }]
        });
        let err = populate(&mut MemoryChart::new(vec![]), &questionnaire, &json!({})).unwrap_err();
        assert!(err.contains("both observation-based and expression-based"));
    }

    #[test]
    fn structure_map_translate_uses_the_chart() {
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
        let questionnaire = json!({
            "resourceType": "Questionnaire",
            "status": "active",
            "contained": [map],
            "extension": [{
                "url": "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-sourceStructureMap",
                "valueCanonical": "#gender"
            }]
        });
        let mut chart = MemoryChart::new(vec![]).translate_as(
            "http://example.org/ConceptMap/gender",
            "M",
            json!("completed"),
        );
        let response = populate(
            &mut chart,
            &questionnaire,
            &json!({"content": [{"resourceType": "Patient", "gender": "M"}]}),
        )
        .unwrap();
        assert_eq!(response["status"], "completed");
    }
}
