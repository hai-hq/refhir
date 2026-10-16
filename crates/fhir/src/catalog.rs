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
use refhir_validate::{expansion_is_current, MemoryTerminology, Schema};
use serde_json::{json, Value};

use crate::compartment::{self, Compartment};
use crate::error::FhirError;
use crate::extract::{self, Extractor, SearchKind};

/// Parse `rows` into JSON values, in order, spread over the available cores.
fn parse_all<T: Sync>(
    rows: &[T],
    text: impl Fn(&T) -> &str + Sync,
    what: &str,
) -> Result<Vec<Value>, PersistError> {
    let workers = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(rows.len() / 64 + 1);
    let parse = |chunk: &[T]| -> Result<Vec<Value>, PersistError> {
        chunk
            .iter()
            .map(|row| {
                serde_json::from_str(text(row))
                    .map_err(|err| PersistError::Definite(format!("{what}: {err}")))
            })
            .collect()
    };
    if workers <= 1 {
        return parse(rows);
    }
    let size = rows.len().div_ceil(workers);
    let parsed = std::thread::scope(|scope| {
        let handles: Vec<_> = rows
            .chunks(size)
            .map(|chunk| scope.spawn(|| parse(chunk)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("json parse"))
            .collect::<Vec<_>>()
    });
    let mut values = Vec::with_capacity(rows.len());
    for chunk in parsed {
        values.extend(chunk?);
    }
    Ok(values)
}

/// How reference targets are checked on write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ReferencePolicy {
    /// Relative `Type/id` only, and the target must exist in the snapshot.
    #[default]
    Strict,
    /// Allow absolute URLs and targets that are not in this server.
    /// A type that is not in `targetProfile` is still rejected.
    Fhir,
}

impl ReferencePolicy {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "strict" => Some(ReferencePolicy::Strict),
            "fhir" => Some(ReferencePolicy::Fhir),
            _ => None,
        }
    }
}

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

/// Which counter a process follows to know its catalog is out of date.
///
/// A catalog change that leaves search parameters pending (their index is still being built)
/// moves only `Latest`. `Stable` moves once the backfill has completed and every parameter is
/// ready. A replica never writes, so it follows `Stable` and never loads a parameter whose index
/// is incomplete. A standby may have to finish the backfill when it takes over, so it follows
/// `Latest` and already knows about the pending parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogClock {
    Stable,
    Latest,
}

impl CatalogClock {
    pub fn read(self, store: &dyn ConformanceStore) -> Result<u64, PersistError> {
        match self {
            CatalogClock::Stable => store.catalog_version(),
            CatalogClock::Latest => store.catalog_revision(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Catalog {
    fhir_version: Option<String>,
    types: BTreeMap<String, ResourceModel>,
    path_model: Model,
    schema: Schema,
    terms: MemoryTerminology,
    reference_policy: ReferencePolicy,
    compartments: BTreeMap<String, Compartment>,
    /// Form Archiver: store QuestionnaireResponse, never return one.
    sdc_archiver: bool,
    /// Whether a PUT to an id that does not exist creates it under that id. Off by default: the
    /// server assigns ids.
    update_create: bool,
    /// Whether any stored expansion was made under other rules than the current ones, so that it
    /// may be wrong and the stored expansions need rebuilding.
    stale_expansions: bool,
}

impl Catalog {
    /// Whether any search parameter is still waiting for its backfill.
    pub fn has_pending_parameters(&self) -> bool {
        self.types
            .values()
            .any(|model| model.pending_params().next().is_some())
    }

    pub fn empty() -> Self {
        Self {
            fhir_version: None,
            types: BTreeMap::new(),
            path_model: Model::new(),
            schema: Schema::default(),
            terms: MemoryTerminology::default(),
            reference_policy: ReferencePolicy::Strict,
            compartments: compartment::load(&[]),
            sdc_archiver: false,
            update_create: false,
            stale_expansions: false,
        }
    }

    /// The catalog and the value of `clock` it reflects. `open` reads five tables one after
    /// another, so a change made in between could give a catalog that mixes two states; this
    /// reads the clock before and after and tries again if it moved. If it never holds still the
    /// catalog is returned with the *older* reading, so a caller that watches the clock sees it
    /// differ and loads again, rather than trusting a catalog that may mix two states.
    pub fn open_consistent(
        store: &dyn ConformanceStore,
        clock: CatalogClock,
    ) -> Result<(Self, u64), PersistError> {
        let mut attempts = 0;
        loop {
            let before = clock.read(store)?;
            let catalog = Self::open(store)?;
            let after = clock.read(store)?;
            attempts += 1;
            if before == after || attempts >= 5 {
                return Ok((catalog, before));
            }
        }
    }

    pub fn open(store: &dyn ConformanceStore) -> Result<Self, PersistError> {
        Self::load(
            &store.load_packages()?,
            &store.load_structure_definitions()?,
            &store.load_search_parameters()?,
            &store.load_terminology()?,
            &store.load_expansions()?,
        )
    }

    pub fn load(
        packages: &[refhir_persistence::StoredPackage],
        definitions: &[StoredStructureDefinition],
        parameters: &[StoredSearchParameter],
        terminology: &[refhir_persistence::StoredTerminology],
        expansions: &[refhir_persistence::StoredExpansion],
    ) -> Result<Self, PersistError> {
        let fhir_version = packages.first().map(|package| package.fhir_version.clone());
        // Loading is dominated by parsing thousands of stored JSON documents, so parse them on
        // every core, then build the two independent indexes side by side.
        let definition_json = parse_all(definitions, |row| &row.json, "structure definition json")?;
        let term_json = parse_all(terminology, |row| &row.json, "terminology json")?;
        let expansion_json = parse_all(expansions, |row| &row.json, "value set expansion json")?;
        // Each resource with the package it came from, so a value set reads the code systems of
        // its own package first, as installing the package expanded it.
        let scoped: Vec<(i64, &serde_json::Value)> = terminology
            .iter()
            .map(|row| row.package_id)
            .zip(&term_json)
            .collect();
        // Each stored expansion with the package that stored it, so that a value set more than one
        // package defines is answered by the definition in use, not by the package installed last.
        let stored: Vec<(i64, &serde_json::Value)> = expansions
            .iter()
            .map(|row| row.package_id)
            .zip(&expansion_json)
            .collect();
        let (schema, terms) = std::thread::scope(|scope| {
            let schema = scope.spawn(|| Schema::from_definitions(&definition_json));
            let terms =
                MemoryTerminology::from_stored_scoped(&scoped, &stored, fhir_version.as_deref());
            (schema.join().expect("schema build"), terms)
        });
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
        let mut component_types = BTreeMap::new();
        for parameter in parameters {
            component_types
                .entry(parameter.url.clone())
                .or_insert_with(|| parameter.param_type.clone());
        }
        for parameter in parameters {
            if parameter.status == ParamStatus::Unsupported {
                continue;
            }
            let Some(model) = types.get_mut(&parameter.base) else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<Value>(&parameter.json) else {
                continue;
            };
            let Some(extractor) = extract::compile_parameter(&json, &component_types) else {
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
            .filter(|json| {
                json.get("derivation").and_then(|value| value.as_str()) != Some("constraint")
            })
            .cloned()
            .collect();
        Ok(Self {
            fhir_version,
            types,
            path_model: Model::from_definitions(&model_json),
            schema,
            terms,
            reference_policy: ReferencePolicy::Strict,
            compartments: compartment::load(&term_json),
            sdc_archiver: false,
            update_create: false,
            stale_expansions: expansion_json
                .iter()
                .any(|document| !expansion_is_current(document)),
        })
    }

    /// Whether any stored expansion was made under other rules than the current ones. A database
    /// the previous version rebuilt has expansions read from one pool for every package, and
    /// nothing else would ever correct them.
    pub fn expansions_stale(&self) -> bool {
        self.stale_expansions
    }

    pub fn reference_policy(&self) -> ReferencePolicy {
        self.reference_policy
    }

    pub fn set_reference_policy(&mut self, policy: ReferencePolicy) {
        self.reference_policy = policy;
    }

    pub fn sdc_archiver(&self) -> bool {
        self.sdc_archiver
    }

    pub fn set_sdc_archiver(&mut self, enabled: bool) {
        self.sdc_archiver = enabled;
    }

    /// Update-as-create: `PUT [type]/[id]` creates the resource when `id` is not in use. FHIR
    /// leaves it to the deployment, so it is a setting, and the capability statement reports it.
    pub fn update_create(&self) -> bool {
        self.update_create
    }

    pub fn set_update_create(&mut self, enabled: bool) {
        self.update_create = enabled;
    }

    /// Form Archiver may store a QuestionnaireResponse and must not return one.
    pub fn deny_response_read(&self, type_name: &str) -> Result<(), FhirError> {
        if self.sdc_archiver && type_name == "QuestionnaireResponse" {
            Err(FhirError::Forbidden(
                "form archiver does not read QuestionnaireResponse".into(),
            ))
        } else {
            Ok(())
        }
    }

    pub fn compartment(&self, code: &str) -> Option<&Compartment> {
        self.compartments.get(code)
    }

    pub fn resource_models(&self) -> impl Iterator<Item = &ResourceModel> {
        self.types.values()
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

    /// For a change to terminology alone, applied in place rather than by rebuilding the whole
    /// catalog from the store.
    pub(crate) fn terms_mut(&mut self) -> &mut MemoryTerminology {
        &mut self.terms
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
                        {"code": "patch"},
                        {"code": "delete"},
                        {"code": "search-type"},
                        {"code": "history-instance"},
                        {"code": "history-type"}
                    ],
                    "searchInclude": model.params.iter().filter(|param| param.status == ParamStatus::Ready && param.kind == SearchKind::Reference).map(|param| format!("{}:{}", model.name, param.name)).collect::<Vec<_>>(),
                    "searchRevInclude": model.params.iter().filter(|param| param.status == ParamStatus::Ready && param.kind == SearchKind::Reference).map(|param| format!("{}:{}", model.name, param.name)).collect::<Vec<_>>(),
                    "searchParam": model.params.iter().filter(|param| param.status == ParamStatus::Ready).map(|param| {
                        json!({
                            "name": param.name,
                            "type": param.kind.fhir_type()
                        })
                    }).collect::<Vec<_>>(),
                    "versioning": "versioned",
                    "readHistory": true,
                    // The server assigns ids. A PUT to an id that does not exist is a 405 unless
                    // the deployment turned update-as-create on. The conditional interactions
                    // are implemented (`If-None-Exist`, `PUT [type]?query`, `DELETE [type]?query`
                    // matching at most one resource).
                    "updateCreate": self.update_create,
                    "conditionalCreate": true,
                    "conditionalUpdate": true,
                    "conditionalDelete": "single"
                })
            })
            .collect();
        let mut statement = json!({
            "resourceType": "CapabilityStatement",
            "status": "active",
            "kind": "instance",
            "fhirVersion": self.fhir_version(),
            "format": ["json", "application/fhir+json"],
            "rest": [{
                "mode": "server",
                "documentation": crate::search::capability_notes(),
                "resource": resources,
                "interaction": [
                    {"code": "transaction"},
                    {"code": "batch"},
                    {"code": "history-system"},
                    {"code": "search-system"}
                ]
            }]
        });
        refhir_sdc::annotate_capability(&mut statement);
        annotate_install(&mut statement);
        statement
    }
}

fn annotate_install(statement: &mut Value) {
    let Some(resources) = statement
        .pointer_mut("/rest/0/resource")
        .and_then(|value| value.as_array_mut())
    else {
        return;
    };
    for resource in resources {
        if resource.get("type").and_then(|value| value.as_str()) == Some("ImplementationGuide") {
            resource["operation"] = json!([{
                "name": "install",
                "definition": "https://refhir.dev/OperationDefinition/ImplementationGuide-install"
            }]);
        }
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

/// The resource type a `Reference` target profile allows. Only a core definition URL names its
/// type; any other profile's id says nothing about it, so it leaves the element unrestricted
/// (`Resource`) and the validator, which can resolve the profile, decides.
fn target_name(profile: &str) -> String {
    match profile.strip_prefix("http://hl7.org/fhir/StructureDefinition/") {
        Some(name) if !name.is_empty() && !name.contains('/') => name.to_string(),
        _ => "Resource".to_string(),
    }
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
        // A choice (`value[x]`) is stored under a name this path does not spell, so it cannot be
        // followed.
        if relative.is_empty() || relative.contains("[x]") {
            continue;
        }
        let nested = relative.contains('.');
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
                    .map(target_name)
                    .collect()
            })
            .unwrap_or_default();
        // `min` is the element's own: at least one value *if its parent is present*. That is
        // only a requirement of the resource for a top-level element.
        let min = if nested {
            0
        } else {
            element
                .get("min")
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as u32
        };
        references.push(ReferenceElement {
            path: relative.to_string(),
            min,
            targets,
        });
    }
    references
}
