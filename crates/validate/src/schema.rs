use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::sync::Arc;

use refhir_fhirpath::{parse, Expr, Model, ModelBuilder};
use serde_json::Value as Json;

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

impl Structure {
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

#[derive(Clone, Debug)]
pub struct Schema {
    by_url: BTreeMap<String, Structure>,
    type_url: BTreeMap<String, String>,
    model: Model,
    primitive_regex: BTreeMap<String, String>,
}

impl Default for Schema {
    fn default() -> Self {
        Self {
            by_url: BTreeMap::new(),
            type_url: BTreeMap::new(),
            model: Model::new(),
            primitive_regex: BTreeMap::new(),
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
#[derive(Default)]
pub struct SchemaBuilder {
    model: ModelBuilder,
    by_url: BTreeMap<String, Structure>,
    type_url: BTreeMap<String, String>,
    primitive_regex: BTreeMap<String, String>,
    shared: Shared,
}

impl SchemaBuilder {
    pub fn add(&mut self, definition: &Json) {
        self.model.add(definition);
        let Some(structure) = compile_structure(definition, &mut self.shared) else {
            return;
        };
        if structure.kind == "primitive-type" {
            if let Some(pattern) = regex_from(definition) {
                if compiled_regex_ok(&pattern) {
                    self.primitive_regex
                        .insert(structure.type_name.clone(), pattern);
                }
            }
        }
        if structure.derivation != "constraint" {
            self.type_url
                .entry(structure.type_name.clone())
                .or_insert_with(|| structure.url.clone());
        }
        // A later package may repeat a core URL with a root-only snapshot.
        // Keep the definition that actually describes the type.
        let keep_existing = self
            .by_url
            .get(&structure.url)
            .is_some_and(|existing: &Structure| {
                existing.elements.len() >= structure.elements.len()
            });
        if !keep_existing {
            self.by_url.insert(structure.url.clone(), structure);
        }
    }

    pub fn finish(self) -> Schema {
        Schema {
            by_url: self.by_url,
            type_url: self.type_url,
            model: self.model.finish(),
            primitive_regex: self.primitive_regex,
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

    pub fn structure(&self, url: &str) -> Option<&Structure> {
        self.by_url.get(canonical(url))
    }

    pub fn base_type(&self, name: &str) -> Option<&Structure> {
        let url = self.type_url.get(name)?;
        self.by_url.get(url)
    }

    pub fn primitive_regex(&self, code: &str) -> Option<&str> {
        self.primitive_regex.get(code).map(String::as_str)
    }
}

fn canonical(url: &str) -> &str {
    url.split('|').next().unwrap_or(url)
}

fn compile_structure(definition: &Json, shared: &mut Shared) -> Option<Structure> {
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
                .filter_map(|element| compile_element(element, shared))
                .collect()
        })
        .unwrap_or_default();
    Some(Structure {
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
