//! Compartment membership from a loaded CompartmentDefinition, with a builtin
//! Patient and Encounter compartment when the package did not ship one.

use std::collections::BTreeMap;

use serde_json::Value;

#[derive(Clone, Debug)]
pub struct CompartmentLink {
    pub code: String,
    pub params: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Compartment {
    pub code: String,
    pub resources: Vec<CompartmentLink>,
}

impl Compartment {
    pub fn params_for(&self, resource_type: &str) -> Option<&[String]> {
        self.resources
            .iter()
            .find(|link| link.code == resource_type)
            .map(|link| link.params.as_slice())
    }
}

pub fn load(resources: &[Value]) -> BTreeMap<String, Compartment> {
    let mut compartments = builtin();
    for resource in resources {
        if resource.get("resourceType").and_then(|value| value.as_str())
            != Some("CompartmentDefinition")
        {
            continue;
        }
        if let Some(compartment) = parse(resource) {
            compartments.insert(compartment.code.clone(), compartment);
        }
    }
    compartments
}

fn parse(resource: &Value) -> Option<Compartment> {
    let code = resource.get("code")?.as_str()?.to_string();
    let mut resources = Vec::new();
    for entry in resource.get("resource")?.as_array()? {
        let Some(type_name) = entry.get("code").and_then(|value| value.as_str()) else {
            continue;
        };
        let params = entry
            .get("param")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .filter(|item| !item.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if params.is_empty() {
            continue;
        }
        resources.push(CompartmentLink {
            code: type_name.to_string(),
            params,
        });
    }
    Some(Compartment { code, resources })
}

fn builtin() -> BTreeMap<String, Compartment> {
    let mut compartments = BTreeMap::new();
    compartments.insert(
        "Patient".into(),
        links(
            "Patient",
            &[
                ("Observation", &["subject", "patient"]),
                ("Encounter", &["patient"]),
                ("Condition", &["subject", "patient"]),
                ("Procedure", &["subject", "patient"]),
                ("MedicationRequest", &["subject"]),
                ("DiagnosticReport", &["subject"]),
                ("AllergyIntolerance", &["patient"]),
                ("CarePlan", &["subject", "patient"]),
                ("ServiceRequest", &["subject"]),
                ("DocumentReference", &["subject"]),
                ("Composition", &["subject"]),
            ],
        ),
    );
    compartments.insert(
        "Encounter".into(),
        links(
            "Encounter",
            &[
                ("Observation", &["encounter"]),
                ("Condition", &["encounter"]),
                ("Procedure", &["encounter"]),
                ("DiagnosticReport", &["encounter"]),
                ("MedicationRequest", &["encounter"]),
                ("ServiceRequest", &["encounter"]),
            ],
        ),
    );
    compartments
}

fn links(code: &str, rows: &[(&str, &[&str])]) -> Compartment {
    Compartment {
        code: code.to_string(),
        resources: rows
            .iter()
            .map(|(type_name, params)| CompartmentLink {
                code: (*type_name).to_string(),
                params: params.iter().map(|param| (*param).to_string()).collect(),
            })
            .collect(),
    }
}
