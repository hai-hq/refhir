use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value as Json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermAnswer {
    Yes,
    No,
    /// The value set has no stored expansion.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeDescription {
    pub display: Option<String>,
    pub definition: Option<String>,
    pub inactive: bool,
    pub properties: Vec<(String, String)>,
}

pub trait Terminology {
    fn contains(&self, value_set: &str, system: Option<&str>, code: &str) -> TermAnswer;

    fn describe(&self, _system: &str, _code: &str) -> Option<CodeDescription> {
        None
    }

    /// Display and inactive state for a code that is already in `value_set`.
    /// A missing system uses the expansion when that code belongs to one system.
    fn describe_bound(
        &self,
        value_set: &str,
        system: Option<&str>,
        code: &str,
    ) -> Option<CodeDescription> {
        let _ = value_set;
        self.describe(system?, code)
    }
}

#[derive(Clone, Debug, Default)]
struct Concept {
    display: Option<String>,
    definition: Option<String>,
    inactive: bool,
    parents: Vec<String>,
    properties: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
struct SystemIndex {
    version: Option<String>,
    /// A complete system is closed. A missing code is really absent.
    closed: bool,
    name: Option<String>,
    codes: BTreeMap<String, Concept>,
}

#[derive(Clone, Debug, Default)]
struct Expansion {
    systems: BTreeSet<(String, String)>,
    codes: BTreeMap<(String, String), Option<String>>,
}

#[derive(Clone, Debug)]
struct Target {
    map_url: String,
    system: String,
    code: String,
    equivalence: String,
}

#[derive(Clone, Debug, Default)]
pub struct MemoryTerminology {
    systems: BTreeMap<String, SystemIndex>,
    expansions: BTreeMap<String, Expansion>,
    /// Source system and code to translation targets.
    maps: BTreeMap<(String, String), Vec<Target>>,
}

impl MemoryTerminology {
    pub fn from_resources(resources: &[Json]) -> Self {
        let (mut terms, value_sets) = index_resources(resources);
        let mut expander = Expander {
            systems: &terms.systems,
            value_sets: &value_sets,
            cache: BTreeMap::new(),
            stack: Vec::new(),
        };
        for url in value_sets.keys() {
            if let Some(expansion) = expander.expand(url) {
                terms.expansions.insert(url.clone(), expansion);
            }
        }
        terms
    }

    /// Membership comes from `expansions`. Code systems and concept maps still come from `resources`.
    pub fn from_stored(resources: &[Json], expansions: &[Json]) -> Self {
        let (mut terms, _) = index_resources(resources);
        for resource in resources {
            if resource.get("resourceType").and_then(|value| value.as_str()) != Some("ValueSet") {
                continue;
            }
            if let Some((url, expansion)) = resource
                .pointer("/expansion/contains")
                .and_then(|value| value.as_array())
                .map(|contains| {
                    (
                        resource
                            .get("url")
                            .and_then(|value| value.as_str())
                            .unwrap_or("")
                            .to_string(),
                        expansion_from_contains(contains),
                    )
                })
            {
                if !url.is_empty() {
                    terms.expansions.insert(canonical(&url).to_string(), expansion);
                }
            }
        }
        for document in expansions {
            if let Some((url, expansion)) = expansion_from_document(document) {
                terms.expansions.insert(url, expansion);
            }
        }
        terms
    }

    pub fn expansion_documents(resources: &[Json]) -> Vec<Json> {
        let terms = Self::from_resources(resources);
        let mut documents = Vec::new();
        for (url, expansion) in &terms.expansions {
            documents.push(expansion_document(url, expansion));
        }
        documents
    }

    pub fn expand(&self, url: &str, text_filter: Option<&str>) -> Option<Json> {
        let expansion = self.expansions.get(canonical(url))?;
        let mut contains = Vec::new();
        for ((system, code), display) in &expansion.codes {
            if let Some(filter) = text_filter {
                let display_text = display.as_deref().unwrap_or("");
                let needle = filter.to_ascii_lowercase();
                if !code.to_ascii_lowercase().contains(&needle)
                    && !display_text.to_ascii_lowercase().contains(&needle)
                {
                    continue;
                }
            }
            let mut entry = json!({"code": code});
            if !system.is_empty() {
                entry["system"] = json!(system);
            }
            if let Some(display) = display {
                entry["display"] = json!(display);
            }
            contains.push(entry);
        }
        Some(json!({
            "resourceType": "ValueSet",
            "url": canonical(url),
            "expansion": {"contains": contains}
        }))
    }

    pub fn validate_code(
        &self,
        value_set: Option<&str>,
        system: Option<&str>,
        code: &str,
        display: Option<&str>,
    ) -> Json {
        if let Some(url) = value_set {
            return match self.contains(url, system, code) {
                TermAnswer::Yes => code_parameters(
                    true,
                    display_of_bound(self, url, system, code, display),
                    None,
                ),
                TermAnswer::No => {
                    code_parameters(false, None, Some("code is not in the value set"))
                }
                TermAnswer::Unknown => json!({
                    "resourceType": "OperationOutcome",
                    "issue": [{
                        "severity": "warning",
                        "code": "not-supported",
                        "diagnostics": format!("value set {url} is not expanded")
                    }]
                }),
            };
        }
        let Some(system) = system else {
            return code_parameters(false, None, Some("system is required"));
        };
        match self.systems.get(canonical(system)) {
            Some(index) if index.codes.contains_key(code) => {
                code_parameters(true, display_of(self, Some(system), code, display), None)
            }
            Some(index) if index.closed => {
                code_parameters(false, None, Some("code is not in the code system"))
            }
            Some(_) => json!({
                "resourceType": "OperationOutcome",
                "issue": [{
                    "severity": "warning",
                    "code": "not-supported",
                    "diagnostics": "code system is not complete"
                }]
            }),
            None => json!({
                "resourceType": "OperationOutcome",
                "issue": [{
                    "severity": "warning",
                    "code": "not-found",
                    "diagnostics": format!("code system {system} is not loaded")
                }]
            }),
        }
    }

    /// `ancestor` subsumes `code` when they are equal or `ancestor` is a parent.
    pub fn subsumes(&self, system: &str, ancestor: &str, code: &str) -> bool {
        let Some(index) = self.systems.get(canonical(system)) else {
            return false;
        };
        is_a(index, code, ancestor)
    }

    pub fn expand_page(
        &self,
        url: &str,
        text_filter: Option<&str>,
        offset: usize,
        count: Option<usize>,
    ) -> Option<Json> {
        let mut expanded = self.expand(url, text_filter)?;
        let Some(contains) = expanded
            .pointer_mut("/expansion/contains")
            .and_then(|value| value.as_array_mut())
        else {
            return Some(expanded);
        };
        let total = contains.len();
        let page: Vec<Json> = contains.iter().skip(offset).take(count.unwrap_or(total)).cloned().collect();
        *contains = page;
        if let Some(expansion) = expanded.get_mut("expansion").and_then(|value| value.as_object_mut()) {
            expansion.insert("total".into(), json!(total));
            expansion.insert("offset".into(), json!(offset));
        }
        Some(expanded)
    }

    pub fn lookup(&self, system: &str, code: &str) -> Option<Json> {
        let index = self.systems.get(canonical(system))?;
        let concept = index.codes.get(code)?;
        let mut parameter =
            vec![json!({"name": "name", "valueString": index.name.clone().unwrap_or_default()})];
        if let Some(display) = &concept.display {
            parameter.push(json!({"name": "display", "valueString": display}));
        }
        if let Some(definition) = &concept.definition {
            parameter.push(json!({"name": "definition", "valueString": definition}));
        }
        for (name, value) in &concept.properties {
            parameter.push(json!({
                "name": "property",
                "part": [
                    {"name": "code", "valueCode": name},
                    {"name": "value", "valueString": value}
                ]
            }));
        }
        Some(json!({"resourceType": "Parameters", "parameter": parameter}))
    }

    pub fn translate(
        &self,
        map_url: Option<&str>,
        system: &str,
        code: &str,
        target_system: Option<&str>,
    ) -> Json {
        let key = (canonical(system).to_string(), code.to_string());
        let targets = self.maps.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        let mut matches = Vec::new();
        for target in targets {
            if map_url.is_some_and(|url| canonical(url) != target.map_url) {
                continue;
            }
            if target_system.is_some_and(|url| canonical(url) != target.system) {
                continue;
            }
            matches.push(json!({
                "name": "match",
                "part": [
                    {"name": "equivalence", "valueCode": target.equivalence},
                    {"name": "concept", "valueCoding": {"system": target.system, "code": target.code}}
                ]
            }));
        }
        let mut parameter = vec![json!({"name": "result", "valueBoolean": !matches.is_empty()})];
        parameter.extend(matches);
        json!({"resourceType": "Parameters", "parameter": parameter})
    }
}

impl Terminology for MemoryTerminology {
    fn contains(&self, value_set: &str, system: Option<&str>, code: &str) -> TermAnswer {
        let Some(expansion) = self.expansions.get(canonical(value_set)) else {
            return TermAnswer::Unknown;
        };
        let found = match system.map(canonical) {
            Some(system) => {
                expansion
                    .codes
                    .contains_key(&(system.to_string(), code.to_string()))
                    || expansion
                        .codes
                        .contains_key(&("".to_string(), code.to_string()))
            }
            None => expansion.codes.keys().any(|(_, item)| item == code),
        };
        if found {
            TermAnswer::Yes
        } else {
            TermAnswer::No
        }
    }

    fn describe(&self, system: &str, code: &str) -> Option<CodeDescription> {
        let concept = self.systems.get(canonical(system))?.codes.get(code)?;
        Some(CodeDescription {
            display: concept.display.clone(),
            definition: concept.definition.clone(),
            inactive: concept.inactive,
            properties: concept.properties.clone(),
        })
    }

    fn describe_bound(
        &self,
        value_set: &str,
        system: Option<&str>,
        code: &str,
    ) -> Option<CodeDescription> {
        if let Some(system) = system {
            return self.describe(system, code);
        }
        let expansion = self.expansions.get(canonical(value_set))?;
        let mut matched: Option<&str> = None;
        for (system, item) in expansion.codes.keys() {
            if item != code || system.is_empty() {
                continue;
            }
            match matched {
                Some(existing) if existing != system.as_str() => return None,
                Some(_) => {}
                None => matched = Some(system.as_str()),
            }
        }
        self.describe(matched?, code)
    }
}

struct Expander<'a> {
    systems: &'a BTreeMap<String, SystemIndex>,
    value_sets: &'a BTreeMap<String, Json>,
    cache: BTreeMap<String, Option<Expansion>>,
    stack: Vec<String>,
}

impl Expander<'_> {
    fn expand(&mut self, url: &str) -> Option<Expansion> {
        let url = canonical(url).to_string();
        if let Some(cached) = self.cache.get(&url) {
            return cached.clone();
        }
        if self.stack.iter().any(|item| item == &url) {
            return None;
        }
        let Some(value_set) = self.value_sets.get(&url).cloned() else {
            return None;
        };
        self.stack.push(url.clone());
        let expansion = expand_value_set(self, &value_set);
        self.stack.pop();
        self.cache.insert(url, expansion.clone());
        expansion
    }
}

fn expand_value_set(expander: &mut Expander<'_>, value_set: &Json) -> Option<Expansion> {
    if let Some(contains) = value_set
        .pointer("/expansion/contains")
        .and_then(|value| value.as_array())
    {
        return Some(expansion_from_contains(contains));
    }
    let include = value_set.pointer("/compose/include")?.as_array()?;
    let mut expansion = Expansion::default();
    for item in include {
        let included = expand_include(expander, item)?;
        merge_expansion(&mut expansion, included);
    }
    if let Some(exclude) = value_set
        .pointer("/compose/exclude")
        .and_then(|value| value.as_array())
    {
        for item in exclude {
            let excluded = expand_include(expander, item)?;
            for code in excluded.codes.keys() {
                expansion.codes.remove(code);
            }
        }
    }
    Some(expansion)
}

fn expand_include(expander: &mut Expander<'_>, include: &Json) -> Option<Expansion> {
    let filters = include.get("filter").and_then(|value| value.as_array());
    if filters.is_some_and(|filters| filters.iter().any(|filter| !filter_supported(filter))) {
        return None;
    }
    let mut expansion = Expansion::default();
    if let Some(nested) = include.get("valueSet").and_then(|value| value.as_array()) {
        for url in nested.iter().filter_map(|item| item.as_str()) {
            merge_expansion(&mut expansion, expander.expand(url)?);
        }
    }
    let system = include
        .get("system")
        .and_then(|value| value.as_str())
        .map(|url| canonical(url).to_string());
    if let Some(concepts) = include.get("concept").and_then(|value| value.as_array()) {
        let mut listed = Expansion::default();
        for concept in concepts {
            let Some(code) = concept.get("code").and_then(|value| value.as_str()) else {
                continue;
            };
            let display = concept
                .get("display")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
                .or_else(|| {
                    system.as_ref().and_then(|system| {
                        expander
                            .systems
                            .get(system)
                            .and_then(|index| index.codes.get(code))
                            .and_then(|concept| concept.display.clone())
                    })
                });
            let system_name = system.clone().unwrap_or_default();
            listed
                .codes
                .insert((system_name, code.to_string()), display);
        }
        if system.is_some() && !expansion.codes.is_empty() {
            listed
                .codes
                .retain(|code, _| expansion.codes.contains_key(code));
        }
        if let Some(system) = &system {
            note_system(&mut listed, expander, system);
        }
        if nested_present(include) {
            expansion.codes = listed.codes;
        } else {
            merge_expansion(&mut expansion, listed);
        }
        return Some(expansion);
    }
    let Some(system) = system else {
        return Some(expansion);
    };
    let index = expander.systems.get(&system)?;
    note_system(&mut expansion, expander, &system);
    if let Some(filters) = filters {
        if !index.closed {
            return None;
        }
        let matched = filter_codes(index, filters);
        if !expansion.codes.is_empty() {
            expansion.codes.retain(|(item_system, code), _| {
                item_system == &system && matched.iter().any(|candidate| candidate == code)
            });
        } else {
            for code in matched {
                let display = index
                    .codes
                    .get(&code)
                    .and_then(|concept| concept.display.clone());
                expansion.codes.insert((system.clone(), code), display);
            }
        }
        return Some(expansion);
    }
    if !expansion.codes.is_empty() {
        return Some(expansion);
    }
    if !index.closed {
        return None;
    }
    for (code, concept) in &index.codes {
        expansion
            .codes
            .insert((system.clone(), code.clone()), concept.display.clone());
    }
    Some(expansion)
}

fn nested_present(include: &Json) -> bool {
    include
        .get("valueSet")
        .and_then(|value| value.as_array())
        .is_some_and(|items| !items.is_empty())
}

fn note_system(expansion: &mut Expansion, expander: &Expander<'_>, system: &str) {
    let version = expander
        .systems
        .get(system)
        .and_then(|index| index.version.clone())
        .unwrap_or_default();
    expansion.systems.insert((system.to_string(), version));
}

fn filter_supported(filter: &Json) -> bool {
    matches!(
        filter.get("op").and_then(|value| value.as_str()),
        Some("=" | "is-a" | "descendent-of" | "is-not-a" | "exists" | "in" | "not-in" | "generalizes")
    )
}

fn filter_codes(index: &SystemIndex, filters: &[Json]) -> Vec<String> {
    index
        .codes
        .keys()
        .filter(|code| {
            filters
                .iter()
                .all(|filter| code_matches(index, code, filter))
        })
        .cloned()
        .collect()
}

fn code_matches(index: &SystemIndex, code: &str, filter: &Json) -> bool {
    let property = filter
        .get("property")
        .and_then(|value| value.as_str())
        .unwrap_or("concept");
    let op = filter
        .get("op")
        .and_then(|value| value.as_str())
        .unwrap_or("=");
    let value = filter
        .get("value")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    match op {
        "is-a" if property == "concept" => is_a(index, code, value),
        "generalizes" if property == "concept" => is_a(index, value, code),
        "is-not-a" if property == "concept" => !is_a(index, code, value),
        "descendent-of" if property == "concept" => code != value && is_a(index, code, value),
        "in" => value.split(',').any(|item| item == code),
        "not-in" => value.split(',').all(|item| item != code),
        "=" if property == "concept" => code == value,
        "=" => index.codes.get(code).is_some_and(|concept| {
            concept
                .properties
                .iter()
                .any(|(name, property_value)| name == property && property_value == value)
        }),
        "exists" => {
            let present = index
                .codes
                .get(code)
                .is_some_and(|concept| concept.properties.iter().any(|(name, _)| name == property));
            if value == "false" {
                !present
            } else {
                present
            }
        }
        _ => false,
    }
}

fn is_a(index: &SystemIndex, code: &str, ancestor: &str) -> bool {
    fn walk(index: &SystemIndex, code: &str, ancestor: &str, depth: usize) -> bool {
        if code == ancestor {
            return true;
        }
        if depth > 64 {
            return false;
        }
        let Some(concept) = index.codes.get(code) else {
            return false;
        };
        let parents = concept.parents.clone();
        parents
            .iter()
            .any(|parent| walk(index, parent, ancestor, depth + 1))
    }
    walk(index, code, ancestor, 0)
}

fn merge_expansion(into: &mut Expansion, other: Expansion) {
    into.systems.extend(other.systems);
    into.codes.extend(other.codes);
}

fn index_resources(resources: &[Json]) -> (MemoryTerminology, BTreeMap<String, Json>) {
    let mut terms = MemoryTerminology::default();
    let mut value_sets = BTreeMap::new();
    for resource in resources {
        match resource
            .get("resourceType")
            .and_then(|value| value.as_str())
        {
            Some("CodeSystem") => terms.add_system(resource),
            Some("ConceptMap") => terms.add_map(resource),
            Some("ValueSet") => {
                if let Some(url) = resource.get("url").and_then(|value| value.as_str()) {
                    value_sets.insert(canonical(url).to_string(), resource.clone());
                }
            }
            _ => {}
        }
    }
    (terms, value_sets)
}

impl MemoryTerminology {
    fn add_system(&mut self, resource: &Json) {
        let Some(url) = resource.get("url").and_then(|value| value.as_str()) else {
            return;
        };
        let content = resource
            .get("content")
            .and_then(|value| value.as_str())
            .unwrap_or("complete");
        let closed = !matches!(
            content,
            "fragment" | "example" | "not-present" | "supplement"
        );
        let mut codes = BTreeMap::new();
        if let Some(concepts) = resource.get("concept").and_then(|value| value.as_array()) {
            collect_concepts(concepts, None, &mut codes);
        }
        self.systems.insert(
            canonical(url).to_string(),
            SystemIndex {
                version: resource
                    .get("version")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string()),
                closed,
                name: resource
                    .get("name")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string()),
                codes,
            },
        );
    }

    fn add_map(&mut self, resource: &Json) {
        let Some(url) = resource.get("url").and_then(|value| value.as_str()) else {
            return;
        };
        let url = canonical(url).to_string();
        let Some(groups) = resource.get("group").and_then(|value| value.as_array()) else {
            return;
        };
        for group in groups {
            let Some(source) = group.get("source").and_then(|value| value.as_str()) else {
                continue;
            };
            let Some(target_system) = group.get("target").and_then(|value| value.as_str()) else {
                continue;
            };
            let Some(elements) = group.get("element").and_then(|value| value.as_array()) else {
                continue;
            };
            for element in elements {
                let Some(code) = element.get("code").and_then(|value| value.as_str()) else {
                    continue;
                };
                let Some(targets) = element.get("target").and_then(|value| value.as_array()) else {
                    continue;
                };
                for target in targets {
                    let Some(target_code) = target.get("code").and_then(|value| value.as_str())
                    else {
                        continue;
                    };
                    let equivalence = target
                        .get("equivalence")
                        .and_then(|value| value.as_str())
                        .unwrap_or("equivalent");
                    self.maps
                        .entry((canonical(source).to_string(), code.to_string()))
                        .or_default()
                        .push(Target {
                            map_url: url.clone(),
                            system: canonical(target_system).to_string(),
                            code: target_code.to_string(),
                            equivalence: equivalence.to_string(),
                        });
                }
            }
        }
    }
}

fn collect_concepts(
    concepts: &[Json],
    parent: Option<&str>,
    codes: &mut BTreeMap<String, Concept>,
) {
    for concept in concepts {
        let Some(code) = concept.get("code").and_then(|value| value.as_str()) else {
            continue;
        };
        let mut properties = Vec::new();
        if let Some(entries) = concept.get("property").and_then(|value| value.as_array()) {
            for entry in entries {
                let Some(name) = entry.get("code").and_then(|value| value.as_str()) else {
                    continue;
                };
                if let Some(value) = property_value(entry) {
                    properties.push((name.to_string(), value));
                }
            }
        }
        let mut parents = parent
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>();
        for (name, value) in &properties {
            if name == "parent" {
                parents.push(value.clone());
            }
        }
        codes.insert(
            code.to_string(),
            Concept {
                display: concept
                    .get("display")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string()),
                definition: concept
                    .get("definition")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string()),
                inactive: concept
                    .get("property")
                    .and_then(|value| value.as_array())
                    .is_some_and(|entries| {
                        entries.iter().any(|entry| {
                            entry.get("code").and_then(|value| value.as_str()) == Some("inactive")
                                && entry.get("valueBoolean").and_then(|value| value.as_bool())
                                    == Some(true)
                        })
                    })
                    || concept.get("inactive").and_then(|value| value.as_bool()) == Some(true),
                parents,
                properties,
            },
        );
        if let Some(nested) = concept.get("concept").and_then(|value| value.as_array()) {
            collect_concepts(nested, Some(code), codes);
        }
    }
}

fn property_value(entry: &Json) -> Option<String> {
    entry
        .get("valueCode")
        .or_else(|| entry.get("valueString"))
        .or_else(|| entry.get("valueUri"))
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .or_else(|| {
            entry
                .get("valueBoolean")
                .and_then(|value| value.as_bool())
                .map(|value| value.to_string())
        })
        .or_else(|| {
            entry
                .get("valueInteger")
                .and_then(|value| value.as_i64())
                .map(|value| value.to_string())
        })
}

fn expansion_from_contains(contains: &[Json]) -> Expansion {
    let mut expansion = Expansion::default();
    collect_expansion(contains, &mut expansion);
    expansion
}

fn collect_expansion(contains: &[Json], expansion: &mut Expansion) {
    for item in contains {
        if let Some(code) = item.get("code").and_then(|value| value.as_str()) {
            let system = item
                .get("system")
                .and_then(|value| value.as_str())
                .map(|url| canonical(url).to_string())
                .unwrap_or_default();
            if !system.is_empty() {
                expansion.systems.insert((system.clone(), String::new()));
            }
            let display = item
                .get("display")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string());
            expansion.codes.insert((system, code.to_string()), display);
        }
        if let Some(nested) = item.get("contains").and_then(|value| value.as_array()) {
            collect_expansion(nested, expansion);
        }
    }
}

fn expansion_from_document(document: &Json) -> Option<(String, Expansion)> {
    let url = document.get("url").and_then(|value| value.as_str())?;
    let mut expansion = Expansion::default();
    if let Some(systems) = document.get("systems").and_then(|value| value.as_array()) {
        for system in systems {
            let Some(system_url) = system.get("url").and_then(|value| value.as_str()) else {
                continue;
            };
            let version = system
                .get("version")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            expansion
                .systems
                .insert((canonical(system_url).to_string(), version));
        }
    }
    if let Some(contains) = document.get("contains").and_then(|value| value.as_array()) {
        collect_expansion(contains, &mut expansion);
    }
    Some((canonical(url).to_string(), expansion))
}

fn expansion_document(url: &str, expansion: &Expansion) -> Json {
    let systems: Vec<Json> = expansion
        .systems
        .iter()
        .map(|(system, version)| json!({"url": system, "version": version}))
        .collect();
    let contains: Vec<Json> = expansion
        .codes
        .iter()
        .map(|((system, code), display)| {
            let mut entry = json!({"code": code});
            if !system.is_empty() {
                entry["system"] = json!(system);
            }
            if let Some(display) = display {
                entry["display"] = json!(display);
            }
            entry
        })
        .collect();
    json!({"url": url, "systems": systems, "contains": contains})
}

fn display_of_bound(
    terms: &MemoryTerminology,
    value_set: &str,
    system: Option<&str>,
    code: &str,
    provided: Option<&str>,
) -> Option<String> {
    let described = terms.describe_bound(value_set, system, code);
    display_message(described, provided)
}

fn display_of(
    terms: &MemoryTerminology,
    system: Option<&str>,
    code: &str,
    provided: Option<&str>,
) -> Option<String> {
    display_message(
        system.and_then(|system| terms.describe(system, code)),
        provided,
    )
}

fn display_message(described: Option<CodeDescription>, provided: Option<&str>) -> Option<String> {
    if let (Some(provided), Some(expected)) = (
        provided,
        described.as_ref().and_then(|info| info.display.as_deref()),
    ) {
        if provided != expected {
            return Some(format!("display does not match {expected}"));
        }
    }
    described.and_then(|info| info.display)
}

fn code_parameters(
    result: bool,
    display_or_message: Option<String>,
    message: Option<&str>,
) -> Json {
    let mut parameter = vec![json!({"name": "result", "valueBoolean": result})];
    if let Some(message) = message {
        parameter.push(json!({"name": "message", "valueString": message}));
    } else if let Some(display) = display_or_message {
        if display.starts_with("display does not match") {
            parameter.push(json!({"name": "message", "valueString": display}));
        } else {
            parameter.push(json!({"name": "display", "valueString": display}));
        }
    }
    json!({"resourceType": "Parameters", "parameter": parameter})
}

pub fn canonical(url: &str) -> &str {
    url.split('|').next().unwrap_or(url)
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
                terms.contains(
                    "http://example.org/parent",
                    Some("http://example.org/sys"),
                    "a"
                ),
                TermAnswer::Yes
            );
            assert_eq!(
                terms.contains(
                    "http://example.org/parent",
                    Some("http://example.org/sys"),
                    "b"
                ),
                TermAnswer::No
            );
        }
    }

    #[test]
    fn is_a_filter_expands_descendants_and_fragment_stays_open() {
        let system = json!({
            "resourceType": "CodeSystem",
            "url": "http://example.org/animals",
            "content": "complete",
            "concept": [
                {"code": "mammal", "display": "Mammal", "concept": [
                    {"code": "dog", "display": "Dog"},
                    {"code": "cat", "display": "Cat"}
                ]}
            ]
        });
        let value_set = json!({
            "resourceType": "ValueSet",
            "url": "http://example.org/pets",
            "compose": {"include": [{"system": "http://example.org/animals", "filter": [{"property": "concept", "op": "is-a", "value": "mammal"}]}]}
        });
        let terms = MemoryTerminology::from_resources(&[system, value_set]);
        assert_eq!(
            terms.contains(
                "http://example.org/pets",
                Some("http://example.org/animals"),
                "dog"
            ),
            TermAnswer::Yes
        );
        assert_eq!(
            terms.contains(
                "http://example.org/pets",
                Some("http://example.org/animals"),
                "cat"
            ),
            TermAnswer::Yes
        );
        assert_eq!(
            terms.contains(
                "http://example.org/pets",
                Some("http://example.org/animals"),
                "fish"
            ),
            TermAnswer::No
        );
        assert!(terms.subsumes("http://example.org/animals", "mammal", "dog"));
        assert!(!terms.subsumes("http://example.org/animals", "dog", "mammal"));
        let page = terms
            .expand_page("http://example.org/pets", None, 0, Some(1))
            .expect("page");
        assert_eq!(page["expansion"]["total"], 3);
        assert_eq!(page["expansion"]["contains"].as_array().unwrap().len(), 1);

        let fragment = json!({
            "resourceType": "CodeSystem",
            "url": "http://example.org/fragment",
            "content": "fragment",
            "concept": [{"code": "a", "display": "A"}]
        });
        let open = json!({
            "resourceType": "ValueSet",
            "url": "http://example.org/open",
            "compose": {"include": [{"system": "http://example.org/fragment"}]}
        });
        let terms = MemoryTerminology::from_resources(&[fragment, open]);
        assert_eq!(
            terms.contains(
                "http://example.org/open",
                Some("http://example.org/fragment"),
                "missing"
            ),
            TermAnswer::Unknown
        );
    }

    #[test]
    fn translate_returns_the_target_code() {
        let map = json!({
            "resourceType": "ConceptMap",
            "url": "http://example.org/map",
            "group": [{
                "source": "http://example.org/src",
                "target": "http://example.org/dst",
                "element": [{"code": "a", "target": [{"code": "b", "equivalence": "equivalent"}]}]
            }]
        });
        let terms = MemoryTerminology::from_resources(&[map]);
        let translated = terms.translate(
            Some("http://example.org/map"),
            "http://example.org/src",
            "a",
            None,
        );
        let result = translated["parameter"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == "result")
            .unwrap();
        assert_eq!(result["valueBoolean"], true);
        let concept = translated["parameter"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == "match")
            .unwrap();
        assert_eq!(concept["part"][1]["valueCoding"]["code"], "b");
    }

    #[test]
    fn stored_expansion_is_what_contains_reads() {
        let system = json!({
            "resourceType": "CodeSystem",
            "url": "http://example.org/animals",
            "content": "complete",
            "concept": [
                {"code": "mammal", "concept": [{"code": "dog"}, {"code": "cat"}]}
            ]
        });
        let value_set = json!({
            "resourceType": "ValueSet",
            "url": "http://example.org/pets",
            "compose": {"include": [{"system": "http://example.org/animals", "filter": [{"property": "concept", "op": "is-a", "value": "mammal"}]}]}
        });
        let documents = MemoryTerminology::expansion_documents(&[system, value_set]);
        let stored = MemoryTerminology::from_stored(&[], &documents);
        assert_eq!(
            stored.contains(
                "http://example.org/pets",
                Some("http://example.org/animals"),
                "dog"
            ),
            TermAnswer::Yes
        );
        assert_eq!(
            stored.contains(
                "http://example.org/pets",
                Some("http://example.org/animals"),
                "fish"
            ),
            TermAnswer::No
        );
    }
}
