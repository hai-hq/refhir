use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermAnswer {
    Yes,
    No,
    /// The value set is missing or its expansion needs a terminology server.
    Unknown,
}

pub trait Terminology {
    fn contains(&self, value_set: &str, system: Option<&str>, code: &str) -> TermAnswer;
}

#[derive(Clone, Debug, Default)]
pub struct MemoryTerminology {
    systems: BTreeMap<String, BTreeSet<String>>,
    sets: BTreeMap<String, ValueSetIndex>,
}

#[derive(Clone, Debug, Default)]
struct ValueSetIndex {
    codes: BTreeSet<(String, String)>,
    bare: BTreeSet<String>,
    whole_systems: BTreeSet<String>,
    includes: Vec<String>,
    unknown: bool,
}

impl MemoryTerminology {
    pub fn from_resources(resources: &[Json]) -> Self {
        let mut terms = Self::default();
        for resource in resources {
            if resource.get("resourceType").and_then(|value| value.as_str()) == Some("CodeSystem") {
                terms.add_system(resource);
            }
        }
        for resource in resources {
            if resource.get("resourceType").and_then(|value| value.as_str()) == Some("ValueSet") {
                terms.add_value_set(resource);
            }
        }
        terms.settle_includes();
        terms
    }

    fn add_system(&mut self, resource: &Json) {
        let Some(url) = resource.get("url").and_then(|value| value.as_str()) else {
            return;
        };
        let mut codes = BTreeSet::new();
        if let Some(concepts) = resource.get("concept").and_then(|value| value.as_array()) {
            collect_concepts(concepts, &mut codes);
        }
        self.systems.insert(canonical(url).to_string(), codes);
    }

    fn add_value_set(&mut self, resource: &Json) {
        let Some(url) = resource.get("url").and_then(|value| value.as_str()) else {
            return;
        };
        let mut index = ValueSetIndex::default();
        if let Some(contains) = resource.pointer("/expansion/contains").and_then(|value| value.as_array()) {
            collect_expansion(contains, &mut index);
        } else if let Some(include) = resource.pointer("/compose/include").and_then(|value| value.as_array()) {
            for item in include {
                Self::add_include(item, &mut index);
            }
        } else {
            index.unknown = true;
        }
        let key = canonical(url).to_string();
        self.sets.insert(key, index);
    }

    fn add_include(include: &Json, index: &mut ValueSetIndex) {
        if include.get("filter").and_then(|value| value.as_array()).is_some_and(|filters| !filters.is_empty()) {
            index.unknown = true;
            return;
        }
        if let Some(nested) = include.get("valueSet").and_then(|value| value.as_array()) {
            for url in nested.iter().filter_map(|item| item.as_str()) {
                index.includes.push(canonical(url).to_string());
            }
        }
        let system = include.get("system").and_then(|value| value.as_str()).map(canonical);
        if let Some(concepts) = include.get("concept").and_then(|value| value.as_array()) {
            for concept in concepts {
                let Some(code) = concept.get("code").and_then(|value| value.as_str()) else {
                    continue;
                };
                match system {
                    Some(system) => {
                        index.codes.insert((system.to_string(), code.to_string()));
                    }
                    None => {
                        index.bare.insert(code.to_string());
                    }
                }
            }
            return;
        }
        if let Some(system) = system {
            index.whole_systems.insert(system.to_string());
        }
    }

    /// Copy nested value sets after every set has an index, so include order does not matter.
    fn settle_includes(&mut self) {
        for _ in 0..self.sets.len().saturating_add(1) {
            let mut progress = false;
            let keys: Vec<String> = self.sets.keys().cloned().collect();
            for key in keys {
                let includes = self
                    .sets
                    .get(&key)
                    .map(|index| index.includes.clone())
                    .unwrap_or_default();
                if includes.is_empty() {
                    continue;
                }
                let mut left = Vec::new();
                let mut codes = BTreeSet::new();
                let mut bare = BTreeSet::new();
                let mut systems = BTreeSet::new();
                let mut unknown = false;
                for url in includes {
                    let Some(existing) = self.sets.get(&url) else {
                        left.push(url);
                        continue;
                    };
                    if !existing.includes.is_empty() {
                        left.push(url);
                        continue;
                    }
                    codes.extend(existing.codes.iter().cloned());
                    bare.extend(existing.bare.iter().cloned());
                    systems.extend(existing.whole_systems.iter().cloned());
                    unknown |= existing.unknown;
                    progress = true;
                }
                if let Some(index) = self.sets.get_mut(&key) {
                    index.codes.extend(codes);
                    index.bare.extend(bare);
                    index.whole_systems.extend(systems);
                    index.unknown |= unknown;
                    index.includes = left;
                }
            }
            if !progress {
                break;
            }
        }
        for index in self.sets.values_mut() {
            if !index.includes.is_empty() {
                index.unknown = true;
            }
        }
    }
}

impl Terminology for MemoryTerminology {
    fn contains(&self, value_set: &str, system: Option<&str>, code: &str) -> TermAnswer {
        let Some(index) = self.sets.get(canonical(value_set)) else {
            return TermAnswer::Unknown;
        };
        if let Some(system) = system.map(canonical) {
            if index.codes.contains(&(system.to_string(), code.to_string())) || index.bare.contains(code) {
                return TermAnswer::Yes;
            }
            if index.whole_systems.contains(system) {
                return match self.systems.get(system) {
                    Some(codes) if codes.contains(code) => TermAnswer::Yes,
                    Some(_) => {
                        if index.unknown {
                            TermAnswer::Unknown
                        } else {
                            TermAnswer::No
                        }
                    }
                    None => TermAnswer::Unknown,
                };
            }
            return if index.unknown {
                TermAnswer::Unknown
            } else {
                TermAnswer::No
            };
        }
        if index.bare.contains(code) || index.codes.iter().any(|(_, item)| item == code) {
            return TermAnswer::Yes;
        }
        let mut unknown = index.unknown;
        for system in &index.whole_systems {
            match self.systems.get(system) {
                Some(codes) if codes.contains(code) => return TermAnswer::Yes,
                Some(_) => {}
                None => unknown = true,
            }
        }
        if unknown {
            TermAnswer::Unknown
        } else {
            TermAnswer::No
        }
    }
}

pub fn canonical(url: &str) -> &str {
    url.split('|').next().unwrap_or(url)
}

fn collect_concepts(concepts: &[Json], codes: &mut BTreeSet<String>) {
    for concept in concepts {
        if let Some(code) = concept.get("code").and_then(|value| value.as_str()) {
            codes.insert(code.to_string());
        }
        if let Some(nested) = concept.get("concept").and_then(|value| value.as_array()) {
            collect_concepts(nested, codes);
        }
    }
}

fn collect_expansion(contains: &[Json], index: &mut ValueSetIndex) {
    for item in contains {
        if let Some(code) = item.get("code").and_then(|value| value.as_str()) {
            match item.get("system").and_then(|value| value.as_str()) {
                Some(system) => {
                    index.codes.insert((canonical(system).to_string(), code.to_string()));
                }
                None => {
                    index.bare.insert(code.to_string());
                }
            }
        }
        if let Some(nested) = item.get("contains").and_then(|value| value.as_array()) {
            collect_expansion(nested, index);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{MemoryTerminology, TermAnswer, Terminology};

    #[test]
    fn nested_value_set_resolves_in_either_order() {
        let child = json!({
            "resourceType": "ValueSet",
            "url": "http://example.org/child",
            "compose": {"include": [{"system": "http://example.org/sys", "concept": [{"code": "a"}]}]}
        });
        let parent = json!({
            "resourceType": "ValueSet",
            "url": "http://example.org/parent",
            "compose": {"include": [{"valueSet": ["http://example.org/child"]}]}
        });
        for resources in [vec![child.clone(), parent.clone()], vec![parent, child]] {
            let terms = MemoryTerminology::from_resources(&resources);
            assert_eq!(
                terms.contains("http://example.org/parent", Some("http://example.org/sys"), "a"),
                TermAnswer::Yes
            );
            assert_eq!(
                terms.contains("http://example.org/parent", Some("http://example.org/sys"), "b"),
                TermAnswer::No
            );
        }
    }
}
