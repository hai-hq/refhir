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

    /// Why `value_set` has no expansion, when it is loaded but cannot be expanded.
    fn expansion_problem(&self, _value_set: &str) -> Option<String> {
        None
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
    /// A complete system is closed. A missing code is really absent.
    closed: bool,
    /// The CodeSystem's `content` mode: `complete`, `example`, `fragment`, `not-present` or
    /// `supplement`.
    content: String,
    name: Option<String>,
    codes: BTreeMap<String, Concept>,
}

/// `system|version` is the canonical form of a versioned reference.
fn split_system(system: &str) -> (&str, Option<&str>) {
    match system.split_once('|') {
        Some((url, version)) if !version.is_empty() => (url, Some(version)),
        Some((url, _)) => (url, None),
        None => (system, None),
    }
}

/// The identity of a code system resource: its URL without any `|version`, and its version.
///
/// A package may carry several versions of one system, and says which a resource is in one of
/// two ways: the URL itself ends in `|version` (HL7's v2 tables: `.../v2-0006|2.4`), or it has a
/// `version`. The URL's own suffix wins, because that is what a reference to it names. No version
/// at all is the empty one.
pub fn code_system_key(resource: &Json) -> Option<(String, String)> {
    let url = resource.get("url").and_then(|value| value.as_str())?;
    let (base, suffix) = split_system(url);
    let version = suffix
        .or_else(|| resource.get("version").and_then(|value| value.as_str()))
        .unwrap_or("");
    Some((base.to_string(), version.to_string()))
}

/// The key a code system is stored under: its URL, with `|version` when it has a version and the
/// URL does not already say so. Two versions of one system are two rows.
pub fn code_system_row_key(resource: &Json) -> Option<String> {
    let (base, version) = code_system_key(resource)?;
    Some(if version.is_empty() {
        base
    } else {
        format!("{base}|{version}")
    })
}

/// Versions compare segment by segment, numerically where both are numbers (so 2.10 is after
/// 2.9), and as text otherwise. No version is the oldest.
fn version_order(left: &str, right: &str) -> std::cmp::Ordering {
    let mut left = left.split('.');
    let mut right = right.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(a), Some(b)) => {
                let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                    (Ok(a), Ok(b)) => a.cmp(&b),
                    _ => a.cmp(b),
                };
                if order != std::cmp::Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

/// Code systems by URL and version. Every version that is installed is kept, so a reference that
/// names one gets that one, whatever order they were loaded in.
#[derive(Clone, Debug, Default)]
struct Systems {
    by_url: BTreeMap<String, BTreeMap<String, SystemIndex>>,
}

impl Systems {
    fn insert(&mut self, url: &str, version: &str, index: SystemIndex) {
        self.by_url
            .entry(url.to_string())
            .or_default()
            .insert(version.to_string(), index);
    }

    /// Remove one version, or every version when `version` is `None`.
    fn remove(&mut self, url: &str, version: Option<&str>) {
        let Some(versions) = self.by_url.get_mut(url) else {
            return;
        };
        match version {
            Some(version) => {
                versions.remove(version);
            }
            None => versions.clear(),
        }
        if versions.is_empty() {
            self.by_url.remove(url);
        }
    }

    /// The version asked for, or, when none is, the latest installed. Returns the version found.
    fn resolve(&self, url: &str, version: Option<&str>) -> Option<(&str, &SystemIndex)> {
        let versions = self.by_url.get(url)?;
        let found = match version.filter(|version| !version.is_empty()) {
            Some(version) => versions.get_key_value(version),
            None => versions
                .iter()
                .max_by(|left, right| version_order(left.0, right.0)),
        };
        found.map(|(version, index)| (version.as_str(), index))
    }

    /// `system` or `system|version` as a reference, resolved the same way.
    fn find(&self, system: &str, version: Option<&str>) -> Option<&SystemIndex> {
        let (url, suffix) = split_system(system);
        self.resolve(canonical(url), version.or(suffix))
            .map(|(_, index)| index)
    }

    fn versions(&self, url: &str) -> Vec<String> {
        self.by_url
            .get(url)
            .map(|versions| versions.keys().cloned().collect())
            .unwrap_or_default()
    }
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
    systems: Systems,
    expansions: BTreeMap<String, Expansion>,
    /// Source system and code to translation targets.
    maps: BTreeMap<(String, String), Vec<Target>>,
    /// Value sets that have no expansion, kept so a refusal can say why.
    unexpanded: BTreeMap<String, Json>,
    /// Why each of those has none. Worked out when the terminology is built or changed, so that
    /// a check against one of them is a lookup, not an expansion.
    reasons: BTreeMap<String, Vec<ExpansionFailure>>,
    /// For each value set, the code systems and value sets its `compose` draws on. It says which
    /// expansions a change to a code system or value set can alter.
    uses: BTreeMap<String, BTreeSet<String>>,
}

impl MemoryTerminology {
    pub fn from_resources(resources: &[Json]) -> Self {
        let (mut terms, value_sets) = index_resources(resources);
        let known = BTreeMap::new();
        let mut expander = Expander {
            systems: &terms.systems,
            value_sets: &value_sets,
            known: &known,
            cache: BTreeMap::new(),
            stack: Vec::new(),
        };
        for url in value_sets.keys() {
            if let Ok(expansion) = expander.expand(url) {
                terms.expansions.insert(url.clone(), expansion);
            }
        }
        terms.keep_unexpanded(value_sets);
        terms
    }

    fn keep_unexpanded(&mut self, value_sets: BTreeMap<String, Json>) {
        self.record_references(&value_sets);
        self.unexpanded = value_sets
            .into_iter()
            .filter(|(url, _)| !self.expansions.contains_key(url))
            .collect();
        let reasons = self
            .unexpanded
            .keys()
            .map(|url| (url.clone(), self.explain(url)))
            .collect();
        self.reasons = reasons;
    }

    /// Why `url`, which is loaded and has no expansion, has none.
    fn explain(&self, url: &str) -> Vec<ExpansionFailure> {
        let mut expander = Expander {
            systems: &self.systems,
            value_sets: &self.unexpanded,
            known: &self.expansions,
            cache: BTreeMap::new(),
            stack: Vec::new(),
        };
        match expander.expand(url) {
            Ok(_) => vec![ExpansionFailure::NotExpanded {
                value_set: url.to_string(),
            }],
            Err(failures) => failures,
        }
    }

    /// Membership comes from `expansions`. Code systems and concept maps still come from `resources`.
    pub fn from_stored(resources: &[Json], expansions: &[Json]) -> Self {
        let (mut terms, value_sets) = index_resources(resources);
        for resource in resources {
            if resource
                .get("resourceType")
                .and_then(|value| value.as_str())
                != Some("ValueSet")
            {
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
                    terms
                        .expansions
                        .insert(canonical(&url).to_string(), expansion);
                }
            }
        }
        for document in expansions {
            if let Some((url, expansion)) = expansion_from_document(document) {
                terms.expansions.insert(url, expansion);
            }
        }
        terms.keep_unexpanded(value_sets);
        terms
    }

    /// Why `url` has no expansion, or `None` when it has one. A value set that cannot be
    /// expanded is reported with every cause, not just the first: the answer to "install what?"
    /// is the whole list.
    pub fn why_not_expanded(&self, url: &str) -> Option<Vec<ExpansionFailure>> {
        let url = canonical(url);
        if self.expansions.contains_key(url) {
            return None;
        }
        if !self.unexpanded.contains_key(url) {
            return Some(vec![ExpansionFailure::NotLoaded {
                value_set: url.to_string(),
            }]);
        }
        Some(
            self.reasons
                .get(url)
                .cloned()
                .unwrap_or_else(|| self.explain(url)),
        )
    }

    /// Every loaded value set that has no expansion, with the reasons. This is what a
    /// deployment check reports before a client asks for one.
    pub fn unexpandable(&self) -> Vec<(String, Vec<ExpansionFailure>)> {
        self.unexpanded
            .keys()
            .filter_map(|url| Some((url.clone(), self.why_not_expanded(url)?)))
            .collect()
    }

    pub fn expansion_documents(resources: &[Json]) -> Vec<Json> {
        Self::from_resources(resources).documents()
    }

    /// The expansion document of every value set that has an expansion, ordered by URL.
    fn documents(&self) -> Vec<Json> {
        self.expansions
            .iter()
            .map(|(url, expansion)| expansion_document(url, expansion))
            .collect()
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

    /// `version` is the version of the code system, when the caller names one; `system|version`
    /// names it too. With neither, the latest installed version answers.
    pub fn validate_code(
        &self,
        value_set: Option<&str>,
        system: Option<&str>,
        version: Option<&str>,
        code: &str,
        display: Option<&str>,
    ) -> Json {
        if let Some(url) = value_set {
            if let (Some(system), Some(requested)) =
                (system, version.filter(|version| !version.is_empty()))
            {
                let base = canonical(split_system(system).0);
                if self.systems.resolve(base, Some(requested)).is_none() {
                    return self.system_not_loaded(base, Some(requested));
                }
            }
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
                        "diagnostics": match self.expansion_problem(url) {
                            Some(reason) => format!("value set {url} cannot be expanded: {reason}"),
                            None => format!("value set {url} is not expanded"),
                        }
                    }]
                }),
            };
        }
        let Some(system) = system else {
            return code_parameters(false, None, Some("system is required"));
        };
        let (url, suffix) = split_system(system);
        let url = canonical(url);
        let requested = version.or(suffix).filter(|version| !version.is_empty());
        match self.systems.resolve(url, requested) {
            Some((_, index)) if index.codes.contains_key(code) => code_parameters(
                true,
                display_message(describe_index(index, code), display),
                None,
            ),
            Some((_, index)) if index.closed => {
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
            None => self.system_not_loaded(url, requested),
        }
    }

    /// The answer for a code system, or a version of one, that is not installed. The versions
    /// that are installed are named, because they are what the caller can ask for instead.
    fn system_not_loaded(&self, url: &str, requested: Option<&str>) -> Json {
        let installed = self.systems.versions(url);
        let diagnostics = match requested {
            Some(requested) if !installed.is_empty() => format!(
                "code system {url} version {requested} is not loaded; installed: {}",
                installed_versions(&installed)
            ),
            _ => format!("code system {url} is not loaded"),
        };
        json!({
            "resourceType": "OperationOutcome",
            "issue": [{
                "severity": "warning",
                "code": "not-found",
                "diagnostics": diagnostics
            }]
        })
    }

    /// `ancestor` subsumes `code` when they are equal or `ancestor` is a parent.
    pub fn subsumes(
        &self,
        system: &str,
        version: Option<&str>,
        ancestor: &str,
        code: &str,
    ) -> bool {
        let Some(index) = self.systems.find(system, version) else {
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
        let page: Vec<Json> = contains
            .iter()
            .skip(offset)
            .take(count.unwrap_or(total))
            .cloned()
            .collect();
        *contains = page;
        if let Some(expansion) = expanded
            .get_mut("expansion")
            .and_then(|value| value.as_object_mut())
        {
            expansion.insert("total".into(), json!(total));
            expansion.insert("offset".into(), json!(offset));
        }
        Some(expanded)
    }

    pub fn lookup(&self, system: &str, version: Option<&str>, code: &str) -> Option<Json> {
        let index = self.systems.find(system, version)?;
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

    /// Target code for a StructureMap `translate` transform.
    ///
    /// The mapping language writes this value onto the target element, so the
    /// result is the code itself. A source system selects one ConceptMap group.
    /// When the system is absent and the groups disagree, there is no translation.
    pub fn translate_coding(
        &self,
        map_url: &str,
        system: Option<&str>,
        code: &str,
    ) -> Option<Json> {
        let map_url = canonical(map_url);
        let mut codes = Vec::new();
        for ((source_system, stored), targets) in &self.maps {
            if stored != code {
                continue;
            }
            if system.is_some_and(|item| canonical(item) != source_system) {
                continue;
            }
            for target in targets {
                if target.map_url != map_url || !usable_equivalence(&target.equivalence) {
                    continue;
                }
                if !codes.iter().any(|found| found == &target.code) {
                    codes.push(target.code.clone());
                }
            }
        }
        match codes.as_slice() {
            [only] => Some(Json::String(only.clone())),
            _ => None,
        }
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

    fn expansion_problem(&self, value_set: &str) -> Option<String> {
        let failures = self.why_not_expanded(value_set)?;
        // A value set that is simply absent has nothing to explain.
        if matches!(failures.as_slice(), [ExpansionFailure::NotLoaded { .. }]) {
            return None;
        }
        Some(
            failures
                .iter()
                .map(ExpansionFailure::message)
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    fn describe(&self, system: &str, code: &str) -> Option<CodeDescription> {
        describe_index(self.systems.find(system, None)?, code)
    }

    fn describe_bound(
        &self,
        value_set: &str,
        system: Option<&str>,
        code: &str,
    ) -> Option<CodeDescription> {
        if let Some(system) = system {
            return self.describe_as_expanded(value_set, system, code);
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
        self.describe_as_expanded(value_set, matched?, code)
    }
}

impl MemoryTerminology {
    /// The code as the version of its system that `value_set` was expanded from describes it,
    /// which is not always the latest installed. With no such version, the latest.
    fn describe_as_expanded(
        &self,
        value_set: &str,
        system: &str,
        code: &str,
    ) -> Option<CodeDescription> {
        let url = canonical(split_system(system).0);
        let version = self
            .expansions
            .get(canonical(value_set))
            .and_then(|expansion| {
                expansion
                    .systems
                    .iter()
                    .find(|(used, _)| used == url)
                    .map(|(_, version)| version.as_str())
            });
        match version
            .filter(|version| !version.is_empty())
            .and_then(|version| self.systems.resolve(url, Some(version)))
        {
            Some((_, index)) => describe_index(index, code),
            None => self.describe(system, code),
        }
    }
}

/// Where in a ValueSet's `compose` an expansion failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncludeAt {
    pub value_set: String,
    /// `include` or `exclude`.
    pub section: &'static str,
    pub index: usize,
}

impl IncludeAt {
    /// FHIRPath of the failing `include`/`exclude` inside the ValueSet.
    pub fn expression(&self) -> String {
        format!("ValueSet.compose.{}[{}]", self.section, self.index)
    }

    fn describe(&self) -> String {
        format!(
            "{} compose.{}[{}]",
            self.value_set, self.section, self.index
        )
    }
}

/// Why a value set has no expansion. An expansion is all or nothing: returning the part that
/// could be computed would look complete to a client that cannot tell it was cut short.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExpansionFailure {
    /// No value set with this URL is loaded.
    NotLoaded { value_set: String },
    /// The value set is loaded but has no stored expansion.
    NotExpanded { value_set: String },
    /// Value sets that include each other.
    Cycle { value_set: String },
    /// Neither `compose.include` nor `expansion` is present.
    NothingToExpand { value_set: String },
    /// An included code system is not installed.
    SystemNotLoaded {
        at: IncludeAt,
        system: String,
        version: Option<String>,
    },
    /// An included code system holds only part of its content (`example`, `fragment`,
    /// `not-present`, `supplement`), so a whole-system or filtered include cannot be completed.
    SystemIncomplete {
        at: IncludeAt,
        system: String,
        version: Option<String>,
        content: String,
        filter: Option<String>,
    },
    /// The system is installed, but not at the version the include names.
    SystemVersionNotLoaded {
        at: IncludeAt,
        system: String,
        version: String,
        installed: Vec<String>,
    },
    UnsupportedFilter {
        at: IncludeAt,
        system: Option<String>,
        property: String,
        op: String,
    },
    /// An included value set cannot be expanded.
    Dependency {
        at: IncludeAt,
        value_set: String,
        causes: Vec<ExpansionFailure>,
    },
}

impl ExpansionFailure {
    /// A short machine-readable reason.
    pub fn reason(&self) -> &'static str {
        match self {
            ExpansionFailure::NotLoaded { .. } => "value-set-not-loaded",
            ExpansionFailure::NotExpanded { .. } => "value-set-not-expanded",
            ExpansionFailure::Cycle { .. } => "value-set-cycle",
            ExpansionFailure::NothingToExpand { .. } => "nothing-to-expand",
            ExpansionFailure::SystemNotLoaded { .. } => "code-system-not-loaded",
            ExpansionFailure::SystemIncomplete { .. } => "code-system-incomplete",
            ExpansionFailure::SystemVersionNotLoaded { .. } => "code-system-version-not-loaded",
            ExpansionFailure::UnsupportedFilter { .. } => "unsupported-filter",
            ExpansionFailure::Dependency { .. } => "dependency",
        }
    }

    /// Where in the value set the failure is, when it has one.
    pub fn expression(&self) -> Option<String> {
        match self {
            ExpansionFailure::SystemNotLoaded { at, .. }
            | ExpansionFailure::SystemIncomplete { at, .. }
            | ExpansionFailure::SystemVersionNotLoaded { at, .. }
            | ExpansionFailure::UnsupportedFilter { at, .. }
            | ExpansionFailure::Dependency { at, .. } => Some(at.expression()),
            _ => None,
        }
    }

    pub fn message(&self) -> String {
        match self {
            ExpansionFailure::NotLoaded { value_set } => {
                format!("value set {value_set} is not loaded")
            }
            ExpansionFailure::NotExpanded { value_set } => {
                format!("value set {value_set} has no stored expansion")
            }
            ExpansionFailure::Cycle { value_set } => {
                format!("value set {value_set} includes itself")
            }
            ExpansionFailure::NothingToExpand { value_set } => {
                format!("value set {value_set} has no compose.include and no expansion")
            }
            ExpansionFailure::SystemNotLoaded {
                at,
                system,
                version,
            } => format!(
                "code system {} is not installed ({})",
                versioned(system, version),
                at.describe()
            ),
            ExpansionFailure::SystemIncomplete {
                at,
                system,
                version,
                content,
                filter,
            } => {
                let system = versioned(system, version);
                let held = match content.as_str() {
                    "not-present" => "none of its concepts are installed".to_string(),
                    other => format!("its content is '{other}', not complete"),
                };
                match filter {
                    Some(filter) => format!(
                        "code system {system} cannot be filtered by {filter}: {held} ({})",
                        at.describe()
                    ),
                    None => format!(
                        "code system {system} cannot be included whole: {held} ({})",
                        at.describe()
                    ),
                }
            }
            ExpansionFailure::SystemVersionNotLoaded {
                at,
                system,
                version,
                installed,
            } => format!(
                "code system {system} is installed, but not at version {version} (installed: {}) ({})",
                installed_versions(installed),
                at.describe()
            ),
            ExpansionFailure::UnsupportedFilter {
                at,
                system,
                property,
                op,
            } => format!(
                "filter '{property} {op}'{} is not supported ({})",
                system
                    .as_ref()
                    .map(|system| format!(" on {system}"))
                    .unwrap_or_default(),
                at.describe()
            ),
            ExpansionFailure::Dependency {
                at,
                value_set,
                causes,
            } => format!(
                "included value set {value_set} cannot be expanded ({}): {}",
                at.describe(),
                causes
                    .iter()
                    .map(ExpansionFailure::message)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        }
    }
}

/// A change to the terminology a catalog holds: what was written, as stored, and what was
/// removed.
#[derive(Clone, Debug, Default)]
pub struct TerminologyChange {
    /// CodeSystem, ValueSet and ConceptMap resources, as stored.
    pub upserted: Vec<Json>,
    /// `(resourceType, canonical url)` of resources that no longer exist.
    pub removed: Vec<(String, String)>,
}

impl TerminologyChange {
    pub fn is_empty(&self) -> bool {
        self.upserted.is_empty() && self.removed.is_empty()
    }

    /// Canonical URLs of everything the change writes or removes.
    pub fn urls(&self) -> Vec<String> {
        let mut urls: Vec<String> = self
            .upserted
            .iter()
            .filter_map(|resource| resource.get("url").and_then(|value| value.as_str()))
            .map(|url| canonical(url).to_string())
            .collect();
        urls.extend(
            self.removed
                .iter()
                .map(|(_, url)| canonical(url).to_string()),
        );
        urls.sort();
        urls.dedup();
        urls
    }
}

/// What a change did to the stored expansion of one value set.
#[derive(Clone, Debug)]
pub struct ExpansionUpdate {
    pub url: String,
    /// The expansion document to store, or `None` when the value set has no expansion now.
    pub document: Option<Json>,
}

/// The canonical URLs a value set's `compose` draws on: code systems and other value sets.
fn compose_references(value_set: &Json) -> BTreeSet<String> {
    let mut references = BTreeSet::new();
    for section in ["include", "exclude"] {
        let Some(items) = value_set
            .pointer(&format!("/compose/{section}"))
            .and_then(|value| value.as_array())
        else {
            continue;
        };
        for item in items {
            if let Some(system) = item.get("system").and_then(|value| value.as_str()) {
                references.insert(canonical(system).to_string());
            }
            for nested in item
                .get("valueSet")
                .and_then(|value| value.as_array())
                .into_iter()
                .flatten()
                .filter_map(|value| value.as_str())
            {
                references.insert(canonical(nested).to_string());
            }
        }
    }
    references
}

impl MemoryTerminology {
    fn record_references(&mut self, value_sets: &BTreeMap<String, Json>) {
        self.uses = value_sets
            .iter()
            .map(|(url, value_set)| (url.clone(), compose_references(value_set)))
            .collect();
    }

    /// The value sets, other than those being changed, whose expansion depends on something the
    /// change touches: they include a code system or value set that is written or removed, or
    /// include one of these value sets in turn.
    pub fn dependents_of(&self, urls: &[String]) -> Vec<String> {
        let mut touched: BTreeSet<String> = urls.iter().cloned().collect();
        let mut dependents = BTreeSet::new();
        loop {
            let before = touched.len();
            for (value_set, uses) in &self.uses {
                if touched.contains(value_set) {
                    continue;
                }
                if uses.iter().any(|used| touched.contains(used)) {
                    touched.insert(value_set.clone());
                    dependents.insert(value_set.clone());
                }
            }
            if touched.len() == before {
                break;
            }
        }
        dependents.into_iter().collect()
    }

    /// Apply `change` and bring every expansion it affects up to date, touching nothing else.
    ///
    /// `dependents` are the stored resources of [`Self::dependents_of`] for this change. The
    /// result is the same terminology a full rebuild from every stored resource would give, at
    /// a cost that depends on the change, not on how much terminology is installed. The updates
    /// returned are the expansion rows to write.
    pub fn apply(
        &mut self,
        change: &TerminologyChange,
        dependents: &[Json],
    ) -> Vec<ExpansionUpdate> {
        let mut value_sets: BTreeMap<String, Json> = BTreeMap::new();
        let mut gone: BTreeSet<String> = BTreeSet::new();
        for (type_name, key) in &change.removed {
            match type_name.as_str() {
                "CodeSystem" => {
                    // The key a code system is stored under is `url` or `url|version`, which
                    // says which version went. Without a version, the unversioned one did.
                    let (base, version) = split_system(key);
                    self.systems.remove(base, Some(version.unwrap_or("")));
                }
                "ConceptMap" => self.remove_map(canonical(key)),
                "ValueSet" => {
                    gone.insert(canonical(key).to_string());
                }
                _ => {}
            }
        }
        for resource in &change.upserted {
            let Some(url) = resource
                .get("url")
                .and_then(|value| value.as_str())
                .map(|url| canonical(url).to_string())
            else {
                continue;
            };
            match resource
                .get("resourceType")
                .and_then(|value| value.as_str())
            {
                Some("CodeSystem") => self.add_system(resource),
                Some("ConceptMap") => {
                    self.remove_map(&url);
                    self.add_map(resource);
                }
                Some("ValueSet") => {
                    gone.remove(&url);
                    value_sets.insert(url, resource.clone());
                }
                _ => {}
            }
        }
        for resource in dependents {
            if let Some(url) = resource.get("url").and_then(|value| value.as_str()) {
                value_sets
                    .entry(canonical(url).to_string())
                    .or_insert_with(|| resource.clone());
            }
        }
        for url in value_sets.keys().chain(gone.iter()) {
            self.expansions.remove(url);
            self.unexpanded.remove(url);
            self.reasons.remove(url);
            self.uses.remove(url);
        }
        for (url, value_set) in &value_sets {
            self.uses.insert(url.clone(), compose_references(value_set));
        }
        // A value set that cannot be expanded may be what a changed one includes, and its own
        // reasons are worked out from its resource, so those are in scope too.
        let mut scope = value_sets.clone();
        for (url, value_set) in &self.unexpanded {
            scope
                .entry(url.clone())
                .or_insert_with(|| value_set.clone());
        }
        let results: Vec<(String, Result<Expansion, Failures>)> = {
            let mut expander = Expander {
                systems: &self.systems,
                value_sets: &scope,
                known: &self.expansions,
                cache: BTreeMap::new(),
                stack: Vec::new(),
            };
            value_sets
                .keys()
                .map(|url| (url.clone(), expander.expand(url)))
                .collect()
        };
        let mut updates = Vec::new();
        for (url, result) in results {
            match result {
                Ok(expansion) => {
                    updates.push(ExpansionUpdate {
                        url: url.clone(),
                        document: Some(expansion_document(&url, &expansion)),
                    });
                    self.expansions.insert(url, expansion);
                }
                Err(failures) => {
                    if let Some(value_set) = value_sets.get(&url) {
                        self.unexpanded.insert(url.clone(), value_set.clone());
                        self.reasons.insert(url.clone(), failures);
                    }
                    updates.push(ExpansionUpdate {
                        url,
                        document: None,
                    });
                }
            }
        }
        for url in gone {
            updates.push(ExpansionUpdate {
                url,
                document: None,
            });
        }
        updates
    }

    fn remove_map(&mut self, map_url: &str) {
        for targets in self.maps.values_mut() {
            targets.retain(|target| target.map_url != map_url);
        }
        self.maps.retain(|_, targets| !targets.is_empty());
    }
}

fn versioned(system: &str, version: &Option<String>) -> String {
    match version {
        Some(version) => format!("{system}|{version}"),
        None => system.to_string(),
    }
}

type Failures = Vec<ExpansionFailure>;

struct Expander<'a> {
    systems: &'a Systems,
    value_sets: &'a BTreeMap<String, Json>,
    /// Expansions that already exist. They are used as they are rather than recomputed.
    known: &'a BTreeMap<String, Expansion>,
    cache: BTreeMap<String, Result<Expansion, Failures>>,
    stack: Vec<String>,
}

impl Expander<'_> {
    fn expand(&mut self, url: &str) -> Result<Expansion, Failures> {
        let url = canonical(url).to_string();
        if let Some(cached) = self.cache.get(&url) {
            return cached.clone();
        }
        if let Some(known) = self.known.get(&url) {
            return Ok(known.clone());
        }
        if self.stack.iter().any(|item| item == &url) {
            return Err(vec![ExpansionFailure::Cycle { value_set: url }]);
        }
        let Some(value_set) = self.value_sets.get(&url).cloned() else {
            return Err(vec![ExpansionFailure::NotLoaded { value_set: url }]);
        };
        self.stack.push(url.clone());
        let expansion = expand_value_set(self, &url, &value_set);
        self.stack.pop();
        self.cache.insert(url, expansion.clone());
        expansion
    }
}

fn expand_value_set(
    expander: &mut Expander<'_>,
    url: &str,
    value_set: &Json,
) -> Result<Expansion, Failures> {
    if let Some(contains) = value_set
        .pointer("/expansion/contains")
        .and_then(|value| value.as_array())
    {
        return Ok(expansion_from_contains(contains));
    }
    let Some(include) = value_set
        .pointer("/compose/include")
        .and_then(|value| value.as_array())
    else {
        return Err(vec![ExpansionFailure::NothingToExpand {
            value_set: url.to_string(),
        }]);
    };
    let mut expansion = Expansion::default();
    let mut failures = Vec::new();
    for (index, item) in include.iter().enumerate() {
        let at = IncludeAt {
            value_set: url.to_string(),
            section: "include",
            index,
        };
        match expand_include(expander, &at, item) {
            Ok(included) => merge_expansion(&mut expansion, included),
            Err(more) => failures.extend(more),
        }
    }
    if let Some(exclude) = value_set
        .pointer("/compose/exclude")
        .and_then(|value| value.as_array())
    {
        for (index, item) in exclude.iter().enumerate() {
            let at = IncludeAt {
                value_set: url.to_string(),
                section: "exclude",
                index,
            };
            match expand_include(expander, &at, item) {
                Ok(excluded) => {
                    for code in excluded.codes.keys() {
                        expansion.codes.remove(code);
                    }
                }
                Err(more) => failures.extend(more),
            }
        }
    }
    if failures.is_empty() {
        Ok(expansion)
    } else {
        Err(failures)
    }
}

fn describe_filters(filters: &[Json]) -> String {
    filters
        .iter()
        .map(|filter| {
            let text = |name: &str| filter.get(name).and_then(|value| value.as_str());
            format!(
                "{} {} {}",
                text("property").unwrap_or("concept"),
                text("op").unwrap_or("="),
                text("value").unwrap_or("")
            )
            .trim_end()
            .to_string()
        })
        .collect::<Vec<_>>()
        .join(" and ")
}

fn expand_include(
    expander: &mut Expander<'_>,
    at: &IncludeAt,
    include: &Json,
) -> Result<Expansion, Failures> {
    let filters = include.get("filter").and_then(|value| value.as_array());
    let system_url = include.get("system").and_then(|value| value.as_str());
    if let Some(filters) = filters {
        let unsupported: Failures = filters
            .iter()
            .filter(|filter| !filter_supported(filter))
            .map(|filter| ExpansionFailure::UnsupportedFilter {
                at: at.clone(),
                system: system_url.map(|url| canonical(url).to_string()),
                property: filter
                    .get("property")
                    .and_then(|value| value.as_str())
                    .unwrap_or("concept")
                    .to_string(),
                op: filter
                    .get("op")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
            .collect();
        if !unsupported.is_empty() {
            return Err(unsupported);
        }
    }
    let mut expansion = Expansion::default();
    if let Some(nested) = include.get("valueSet").and_then(|value| value.as_array()) {
        let mut failures = Vec::new();
        for url in nested.iter().filter_map(|item| item.as_str()) {
            match expander.expand(url) {
                Ok(included) => merge_expansion(&mut expansion, included),
                Err(causes) => failures.push(ExpansionFailure::Dependency {
                    at: at.clone(),
                    value_set: canonical(url).to_string(),
                    causes,
                }),
            }
        }
        if !failures.is_empty() {
            return Err(failures);
        }
    }
    let system = system_url.map(|url| canonical(url).to_string());
    // The version an include names: `system|version`, or the include's own `version`. With
    // neither, the latest installed one is used.
    let version = system_url
        .and_then(|url| split_system(url).1)
        .or_else(|| include.get("version").and_then(|value| value.as_str()))
        .filter(|version| !version.is_empty())
        .map(|value| value.to_string());
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
                            .resolve(system, version.as_deref())
                            .and_then(|(_, index)| index.codes.get(code))
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
            note_system(&mut listed, expander, system, version.as_deref());
        }
        if nested_present(include) {
            expansion.codes = listed.codes;
        } else {
            merge_expansion(&mut expansion, listed);
        }
        return Ok(expansion);
    }
    let Some(system) = system else {
        return Ok(expansion);
    };
    let Some((_, index)) = expander.systems.resolve(&system, version.as_deref()) else {
        // A version that is not installed is not the same as a system that is not: the others
        // that are installed are named, because they are what a fix would add to or pick from.
        let installed = expander.systems.versions(&system);
        return Err(vec![match &version {
            Some(requested) if !installed.is_empty() => ExpansionFailure::SystemVersionNotLoaded {
                at: at.clone(),
                system,
                version: requested.clone(),
                installed,
            },
            _ => ExpansionFailure::SystemNotLoaded {
                at: at.clone(),
                system,
                version,
            },
        }]);
    };
    let incomplete = |filter: Option<String>| {
        vec![ExpansionFailure::SystemIncomplete {
            at: at.clone(),
            system: system.clone(),
            version: version.clone(),
            content: index.content.clone(),
            filter,
        }]
    };
    note_system(&mut expansion, expander, &system, version.as_deref());
    if let Some(filters) = filters {
        if !index.closed {
            return Err(incomplete(Some(describe_filters(filters))));
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
        return Ok(expansion);
    }
    if !expansion.codes.is_empty() {
        return Ok(expansion);
    }
    if !index.closed {
        return Err(incomplete(None));
    }
    for (code, concept) in &index.codes {
        expansion
            .codes
            .insert((system.clone(), code.clone()), concept.display.clone());
    }
    Ok(expansion)
}

fn nested_present(include: &Json) -> bool {
    include
        .get("valueSet")
        .and_then(|value| value.as_array())
        .is_some_and(|items| !items.is_empty())
}

/// Record the system an expansion drew on, with the version it was actually read at.
fn note_system(
    expansion: &mut Expansion,
    expander: &Expander<'_>,
    system: &str,
    requested: Option<&str>,
) {
    let version = expander
        .systems
        .resolve(system, requested)
        .map(|(version, _)| version.to_string())
        .unwrap_or_else(|| requested.unwrap_or("").to_string());
    expansion.systems.insert((system.to_string(), version));
}

fn filter_supported(filter: &Json) -> bool {
    matches!(
        filter.get("op").and_then(|value| value.as_str()),
        Some(
            "=" | "is-a"
                | "descendent-of"
                | "is-not-a"
                | "exists"
                | "in"
                | "not-in"
                | "generalizes"
        )
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
        let Some((url, version)) = code_system_key(resource) else {
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
        let content = content.to_string();
        let mut codes = BTreeMap::new();
        if let Some(concepts) = resource.get("concept").and_then(|value| value.as_array()) {
            collect_concepts(concepts, None, &mut codes);
        }
        self.systems.insert(
            &url,
            &version,
            SystemIndex {
                closed,
                content,
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
    collect_expansion(contains, &mut expansion, true);
    expansion
}

/// `note_systems` records each system a code names, with no version. It is for a `contains` that
/// is all there is to go on; a stored document already lists its systems, with their versions.
fn collect_expansion(contains: &[Json], expansion: &mut Expansion, note_systems: bool) {
    for item in contains {
        if let Some(code) = item.get("code").and_then(|value| value.as_str()) {
            let system = item
                .get("system")
                .and_then(|value| value.as_str())
                .map(|url| canonical(url).to_string())
                .unwrap_or_default();
            if note_systems && !system.is_empty() {
                expansion.systems.insert((system.clone(), String::new()));
            }
            let display = item
                .get("display")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string());
            expansion.codes.insert((system, code.to_string()), display);
        }
        if let Some(nested) = item.get("contains").and_then(|value| value.as_array()) {
            collect_expansion(nested, expansion, note_systems);
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
    // A document that lists its systems is read as it is, so that what is stored comes back
    // exactly as it was written; one that does not is worked out from its codes.
    let systems_listed = document.get("systems").is_some();
    if let Some(contains) = document.get("contains").and_then(|value| value.as_array()) {
        collect_expansion(contains, &mut expansion, !systems_listed);
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

fn describe_index(index: &SystemIndex, code: &str) -> Option<CodeDescription> {
    let concept = index.codes.get(code)?;
    Some(CodeDescription {
        display: concept.display.clone(),
        definition: concept.definition.clone(),
        inactive: concept.inactive,
        properties: concept.properties.clone(),
    })
}

fn installed_versions(versions: &[String]) -> String {
    versions
        .iter()
        .map(|version| {
            if version.is_empty() {
                "(no version)".to_string()
            } else {
                version.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
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

fn usable_equivalence(equivalence: &str) -> bool {
    !matches!(equivalence, "disjoint" | "unmatched" | "unrelated")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ExpansionFailure, MemoryTerminology, TermAnswer, Terminology};

    fn code_system(url: &str, content: &str, codes: &[&str]) -> serde_json::Value {
        json!({
            "resourceType": "CodeSystem", "url": url, "content": content,
            "concept": codes.iter().map(|code| json!({"code": code})).collect::<Vec<_>>()
        })
    }

    fn value_set(url: &str, compose: serde_json::Value) -> serde_json::Value {
        json!({"resourceType": "ValueSet", "url": url, "compose": compose})
    }

    fn messages(failures: &[ExpansionFailure]) -> Vec<String> {
        failures.iter().map(ExpansionFailure::message).collect()
    }

    #[test]
    fn every_failing_include_is_reported_with_its_position() {
        let terms = MemoryTerminology::from_resources(&[
            code_system("http://example.org/roles", "complete", &["doctor", "nurse"]),
            json!({"resourceType": "CodeSystem", "url": "http://example.org/sct",
                   "version": "2024", "content": "not-present"}),
            code_system("http://example.org/examples", "example", &["a"]),
            value_set(
                "http://example.org/staff",
                json!({"include": [
                    {"system": "http://example.org/roles"},
                    {"system": "http://example.org/sct", "version": "2024",
                     "filter": [{"property": "concept", "op": "is-a", "value": "223366009"}]},
                    {"system": "http://example.org/examples"},
                    {"system": "http://example.org/absent"}
                ]}),
            ),
        ]);
        assert_eq!(
            terms.contains("http://example.org/staff", None, "doctor"),
            TermAnswer::Unknown
        );
        let failures = terms.why_not_expanded("http://example.org/staff").unwrap();
        let text = messages(&failures);
        assert_eq!(failures.len(), 3, "{text:?}");
        assert_eq!(
            failures[0].expression().unwrap(),
            "ValueSet.compose.include[1]"
        );
        assert!(text[0].contains("http://example.org/sct|2024"), "{text:?}");
        assert!(text[0].contains("concept is-a 223366009"), "{text:?}");
        assert!(
            text[0].contains("none of its concepts are installed"),
            "{text:?}"
        );
        assert_eq!(
            failures[1].expression().unwrap(),
            "ValueSet.compose.include[2]"
        );
        assert!(
            text[1].contains("'example'") && text[1].contains("included whole"),
            "{text:?}"
        );
        assert_eq!(
            failures[2].expression().unwrap(),
            "ValueSet.compose.include[3]"
        );
        assert!(
            text[2].contains("http://example.org/absent is not installed"),
            "{text:?}"
        );
        assert_eq!(
            failures
                .iter()
                .map(ExpansionFailure::reason)
                .collect::<Vec<_>>(),
            [
                "code-system-incomplete",
                "code-system-incomplete",
                "code-system-not-loaded"
            ]
        );
    }

    #[test]
    fn a_failing_dependency_is_named_and_carries_its_own_causes() {
        let terms = MemoryTerminology::from_resources(&[
            code_system("http://example.org/examples", "example", &["a"]),
            value_set(
                "http://example.org/inner",
                json!({"include": [{"system": "http://example.org/examples"}]}),
            ),
            value_set(
                "http://example.org/outer",
                json!({"include": [{"valueSet": ["http://example.org/inner"]}]}),
            ),
            value_set(
                "http://example.org/missing-dependency",
                json!({"include": [{"valueSet": ["http://example.org/nowhere"]}]}),
            ),
        ]);
        let failures = terms.why_not_expanded("http://example.org/outer").unwrap();
        let [ExpansionFailure::Dependency {
            value_set, causes, ..
        }] = failures.as_slice()
        else {
            panic!("{failures:?}");
        };
        assert_eq!(value_set, "http://example.org/inner");
        assert_eq!(causes[0].reason(), "code-system-incomplete");
        let text = failures[0].message();
        assert!(
            text.contains("included value set http://example.org/inner"),
            "{text}"
        );
        assert!(text.contains("'example'"), "{text}");
        let missing = terms
            .why_not_expanded("http://example.org/missing-dependency")
            .unwrap();
        assert!(messages(&missing)[0].contains("http://example.org/nowhere is not loaded"));
    }

    #[test]
    fn cycles_and_unsupported_filters_are_named() {
        let terms = MemoryTerminology::from_resources(&[
            code_system("http://example.org/sys", "complete", &["a"]),
            value_set(
                "http://example.org/a",
                json!({"include": [{"valueSet": ["http://example.org/b"]}]}),
            ),
            value_set(
                "http://example.org/b",
                json!({"include": [{"valueSet": ["http://example.org/a"]}]}),
            ),
            value_set(
                "http://example.org/regex",
                json!({"include": [{"system": "http://example.org/sys",
                    "filter": [{"property": "code", "op": "regex", "value": "a.*"}]}]}),
            ),
            json!({"resourceType": "ValueSet", "url": "http://example.org/empty"}),
        ]);
        let cycle = messages(&terms.why_not_expanded("http://example.org/a").unwrap());
        assert!(cycle[0].contains("includes itself"), "{cycle:?}");
        let regex = terms.why_not_expanded("http://example.org/regex").unwrap();
        assert_eq!(regex[0].reason(), "unsupported-filter");
        assert!(regex[0]
            .message()
            .contains("'code regex' on http://example.org/sys"));
        let empty = terms.why_not_expanded("http://example.org/empty").unwrap();
        assert_eq!(empty[0].reason(), "nothing-to-expand");
        let unknown = terms
            .why_not_expanded("http://example.org/unknown")
            .unwrap();
        assert_eq!(unknown[0].reason(), "value-set-not-loaded");
        assert_eq!(terms.expansion_problem("http://example.org/unknown"), None);
    }

    #[test]
    fn a_stored_catalog_still_explains_what_it_could_not_expand() {
        let resources = vec![
            code_system("http://example.org/fragment", "fragment", &["a"]),
            code_system("http://example.org/sys", "complete", &["x"]),
            value_set(
                "http://example.org/ok",
                json!({"include": [{"system": "http://example.org/sys"}]}),
            ),
            value_set(
                "http://example.org/bad",
                json!({"include": [{"system": "http://example.org/fragment"}]}),
            ),
            value_set(
                "http://example.org/uses-both",
                json!({"include": [
                    {"valueSet": ["http://example.org/ok"]},
                    {"valueSet": ["http://example.org/bad"]}
                ]}),
            ),
        ];
        // What the database holds: the resources, and an expansion only for what could expand.
        let documents = MemoryTerminology::expansion_documents(&resources);
        assert_eq!(documents.len(), 1, "{documents:?}");
        let stored = MemoryTerminology::from_stored(&resources, &documents);
        let unexpandable: Vec<String> = stored
            .unexpandable()
            .into_iter()
            .map(|(url, _)| url)
            .collect();
        assert_eq!(
            unexpandable,
            ["http://example.org/bad", "http://example.org/uses-both"]
        );
        let text = messages(
            &stored
                .why_not_expanded("http://example.org/uses-both")
                .unwrap(),
        )
        .join(" ");
        assert!(
            text.contains("included value set http://example.org/bad"),
            "{text}"
        );
        assert!(text.contains("'fragment'"), "{text}");
        assert!(stored.why_not_expanded("http://example.org/ok").is_none());
    }

    // Versions of one code system ---------------------------------------------------------

    fn versioned(url: &str, version: &str, codes: &[&str]) -> serde_json::Value {
        let mut system = code_system(url, "complete", codes);
        system["version"] = json!(version);
        system
    }

    fn contains_codes(terms: &MemoryTerminology, url: &str) -> Vec<String> {
        let expanded = terms.expand(url, None).expect("it expands");
        expanded["expansion"]["contains"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["code"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn every_version_of_a_code_system_is_kept_and_a_reference_gets_the_one_it_names() {
        // The version can be in the URL, as HL7 packages the v2 tables, or in `version`.
        let mut resources = vec![
            code_system("http://example.org/v2|2.1", "complete", &["a"]),
            code_system("http://example.org/v2|2.4", "complete", &["a", "b", "c"]),
            versioned("http://example.org/loinc", "2.9", &["x"]),
            versioned("http://example.org/loinc", "2.10", &["x", "y"]),
        ];
        for (id, system) in [
            ("old", "http://example.org/v2|2.1"),
            ("new", "http://example.org/v2|2.4"),
            ("latest", "http://example.org/v2"),
        ] {
            resources.push(value_set(
                &format!("http://example.org/{id}"),
                json!({"include": [{"system": system}]}),
            ));
        }
        for (id, version) in [("loinc-old", "2.9"), ("loinc-new", "2.10")] {
            resources.push(value_set(
                &format!("http://example.org/{id}"),
                json!({"include": [{"system": "http://example.org/loinc", "version": version}]}),
            ));
        }
        resources.push(value_set(
            "http://example.org/loinc-latest",
            json!({"include": [{"system": "http://example.org/loinc"}]}),
        ));
        // Whatever the order they are loaded in.
        for order in [resources.clone(), resources.iter().rev().cloned().collect()] {
            let terms = MemoryTerminology::from_resources(&order);
            assert_eq!(contains_codes(&terms, "http://example.org/old"), ["a"]);
            assert_eq!(
                contains_codes(&terms, "http://example.org/new"),
                ["a", "b", "c"]
            );
            assert_eq!(
                contains_codes(&terms, "http://example.org/latest"),
                ["a", "b", "c"]
            );
            assert_eq!(
                contains_codes(&terms, "http://example.org/loinc-old"),
                ["x"]
            );
            assert_eq!(
                contains_codes(&terms, "http://example.org/loinc-new"),
                ["x", "y"]
            );
            // 2.10 comes after 2.9, which text order would get wrong.
            assert_eq!(
                contains_codes(&terms, "http://example.org/loinc-latest"),
                ["x", "y"]
            );
            // The expansion says which version it was read at.
            let documents = terms.documents();
            let old = documents
                .iter()
                .find(|d| d["url"] == "http://example.org/old")
                .unwrap();
            assert_eq!(
                old["systems"],
                json!([{"url": "http://example.org/v2", "version": "2.1"}])
            );
        }
    }

    #[test]
    fn a_version_that_is_not_installed_is_refused_not_read_from_another() {
        let terms = MemoryTerminology::from_resources(&[
            versioned("http://example.org/tasks", "4.0.1", &["a"]),
            code_system("http://example.org/v2|2.4", "complete", &["a"]),
            value_set(
                "http://example.org/pinned",
                json!({"include": [{"system": "http://example.org/tasks", "version": "3.6.0"}]}),
            ),
            value_set(
                "http://example.org/suffix",
                json!({"include": [{"system": "http://example.org/v2|2.1"}]}),
            ),
            value_set(
                "http://example.org/unpinned",
                json!({"include": [{"system": "http://example.org/tasks"}]}),
            ),
        ]);
        let pinned = terms.why_not_expanded("http://example.org/pinned").unwrap();
        assert_eq!(pinned[0].reason(), "code-system-version-not-loaded");
        let text = pinned[0].message();
        assert!(
            text.contains("not at version 3.6.0") && text.contains("installed: 4.0.1"),
            "{text}"
        );
        assert_eq!(
            pinned[0].expression().unwrap(),
            "ValueSet.compose.include[0]"
        );
        let suffix = terms.why_not_expanded("http://example.org/suffix").unwrap();
        assert!(
            suffix[0].message().contains("installed: 2.4"),
            "{:?}",
            suffix[0]
        );
        // No version named: the one that is installed.
        assert_eq!(contains_codes(&terms, "http://example.org/unpinned"), ["a"]);
    }

    #[test]
    fn the_code_system_operations_take_a_version() {
        let terms = MemoryTerminology::from_resources(&[
            code_system("http://example.org/v2|2.1", "complete", &["a"]),
            code_system("http://example.org/v2|2.4", "complete", &["a", "b"]),
        ]);
        let result = |version: Option<&str>, code: &str| {
            let outcome =
                terms.validate_code(None, Some("http://example.org/v2"), version, code, None);
            outcome["parameter"][0]["valueBoolean"].as_bool()
        };
        assert_eq!(result(Some("2.1"), "b"), Some(false), "b is not in 2.1");
        assert_eq!(result(Some("2.4"), "b"), Some(true));
        assert_eq!(
            result(None, "b"),
            Some(true),
            "the latest answers when none is named"
        );
        // `system|version` names one too, and one that is not installed says what is.
        let named = terms.validate_code(None, Some("http://example.org/v2|2.1"), None, "b", None);
        assert_eq!(named["parameter"][0]["valueBoolean"], false);
        let missing =
            terms.validate_code(None, Some("http://example.org/v2"), Some("2.9"), "a", None);
        let text = missing["issue"][0]["diagnostics"].as_str().unwrap();
        assert!(
            text.contains("version 2.9") && text.contains("installed: 2.1, 2.4"),
            "{text}"
        );
        assert!(terms
            .lookup("http://example.org/v2", Some("2.4"), "b")
            .is_some());
        assert!(terms
            .lookup("http://example.org/v2", Some("2.1"), "b")
            .is_none());
        assert!(terms.lookup("http://example.org/v2", None, "b").is_some());
    }

    #[test]
    fn a_value_set_check_refuses_a_system_version_that_is_not_installed() {
        let terms = MemoryTerminology::from_resources(&[
            code_system("http://example.org/v2|2.1", "complete", &["a"]),
            value_set(
                "http://example.org/vs",
                json!({"include": [{"system": "http://example.org/v2|2.1"}]}),
            ),
        ]);
        let check = |version: &str| {
            terms.validate_code(
                Some("http://example.org/vs"),
                Some("http://example.org/v2"),
                Some(version),
                "a",
                None,
            )
        };
        assert_eq!(check("2.1")["parameter"][0]["valueBoolean"], true);
        let refused = check("2.9");
        let text = refused["issue"][0]["diagnostics"].as_str().unwrap();
        assert!(
            text.contains("version 2.9") && text.contains("installed: 2.1"),
            "{text}"
        );
    }

    #[test]
    fn a_code_is_described_by_the_version_its_value_set_was_expanded_from() {
        let system = |version: &str, display: &str| {
            json!({"resourceType": "CodeSystem", "url": "http://example.org/v2", "version": version,
                   "content": "complete", "concept": [{"code": "a", "display": display}]})
        };
        let terms = MemoryTerminology::from_resources(&[
            system("2.1", "Old name"),
            system("2.4", "New name"),
            value_set(
                "http://example.org/vs",
                json!({"include": [{"system": "http://example.org/v2|2.1"}]}),
            ),
        ]);
        let display = |system: Option<&str>| {
            terms
                .describe_bound("http://example.org/vs", system, "a")
                .and_then(|described| described.display)
        };
        assert_eq!(
            display(Some("http://example.org/v2")).as_deref(),
            Some("Old name")
        );
        assert_eq!(display(None).as_deref(), Some("Old name"));
        assert_eq!(
            terms
                .describe("http://example.org/v2", "a")
                .and_then(|described| described.display)
                .as_deref(),
            Some("New name"),
            "with no value set, the latest version describes it"
        );
    }

    #[test]
    fn the_reasons_a_value_set_has_no_expansion_follow_the_terminology() {
        let set = value_set(
            "http://example.org/vs",
            json!({"include": [{"system": "http://example.org/sys"}]}),
        );
        let mut terms = MemoryTerminology::from_resources(std::slice::from_ref(&set));
        let problem = |terms: &MemoryTerminology| terms.expansion_problem("http://example.org/vs");
        assert!(problem(&terms).unwrap().contains("not installed"));
        let arrives = TerminologyChange {
            upserted: vec![code_system("http://example.org/sys", "complete", &["a"])],
            removed: vec![],
        };
        terms.apply(&arrives, std::slice::from_ref(&set));
        assert_eq!(problem(&terms), None, "now it expands");
        let leaves = TerminologyChange {
            upserted: vec![],
            removed: vec![("CodeSystem".into(), "http://example.org/sys".into())],
        };
        terms.apply(&leaves, &[set]);
        assert!(problem(&terms).unwrap().contains("not installed"));
    }

    #[test]
    fn the_row_key_of_a_code_system_says_which_version_it_is() {
        let key = |resource: serde_json::Value| super::code_system_row_key(&resource);
        assert_eq!(key(json!({"url": "http://e/s"})), Some("http://e/s".into()));
        assert_eq!(
            key(json!({"url": "http://e/s", "version": "2"})),
            Some("http://e/s|2".into())
        );
        // A URL that carries its version already says it; the `version` element does not override.
        assert_eq!(
            key(json!({"url": "http://e/s|2.1", "version": "0006"})),
            Some("http://e/s|2.1".into())
        );
        assert_eq!(
            super::code_system_key(&json!({"url": "http://e/s|2.1"})),
            Some(("http://e/s".into(), "2.1".into()))
        );
    }

    #[test]
    fn versions_survive_an_incremental_change() {
        let base = vec![
            code_system("http://example.org/v2|2.1", "complete", &["a"]),
            code_system("http://example.org/v2|2.4", "complete", &["a", "b"]),
            value_set(
                "http://example.org/old",
                json!({"include": [{"system": "http://example.org/v2|2.1"}]}),
            ),
            value_set(
                "http://example.org/new",
                json!({"include": [{"system": "http://example.org/v2|2.4"}]}),
            ),
        ];
        // One version changes and the other does not: only its dependents move.
        assert_matches_a_rebuild(
            &base,
            change(
                vec![code_system(
                    "http://example.org/v2|2.4",
                    "complete",
                    &["a", "b", "z"],
                )],
                &[],
            ),
        );
        // One version goes: what named it can no longer expand, and the other is untouched.
        assert_matches_a_rebuild(
            &base,
            change(vec![], &[("CodeSystem", "http://example.org/v2|2.1")]),
        );
        // A version arrives that something was waiting for.
        let mut waiting = base.clone();
        waiting.push(value_set(
            "http://example.org/later",
            json!({"include": [{"system": "http://example.org/v2|3.0"}]}),
        ));
        assert_matches_a_rebuild(
            &waiting,
            change(
                vec![code_system("http://example.org/v2|3.0", "complete", &["q"])],
                &[],
            ),
        );
    }

    #[test]
    fn a_stored_expansion_comes_back_exactly_as_it_was_written() {
        // The systems an expansion used, with their versions, survive the trip through the store.
        let terms = MemoryTerminology::from_resources(&[
            json!({"resourceType": "CodeSystem", "url": "http://example.org/v", "version": "2.1",
                   "content": "complete", "concept": [{"code": "a"}, {"code": "b"}]}),
            value_set(
                "http://example.org/vs",
                json!({"include": [{"system": "http://example.org/v"}]}),
            ),
        ]);
        let written = terms.documents();
        assert_eq!(
            written[0]["systems"],
            json!([{"url": "http://example.org/v", "version": "2.1"}])
        );
        let reloaded = MemoryTerminology::from_stored(&[], &written);
        assert_eq!(reloaded.documents(), written);
    }

    // Incremental application ------------------------------------------------------------

    use super::{ExpansionUpdate, TerminologyChange};

    fn url_of(resource: &serde_json::Value) -> String {
        resource["url"].as_str().unwrap().to_string()
    }

    /// The state a server is in: stored resources, and an expansion row for each value set that
    /// has one. Apply `change` incrementally, and return it with the rows after the updates.
    fn incremental(
        base: &[serde_json::Value],
        change: &TerminologyChange,
    ) -> (MemoryTerminology, Vec<serde_json::Value>) {
        let documents = MemoryTerminology::expansion_documents(base);
        let mut terms = MemoryTerminology::from_stored(base, &documents);
        let dependents = terms.dependents_of(&change.urls());
        let loaded: Vec<serde_json::Value> = base
            .iter()
            .filter(|resource| {
                resource["resourceType"] == "ValueSet" && dependents.contains(&url_of(resource))
            })
            .cloned()
            .collect();
        let updates = terms.apply(change, &loaded);
        // Replay what a store would do with the updates.
        let mut rows: std::collections::BTreeMap<String, serde_json::Value> = documents
            .into_iter()
            .map(|document| (url_of(&document), document))
            .collect();
        for ExpansionUpdate { url, document } in updates {
            match document {
                Some(document) => rows.insert(url, document),
                None => rows.remove(&url),
            };
        }
        (terms, rows.into_values().collect())
    }

    fn after(base: &[serde_json::Value], change: &TerminologyChange) -> Vec<serde_json::Value> {
        let removed: Vec<String> = change.removed.iter().map(|(_, url)| url.clone()).collect();
        let replaced: Vec<String> = change.upserted.iter().map(url_of).collect();
        let mut resources: Vec<serde_json::Value> = base
            .iter()
            .filter(|resource| {
                let url = url_of(resource);
                !removed.contains(&url) && !replaced.contains(&url)
            })
            .cloned()
            .collect();
        resources.extend(change.upserted.iter().cloned());
        resources
    }

    /// A full rebuild of the changed terminology and the incremental result agree on every
    /// expansion, on why each value set cannot expand, and on concept map translation.
    fn assert_matches_a_rebuild(base: &[serde_json::Value], change: TerminologyChange) {
        let (inc, rows) = incremental(base, &change);
        let final_resources = after(base, &change);
        let full = MemoryTerminology::from_resources(&final_resources);
        assert_eq!(inc.documents(), full.documents(), "expansions differ");
        assert_eq!(rows, full.documents(), "rows to store differ");
        let reasons = |terms: &MemoryTerminology| -> Vec<(String, Vec<String>)> {
            terms
                .unexpandable()
                .into_iter()
                .map(|(url, failures)| (url, failures.iter().map(|f| f.message()).collect()))
                .collect()
        };
        assert_eq!(reasons(&inc), reasons(&full), "reasons differ");
        for resource in &final_resources {
            if resource["resourceType"] == "ConceptMap" {
                for code in ["a", "b", "c"] {
                    assert_eq!(
                        inc.translate(
                            Some(&url_of(resource)),
                            "http://example.org/src",
                            code,
                            None
                        ),
                        full.translate(
                            Some(&url_of(resource)),
                            "http://example.org/src",
                            code,
                            None
                        ),
                        "translation of {code} differs"
                    );
                }
            }
        }
        // The same answer comes back for membership.
        for resource in &final_resources {
            if resource["resourceType"] == "ValueSet" {
                for code in ["a", "b", "c", "x"] {
                    assert_eq!(
                        inc.contains(&url_of(resource), None, code),
                        full.contains(&url_of(resource), None, code)
                    );
                }
            }
        }
    }

    fn change(upserted: Vec<serde_json::Value>, removed: &[(&str, &str)]) -> TerminologyChange {
        TerminologyChange {
            upserted,
            removed: removed
                .iter()
                .map(|(kind, url)| (kind.to_string(), url.to_string()))
                .collect(),
        }
    }

    fn catalog() -> Vec<serde_json::Value> {
        vec![
            code_system("http://example.org/src", "complete", &["a", "b", "c"]),
            code_system("http://example.org/partial", "example", &["a"]),
            value_set(
                "http://example.org/all",
                json!({"include": [{"system": "http://example.org/src"}]}),
            ),
            value_set(
                "http://example.org/over-all",
                json!({"include": [{"valueSet": ["http://example.org/all"]}]}),
            ),
            value_set(
                "http://example.org/over-over",
                json!({"include": [{"valueSet": ["http://example.org/over-all"]}],
                       "exclude": [{"system": "http://example.org/src", "concept": [{"code": "c"}]}]}),
            ),
            value_set(
                "http://example.org/needs-partial",
                json!({"include": [{"system": "http://example.org/partial"}]}),
            ),
            value_set(
                "http://example.org/over-partial",
                json!({"include": [{"valueSet": ["http://example.org/needs-partial"]}]}),
            ),
            json!({"resourceType": "ConceptMap", "url": "http://example.org/map",
                   "group": [{"source": "http://example.org/src", "target": "http://example.org/dst",
                              "element": [{"code": "a", "target": [{"code": "x", "equivalence": "equivalent"}]}]}]}),
        ]
    }

    #[test]
    fn a_new_value_set_is_expanded_alone() {
        assert_matches_a_rebuild(
            &catalog(),
            change(
                vec![value_set(
                    "http://example.org/new",
                    json!({"include": [{"system": "http://example.org/src", "concept": [{"code": "a"}]}]}),
                )],
                &[],
            ),
        );
        // ...and one that includes what already exists, or cannot expand.
        assert_matches_a_rebuild(
            &catalog(),
            change(
                vec![
                    value_set(
                        "http://example.org/new",
                        json!({"include": [{"valueSet": ["http://example.org/over-all"]},
                                           {"system": "http://example.org/partial"}]}),
                    ),
                    value_set(
                        "http://example.org/new-over-new",
                        json!({"include": [{"valueSet": ["http://example.org/new"]}]}),
                    ),
                ],
                &[],
            ),
        );
    }

    #[test]
    fn an_edited_value_set_carries_its_dependents_with_it() {
        assert_matches_a_rebuild(
            &catalog(),
            change(
                vec![value_set(
                    "http://example.org/all",
                    json!({"include": [{"system": "http://example.org/src", "concept": [{"code": "a"}]}]}),
                )],
                &[],
            ),
        );
        // Edited so that it can no longer expand: the two above it cannot either.
        assert_matches_a_rebuild(
            &catalog(),
            change(
                vec![value_set(
                    "http://example.org/all",
                    json!({"include": [{"system": "http://example.org/partial"}]}),
                )],
                &[],
            ),
        );
    }

    #[test]
    fn a_deleted_value_set_leaves_those_above_it_unexpandable() {
        assert_matches_a_rebuild(
            &catalog(),
            change(vec![], &[("ValueSet", "http://example.org/all")]),
        );
        assert_matches_a_rebuild(
            &catalog(),
            change(vec![], &[("ValueSet", "http://example.org/needs-partial")]),
        );
    }

    #[test]
    fn a_code_system_that_arrives_completes_every_value_set_waiting_for_it() {
        // `partial` is an example system, so two value sets cannot expand; once it is complete
        // they can, and the one that includes the other follows.
        assert_matches_a_rebuild(
            &catalog(),
            change(
                vec![code_system(
                    "http://example.org/partial",
                    "complete",
                    &["a", "b"],
                )],
                &[],
            ),
        );
        // A system nothing could find before.
        let mut base = catalog();
        base.push(value_set(
            "http://example.org/waiting",
            json!({"include": [{"system": "http://example.org/late"}]}),
        ));
        assert_matches_a_rebuild(
            &base,
            change(
                vec![code_system("http://example.org/late", "complete", &["x"])],
                &[],
            ),
        );
    }

    #[test]
    fn a_code_system_that_changes_or_goes_changes_what_includes_it() {
        assert_matches_a_rebuild(
            &catalog(),
            change(
                vec![code_system(
                    "http://example.org/src",
                    "complete",
                    &["a", "b", "c", "x"],
                )],
                &[],
            ),
        );
        assert_matches_a_rebuild(
            &catalog(),
            change(
                vec![code_system("http://example.org/src", "fragment", &["a"])],
                &[],
            ),
        );
        assert_matches_a_rebuild(
            &catalog(),
            change(vec![], &[("CodeSystem", "http://example.org/src")]),
        );
    }

    #[test]
    fn a_concept_map_is_replaced_and_removed_like_the_rest() {
        let replacement = json!({"resourceType": "ConceptMap", "url": "http://example.org/map",
            "group": [{"source": "http://example.org/src", "target": "http://example.org/dst",
                       "element": [{"code": "b", "target": [{"code": "y", "equivalence": "equivalent"}]}]}]});
        assert_matches_a_rebuild(&catalog(), change(vec![replacement], &[]));
        assert_matches_a_rebuild(
            &catalog(),
            change(vec![], &[("ConceptMap", "http://example.org/map")]),
        );
    }

    #[test]
    fn dependents_are_found_through_every_level_and_never_include_the_change_itself() {
        let terms = MemoryTerminology::from_resources(&catalog());
        let urls = |changed: &[&str]| {
            terms.dependents_of(
                &changed
                    .iter()
                    .map(|url| url.to_string())
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            urls(&["http://example.org/src"]),
            [
                "http://example.org/all",
                "http://example.org/over-all",
                "http://example.org/over-over"
            ]
        );
        assert_eq!(
            urls(&["http://example.org/all"]),
            [
                "http://example.org/over-all",
                "http://example.org/over-over"
            ]
        );
        assert_eq!(
            urls(&["http://example.org/over-over"]),
            Vec::<String>::new()
        );
        assert_eq!(
            urls(&["http://example.org/unrelated"]),
            Vec::<String>::new()
        );
    }

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
        assert!(terms.subsumes("http://example.org/animals", None, "mammal", "dog"));
        assert!(!terms.subsumes("http://example.org/animals", None, "dog", "mammal"));
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
        assert_eq!(
            terms.translate_coding(
                "http://example.org/map",
                Some("http://example.org/src"),
                "a"
            ),
            Some(json!("b"))
        );
        assert!(terms
            .translate_coding(
                "http://example.org/other",
                Some("http://example.org/src"),
                "a"
            )
            .is_none());
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
        let ambiguous = json!({
            "resourceType": "ConceptMap",
            "url": "http://example.org/map",
            "group": [
                {
                    "source": "http://example.org/src-a",
                    "target": "http://example.org/dst",
                    "element": [{"code": "M", "target": [{"code": "male", "equivalence": "equivalent"}]}]
                },
                {
                    "source": "http://example.org/src-b",
                    "target": "http://example.org/dst",
                    "element": [{"code": "M", "target": [{"code": "other", "equivalence": "equivalent"}]}]
                }
            ]
        });
        let terms = MemoryTerminology::from_resources(&[ambiguous]);
        assert!(terms
            .translate_coding("http://example.org/map", None, "M")
            .is_none());
        assert_eq!(
            terms.translate_coding(
                "http://example.org/map",
                Some("http://example.org/src-a"),
                "M"
            ),
            Some(json!("male"))
        );
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
