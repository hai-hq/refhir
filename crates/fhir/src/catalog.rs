//! Resource catalog loaded from conformance tables.
//!
//! Resource instances stay on the document log. This model is the compiled
//! package: which types exist, which references they enforce, and which search
//! parameters writers and search may use.

use std::collections::{BTreeMap, BTreeSet};

use refhir_fhirpath::{Host, Model};
use refhir_persistence::{
    ConformanceStore, ParamStatus, PersistError, StoredSearchParameter, StoredStructureDefinition,
};
use refhir_types::{exact_key, IndexId, TabletId};
use refhir_validate::{MemoryTerminology, Schema};
use serde_json::{json, Value};

use crate::error::FhirError;
use crate::extract::{self, Extractor, SearchKind};

#[derive(Clone, Debug)]
pub struct ReferenceElement {
    pub path: String,
    pub min: u32,
    pub targets: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SearchParam {
    pub name: String,
    pub kind: SearchKind,
    pub status: ParamStatus,
    pub extractor: Extractor,
}

#[derive(Clone, Debug)]
pub struct ResourceModel {
    pub name: String,
    pub references: Vec<ReferenceElement>,
    pub params: Vec<SearchParam>,
}

impl ResourceModel {
    pub fn tablet(&self) -> TabletId {
        TabletId::new(self.name.clone())
    }

    pub fn by_id(&self) -> IndexId {
        IndexId::new(format!("{}/by_id", self.name))
    }

    pub fn search_index(&self, name: &str) -> IndexId {
        IndexId::new(format!("{}/{}", self.name, name))
    }

    pub fn ready_param(&self, name: &str) -> Option<&SearchParam> {
        self.params
            .iter()
            .find(|param| param.name == name && param.status == ParamStatus::Ready)
    }

    pub fn writer_params(&self) -> impl Iterator<Item = &SearchParam> {
        self.params.iter().filter(|param| {
            param.status == ParamStatus::Ready || param.status == ParamStatus::Pending
        })
    }

    pub fn pending_params(&self) -> impl Iterator<Item = &SearchParam> {
        self.params
            .iter()
            .filter(|param| param.status == ParamStatus::Pending)
    }
}

#[derive(Clone, Debug)]
pub struct Catalog {
    fhir_version: Option<String>,
    types: BTreeMap<String, ResourceModel>,
    path_model: Model,
    schema: Schema,
    terms: MemoryTerminology,
}

impl Catalog {
    pub fn empty() -> Self {
        Self {
            fhir_version: None,
            types: BTreeMap::new(),
            path_model: Model::new(),
            schema: Schema::default(),
            terms: MemoryTerminology::default(),
        }
    }

    pub fn open(store: &dyn ConformanceStore) -> Result<Self, PersistError> {
        Self::load(
            &store.load_packages()?,
            &store.load_structure_definitions()?,
            &store.load_search_parameters()?,
            &store.load_terminology()?,
        )
    }

    pub fn load(
        packages: &[refhir_persistence::StoredPackage],
        definitions: &[StoredStructureDefinition],
        parameters: &[StoredSearchParameter],
        terminology: &[refhir_persistence::StoredTerminology],
    ) -> Result<Self, PersistError> {
        let fhir_version = packages.first().map(|package| package.fhir_version.clone());
        let mut definition_json = Vec::new();
        for definition in definitions {
            let json: Value = serde_json::from_str(&definition.json).map_err(|err| {
                PersistError::Definite(format!("structure definition json: {err}"))
            })?;
            definition_json.push(json);
        }
        let schema = Schema::from_definitions(&definition_json);
        let mut term_json = Vec::new();
        for resource in terminology {
            let json: Value = serde_json::from_str(&resource.json).map_err(|err| {
                PersistError::Definite(format!("terminology json: {err}"))
            })?;
            term_json.push(json);
        }
        let terms = MemoryTerminology::from_resources(&term_json);
        let mut types = BTreeMap::new();
        for (definition, json) in definitions.iter().zip(&definition_json) {
            if json.get("derivation").and_then(|value| value.as_str()) == Some("constraint") {
                continue;
            }
            if definition.kind != "resource" || definition.is_abstract {
                continue;
            }
            let model = types
                .entry(definition.type_name.clone())
                .or_insert_with(|| ResourceModel {
                    name: definition.type_name.clone(),
                    references: Vec::new(),
                    params: Vec::new(),
                });
            for reference in references_from(&definition.type_name, json) {
                if !model
                    .references
                    .iter()
                    .any(|item| item.path == reference.path)
                {
                    model.references.push(reference);
                }
            }
        }
        for parameter in parameters {
            if parameter.status == ParamStatus::Unsupported {
                continue;
            }
            let Some(model) = types.get_mut(&parameter.base) else {
                continue;
            };
            let Some(extractor) =
                extract::compile(&parameter.param_type, parameter.expression.as_deref())
            else {
                continue;
            };
            if model
                .params
                .iter()
                .any(|param| param.name == parameter.code)
            {
                continue;
            }
            model.params.push(SearchParam {
                name: parameter.code.clone(),
                kind: extractor.kind(),
                status: parameter.status,
                extractor,
            });
        }
        for model in types.values_mut() {
            model
                .params
                .sort_by(|left, right| left.name.cmp(&right.name));
        }
        let model_json: Vec<Value> = definition_json
            .iter()
            .filter(|json| json.get("derivation").and_then(|value| value.as_str()) != Some("constraint"))
            .cloned()
            .collect();
        Ok(Self {
            fhir_version,
            types,
            path_model: Model::from_definitions(&model_json),
            schema,
            terms,
        })
    }

    pub fn path_model(&self) -> &Model {
        &self.path_model
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn terms(&self) -> &MemoryTerminology {
        &self.terms
    }

    pub fn get(&self, name: &str) -> Result<&ResourceModel, FhirError> {
        self.types
            .get(name)
            .ok_or_else(|| FhirError::UnknownResource(format!("unknown resource type {name}")))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.types.contains_key(name)
    }

    pub fn fhir_version(&self) -> &str {
        self.fhir_version.as_deref().unwrap_or("4.0.1")
    }

    pub fn capability_statement(&self) -> Value {
        let resources: Vec<Value> = self
            .types
            .values()
            .map(|model| {
                json!({
                    "type": model.name,
                    "interaction": [
                        {"code": "create"},
                        {"code": "read"},
                        {"code": "vread"},
                        {"code": "update"},
                        {"code": "delete"},
                        {"code": "search-type"},
                        {"code": "history-instance"}
                    ],
                    "searchParam": model.params.iter().filter(|param| param.status == ParamStatus::Ready).map(|param| {
                        json!({
                            "name": param.name,
                            "type": match param.kind {
                                SearchKind::Token => "token",
                                SearchKind::String => "string",
                                SearchKind::Reference => "reference",
                                SearchKind::Date => "date",
                            }
                        })
                    }).collect::<Vec<_>>(),
                    "versioning": "versioned",
                    "readHistory": true
                })
            })
            .collect();
        json!({
            "resourceType": "CapabilityStatement",
            "status": "active",
            "kind": "instance",
            "fhirVersion": self.fhir_version(),
            "format": ["json", "application/fhir+json"],
            "rest": [{
                "mode": "server",
                "resource": resources,
                "interaction": [{"code": "transaction"}]
            }]
        })
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.')
}

pub fn index_keys(
    path_model: &Model,
    model: &ResourceModel,
    resource: &Value,
    host: &dyn Host,
) -> Result<Vec<(IndexId, Vec<u8>)>, FhirError> {
    let id = resource_id(resource)?;
    let mut keys = vec![(model.by_id(), exact_key(id))];
    for param in model.writer_params() {
        let index = model.search_index(&param.name);
        keys.extend(param.extractor.keys(resource, id, &index, path_model, host));
    }
    Ok(dedup_keys(keys))
}

pub fn pending_index_keys(
    path_model: &Model,
    model: &ResourceModel,
    resource: &Value,
    host: &dyn Host,
) -> Result<Vec<(IndexId, Vec<u8>)>, FhirError> {
    let id = resource_id(resource)?;
    let mut keys = Vec::new();
    for param in model.pending_params() {
        let index = model.search_index(&param.name);
        keys.extend(param.extractor.keys(resource, id, &index, path_model, host));
    }
    Ok(dedup_keys(keys))
}

fn resource_id(resource: &Value) -> Result<&str, FhirError> {
    let id = resource
        .get("id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| FhirError::Invalid("resource id is required to index".into()))?;
    if !valid_id(id) {
        return Err(FhirError::Invalid(format!("invalid id {id}")));
    }
    Ok(id)
}

fn dedup_keys(mut keys: Vec<(IndexId, Vec<u8>)>) -> Vec<(IndexId, Vec<u8>)> {
    let mut seen = BTreeSet::new();
    keys.retain(|key| seen.insert(key.clone()));
    keys
}

fn references_from(type_name: &str, json: &Value) -> Vec<ReferenceElement> {
    let Some(list) = json
        .pointer("/snapshot/element")
        .and_then(|value| value.as_array())
    else {
        return Vec::new();
    };
    let prefix = format!("{type_name}.");
    let mut references = Vec::new();
    for element in list {
        let Some(path) = element.get("path").and_then(|value| value.as_str()) else {
            continue;
        };
        let Some(relative) = path.strip_prefix(&prefix) else {
            continue;
        };
        if relative.is_empty() || relative.contains('.') {
            continue;
        }
        let Some(types) = element.get("type").and_then(|value| value.as_array()) else {
            continue;
        };
        let reference = types
            .iter()
            .find(|item| item.get("code").and_then(|code| code.as_str()) == Some("Reference"));
        let Some(reference) = reference else {
            continue;
        };
        let targets = reference
            .get("targetProfile")
            .and_then(|value| value.as_array())
            .map(|profiles| {
                profiles
                    .iter()
                    .filter_map(|profile| profile.as_str())
                    .filter_map(|profile| profile.rsplit('/').next())
                    .map(|name| name.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let min = element
            .get("min")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as u32;
        references.push(ReferenceElement {
            path: relative.to_string(),
            min,
            targets,
        });
    }
    references
}
