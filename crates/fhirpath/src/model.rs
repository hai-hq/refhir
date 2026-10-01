//! Element types and the FHIR type hierarchy, built from StructureDefinition snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub struct Model {
    inner: Arc<ModelInner>,
}

#[derive(Clone, Debug, Default)]
struct ModelInner {
    /// `Patient.gender` -> `code`. Choice paths keep the `[x]` suffix.
    elements: BTreeMap<String, Vec<String>>,
    /// Recursive snapshot paths point at the element they repeat. `ValueSet.expansion.contains.contains` -> `ValueSet.expansion.contains`.
    references: BTreeMap<String, String>,
    /// Concrete type -> parent type. `code` -> `string`, `Age` -> `Quantity`.
    parents: BTreeMap<String, String>,
    types: BTreeSet<String>,
}

impl Model {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_definitions(definitions: &[Value]) -> Self {
        let mut inner = ModelInner::default();
        let mut references = Vec::new();
        for definition in definitions {
            let Some(name) = definition.get("name").and_then(|value| value.as_str()) else {
                continue;
            };
            inner.types.insert(name.to_string());
            if let Some(parent) = definition
                .get("baseDefinition")
                .and_then(|value| value.as_str())
                .and_then(|url| url.rsplit('/').next())
            {
                if parent != name {
                    inner.parents.insert(name.to_string(), parent.to_string());
                    inner.types.insert(parent.to_string());
                }
            }
            let Some(elements) = definition.pointer("/snapshot/element").and_then(|value| value.as_array()) else {
                continue;
            };
            for element in elements {
                let Some(path) = element.get("path").and_then(|value| value.as_str()) else {
                    continue;
                };
                let codes = element
                    .get("type")
                    .and_then(|value| value.as_array())
                    .map(|types| {
                        types
                            .iter()
                            .filter_map(|item| item.get("code").and_then(|code| code.as_str()))
                            .map(|code| code.to_string())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if !codes.is_empty() {
                    inner.elements.insert(path.to_string(), codes);
                } else if let Some(reference) = element.get("contentReference").and_then(|value| value.as_str()) {
                    references.push((path.to_string(), reference.trim_start_matches('#').to_string()));
                }
            }
        }
        for (path, target) in references {
            if let Some(types) = inner.elements.get(&target).cloned() {
                inner.elements.insert(path.clone(), types);
            }
            inner.references.insert(path, target);
        }
        Self { inner: Arc::new(inner) }
    }

    /// Follow `contentReference` so a repeated backbone keeps the original element's children.
    pub fn canonical<'a>(&'a self, path: &'a str) -> &'a str {
        let mut current = path;
        for _ in 0..8 {
            match self.inner.references.get(current) {
                Some(target) => current = target,
                None => break,
            }
        }
        current
    }

    pub fn is_type(&self, name: &str) -> bool {
        let name = simple_name(name);
        self.inner.types.contains(name)
    }

    pub fn element_types(&self, path: &str) -> &[String] {
        self.inner.elements.get(self.canonical(path)).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn is_choice(&self, path: &str, name: &str) -> bool {
        self.inner.elements.contains_key(&format!("{path}.{name}[x]"))
            || self.inner.elements.keys().any(|key| key == &format!("{name}[x]"))
    }

    pub fn choice_path(&self, path: &str, name: &str) -> Option<String> {
        let path = self.canonical(path);
        let candidate = if path.is_empty() {
            format!("{name}[x]")
        } else {
            format!("{path}.{name}[x]")
        };
        self.inner.elements.contains_key(&candidate).then_some(candidate)
    }

    pub fn has_descendants(&self, path: &str) -> bool {
        let path = self.canonical(path);
        let prefix = format!("{path}.");
        self.inner.elements.keys().any(|key| key.starts_with(&prefix))
    }

    pub fn known_child(&self, path: &str, name: &str) -> bool {
        let path = self.canonical(path);
        let prefix = if path.is_empty() {
            name.to_string()
        } else {
            format!("{path}.{name}")
        };
        self.inner.elements.keys().any(|key| key == &prefix || key == &format!("{prefix}[x]"))
    }

    /// `code` is a `string`. `boolean` is not a `Boolean`.
    pub fn is_a(&self, concrete: &str, target: &str) -> bool {
        let concrete = simple_name(concrete);
        let target = simple_name(target);
        if concrete == target {
            return true;
        }
        let mut current = concrete;
        let mut guard = 0;
        while let Some(parent) = self.inner.parents.get(current) {
            if parent == target {
                return true;
            }
            current = parent;
            guard += 1;
            if guard > 16 {
                break;
            }
        }
        false
    }
}

pub fn simple_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name).trim_matches('`')
}
