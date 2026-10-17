use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::Hash;
use std::sync::{Arc, Mutex, PoisonError};

use refhir_fhirpath::{parse, Expr, Model, ModelBuilder};
use serde_json::Value as Json;

use crate::cache::Cache;
use crate::outcome::Severity;
use crate::primitive::compiled_regex_ok;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cardinality {
    Zero,
    One,
    Limited(u32),
    Many,
}

impl Cardinality {
    pub fn parse(text: &str) -> Self {
        match text {
            "0" => Cardinality::Zero,
            "1" => Cardinality::One,
            "*" => Cardinality::Many,
            other => other
                .parse::<u32>()
                .map(Cardinality::Limited)
                .unwrap_or(Cardinality::Many),
        }
    }

    pub fn allows(self, count: usize) -> bool {
        match self {
            Cardinality::Zero => count == 0,
            Cardinality::One => count <= 1,
            Cardinality::Limited(max) => count <= max as usize,
            Cardinality::Many => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SliceRules {
    Open,
    Closed,
    OpenAtEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DiscriminatorKind {
    Value,
    Exists,
    Pattern,
    Type,
    Profile,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Discriminator {
    pub kind: DiscriminatorKind,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Slicing {
    pub discriminators: Vec<Discriminator>,
    pub rules: SliceRules,
    pub ordered: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypeRef {
    pub code: String,
    pub target_profiles: Vec<String>,
    pub profiles: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Binding {
    pub strength: String,
    pub value_set: String,
}

impl Binding {
    /// Whether a code is checked against the value set: a `preferred` or `example` binding is
    /// guidance, and nothing is asked of the terminology for it.
    pub fn is_checked(&self) -> bool {
        self.strength != "preferred" && self.strength != "example"
    }
}

#[derive(Clone, Debug)]
pub struct Constraint {
    pub key: String,
    pub severity: Severity,
    pub human: String,
    pub parsed: Option<Expr>,
}

#[derive(Clone, Debug)]
pub struct ElementDef {
    pub id: String,
    pub path: String,
    pub slice_name: Option<String>,
    pub min: u32,
    pub max: Cardinality,
    pub types: Vec<Arc<TypeRef>>,
    pub constraints: Vec<Arc<Constraint>>,
    pub binding: Option<Arc<Binding>>,
    pub fixed: Option<Json>,
    pub pattern: Option<Json>,
    pub min_value: Option<Json>,
    pub max_value: Option<Json>,
    pub slicing: Option<Arc<Slicing>>,
    /// Element id this node reuses (`contentReference` without the leading `#`).
    pub content_reference: Option<String>,
    pub must_support: bool,
}

#[derive(Clone, Debug)]
pub struct Structure {
    pub url: String,
    pub type_name: String,
    pub kind: String,
    pub derivation: String,
    pub elements: Vec<ElementDef>,
}

/// Bytes a parsed JSON value takes, roughly: enough to weigh what a cache holds.
pub(crate) fn json_bytes(value: &Json) -> usize {
    match value {
        Json::String(text) => text.len() + 24,
        Json::Array(items) => 24 + items.iter().map(json_bytes).sum::<usize>(),
        Json::Object(fields) => {
            48 + fields
                .iter()
                .map(|(name, item)| name.len() + 24 + json_bytes(item))
                .sum::<usize>()
        }
        _ => 32,
    }
}

impl ElementDef {
    /// Bytes this element takes, roughly. The types, constraints and bindings it points at are
    /// shared between elements, so only the pointers count.
    fn approx_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.id.len()
            + self.path.len()
            + self.slice_name.as_ref().map_or(0, String::len)
            + self.content_reference.as_ref().map_or(0, String::len)
            + 8 * (self.types.len() + self.constraints.len())
            + [&self.fixed, &self.pattern, &self.min_value, &self.max_value]
                .into_iter()
                .flatten()
                .map(json_bytes)
                .sum::<usize>()
    }
}

impl Structure {
    /// Bytes this structure takes, roughly: what a cache weighs it by.
    pub fn approx_bytes(&self) -> usize {
        self.elements
            .iter()
            .map(ElementDef::approx_bytes)
            .sum::<usize>()
            + self.url.len()
            + self.type_name.len()
            + self.kind.len()
            + self.derivation.len()
            + 128
    }

    pub fn root_path(&self) -> &str {
        self.elements
            .first()
            .map(|element| element.path.as_str())
            .unwrap_or(self.type_name.as_str())
    }

    pub fn base_element(&self, path: &str) -> Option<&ElementDef> {
        self.elements
            .iter()
            .find(|element| element.path == path && element.slice_name.is_none())
    }

    pub fn element_by_id(&self, id: &str) -> Option<&ElementDef> {
        self.elements.iter().find(|element| element.id == id)
    }

    /// Follow `contentReference` so a nested backbone uses the original element's children.
    pub fn canonical_path(&self, path: &str) -> String {
        let mut current = path.to_string();
        for _ in 0..8 {
            let Some(element) = self.element_by_id(&current) else {
                break;
            };
            match &element.content_reference {
                Some(target) => current = target.clone(),
                None => break,
            }
        }
        current
    }

    pub fn direct_children(&self, parent: &str) -> Vec<&ElementDef> {
        let parent = self.canonical_path(parent);
        let mut seen = Vec::new();
        let mut children = Vec::new();
        for element in &self.elements {
            if element.slice_name.is_some() || !is_direct_child(&parent, &element.path) {
                continue;
            }
            if seen.iter().any(|path| path == &element.path) {
                continue;
            }
            seen.push(element.path.clone());
            children.push(element);
        }
        children
    }

    pub fn slices(&self, path: &str) -> Vec<(usize, &ElementDef)> {
        self.elements
            .iter()
            .enumerate()
            .filter(|(_, element)| element.path == path && element.slice_name.is_some())
            .collect()
    }

    pub fn slice_children(&self, slice_index: usize) -> Vec<&ElementDef> {
        let slice = &self.elements[slice_index];
        let prefix = format!("{}.", slice.path);
        let mut children = Vec::new();
        for element in self.elements.iter().skip(slice_index + 1) {
            if element.path == slice.path {
                break;
            }
            if element.path.starts_with(&prefix) && is_direct_child(&slice.path, &element.path) {
                children.push(element);
            }
        }
        children
    }
}

pub fn is_direct_child(parent: &str, path: &str) -> bool {
    let Some(rest) = path.strip_prefix(parent) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix('.') else {
        return false;
    };
    !rest.is_empty() && !rest.contains('.')
}

/// Where a [`Schema`] reads the definitions it holds only an index of.
pub trait DefinitionSource: Send + Sync {
    /// The StructureDefinition with this URL that package `package` stores, as JSON.
    fn definition(&self, package: i64, url: &str) -> Option<Json>;
}

/// What a schema holds of one structure.
#[derive(Clone, Debug)]
enum Entry {
    Held(Arc<Structure>),
    /// Read from the source when first used. `elements` is how many elements it has, which says
    /// which of two definitions of one URL describes the type better, as it does for a held one.
    Stored {
        package: i64,
        elements: usize,
    },
}

impl Entry {
    fn elements(&self) -> usize {
        match self {
            Entry::Held(structure) => structure.elements.len(),
            Entry::Stored { elements, .. } => *elements,
        }
    }
}

/// How a schema that holds an index reads what it does not hold: the source, a cache of what it
/// read, and the constraints and types the structures read have in common.
struct Loading {
    source: Arc<dyn DefinitionSource>,
    cache: Cache<String, Structure>,
    shared: Mutex<Shared>,
}

impl std::fmt::Debug for Loading {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Loading")
    }
}

/// What the structures a schema has read may weigh together. The common types are a few hundred
/// kilobytes each; a request touches a few dozen of the 3,600 definitions of the R4 packages.
const STRUCTURE_CACHE_BYTES: usize = 8 << 20;

#[derive(Clone, Debug)]
pub struct Schema {
    by_url: BTreeMap<String, Entry>,
    type_url: BTreeMap<String, String>,
    model: Model,
    primitive_regex: BTreeMap<String, String>,
    lazy: Option<Arc<Loading>>,
}

impl Default for Schema {
    fn default() -> Self {
        Self {
            by_url: BTreeMap::new(),
            type_url: BTreeMap::new(),
            model: Model::new(),
            primitive_regex: BTreeMap::new(),
            lazy: None,
        }
    }
}

/// The constraints, types, bindings and slicings the elements of a schema have, each kept once.
///
/// Elements repeat them again and again: the R4 packages hold about 66,000 constraints of which
/// 299 differ (`ele-1` is on every element of every definition), and about 91,000 type references
/// of which 2,100 differ. Giving every element its own copy, with its own parsed FHIRPath tree,
/// made them most of what a schema takes.
#[derive(Default)]
struct Shared {
    /// By key, severity, human text and expression, so that one that is already held is not
    /// parsed again.
    constraints: HashMap<(String, String, String, String), Arc<Constraint>>,
    types: HashSet<Arc<TypeRef>>,
    bindings: HashSet<Arc<Binding>>,
    slicings: HashSet<Arc<Slicing>>,
}

/// The shared copy of `value`: the one already held when there is one.
fn share<T: Hash + Eq>(held: &mut HashSet<Arc<T>>, value: T) -> Arc<T> {
    if let Some(existing) = held.get(&value) {
        return existing.clone();
    }
    let shared = Arc::new(value);
    held.insert(shared.clone());
    shared
}

/// Builds a [`Schema`] from StructureDefinitions handed over one at a time, so a caller that
/// reads its definitions one by one does not have to keep them all parsed at once.
///
/// A builder made by [`SchemaBuilder::lazy`] goes further: it keeps no structure, only what
/// says where each is stored, and the schema it finishes reads a structure when it is first
/// used. The R4 packages hold 3,600 definitions of which a server uses a few dozen.
#[derive(Default)]
pub struct SchemaBuilder {
    model: ModelBuilder,
    by_url: BTreeMap<String, Entry>,
    type_url: BTreeMap<String, String>,
    primitive_regex: BTreeMap<String, String>,
    shared: Shared,
    source: Option<Arc<dyn DefinitionSource>>,
    /// What the finished schema's cache may weigh, when not the default.
    budget: Option<usize>,
}

/// What a StructureDefinition says about itself before its elements are compiled.
struct Head {
    url: String,
    type_name: String,
    kind: String,
    derivation: String,
    /// How many elements the structure has once compiled.
    elements: usize,
}

impl SchemaBuilder {
    /// A builder that keeps an index, with `source` to read each structure from when it is used.
    pub fn lazy(source: Arc<dyn DefinitionSource>) -> Self {
        Self {
            source: Some(source),
            ..Self::default()
        }
    }

    /// Let the finished schema's cache weigh at most `bytes`.
    #[cfg(test)]
    fn cache_bytes(mut self, bytes: usize) -> Self {
        self.budget = Some(bytes);
        self
    }

    /// Add a definition and keep the structure compiled from it.
    pub fn add(&mut self, definition: &Json) {
        self.model.add(definition);
        let Some(structure) = compile_structure(definition, &mut self.shared) else {
            return;
        };
        let head = Head {
            url: structure.url.clone(),
            type_name: structure.type_name.clone(),
            kind: structure.kind.clone(),
            derivation: structure.derivation.clone(),
            elements: structure.elements.len(),
        };
        self.index(definition, &head, Entry::Held(Arc::new(structure)));
    }

    /// Add a definition that package `package` stores. On a builder made by [`Self::lazy`] only an
    /// index of it is kept, except for a base type (a resource, a data type or a primitive: what
    /// `derivation` calls a specialization), which is compiled and kept. Every request is
    /// validated against base types, so the first request of each kind must not wait for one to
    /// be read; they are about a quarter of the definitions of the R4 packages and a few
    /// megabytes. A profile, an extension or anything else is read when it is used. On a builder
    /// that is not lazy every structure is compiled and kept, as [`Self::add`] does.
    pub fn add_stored(&mut self, package: i64, definition: &Json) {
        if self.source.is_none() {
            self.add(definition);
            return;
        }
        let Some(head) = head_of(definition) else {
            self.model.add(definition);
            return;
        };
        if head.derivation == "specialization" {
            self.add(definition);
            return;
        }
        self.model.add(definition);
        let elements = head.elements;
        self.index(definition, &head, Entry::Stored { package, elements });
    }

    fn index(&mut self, definition: &Json, head: &Head, entry: Entry) {
        if head.kind == "primitive-type" {
            if let Some(pattern) = regex_from(definition) {
                if compiled_regex_ok(&pattern) {
                    self.primitive_regex.insert(head.type_name.clone(), pattern);
                }
            }
        }
        if head.derivation != "constraint" {
            self.type_url
                .entry(head.type_name.clone())
                .or_insert_with(|| head.url.clone());
        }
        // A later package may repeat a core URL with a root-only snapshot.
        // Keep the definition that actually describes the type.
        let keep_existing = self
            .by_url
            .get(&head.url)
            .is_some_and(|existing| existing.elements() >= head.elements);
        if !keep_existing {
            self.by_url.insert(head.url.clone(), entry);
        }
    }

    pub fn finish(self) -> Schema {
        let Self {
            model,
            by_url,
            type_url,
            primitive_regex,
            shared,
            source,
            budget,
        } = self;
        Schema {
            by_url,
            type_url,
            model: model.finish(),
            primitive_regex,
            lazy: source.map(|source| {
                Arc::new(Loading {
                    source,
                    cache: Cache::new(budget.unwrap_or(STRUCTURE_CACHE_BYTES)),
                    shared: Mutex::new(shared),
                })
            }),
        }
    }
}

impl Schema {
    pub fn from_definitions(definitions: &[Json]) -> Self {
        let mut builder = SchemaBuilder::default();
        for definition in definitions {
            builder.add(definition);
        }
        builder.finish()
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    /// The structure with this URL (a `|version` is ignored). One the schema holds only an index
    /// of is read now, and kept for the next request while it is among the most recently used.
    pub fn structure(&self, url: &str) -> Option<Arc<Structure>> {
        let url = canonical(url);
        match self.by_url.get(url)? {
            Entry::Held(structure) => Some(structure.clone()),
            Entry::Stored { package, .. } => {
                let loading = self.lazy.as_ref()?;
                loading.cache.get_or_load(
                    url,
                    |url| url.to_string(),
                    || {
                        let definition = loading.source.definition(*package, url)?;
                        let structure = {
                            let mut shared = loading
                                .shared
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner);
                            compile_structure(&definition, &mut shared)?
                        };
                        let weight = structure.approx_bytes();
                        Some((structure, weight))
                    },
                )
            }
        }
    }

    pub fn base_type(&self, name: &str) -> Option<Arc<Structure>> {
        let url = self.type_url.get(name)?;
        self.structure(url)
    }

    pub fn primitive_regex(&self, code: &str) -> Option<&str> {
        self.primitive_regex.get(code).map(String::as_str)
    }

    /// The value sets that a code is checked against, as the structures held here bind them
    /// (see [`Binding::is_checked`]). A terminology that reads when used is told to keep what
    /// these draw on, so a request does not wait for it.
    pub fn checked_value_sets(&self) -> BTreeSet<String> {
        self.by_url
            .values()
            .filter_map(|entry| match entry {
                Entry::Held(structure) => Some(structure),
                Entry::Stored { .. } => None,
            })
            .flat_map(|structure| structure.elements.iter())
            .filter_map(|element| element.binding.as_ref())
            .filter(|binding| binding.is_checked())
            .map(|binding| binding.value_set.clone())
            .collect()
    }

    /// The URLs of the structures read from the source lately and still kept, which a schema
    /// that replaces this one can be told to read at once (see [`Self::warm`]).
    pub fn resident(&self) -> Vec<String> {
        self.lazy
            .as_ref()
            .map(|loading| loading.cache.keys())
            .unwrap_or_default()
    }

    /// Read the structures at `urls` now, so that the requests that used them before do not wait
    /// for them again. A URL this schema has no structure for is passed over.
    pub fn warm(&self, urls: &[String]) {
        for url in urls {
            let _ = self.structure(url);
        }
    }
}

fn canonical(url: &str) -> &str {
    url.split('|').next().unwrap_or(url)
}

/// What `definition` says about itself, or `None` when it names neither a URL nor a type.
fn head_of(definition: &Json) -> Option<Head> {
    let url = definition.get("url")?.as_str()?.to_string();
    let type_name = definition
        .get("type")
        .and_then(|value| value.as_str())
        .or_else(|| definition.get("name").and_then(|value| value.as_str()))?
        .to_string();
    let elements = definition
        .pointer("/snapshot/element")
        .and_then(|value| value.as_array())
        .map(|elements| {
            elements
                .iter()
                .filter(|element| element.get("path").is_some_and(Json::is_string))
                .count()
        })
        .unwrap_or(0);
    Some(Head {
        url,
        type_name,
        kind: definition
            .get("kind")
            .and_then(|value| value.as_str())
            .unwrap_or("resource")
            .to_string(),
        derivation: definition
            .get("derivation")
            .and_then(|value| value.as_str())
            .unwrap_or("specialization")
            .to_string(),
        elements,
    })
}

fn compile_structure(definition: &Json, shared: &mut Shared) -> Option<Structure> {
    let head = head_of(definition)?;
    let elements = definition
        .pointer("/snapshot/element")
        .and_then(|value| value.as_array())
        .map(|elements| {
            elements
                .iter()
                .filter_map(|element| compile_element(element, shared))
                .collect()
        })
        .unwrap_or_default();
    Some(Structure {
        url: head.url,
        type_name: head.type_name,
        kind: head.kind,
        derivation: head.derivation,
        elements,
    })
}

fn compile_element(element: &Json, shared: &mut Shared) -> Option<ElementDef> {
    let path = element.get("path")?.as_str()?.to_string();
    let id = element
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or(path.as_str())
        .to_string();
    let slice_name = element
        .get("sliceName")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string());
    let min = element
        .get("min")
        .and_then(|value| value.as_u64())
        .unwrap_or(0) as u32;
    let max = Cardinality::parse(
        element
            .get("max")
            .and_then(|value| value.as_str())
            .unwrap_or("*"),
    );
    let types = element
        .get("type")
        .and_then(|value| value.as_array())
        .map(|types| {
            types
                .iter()
                .filter_map(compile_type)
                .map(|type_ref| share(&mut shared.types, type_ref))
                .collect()
        })
        .unwrap_or_default();
    let constraints = element
        .get("constraint")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| shared_constraint(item, &mut shared.constraints))
                .collect()
        })
        .unwrap_or_default();
    let binding = element
        .get("binding")
        .and_then(compile_binding)
        .map(|binding| share(&mut shared.bindings, binding));
    let content_reference = element
        .get("contentReference")
        .and_then(|value| value.as_str())
        .map(|value| value.trim_start_matches('#').to_string());
    Some(ElementDef {
        id,
        path,
        slice_name,
        min,
        max,
        types,
        constraints,
        binding,
        fixed: prefixed(element, "fixed"),
        pattern: prefixed(element, "pattern"),
        min_value: prefixed(element, "minValue"),
        max_value: prefixed(element, "maxValue"),
        slicing: element
            .get("slicing")
            .and_then(compile_slicing)
            .map(|slicing| share(&mut shared.slicings, slicing)),
        content_reference,
        must_support: element
            .get("mustSupport")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
    })
}

fn compile_type(value: &Json) -> Option<TypeRef> {
    let code = value.get("code")?.as_str()?.to_string();
    let target_profiles = canonical_list(value.get("targetProfile"));
    let profiles = canonical_list(value.get("profile"));
    Some(TypeRef {
        code,
        target_profiles,
        profiles,
    })
}

fn canonical_list(value: Option<&Json>) -> Vec<String> {
    match value {
        Some(Json::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(|url| canonical(url).to_string()))
            .collect(),
        Some(Json::String(url)) => vec![canonical(url).to_string()],
        _ => Vec::new(),
    }
}

/// The constraint `value` describes, compiled the first time it is seen and shared after.
fn shared_constraint(
    value: &Json,
    held: &mut HashMap<(String, String, String, String), Arc<Constraint>>,
) -> Option<Arc<Constraint>> {
    let text = |name: &str| {
        value
            .get(name)
            .and_then(|item| item.as_str())
            .unwrap_or("")
            .to_string()
    };
    let key = (
        text("key"),
        text("severity"),
        text("human"),
        text("expression"),
    );
    if let Some(existing) = held.get(&key) {
        return Some(existing.clone());
    }
    let constraint = Arc::new(compile_constraint(value)?);
    held.insert(key, constraint.clone());
    Some(constraint)
}

fn compile_constraint(value: &Json) -> Option<Constraint> {
    let expression = value.get("expression").and_then(|item| item.as_str())?;
    let parsed = parse(expression).ok();
    Some(Constraint {
        key: value
            .get("key")
            .and_then(|item| item.as_str())
            .unwrap_or("")
            .to_string(),
        severity: Severity::parse(
            value
                .get("severity")
                .and_then(|item| item.as_str())
                .unwrap_or("error"),
        ),
        human: value
            .get("human")
            .and_then(|item| item.as_str())
            .unwrap_or(expression)
            .to_string(),
        parsed,
    })
}

fn compile_binding(value: &Json) -> Option<Binding> {
    let value_set = value.get("valueSet").and_then(|item| item.as_str())?;
    Some(Binding {
        strength: value
            .get("strength")
            .and_then(|item| item.as_str())
            .unwrap_or("example")
            .to_string(),
        value_set: canonical(value_set).to_string(),
    })
}

fn compile_slicing(value: &Json) -> Option<Slicing> {
    let discriminators = value
        .get("discriminator")
        .and_then(|item| item.as_array())
        .map(|items| items.iter().filter_map(compile_discriminator).collect())
        .unwrap_or_default();
    let rules = match value.get("rules").and_then(|item| item.as_str()) {
        Some("closed") => SliceRules::Closed,
        Some("openAtEnd") => SliceRules::OpenAtEnd,
        _ => SliceRules::Open,
    };
    Some(Slicing {
        discriminators,
        rules,
        ordered: value
            .get("ordered")
            .and_then(|item| item.as_bool())
            .unwrap_or(false),
    })
}

fn compile_discriminator(value: &Json) -> Option<Discriminator> {
    let kind = match value.get("type")?.as_str()? {
        "value" => DiscriminatorKind::Value,
        "exists" => DiscriminatorKind::Exists,
        "pattern" => DiscriminatorKind::Pattern,
        "type" => DiscriminatorKind::Type,
        "profile" => DiscriminatorKind::Profile,
        _ => return None,
    };
    Some(Discriminator {
        kind,
        path: value.get("path")?.as_str()?.to_string(),
    })
}

fn prefixed(element: &Json, prefix: &str) -> Option<Json> {
    let object = element.as_object()?;
    object.iter().find_map(|(key, value)| {
        if key.starts_with(prefix) && key.len() > prefix.len() {
            Some(value.clone())
        } else {
            None
        }
    })
}

fn regex_from(definition: &Json) -> Option<String> {
    let mut found = None;
    walk_regex(definition, &mut found);
    found
}

fn walk_regex(value: &Json, found: &mut Option<String>) {
    if found.is_some() {
        return;
    }
    match value {
        Json::Array(items) => {
            for item in items {
                walk_regex(item, found);
            }
        }
        Json::Object(object) => {
            if let Some(extensions) = object.get("extension").and_then(|item| item.as_array()) {
                for extension in extensions {
                    let url = extension
                        .get("url")
                        .and_then(|item| item.as_str())
                        .unwrap_or("");
                    if url.ends_with("structuredefinition-regex") {
                        if let Some(pattern) =
                            extension.get("valueString").and_then(|item| item.as_str())
                        {
                            *found = Some(pattern.to_string());
                            return;
                        }
                    }
                }
            }
            for child in object.values() {
                walk_regex(child, found);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet as Names;

    use serde_json::json;

    use super::*;

    /// The definitions of some packages, by package id, as a store would hold them.
    struct Packages(Vec<(i64, Json)>);

    impl DefinitionSource for Packages {
        fn definition(&self, package: i64, url: &str) -> Option<Json> {
            self.0
                .iter()
                .find(|(held, definition)| *held == package && definition["url"] == url)
                .map(|(_, definition)| definition.clone())
        }
    }

    fn definition(url: &str, name: &str, kind: &str, elements: &[(&str, &str)]) -> Json {
        let elements: Vec<Json> = elements
            .iter()
            .map(|(path, code)| {
                json!({"path": path, "min": 0, "max": "1", "type": [{"code": code}],
                       "constraint": [{"key": "ele-1", "severity": "error",
                                       "human": "no empty elements", "expression": "hasValue()"}]})
            })
            .collect();
        json!({"resourceType": "StructureDefinition", "url": url, "name": name, "type": name,
               "kind": kind, "snapshot": {"element": elements}})
    }

    /// What a store holds of some packages: a type, a second package that repeats its URL with
    /// less (and with the same, which the first keeps), a profile of it, a primitive with a
    /// pattern, and an extension.
    fn packages() -> Vec<(i64, Json)> {
        let patient = definition(
            "http://hl7.org/fhir/StructureDefinition/Patient",
            "Patient",
            "resource",
            &[
                ("Patient", "Patient"),
                ("Patient.active", "boolean"),
                ("Patient.gender", "code"),
            ],
        );
        let mut root_only = patient.clone();
        root_only["snapshot"]["element"] = json!([{"path": "Patient"}]);
        let mut tie = patient.clone();
        tie["snapshot"]["element"][1]["max"] = json!("*");
        let mut profile = definition(
            "http://example.org/StructureDefinition/my-patient",
            "MyPatient",
            "resource",
            &[("Patient", "Patient"), ("Patient.active", "boolean")],
        );
        profile["type"] = json!("Patient");
        profile["derivation"] = json!("constraint");
        let mut id = definition(
            "http://hl7.org/fhir/StructureDefinition/id",
            "id",
            "primitive-type",
            &[("id", "string")],
        );
        id["snapshot"]["element"][0]["extension"] = json!([{
            "url": "http://hl7.org/fhir/StructureDefinition/structuredefinition-regex",
            "valueString": "[A-Za-z0-9\\-\\.]{1,64}"
        }]);
        let extension = definition(
            "http://hl7.org/fhir/StructureDefinition/Extension",
            "Extension",
            "complex-type",
            &[("Extension", "Extension"), ("Extension.url", "uri")],
        );
        vec![
            (1, patient),
            (2, root_only),
            (3, tie),
            (2, profile),
            (1, id),
            (1, extension),
            (
                1,
                json!({"resourceType": "StructureDefinition", "name": "no url"}),
            ),
        ]
    }

    fn eager(packages: &[(i64, Json)]) -> Schema {
        let definitions: Vec<Json> = packages
            .iter()
            .map(|(_, definition)| definition.clone())
            .collect();
        Schema::from_definitions(&definitions)
    }

    fn lazy(packages: &[(i64, Json)], budget: usize) -> Schema {
        let mut builder =
            SchemaBuilder::lazy(Arc::new(Packages(packages.to_vec()))).cache_bytes(budget);
        for (package, definition) in packages {
            builder.add_stored(*package, definition);
        }
        builder.finish()
    }

    fn assert_same(eager: &Schema, lazy: &Schema) {
        for url in [
            "http://hl7.org/fhir/StructureDefinition/Patient",
            "http://hl7.org/fhir/StructureDefinition/Patient|4.0.1",
            "http://example.org/StructureDefinition/my-patient",
            "http://hl7.org/fhir/StructureDefinition/id",
            "http://hl7.org/fhir/StructureDefinition/Extension",
            "http://example.org/nothing",
        ] {
            assert_eq!(
                format!("{:?}", eager.structure(url)),
                format!("{:?}", lazy.structure(url)),
                "{url}"
            );
        }
        for name in ["Patient", "MyPatient", "id", "Extension", "Nothing"] {
            assert_eq!(
                format!("{:?}", eager.base_type(name)),
                format!("{:?}", lazy.base_type(name)),
                "{name}"
            );
        }
        for code in ["id", "string", "Patient"] {
            assert_eq!(eager.primitive_regex(code), lazy.primitive_regex(code));
        }
        assert!(eager.model().is_type("Patient") && lazy.model().is_type("Patient"));
    }

    #[test]
    fn a_schema_that_reads_its_structures_gives_what_one_that_holds_them_gives() {
        let packages = packages();
        let eager = eager(&packages);
        assert_same(&eager, &lazy(&packages, STRUCTURE_CACHE_BYTES));
        // The tie goes to the package loaded first, and a root-only copy never replaces a
        // fuller one, in both.
        let patient = lazy(&packages, STRUCTURE_CACHE_BYTES)
            .structure("http://hl7.org/fhir/StructureDefinition/Patient")
            .unwrap();
        assert_eq!(patient.elements.len(), 3);
        assert_eq!(patient.elements[1].max, Cardinality::One);
    }

    #[test]
    fn what_is_read_again_after_the_cache_let_it_go_is_the_same() {
        let packages = packages();
        let eager = eager(&packages);
        // A budget that holds almost nothing: every other lookup reads from the source.
        let tight = lazy(&packages, 1);
        for _ in 0..3 {
            assert_same(&eager, &tight);
        }
        let held = lazy(&packages, STRUCTURE_CACHE_BYTES);
        let url = "http://hl7.org/fhir/StructureDefinition/Patient";
        assert!(Arc::ptr_eq(
            &held.structure(url).unwrap(),
            &held.structure(url).unwrap()
        ));
    }

    #[test]
    fn a_definition_the_source_does_not_have_is_not_found() {
        let packages = packages();
        let mut builder = SchemaBuilder::lazy(Arc::new(Packages(Vec::new())));
        for (package, definition) in &packages {
            builder.add_stored(*package, definition);
        }
        let schema = builder.finish();
        // A profile is read when used, and the source has no such row. A base type is held, so
        // it never asks.
        assert!(schema
            .structure("http://example.org/StructureDefinition/my-patient")
            .is_none());
        assert!(schema
            .structure("http://hl7.org/fhir/StructureDefinition/Patient")
            .is_some());
    }

    #[test]
    fn a_builder_without_a_source_keeps_what_it_is_given() {
        let packages = packages();
        let mut builder = SchemaBuilder::default();
        for (package, definition) in &packages {
            builder.add_stored(*package, definition);
        }
        assert_same(&eager(&packages), &builder.finish());
    }

    /// A source that counts what it is asked for.
    struct Counted {
        inner: Packages,
        reads: std::sync::atomic::AtomicUsize,
    }

    impl DefinitionSource for Counted {
        fn definition(&self, package: i64, url: &str) -> Option<Json> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.definition(package, url)
        }
    }

    #[test]
    fn base_types_are_held_and_the_rest_is_read_when_used() {
        use std::sync::atomic::Ordering::SeqCst;
        let packages = packages();
        let source = Arc::new(Counted {
            inner: Packages(packages.clone()),
            reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut builder = SchemaBuilder::lazy(source.clone());
        for (package, definition) in &packages {
            builder.add_stored(*package, definition);
        }
        let schema = builder.finish();
        assert_eq!(source.reads.load(SeqCst), 0, "nothing is read to build it");
        // A resource, a data type, a primitive: held, so asking for them reads nothing.
        for url in [
            "http://hl7.org/fhir/StructureDefinition/Patient",
            "http://hl7.org/fhir/StructureDefinition/id",
            "http://hl7.org/fhir/StructureDefinition/Extension",
        ] {
            assert!(schema.structure(url).is_some(), "{url}");
        }
        assert!(schema.base_type("Patient").is_some());
        assert_eq!(source.reads.load(SeqCst), 0, "base types are held");
        // A profile is read, once.
        let profile = "http://example.org/StructureDefinition/my-patient";
        assert!(schema.structure(profile).is_some());
        assert!(schema.structure(profile).is_some());
        assert_eq!(source.reads.load(SeqCst), 1);
        // It is what a schema that holds everything holds.
        assert_same(&eager(&packages), &schema);
    }

    #[test]
    fn what_is_read_can_be_read_again_by_a_schema_that_replaces_this_one() {
        use std::sync::atomic::Ordering::SeqCst;
        let packages = packages();
        let build = || {
            let source = Arc::new(Counted {
                inner: Packages(packages.clone()),
                reads: std::sync::atomic::AtomicUsize::new(0),
            });
            let mut builder = SchemaBuilder::lazy(source.clone());
            for (package, definition) in &packages {
                builder.add_stored(*package, definition);
            }
            (builder.finish(), source)
        };
        let (old, _) = build();
        let profile = "http://example.org/StructureDefinition/my-patient";
        assert!(old.structure(profile).is_some());
        assert_eq!(old.resident(), [profile.to_string()]);
        let (new, source) = build();
        assert!(new.resident().is_empty());
        new.warm(&old.resident());
        assert_eq!(source.reads.load(SeqCst), 1);
        assert_eq!(new.resident(), [profile.to_string()]);
        // Asking for it now reads nothing, and a URL that is gone is passed over.
        assert!(new.structure(profile).is_some());
        new.warm(&["http://example.org/gone".to_string()]);
        assert_eq!(source.reads.load(SeqCst), 1);
    }

    #[test]
    fn the_value_sets_a_held_structure_is_checked_against_are_listed() {
        let mut patient = definition(
            "http://hl7.org/fhir/StructureDefinition/Patient",
            "Patient",
            "resource",
            &[("Patient", "Patient"), ("Patient.gender", "code")],
        );
        patient["snapshot"]["element"][1]["binding"] =
            json!({"strength": "required", "valueSet": "http://example.org/vs/gender|4.0.1"});
        let mut preferred = patient.clone();
        preferred["snapshot"]["element"][1]["binding"] =
            json!({"strength": "preferred", "valueSet": "http://example.org/vs/language"});
        preferred["url"] = json!("http://hl7.org/fhir/StructureDefinition/Other");
        preferred["name"] = json!("Other");
        preferred["type"] = json!("Other");
        let mut profile = patient.clone();
        profile["url"] = json!("http://example.org/StructureDefinition/p");
        profile["derivation"] = json!("constraint");
        profile["snapshot"]["element"][1]["binding"] =
            json!({"strength": "extensible", "valueSet": "http://example.org/vs/profiled"});
        let packages = vec![(1, patient), (1, preferred), (1, profile)];
        let mut builder = SchemaBuilder::lazy(Arc::new(Packages(packages.clone())));
        for (package, definition) in &packages {
            builder.add_stored(*package, definition);
        }
        // What a held base type binds and a code is checked against; not a `preferred` binding,
        // and not a profile, which is not held.
        assert_eq!(
            builder.finish().checked_value_sets(),
            Names::from(["http://example.org/vs/gender".to_string()])
        );
    }
}
