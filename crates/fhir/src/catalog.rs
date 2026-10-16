//! Resource catalog loaded from conformance tables.
//!
//! Resource instances stay on the document log. This model is the compiled
//! package: which types exist, which references they enforce, and which search
//! parameters writers and search may use.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use refhir_fhirpath::{Host, Model, ModelBuilder};
use refhir_persistence::{
    ConformanceStore, ParamStatus, PersistError, StoredSearchParameter, StoredStructureDefinition,
};
use refhir_types::{exact_key, valid_id, IndexId, TabletId};
use refhir_validate::{
    expansion_is_current, DefinitionSource, MemoryTerminology, Schema, SchemaBuilder, TermSource,
    TerminologyBuilder,
};
use serde_json::{json, Value};

use crate::compartment::{self, Compartment};
use crate::error::FhirError;
use crate::extract::{self, Extractor, SearchKind};
use crate::lazy::StoreSource;

/// Parse `rows` into JSON values, in order, spread over the available cores.
fn parse_all<T: Sync>(
    rows: &[T],
    text: impl Fn(&T) -> &str + Sync,
    what: &str,
) -> Result<Vec<Value>, PersistError> {
    let workers = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(rows.len() / 32 + 1);
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

/// What a catalog has read from the store lately and still keeps.
#[derive(Clone, Debug, Default)]
pub(crate) struct Warmth {
    structures: Vec<String>,
    terminology: refhir_validate::Resident,
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
    /// Whether a rebuild of this catalog lets go of it before it builds the new one, instead of
    /// holding both. See [`Catalog::set_rebuild_in_place`].
    rebuild_in_place: bool,
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

    /// The search parameters [`Catalog::open`] would attach to the resource types this catalog
    /// has, for `parameters` as they are stored now. A backfill that completes changes the status
    /// of stored parameters and nothing else, and these are all it changes in a catalog, so the
    /// schema and the terminology, which are most of what a load builds, need not be built again.
    /// The result is made under a read lock; [`Catalog::set_parameters`] applies it.
    pub fn compiled_parameters(
        &self,
        parameters: &[StoredSearchParameter],
    ) -> BTreeMap<String, Vec<SearchParam>> {
        let mut types: BTreeMap<String, ResourceModel> = self
            .types
            .iter()
            .map(|(name, model)| {
                let bare = ResourceModel {
                    name: model.name.clone(),
                    references: model.references.clone(),
                    params: Vec::new(),
                };
                (name.clone(), bare)
            })
            .collect();
        add_parameters(&mut types, parameters);
        types
            .into_iter()
            .map(|(name, model)| (name, model.params))
            .collect()
    }

    /// Replace the search parameters of the types in `compiled`, which
    /// [`Catalog::compiled_parameters`] made. A type the catalog gained since is left as it is.
    pub fn set_parameters(&mut self, compiled: BTreeMap<String, Vec<SearchParam>>) {
        for (name, params) in compiled {
            if let Some(model) = self.types.get_mut(&name) {
                model.params = params;
            }
        }
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
            rebuild_in_place: false,
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

    /// Read the conformance tables and compile them.
    ///
    /// The tables are read a page at a time and each page is compiled and dropped before the next
    /// is read. A parsed StructureDefinition or code system is several times the size of its
    /// text, and the catalog keeps only a fraction of it; holding every table's rows and parsed
    /// JSON at once is what made loading the R4 packages take several times the memory the
    /// finished catalog uses. The terminology goes first because it has the most to hold while
    /// it is built: loaded before the schema exists, its peak does not sit on top of the schema.
    ///
    /// A store that can give a handle to itself (see [`ConformanceStore::shared`]) gets a catalog
    /// that keeps an index of its definitions and reads each from the store when it is first
    /// used; any other is read whole, as [`Self::open_whole`] does.
    pub fn open(store: &dyn ConformanceStore) -> Result<Self, PersistError> {
        let source = store
            .shared()
            .map(|shared| Arc::new(StoreSource::new(shared)));
        Self::open_from(store, source)
    }

    /// [`Self::open`] with every definition read and compiled now, whatever the store can do. It
    /// answers what `open` answers, and is what that is checked against.
    pub fn open_whole(store: &dyn ConformanceStore) -> Result<Self, PersistError> {
        Self::open_from(store, None)
    }

    pub(crate) fn open_from(
        store: &dyn ConformanceStore,
        source: Option<Arc<StoreSource>>,
    ) -> Result<Self, PersistError> {
        let lazy = source.is_some();
        let packages = store.load_packages()?;
        let fhir_version = packages.first().map(|package| package.fhir_version.clone());
        drop(packages);
        let mut terminology = TerminologyCompiler::new(
            fhir_version.as_deref(),
            source.clone().map(|source| source as Arc<dyn TermSource>),
        );
        store.visit_terminology(&mut |rows| terminology.add_resources(&rows))?;
        store.visit_expansions(&mut |rows| terminology.add_expansions(&rows))?;
        let terminology = terminology.finish();
        let mut definitions =
            DefinitionsCompiler::new(source.map(|source| source as Arc<dyn DefinitionSource>));
        store.visit_structure_definitions(&mut |rows| definitions.add(&rows))?;
        let mut definitions = definitions.finish();
        add_parameters(&mut definitions.types, &store.load_search_parameters()?);
        let mut catalog = Self::assemble(fhir_version, definitions, terminology);
        if lazy {
            // The base types are held, and so is what they bind: the value sets a code is checked
            // against and the code systems behind them. A request of any common kind finds all of
            // it in memory. What it uses beyond that (a profile, an extension, a table nothing
            // binds) is read when it is first used.
            let checked = catalog.schema.checked_value_sets();
            catalog.terms.hold(&checked);
        }
        // The load parsed several times what the catalog keeps; do not let the process stay at
        // that peak.
        crate::memory::release_unused_memory();
        Ok(catalog)
    }

    fn assemble(
        fhir_version: Option<String>,
        definitions: CompiledDefinitions,
        terminology: CompiledTerminology,
    ) -> Self {
        Self {
            fhir_version,
            types: definitions.types,
            path_model: definitions.path_model,
            schema: definitions.schema,
            terms: terminology.terms,
            reference_policy: ReferencePolicy::Strict,
            compartments: terminology.compartments,
            sdc_archiver: false,
            update_create: false,
            rebuild_in_place: false,
            stale_expansions: terminology.stale_expansions,
        }
    }

    /// What this catalog has read from the store lately and still keeps, which a catalog that
    /// replaces it can be told to read at once (see [`Self::warm`]).
    pub(crate) fn warmth(&self) -> Warmth {
        Warmth {
            structures: self.schema.resident(),
            terminology: self.terms.resident(),
        }
    }

    /// Read what `warmth` lists, so that a reload does not send the requests that were being
    /// answered back to waiting for reads. It is done before the catalog is put in place, while
    /// the one it replaces is still answering.
    pub(crate) fn warm(&self, warmth: &Warmth) {
        self.schema.warm(&warmth.structures);
        self.terms.warm(&warmth.terminology);
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

    /// Whether a rebuild of this catalog lets go of it first.
    pub fn rebuild_in_place(&self) -> bool {
        self.rebuild_in_place
    }

    /// Let a rebuild of this catalog release it before the new one is built, so that two are not
    /// held at once (a whole catalog's memory, at the moment a starting server needs the most).
    /// Only a server that is still starting may ask for it: until it serves, nothing reads the
    /// catalog, so nothing can find it empty. A rebuild that then fails leaves it empty.
    pub fn set_rebuild_in_place(&mut self, enabled: bool) {
        self.rebuild_in_place = enabled;
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

/// Stored JSON is parsed this much text at a time. A parsed document is several times the size of
/// its text and the catalog keeps a fraction of it, so the batch is what bounds the memory loading
/// takes: about eight times this much. It is by size rather than by count because a code system
/// can be thousands of times the size of another. A catalog that keeps only an index of what it
/// loads keeps none of the parse, so its peak is this.
const PARSE_BATCH_BYTES: usize = 1024 * 1024;

/// `rows` in runs of about [`PARSE_BATCH_BYTES`] of text, at least one row each.
fn batches<'a, T>(
    rows: &'a [T],
    text_len: impl Fn(&T) -> usize + 'a,
) -> impl Iterator<Item = &'a [T]> + 'a {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start >= rows.len() {
            return None;
        }
        let mut end = start;
        let mut bytes = 0;
        while end < rows.len() {
            let next = text_len(&rows[end]);
            if end > start && bytes + next > PARSE_BATCH_BYTES {
                break;
            }
            bytes += next;
            end += 1;
        }
        let batch = &rows[start..end];
        start = end;
        Some(batch)
    })
}

struct CompiledDefinitions {
    types: BTreeMap<String, ResourceModel>,
    path_model: Model,
    schema: Schema,
}

/// Compiles StructureDefinitions handed over a page at a time into the schema, the FHIRPath type
/// model and the reference elements of each resource type. Only one batch of them is parsed at
/// any moment.
struct DefinitionsCompiler {
    schema: SchemaBuilder,
    path_model: ModelBuilder,
    types: BTreeMap<String, ResourceModel>,
}

impl DefinitionsCompiler {
    /// With a `source`, the schema keeps an index and reads each structure from it when used.
    fn new(source: Option<Arc<dyn DefinitionSource>>) -> Self {
        Self {
            schema: match source {
                Some(source) => SchemaBuilder::lazy(source),
                None => SchemaBuilder::default(),
            },
            path_model: ModelBuilder::default(),
            types: BTreeMap::new(),
        }
    }

    fn add(&mut self, rows: &[StoredStructureDefinition]) -> Result<(), PersistError> {
        for batch in batches(rows, |row| row.json.len()) {
            let parsed = parse_all(batch, |row| &row.json, "structure definition json")?;
            for (definition, json) in batch.iter().zip(&parsed) {
                self.schema.add_stored(definition.package_id, json);
                if json.get("derivation").and_then(|value| value.as_str()) == Some("constraint") {
                    continue;
                }
                self.path_model.add(json);
                if definition.kind != "resource" || definition.is_abstract {
                    continue;
                }
                let model = self
                    .types
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
        }
        Ok(())
    }

    fn finish(self) -> CompiledDefinitions {
        CompiledDefinitions {
            types: self.types,
            path_model: self.path_model.finish(),
            schema: self.schema.finish(),
        }
    }
}

/// Attach the search parameters to the resource types they are defined for.
fn add_parameters(
    types: &mut BTreeMap<String, ResourceModel>,
    parameters: &[StoredSearchParameter],
) {
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
        let Some(mut extractor) = extract::compile_parameter(&json, &component_types) else {
            continue;
        };
        // What the parameter can point at in this type, not in every type it is defined for.
        extractor.narrow_targets(&parameter.base, |path| {
            model
                .references
                .iter()
                .find(|element| element.path == path)
                .map(|element| element.targets.as_slice())
        });
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
}

struct CompiledTerminology {
    terms: MemoryTerminology,
    compartments: BTreeMap<String, Compartment>,
    stale_expansions: bool,
}

/// Compiles terminology handed over a page at a time. Each resource goes in with the package it
/// came from, so a value set reads the code systems of its own package first, as installing the
/// package expanded it; each stored expansion goes in with the package that stored it, so that a
/// value set more than one package defines is answered by the definition in use, not by the
/// package installed last. Code systems are indexed and dropped as they are parsed; see
/// [`TerminologyBuilder`]. Resources go in before expansions.
struct TerminologyCompiler {
    builder: TerminologyBuilder,
    compartment_definitions: Vec<Value>,
    stale_expansions: bool,
    /// Set when terminology is to be read from the store when used: then the builder is told
    /// what each row is stored under, and keeps no more than an index.
    stored: bool,
}

impl TerminologyCompiler {
    /// With a `source`, nothing of a code system, value set or expansion is kept but what answers
    /// a question without reading it; the rest is read from the source when used.
    fn new(fhir_version: Option<&str>, source: Option<Arc<dyn TermSource>>) -> Self {
        Self {
            stored: source.is_some(),
            builder: match source {
                Some(source) => TerminologyBuilder::for_source(fhir_version, source),
                None => TerminologyBuilder::for_stored(fhir_version),
            },
            compartment_definitions: Vec::new(),
            stale_expansions: false,
        }
    }

    fn add_resources(
        &mut self,
        rows: &[refhir_persistence::StoredTerminology],
    ) -> Result<(), PersistError> {
        for batch in batches(rows, |row| row.json.len()) {
            let parsed = parse_all(batch, |row| &row.json, "terminology json")?;
            for (row, json) in batch.iter().zip(parsed) {
                if json.get("resourceType").and_then(|value| value.as_str())
                    == Some("CompartmentDefinition")
                {
                    self.compartment_definitions.push(json);
                } else if self.stored {
                    self.builder
                        .add_stored_resource(row.package_id, &row.url, json);
                } else {
                    self.builder.add_resource(row.package_id, json);
                }
            }
        }
        Ok(())
    }

    fn add_expansions(
        &mut self,
        rows: &[refhir_persistence::StoredExpansion],
    ) -> Result<(), PersistError> {
        for batch in batches(rows, |row| row.json.len()) {
            let parsed = parse_all(batch, |row| &row.json, "value set expansion json")?;
            for (row, document) in batch.iter().zip(&parsed) {
                self.stale_expansions |= !expansion_is_current(document);
                if self.stored {
                    self.builder
                        .add_stored_expansion(row.package_id, &row.url, document);
                } else {
                    self.builder.add_expansion(row.package_id, document);
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> CompiledTerminology {
        CompiledTerminology {
            terms: self.builder.finish(),
            compartments: compartment::load(&self.compartment_definitions),
            stale_expansions: self.stale_expansions,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn batched(sizes: &[usize]) -> Vec<Vec<usize>> {
        batches(sizes, |size| *size)
            .map(|batch| batch.to_vec())
            .collect()
    }

    #[test]
    fn rows_are_batched_by_the_text_they_hold() {
        assert!(batched(&[]).is_empty());
        let budget = PARSE_BATCH_BYTES;
        // Small rows share a batch, in order, until the next would pass the budget.
        assert_eq!(
            batched(&[budget / 2, budget / 2, 1, 1]),
            vec![vec![budget / 2, budget / 2], vec![1, 1]]
        );
        // A row over the budget on its own is a batch of its own: never dropped, never split.
        assert_eq!(
            batched(&[1, budget * 3, 1]),
            vec![vec![1], vec![budget * 3], vec![1]]
        );
        // Whatever the sizes, every row is in exactly one batch and the order is kept.
        let sizes: Vec<usize> = (1..500).map(|n| n * 997).collect();
        assert_eq!(batched(&sizes).concat(), sizes);
    }

    /// What a type's parameters come to, for comparing two catalogs.
    fn parameters_of(catalog: &Catalog) -> BTreeMap<String, String> {
        catalog
            .types
            .iter()
            .map(|(name, model)| (name.clone(), format!("{:?}", model.params)))
            .collect()
    }

    #[test]
    fn the_parameters_of_a_catalog_are_what_a_fresh_load_gives_once_a_backfill_is_done() {
        use std::sync::{Arc, RwLock};

        use refhir_database::Database;
        use refhir_sqlite::SqlitePersistence;

        use crate::package::{
            extra_dir, install_package_dir, minir4_dir, publish, run_backfills,
            stage_package_deferred,
        };

        let store = SqlitePersistence::open_in_memory().unwrap();
        let db = Database::open(Arc::new(store.clone())).unwrap();
        let catalog = RwLock::new(Catalog::empty());
        install_package_dir(&store, &db, &catalog, minir4_dir()).unwrap();
        let pending = stage_package_deferred(&store, &db, &catalog, extra_dir())
            .unwrap()
            .expect("the package has pending search parameters");
        publish(&store, &catalog).unwrap();
        pending.create(&store, &db).unwrap();
        assert!(catalog.read().unwrap().has_pending_parameters());

        // The backfill refreshes the parameters in the catalog it is given.
        run_backfills(&store, &db, &catalog).unwrap();
        let kept = catalog.read().unwrap();
        assert!(!kept.has_pending_parameters());
        let fresh = Catalog::open(&store).unwrap();
        assert_eq!(parameters_of(&kept), parameters_of(&fresh));
        assert!(
            kept.get("Patient")
                .unwrap()
                .ready_param("birthdate")
                .is_some(),
            "the parameter the package added is searchable"
        );
    }
}
