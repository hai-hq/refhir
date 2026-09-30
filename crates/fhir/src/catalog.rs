//! Closed resource catalog.
//!
//! Schema is the FHIR types HaiFHIR ships. Indexes are search parameters we
//! declare. Clients do not define tables or indexes.
//!
//! Observation.subject must be a relative reference `Patient/{id}` to a Patient
//! that exists in the same snapshot. FHIR allows a dangling reference. HaiFHIR
//! rejects it, so `Observation?subject=` does not point at a missing Patient.

use std::collections::BTreeSet;

use haifhir_types::{exact_key, string_key, token_key, IndexId, TabletId};
use serde_json::Value;

use crate::error::FhirError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceType {
    Patient,
    Observation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchKind {
    Token,
    String,
}

#[derive(Clone, Copy, Debug)]
pub struct SearchParam {
    pub name: &'static str,
    pub kind: SearchKind,
}

const PATIENT_PARAMS: &[SearchParam] = &[
    SearchParam {
        name: "_id",
        kind: SearchKind::Token,
    },
    SearchParam {
        name: "identifier",
        kind: SearchKind::Token,
    },
    SearchParam {
        name: "name",
        kind: SearchKind::String,
    },
    SearchParam {
        name: "active",
        kind: SearchKind::Token,
    },
];

const OBSERVATION_PARAMS: &[SearchParam] = &[
    SearchParam {
        name: "_id",
        kind: SearchKind::Token,
    },
    SearchParam {
        name: "subject",
        kind: SearchKind::Token,
    },
    SearchParam {
        name: "code",
        kind: SearchKind::Token,
    },
    SearchParam {
        name: "date",
        kind: SearchKind::String,
    },
];

impl ResourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceType::Patient => "Patient",
            ResourceType::Observation => "Observation",
        }
    }

    pub fn parse(name: &str) -> Result<Self, FhirError> {
        match name {
            "Patient" => Ok(ResourceType::Patient),
            "Observation" => Ok(ResourceType::Observation),
            other => Err(FhirError::UnknownResource(format!(
                "unknown resource type {other}"
            ))),
        }
    }

    pub fn all() -> &'static [ResourceType] {
        &[ResourceType::Patient, ResourceType::Observation]
    }

    pub fn tablet(self) -> TabletId {
        TabletId::new(self.as_str())
    }

    pub fn by_id(self) -> IndexId {
        IndexId::new(format!("{}/by_id", self.as_str()))
    }

    pub fn search_index(self, name: &str) -> IndexId {
        IndexId::new(format!("{}/{}", self.as_str(), name))
    }

    pub fn search_params(self) -> &'static [SearchParam] {
        match self {
            ResourceType::Patient => PATIENT_PARAMS,
            ResourceType::Observation => OBSERVATION_PARAMS,
        }
    }

    pub fn param(self, name: &str) -> Option<SearchParam> {
        self.search_params()
            .iter()
            .copied()
            .find(|param| param.name == name)
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// Index rows for one resource revision, including `by_id`.
pub fn index_keys(
    ty: ResourceType,
    resource: &Value,
) -> Result<Vec<(IndexId, Vec<u8>)>, FhirError> {
    let id = resource
        .get("id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| FhirError::Invalid("resource id is required to index".into()))?;
    if !valid_id(id) {
        return Err(FhirError::Invalid(format!("invalid id {id}")));
    }
    let mut keys = vec![(ty.by_id(), exact_key(id))];
    match ty {
        ResourceType::Patient => patient_keys(ty, id, resource, &mut keys)?,
        ResourceType::Observation => observation_keys(ty, id, resource, &mut keys)?,
    }
    let mut seen = BTreeSet::new();
    keys.retain(|key| seen.insert(key.clone()));
    Ok(keys)
}

fn patient_keys(
    ty: ResourceType,
    id: &str,
    resource: &Value,
    keys: &mut Vec<(IndexId, Vec<u8>)>,
) -> Result<(), FhirError> {
    if let Some(identifiers) = resource.get("identifier") {
        let identifiers = identifiers
            .as_array()
            .ok_or_else(|| FhirError::Invalid("Patient.identifier must be an array".into()))?;
        for identifier in identifiers {
            let Some(value) = identifier.get("value").and_then(|item| item.as_str()) else {
                continue;
            };
            keys.push((ty.search_index("identifier"), token_key(value, id)));
            if let Some(system) = identifier.get("system").and_then(|item| item.as_str()) {
                keys.push((
                    ty.search_index("identifier"),
                    token_key(&format!("{system}|{value}"), id),
                ));
            }
        }
    }
    if let Some(names) = resource.get("name") {
        let names = names
            .as_array()
            .ok_or_else(|| FhirError::Invalid("Patient.name must be an array".into()))?;
        for name in names {
            push_name(ty, id, name, keys);
        }
    }
    if let Some(active) = resource.get("active") {
        let active = active
            .as_bool()
            .ok_or_else(|| FhirError::Invalid("Patient.active must be a boolean".into()))?;
        let token = if active { "true" } else { "false" };
        keys.push((ty.search_index("active"), token_key(token, id)));
    }
    Ok(())
}

fn push_name(ty: ResourceType, id: &str, name: &Value, keys: &mut Vec<(IndexId, Vec<u8>)>) {
    let Some(name) = name.as_object() else {
        return;
    };
    let index = ty.search_index("name");
    if let Some(family) = name.get("family").and_then(|value| value.as_str()) {
        keys.push((index.clone(), string_key(family, id)));
    }
    if let Some(text) = name.get("text").and_then(|value| value.as_str()) {
        keys.push((index.clone(), string_key(text, id)));
    }
    if let Some(given) = name.get("given").and_then(|value| value.as_array()) {
        for item in given {
            if let Some(text) = item.as_str() {
                keys.push((index.clone(), string_key(text, id)));
            }
        }
    }
}

fn observation_keys(
    ty: ResourceType,
    id: &str,
    resource: &Value,
    keys: &mut Vec<(IndexId, Vec<u8>)>,
) -> Result<(), FhirError> {
    if let Some(reference) = resource
        .pointer("/subject/reference")
        .and_then(|value| value.as_str())
    {
        keys.push((ty.search_index("subject"), token_key(reference, id)));
    }
    if let Some(codings) = resource
        .pointer("/code/coding")
        .and_then(|value| value.as_array())
    {
        for coding in codings {
            let Some(code) = coding.get("code").and_then(|value| value.as_str()) else {
                continue;
            };
            keys.push((ty.search_index("code"), token_key(code, id)));
            if let Some(system) = coding.get("system").and_then(|value| value.as_str()) {
                keys.push((
                    ty.search_index("code"),
                    token_key(&format!("{system}|{code}"), id),
                ));
            }
        }
    }
    if let Some(date) = resource
        .get("effectiveDateTime")
        .and_then(|value| value.as_str())
    {
        keys.push((ty.search_index("date"), string_key(date, id)));
    }
    Ok(())
}

pub fn capability_statement() -> Value {
    let resources: Vec<Value> = ResourceType::all()
        .iter()
        .copied()
        .map(|ty| {
            serde_json::json!({
                "type": ty.as_str(),
                "interaction": [
                    {"code": "create"},
                    {"code": "read"},
                    {"code": "vread"},
                    {"code": "update"},
                    {"code": "delete"},
                    {"code": "search-type"},
                    {"code": "history-instance"}
                ],
                "searchParam": ty.search_params().iter().filter(|param| param.name != "_id").map(|param| {
                    serde_json::json!({
                        "name": param.name,
                        "type": match param.kind {
                            SearchKind::Token => "token",
                            SearchKind::String => "string",
                        }
                    })
                }).collect::<Vec<_>>(),
                "versioning": "versioned",
                "readHistory": true
            })
        })
        .collect();
    serde_json::json!({
        "resourceType": "CapabilityStatement",
        "status": "active",
        "kind": "instance",
        "fhirVersion": "4.0.1",
        "format": ["json", "application/fhir+json"],
        "rest": [{
            "mode": "server",
            "resource": resources,
            "interaction": [{"code": "transaction"}]
        }]
    })
}
