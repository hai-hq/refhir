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

/// Where a resource came from: the id of the package that holds it. A value set is read against
/// the code systems its own package holds before any other, because that is what it was written
/// for, and what installing the package expanded it with. Terminology built from plain resources
/// (`from_resources`) is all one scope.
pub type Scope = i64;

#[derive(Clone, Debug, Default, PartialEq)]
struct Concept {
    display: Option<String>,
    definition: Option<String>,
    inactive: bool,
    parents: Vec<String>,
    properties: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
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

/// One content of a code system version, and the packages that hold exactly it. Two packages
/// that carry the same version of a system almost always carry the same content (`core` and
/// `examples`), and then they share one copy; when they differ, each content is kept.
#[derive(Clone, Debug)]
struct Variant {
    scopes: BTreeSet<Scope>,
    index: SystemIndex,
}

/// Code systems by URL and version. Every version that is installed is kept, so a reference that
/// names one gets that one, whatever order they were loaded in.
#[derive(Clone, Debug, Default)]
struct Systems {
    by_url: BTreeMap<String, BTreeMap<String, Vec<Variant>>>,
    /// The FHIR release the server serves (`4.0.1`). A reference that names no version means the
    /// version of that release when one is installed, not the highest version: a cross-version
    /// package such as `hl7.fhir.uv.xver-r5.r4` carries `5.0.0` copies of R4's own code systems,
    /// and a value set written for R4 does not mean R5's codes.
    release: Option<String>,
}

/// The content `scope` holds, or the one loaded last when it holds none.
fn variant_for(variants: &[Variant], scope: Option<Scope>) -> Option<&SystemIndex> {
    scope
        .and_then(|scope| variants.iter().rev().find(|v| v.scopes.contains(&scope)))
        .or_else(|| variants.last())
        .map(|variant| &variant.index)
}

fn held_by(variants: &[Variant], scope: Scope) -> bool {
    variants
        .iter()
        .any(|variant| variant.scopes.contains(&scope))
}

impl Systems {
    /// What `scope` held under this URL and version is replaced by `index`.
    fn insert(&mut self, scope: Scope, url: &str, version: &str, index: SystemIndex) {
        let variants = self
            .by_url
            .entry(url.to_string())
            .or_default()
            .entry(version.to_string())
            .or_default();
        for variant in variants.iter_mut() {
            variant.scopes.remove(&scope);
        }
        variants.retain(|variant| !variant.scopes.is_empty());
        // The variant loaded last answers a lookup that names no package, so one that matches
        // moves to the end rather than staying where it was.
        let mut variant = match variants.iter().position(|held| held.index == index) {
            Some(position) => variants.remove(position),
            None => Variant {
                scopes: BTreeSet::new(),
                index,
            },
        };
        variant.scopes.insert(scope);
        variants.push(variant);
    }

    /// Remove what `scope` holds of one version, or of every version when `version` is `None`.
    /// A copy that another package also holds stays for that package.
    fn remove(&mut self, scope: Scope, url: &str, version: Option<&str>) {
        let Some(versions) = self.by_url.get_mut(url) else {
            return;
        };
        for (held, variants) in versions.iter_mut() {
            if version.is_some_and(|version| version != held) {
                continue;
            }
            for variant in variants.iter_mut() {
                variant.scopes.remove(&scope);
            }
            variants.retain(|variant| !variant.scopes.is_empty());
        }
        versions.retain(|_, variants| !variants.is_empty());
        if versions.is_empty() {
            self.by_url.remove(url);
        }
    }

    /// The version asked for, or, when none is, the installed version that is the server's
    /// release, or else the latest installed. Returns the version found.
    fn resolve(&self, url: &str, version: Option<&str>) -> Option<(&str, &SystemIndex)> {
        self.resolve_in(None, false, url, version)
    }

    /// Like [`Self::resolve`], for a value set that came from `scope`. With no version named,
    /// the versions that package holds come first, the release's among them first of all:
    /// installing the package expanded its value sets with exactly those, and a copy of the same
    /// system that another package carries (another release's, an older snapshot's) is not what
    /// the value set meant. Only a system the package does not hold is read from the others,
    /// unless `strict`: then only what the package holds is installed, as when it is installed
    /// alone.
    fn resolve_in(
        &self,
        scope: Option<Scope>,
        strict: bool,
        url: &str,
        version: Option<&str>,
    ) -> Option<(&str, &SystemIndex)> {
        let versions = self.by_url.get(url)?;
        let found = match version.filter(|version| !version.is_empty()) {
            Some(version) => versions.get_key_value(version).filter(|(_, variants)| {
                !strict || scope.is_none_or(|scope| held_by(variants, scope))
            }),
            None => {
                let own = scope.filter(|scope| versions.values().any(|v| held_by(v, *scope)));
                if strict && scope.is_some() && own.is_none() {
                    return None;
                }
                let pool = || {
                    versions.iter().filter(move |(_, variants)| {
                        own.is_none_or(|scope| held_by(variants, scope))
                    })
                };
                self.release
                    .as_deref()
                    .and_then(|release| pool().find(|(version, _)| version.as_str() == release))
                    .or_else(|| pool().max_by(|left, right| version_order(left.0, right.0)))
            }
        };
        found
            .and_then(|(version, variants)| Some((version.as_str(), variant_for(variants, scope)?)))
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
    /// The package whose definition of the value set this was expanded from, when known. It says
    /// which content of a code system version two packages hold differently the codes came from.
    scope: Option<Scope>,
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
    unexpanded: Definitions,
    /// Why each of those has none. Worked out when the terminology is built or changed, so that
    /// a check against one of them is a lookup, not an expansion.
    reasons: BTreeMap<String, Vec<ExpansionFailure>>,
    /// For each value set, the code systems and value sets its `compose` draws on. It says which
    /// expansions a change to a code system or value set can alter.
    uses: BTreeMap<String, BTreeSet<String>>,
}

/// An expansion on offer for a value set: the one inside its definition, or a stored document.
enum Offer<'a> {
    Inline(&'a [Json]),
    Stored(&'a Json),
}

/// Every definition of each value set URL, with the package it came from, ordered for use (see
/// `order_for_use`): the one in use is last, which is the one at the server's release, or with
/// none the one loaded last. It answers a reference that comes from nowhere in particular; a
/// reference from a package prefers that package's own definition, as installing the package did.
type Definitions = BTreeMap<String, Vec<(Scope, Json)>>;

impl MemoryTerminology {
    pub fn from_resources(resources: &[Json]) -> Self {
        Self::from_resources_scoped(&unscoped(resources), None)
    }

    /// Like [`Self::from_resources`], for a server that serves FHIR `release` and resources that
    /// each come from a package. A value set is expanded with the versions of a code system that
    /// its own package holds; only a system it does not hold is read from the others. With no
    /// version named, the release's version comes first.
    pub fn from_resources_scoped(resources: &[(Scope, &Json)], release: Option<&str>) -> Self {
        let (mut terms, value_sets) = index_resources(resources, release);
        let known = BTreeMap::new();
        let mut expander = Expander {
            systems: &terms.systems,
            value_sets: &value_sets,
            scope: None,
            strict: false,
            known: &known,
            cache: BTreeMap::new(),
            stack: Vec::new(),
        };
        for url in value_sets.keys() {
            if let Ok(expansion) = expander.expand_in_use(url) {
                terms.expansions.insert(url.clone(), expansion);
            }
        }
        terms.keep_unexpanded(value_sets);
        terms
    }

    fn keep_unexpanded(&mut self, value_sets: Definitions) {
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
            scope: None,
            strict: false,
            known: &self.expansions,
            cache: BTreeMap::new(),
            stack: Vec::new(),
        };
        match expander.expand_in_use(url) {
            Ok(_) => vec![ExpansionFailure::NotExpanded {
                value_set: url.to_string(),
            }],
            Err(failures) => failures,
        }
    }

    /// Membership comes from `expansions`. Code systems and concept maps still come from `resources`.
    pub fn from_stored(resources: &[Json], expansions: &[Json]) -> Self {
        Self::from_stored_scoped(&unscoped(resources), &unscoped(expansions), None)
    }

    /// Like [`Self::from_stored`], for a server that serves FHIR `release` and resources and
    /// stored expansions that each come from a package. A value set that several packages define, and expand, is answered by the
    /// expansion of the definition in use (see [`winning_scope`]), or of the next one that has
    /// one: not by whichever package was installed last.
    pub fn from_stored_scoped(
        resources: &[(Scope, &Json)],
        expansions: &[(Scope, &Json)],
        release: Option<&str>,
    ) -> Self {
        let (mut terms, value_sets) = index_resources(resources, release);
        // Every expansion on offer for each value set, with the package it came from, in the
        // order loaded: those inside the value sets, then the stored ones. They are only borrowed
        // here; the one that is chosen is read once.
        let mut offered: BTreeMap<String, Vec<(Scope, Offer<'_>)>> = BTreeMap::new();
        for (scope, resource) in resources {
            if resource
                .get("resourceType")
                .and_then(|value| value.as_str())
                != Some("ValueSet")
            {
                continue;
            }
            let Some(contains) = resource
                .pointer("/expansion/contains")
                .and_then(|value| value.as_array())
            else {
                continue;
            };
            let Some(url) = resource
                .get("url")
                .and_then(|value| value.as_str())
                .filter(|url| !url.is_empty())
            else {
                continue;
            };
            offered
                .entry(canonical(url).to_string())
                .or_default()
                .push((*scope, Offer::Inline(contains)));
        }
        for (scope, document) in expansions {
            if let Some(url) = document.get("url").and_then(|value| value.as_str()) {
                offered
                    .entry(canonical(url).to_string())
                    .or_default()
                    .push((*scope, Offer::Stored(document)));
            }
        }
        for (url, candidates) in offered {
            let chosen = value_sets
                .get(&url)
                .and_then(|definitions| {
                    definitions.iter().rev().find_map(|(owner, _)| {
                        candidates.iter().rposition(|(scope, _)| scope == owner)
                    })
                })
                .unwrap_or(candidates.len() - 1);
            let (scope, offer) = &candidates[chosen];
            let mut expansion = match offer {
                Offer::Inline(contains) => expansion_from_contains(contains),
                Offer::Stored(document) => match expansion_from_document(document) {
                    Some((_, expansion)) => expansion,
                    None => continue,
                },
            };
            expansion.scope = Some(*scope);
            terms.expansions.insert(url, expansion);
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
        Self::expansion_documents_scoped(&unscoped(resources), None)
    }

    /// The expansions of every value set in `resources`, each read with the code system versions
    /// its own package holds. A rebuild over every package at once gives each value set the
    /// expansion installing its package gave it, not one that depends on what else is installed.
    pub fn expansion_documents_scoped(
        resources: &[(Scope, &Json)],
        release: Option<&str>,
    ) -> Vec<Json> {
        Self::from_resources_scoped(resources, release).documents()
    }

    /// The FHIR release this terminology is read for, when it was told.
    pub fn release(&self) -> Option<&str> {
        self.systems.release.as_deref()
    }

    /// Set the FHIR release the server serves, for terminology built without it. Later lookups
    /// and incremental changes then resolve an unversioned reference the same way.
    pub fn set_release(&mut self, release: Option<&str>) {
        self.systems.release = release
            .filter(|release| !release.is_empty())
            .map(str::to_string);
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
        let expansion = self.expansions.get(canonical(value_set));
        let version = expansion.and_then(|expansion| {
            expansion
                .systems
                .iter()
                .find(|(used, _)| used == url)
                .map(|(_, version)| version.as_str())
        });
        let made_from = expansion.and_then(|expansion| expansion.scope);
        match version
            .filter(|version| !version.is_empty())
            .and_then(|version| {
                self.systems
                    .resolve_in(made_from, false, url, Some(version))
            }) {
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
/// What any definition of a value set includes: whichever one answers for it, a change to
/// something it includes can alter the answer.
fn references_of(definitions: &[(Scope, Json)]) -> BTreeSet<String> {
    definitions
        .iter()
        .flat_map(|(_, definition)| compose_references(definition))
        .collect()
}

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
    fn record_references(&mut self, value_sets: &Definitions) {
        self.uses = value_sets
            .iter()
            .map(|(url, definitions)| (url.clone(), references_of(definitions)))
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
        self.apply_scoped(change, 0, &unscoped(dependents))
    }

    /// Like [`Self::apply`], for terminology whose resources come from packages: `scope` is the
    /// package the change is written to, and each dependent comes with the package that defines
    /// it, once for every package that defines it. Those value sets are read as
    /// [`Self::from_resources_scoped`] reads them, and which definition is in use is decided as
    /// it decides it. A value set they include that is not among them is read from the
    /// expansions that exist, which is the definition in use for it and not necessarily the one
    /// the including package defines.
    pub fn apply_scoped(
        &mut self,
        change: &TerminologyChange,
        scope: Scope,
        dependents: &[(Scope, &Json)],
    ) -> Vec<ExpansionUpdate> {
        // Every definition of each value set that is read again, with its package.
        let mut changed = Definitions::new();
        let mut gone: BTreeSet<String> = BTreeSet::new();
        for (type_name, key) in &change.removed {
            match type_name.as_str() {
                "CodeSystem" => {
                    // The key a code system is stored under is `url` or `url|version`, which
                    // says which version went. Without a version, the unversioned one did.
                    let (base, version) = split_system(key);
                    self.systems
                        .remove(scope, base, Some(version.unwrap_or("")));
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
                Some("CodeSystem") => self.add_system(resource, scope),
                Some("ConceptMap") => {
                    self.remove_map(&url);
                    self.add_map(resource);
                }
                Some("ValueSet") => {
                    gone.remove(&url);
                    let definitions = changed.entry(url).or_default();
                    definitions.retain(|(owner, _)| *owner != scope);
                    definitions.push((scope, resource.clone()));
                }
                _ => {}
            }
        }
        for (owner, resource) in dependents {
            if let Some(url) = resource.get("url").and_then(|value| value.as_str()) {
                let definitions = changed.entry(canonical(url).to_string()).or_default();
                definitions.retain(|(held, _)| held != owner);
                definitions.push((*owner, (*resource).clone()));
            }
        }
        let release = self.systems.release.clone();
        for definitions in changed.values_mut() {
            prefer_release(definitions, release.as_deref());
        }
        for url in changed.keys().chain(gone.iter()) {
            self.expansions.remove(url);
            self.unexpanded.remove(url);
            self.reasons.remove(url);
            self.uses.remove(url);
        }
        for (url, definitions) in &changed {
            self.uses.insert(url.clone(), references_of(definitions));
        }
        // A value set that cannot be expanded may be what a changed one includes, and its own
        // reasons are worked out from its resource, so those are in scope too.
        let mut pool: Definitions = changed.clone();
        for (url, definitions) in &self.unexpanded {
            pool.entry(url.clone())
                .or_insert_with(|| definitions.clone());
        }
        let results: Vec<(String, Result<Expansion, Failures>)> = {
            let mut expander = Expander {
                systems: &self.systems,
                value_sets: &pool,
                scope: None,
                strict: false,
                known: &self.expansions,
                cache: BTreeMap::new(),
                stack: Vec::new(),
            };
            changed
                .keys()
                .map(|url| (url.clone(), expander.expand_in_use(url)))
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
                    if let Some(definitions) = changed.get(&url) {
                        self.unexpanded.insert(url.clone(), definitions.clone());
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
    value_sets: &'a Definitions,
    /// The scope of the value set being expanded, which says whose code system versions and
    /// value set definitions come first. At the top level, where it is not known, it is `None`.
    scope: Option<Scope>,
    /// Read each value set against what its own package holds and nothing else, as installing
    /// the package alone did. Expansions that already exist are not used either.
    strict: bool,
    /// Expansions that already exist. They are used as they are rather than recomputed.
    known: &'a BTreeMap<String, Expansion>,
    cache: BTreeMap<(String, Scope, bool), Result<Expansion, Failures>>,
    stack: Vec<String>,
}

impl Expander<'_> {
    /// A system, read as the value set being expanded reads it.
    fn resolve(&self, system: &str, version: Option<&str>) -> Option<(&str, &SystemIndex)> {
        self.systems
            .resolve_in(self.scope, self.strict, system, version)
    }

    /// A value set asked for by its URL alone: the definition in use, or, when that cannot be
    /// expanded, the next one that can. Each is tried first as installing its own package tried
    /// it, against what that package holds alone, so that a start (which reads what installing
    /// each package stored) and a rebuild name the same definition. Only when none can be
    /// expanded that way does the rest of the store come in, which is how a value set that waits
    /// on another package's code system gets its expansion. When none can be expanded at all, it
    /// is the definition in use that is explained.
    fn expand_in_use(&mut self, url: &str) -> Result<Expansion, Failures> {
        let url = canonical(url).to_string();
        let owners: Vec<Scope> = self
            .value_sets
            .get(&url)
            .map(|definitions| definitions.iter().rev().map(|(owner, _)| *owner).collect())
            .unwrap_or_default();
        let outer_strict = self.strict;
        let mut first = None;
        for strict in [true, false] {
            self.strict = strict;
            for &owner in &owners {
                let outer = self.scope.replace(owner);
                let result = self.expand(&url);
                self.scope = outer;
                match result {
                    Ok(expansion) => {
                        self.strict = outer_strict;
                        return Ok(expansion);
                    }
                    Err(failures) if !strict => {
                        first.get_or_insert(failures);
                    }
                    Err(_) => {}
                }
            }
        }
        self.strict = outer_strict;
        match first {
            Some(failures) => Err(failures),
            None => self.expand(&url),
        }
    }

    fn expand(&mut self, url: &str) -> Result<Expansion, Failures> {
        let url = canonical(url).to_string();
        if !self.strict {
            if let Some(known) = self.known.get(&url) {
                return Ok(known.clone());
            }
        }
        // The definition of a value set named from a package is that package's own, when it has
        // one; otherwise the one in use, which is the last. A package read alone has only its
        // own. The definitions are borrowed from the data, not from `self`, so they are not
        // copied to be read.
        let value_sets = self.value_sets;
        let definition = value_sets.get(&url).and_then(|definitions| {
            let own = self
                .scope
                .and_then(|scope| definitions.iter().rev().find(|(owner, _)| *owner == scope));
            if self.strict && self.scope.is_some() {
                own
            } else {
                own.or_else(|| definitions.last())
            }
        });
        let Some((owner, value_set)) = definition else {
            return Err(vec![ExpansionFailure::NotLoaded { value_set: url }]);
        };
        let owner = *owner;
        let key = (url.clone(), owner, self.strict);
        if let Some(cached) = self.cache.get(&key) {
            return cached.clone();
        }
        if self.stack.iter().any(|item| item == &url) {
            return Err(vec![ExpansionFailure::Cycle { value_set: url }]);
        }
        self.stack.push(url.clone());
        // Its own includes are read as its own package reads them.
        let outer = self.scope.replace(owner);
        let mut expansion = expand_value_set(self, &url, value_set);
        self.scope = outer;
        self.stack.pop();
        if let Ok(expansion) = &mut expansion {
            expansion.scope = Some(owner);
        }
        self.cache.insert(key, expansion.clone());
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
    let Some((_, index)) = expander.resolve(&system, version.as_deref()) else {
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

/// Plain resources as one scope: terminology that does not track packages.
fn unscoped(resources: &[Json]) -> Vec<(Scope, &Json)> {
    resources.iter().map(|resource| (0, resource)).collect()
}

/// Whether a definition is at `release`: the version a server for that release means by a
/// canonical. Other packages carry other releases' copies of a release's own value sets (the
/// extensions package has `5.3.0-ballot-tc1` copies of R4's, pointing at R5's code system URLs),
/// and those are not what an R4 server's value sets mean.
fn at_release(definition: &Json, release: Option<&str>) -> bool {
    release.is_some() && definition.get("version").and_then(|value| value.as_str()) == release
}

/// Order the definitions of one canonical so that the one in use is last: those not at the
/// release first, then those at the release, each in the order loaded. The one in use is then the
/// last at the release, or with none the last loaded, and when it cannot be expanded the ones
/// before it are tried in turn, those at the release ahead of those that are not. Versions are
/// not compared. They come in styles that do not order against each other (core's `2018-08-12`
/// and `2.9` against terminology's `3.0.0`, and in one case `1.0.0`), so a rule that took the
/// highest would be wrong about some of them, where the install order is not.
fn order_for_use<T>(definitions: &mut Vec<(Scope, T)>, at_release: impl Fn(&T) -> bool) {
    let (mut rest, at): (Vec<_>, Vec<_>) = std::mem::take(definitions)
        .into_iter()
        .partition(|(_, definition)| !at_release(definition));
    rest.extend(at);
    *definitions = rest;
}

fn prefer_release(definitions: &mut Vec<(Scope, Json)>, release: Option<&str>) {
    order_for_use(definitions, |definition| at_release(definition, release));
}

/// Of the definitions of one canonical, in the order loaded, the scope of the one in use. It is
/// the rule the index orders by, so everything that stores or reads an expansion of the value set
/// agrees on this one and what a rebuild stores is what a start loads. A package that has more
/// than one is counted by the last, as the index keeps it.
pub fn winning_scope(definitions: &[(Scope, &Json)], release: Option<&str>) -> Option<Scope> {
    let mut held: Vec<(Scope, &Json)> = Vec::new();
    for (scope, definition) in definitions {
        held.retain(|(owner, _)| owner != scope);
        held.push((*scope, definition));
    }
    order_for_use(&mut held, |definition| at_release(definition, release));
    held.last().map(|(scope, _)| *scope)
}

/// [`winning_scope`] for every value set in `resources`, by canonical URL.
pub fn winning_scopes(
    resources: &[(Scope, &Json)],
    release: Option<&str>,
) -> BTreeMap<String, Scope> {
    let mut by_url: BTreeMap<String, Vec<(Scope, &Json)>> = BTreeMap::new();
    for (scope, resource) in resources {
        if resource
            .get("resourceType")
            .and_then(|value| value.as_str())
            != Some("ValueSet")
        {
            continue;
        }
        if let Some(url) = resource.get("url").and_then(|value| value.as_str()) {
            by_url
                .entry(canonical(url).to_string())
                .or_default()
                .push((*scope, resource));
        }
    }
    by_url
        .into_iter()
        .filter_map(|(url, held)| Some((url, winning_scope(&held, release)?)))
        .collect()
}

fn index_resources(
    resources: &[(Scope, &Json)],
    release: Option<&str>,
) -> (MemoryTerminology, Definitions) {
    let mut terms = MemoryTerminology::default();
    terms.set_release(release);
    let mut value_sets = Definitions::new();
    for (scope, resource) in resources {
        match resource
            .get("resourceType")
            .and_then(|value| value.as_str())
        {
            Some("CodeSystem") => terms.add_system(resource, *scope),
            Some("ConceptMap") => terms.add_map(resource),
            Some("ValueSet") => {
                if let Some(url) = resource.get("url").and_then(|value| value.as_str()) {
                    // What this package defined before is replaced; the definition loaded last
                    // stays last.
                    let definitions = value_sets.entry(canonical(url).to_string()).or_default();
                    definitions.retain(|(owner, _)| owner != scope);
                    definitions.push((*scope, (*resource).clone()));
                }
            }
            _ => {}
        }
    }
    for definitions in value_sets.values_mut() {
        prefer_release(definitions, release);
    }
    (terms, value_sets)
}

impl MemoryTerminology {
    fn add_system(&mut self, resource: &Json, scope: Scope) {
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
            scope,
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

/// The rules stored expansions are made under. A document without this number, or with another,
/// was made by code that chose a code system version or a value set definition differently (one
/// pool for every package, the package installed last), so it is rebuilt once rather than read.
pub const EXPANSION_RULES: u32 = 2;

/// Whether a stored expansion document was made under the current [`EXPANSION_RULES`].
pub fn expansion_is_current(document: &Json) -> bool {
    document.get("rules").and_then(|rules| rules.as_u64()) == Some(u64::from(EXPANSION_RULES))
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
    json!({"url": url, "systems": systems, "contains": contains, "rules": EXPANSION_RULES})
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

    const STATUS: &str = "http://hl7.org/fhir/encounter-status";
    const STATUS_SET: &str = "http://hl7.org/fhir/ValueSet/encounter-status";

    fn versioned_system(url: &str, version: &str, codes: &[&str]) -> serde_json::Value {
        json!({
            "resourceType": "CodeSystem", "url": url, "version": version, "content": "complete",
            "concept": codes.iter().map(|code| json!({"code": code})).collect::<Vec<_>>()
        })
    }

    /// R4's `encounter-status`, and the `5.0.0` copy of it that `hl7.fhir.uv.xver-r5.r4` carries.
    fn encounter_status() -> Vec<serde_json::Value> {
        vec![
            versioned_system(
                STATUS,
                "4.0.1",
                &["planned", "arrived", "in-progress", "finished"],
            ),
            versioned_system(
                STATUS,
                "5.0.0",
                &["planned", "in-progress", "completed", "discharged"],
            ),
            value_set(STATUS_SET, json!({"include": [{"system": STATUS}]})),
        ]
    }

    fn admits(terms: &MemoryTerminology, value_set: &str, code: &str) -> bool {
        terms.contains(value_set, Some(STATUS), code) == TermAnswer::Yes
    }

    #[test]
    fn an_unversioned_reference_means_the_servers_release_when_it_is_installed() {
        let resources = encounter_status();

        // With no release the highest version answers, as it did before.
        let latest = MemoryTerminology::from_resources(&resources);
        assert!(admits(&latest, STATUS_SET, "completed"));
        assert!(!admits(&latest, STATUS_SET, "arrived"));

        // An R4 server reads R4's codes: `arrived` and `finished` are in, R5's `completed` is not.
        let r4 = flat_for(&resources, Some("4.0.1"));
        assert!(admits(&r4, STATUS_SET, "arrived"));
        assert!(admits(&r4, STATUS_SET, "finished"));
        assert!(!admits(&r4, STATUS_SET, "completed"));
        // Neither does load order matter.
        let mut reversed = resources.clone();
        reversed.reverse();
        let r4 = flat_for(&reversed, Some("4.0.1"));
        assert!(admits(&r4, STATUS_SET, "arrived") && !admits(&r4, STATUS_SET, "completed"));

        // The same reading applies to lookups that name no version.
        assert!(r4.lookup(STATUS, None, "arrived").is_some());
        assert!(r4.lookup(STATUS, None, "completed").is_none());
        // A version that is named still gets exactly that one, by either spelling.
        assert!(r4.lookup(STATUS, Some("5.0.0"), "completed").is_some());
        assert!(r4
            .lookup(&format!("{STATUS}|5.0.0"), None, "completed")
            .is_some());
        let mut pinned = resources.clone();
        pinned.push(value_set(
            "http://example.org/r5-status",
            json!({"include": [{"system": STATUS, "version": "5.0.0"}]}),
        ));
        let pinned = flat_for(&pinned, Some("4.0.1"));
        assert!(admits(&pinned, "http://example.org/r5-status", "completed"));
        assert!(!admits(&pinned, "http://example.org/r5-status", "arrived"));
    }

    #[test]
    fn a_release_that_is_not_installed_falls_back_to_the_latest_version() {
        let terms = flat_for(&encounter_status(), Some("4.3.0"));
        assert!(admits(&terms, STATUS_SET, "completed"));
        assert!(!admits(&terms, STATUS_SET, "arrived"));
        // An empty release is no release.
        let terms = flat_for(&encounter_status(), Some(""));
        assert!(admits(&terms, STATUS_SET, "completed"));
    }

    #[test]
    fn the_release_holds_for_stored_expansions_rebuilds_and_incremental_changes() {
        let resources = encounter_status();

        // What install stores: each package expands on its own, so `4.0.1` meets only R4's
        // package. Loading it back for an R4 server reads the same.
        let r4_package = [resources[0].clone(), resources[2].clone()];
        let r5_copy = [resources[1].clone()];
        let mut stored = MemoryTerminology::expansion_documents(&r4_package);
        stored.extend(MemoryTerminology::expansion_documents(&r5_copy));
        let booted = {
            let tagged: Vec<(super::Scope, &serde_json::Value)> =
                resources.iter().map(|resource| (0, resource)).collect();
            let stored: Vec<(super::Scope, &serde_json::Value)> =
                stored.iter().map(|document| (0, document)).collect();
            MemoryTerminology::from_stored_scoped(&tagged, &stored, Some("4.0.1"))
        };
        assert!(admits(&booted, STATUS_SET, "arrived"));

        // What a rebuild over every package makes of it agrees with that.
        let documents = flat_documents(&resources, Some("4.0.1"));
        let document = documents
            .iter()
            .find(|document| document["url"] == STATUS_SET)
            .expect("the value set expands");
        let codes: Vec<&str> = document["contains"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["code"].as_str())
            .collect();
        assert_eq!(codes, ["arrived", "finished", "in-progress", "planned"]);
        // The expansion says which version it read.
        assert_eq!(
            document["systems"],
            json!([{"url": STATUS, "version": "4.0.1"}])
        );

        // And a value set written afterwards resolves the same way, through `apply`.
        let mut live = booted;
        live.apply(
            &super::TerminologyChange {
                upserted: vec![value_set(
                    "http://example.org/ops-status",
                    json!({"include": [{"system": STATUS}]}),
                )],
                removed: Vec::new(),
            },
            &[],
        );
        assert!(admits(&live, "http://example.org/ops-status", "arrived"));
        assert!(!admits(&live, "http://example.org/ops-status", "completed"));
        // The release can also be set on terminology that was built without it.
        let mut late = MemoryTerminology::from_resources(&resources);
        late.set_release(Some("4.0.1"));
        assert!(late.lookup(STATUS, None, "arrived").is_some());
    }

    const SYS: &str = "http://example.org/s";
    const SET: &str = "http://example.org/vs";

    /// `resources` as one package's: each tagged with the package's scope.
    fn package(
        scope: super::Scope,
        resources: Vec<serde_json::Value>,
    ) -> Vec<(super::Scope, serde_json::Value)> {
        resources
            .into_iter()
            .map(|resource| (scope, resource))
            .collect()
    }

    fn together(
        packages: &[&[(super::Scope, serde_json::Value)]],
    ) -> Vec<(super::Scope, serde_json::Value)> {
        packages
            .iter()
            .flat_map(|package| package.iter().cloned())
            .collect()
    }

    fn scoped(all: &[(super::Scope, serde_json::Value)]) -> MemoryTerminology {
        let tagged: Vec<(super::Scope, &serde_json::Value)> = all
            .iter()
            .map(|(scope, resource)| (*scope, resource))
            .collect();
        MemoryTerminology::from_resources_scoped(&tagged, None)
    }

    fn has(terms: &MemoryTerminology, value_set: &str, system: &str, code: &str) -> bool {
        terms.contains(value_set, Some(system), code) == TermAnswer::Yes
    }

    #[test]
    fn a_value_set_reads_the_code_systems_of_its_own_package_first() {
        // One package carries version 1 of a system and a value set that includes it; another
        // carries version 2, which is newer.
        let own = package(
            1,
            vec![
                versioned_system(SYS, "1", &["a", "b"]),
                value_set(SET, json!({"include": [{"system": SYS}]})),
            ],
        );
        let newer = package(2, vec![versioned_system(SYS, "2", &["c"])]);
        // The value set means its own package's version, whichever was installed first.
        for all in [together(&[&own, &newer]), together(&[&newer, &own])] {
            let terms = scoped(&all);
            assert!(has(&terms, SET, SYS, "a") && has(&terms, SET, SYS, "b"));
            assert!(!has(&terms, SET, SYS, "c"));
        }
        // The expansion says which version it read.
        let all = together(&[&own, &newer]);
        let tagged: Vec<(super::Scope, &serde_json::Value)> = all
            .iter()
            .map(|(scope, resource)| (*scope, resource))
            .collect();
        let documents = MemoryTerminology::expansion_documents_scoped(&tagged, None);
        let document = documents.iter().find(|d| d["url"] == SET).unwrap();
        assert_eq!(document["systems"], json!([{"url": SYS, "version": "1"}]));
        // Terminology that is not told where its resources came from has one pool, and its value
        // sets read the latest version, as they always did.
        let flat: Vec<serde_json::Value> = all.iter().map(|(_, r)| r.clone()).collect();
        let flat = MemoryTerminology::from_resources(&flat);
        assert!(has(&flat, SET, SYS, "c") && !has(&flat, SET, SYS, "a"));
    }

    #[test]
    fn a_system_the_package_does_not_hold_is_read_from_the_others() {
        let other = "http://example.org/t";
        let waiting = package(
            1,
            vec![value_set(SET, json!({"include": [{"system": other}]}))],
        );
        // Nothing holds it yet: the value set cannot be expanded.
        let terms = scoped(&waiting);
        assert_eq!(terms.contains(SET, None, "x"), TermAnswer::Unknown);
        // Another package brings it, in two versions: the latest answers, and the release's
        // when the server has one.
        let held = package(
            2,
            vec![
                versioned_system(other, "1", &["x"]),
                versioned_system(other, "2", &["y"]),
            ],
        );
        let terms = scoped(&together(&[&waiting, &held]));
        assert!(has(&terms, SET, other, "y") && !has(&terms, SET, other, "x"));
        let all = together(&[&waiting, &held]);
        let tagged: Vec<(super::Scope, &serde_json::Value)> = all
            .iter()
            .map(|(scope, resource)| (*scope, resource))
            .collect();
        let terms = MemoryTerminology::from_resources_scoped(&tagged, Some("1"));
        assert!(has(&terms, SET, other, "x") && !has(&terms, SET, other, "y"));
    }

    #[test]
    fn packages_that_hold_one_version_with_different_content_each_keep_their_own() {
        let one = package(
            1,
            vec![
                versioned_system(SYS, "1", &["a", "b"]),
                value_set(SET, json!({"include": [{"system": SYS}]})),
            ],
        );
        let two = package(
            2,
            vec![
                versioned_system(SYS, "1", &["c"]),
                value_set(
                    "http://example.org/vs2",
                    json!({"include": [{"system": SYS}]}),
                ),
            ],
        );
        // A third package holds the same content as the first, and shares its copy.
        let copy = package(3, vec![versioned_system(SYS, "1", &["a", "b"])]);
        let terms = scoped(&together(&[&one, &two, &copy]));
        assert!(has(&terms, SET, SYS, "a") && !has(&terms, SET, SYS, "c"));
        assert!(has(&terms, "http://example.org/vs2", SYS, "c"));
        assert!(!has(&terms, "http://example.org/vs2", SYS, "a"));
    }

    #[test]
    fn an_included_value_set_is_the_requesting_packages_own_definition() {
        let inner = "http://example.org/inner";
        let with = |scope, code: &str| {
            package(
                scope,
                vec![
                    versioned_system(SYS, "1", &[code]),
                    value_set(inner, json!({"include": [{"system": SYS}]})),
                ],
            )
        };
        let first = with(1, "a");
        let second = with(2, "b");
        // `outer` is in the first package and includes `inner`, which both packages define
        // (differently). The second is loaded last, so it is the one a plain reference means.
        let outer = package(
            1,
            vec![value_set(SET, json!({"include": [{"valueSet": [inner]}]}))],
        );
        let terms = scoped(&together(&[&first, &outer, &second]));
        assert!(has(&terms, SET, SYS, "a") && !has(&terms, SET, SYS, "b"));
        assert!(has(&terms, inner, SYS, "b") && !has(&terms, inner, SYS, "a"));
    }

    #[test]
    fn an_incremental_change_reads_value_sets_as_a_rebuild_does() {
        // Two packages: one holds version 1 of a system and a value set that includes it, the
        // other a value set that includes it and holds none.
        let holder = package(
            1,
            vec![
                versioned_system(SYS, "1", &["a", "b"]),
                value_set(SET, json!({"include": [{"system": SYS}]})),
            ],
        );
        let bare_set = "http://example.org/bare";
        let bare = package(
            4,
            vec![value_set(bare_set, json!({"include": [{"system": SYS}]}))],
        );
        let base = together(&[&holder, &bare]);
        let mut live = scoped(&base);
        assert!(has(&live, bare_set, SYS, "a"));

        // Version 3 of the system is written to the local package.
        let newest = versioned_system(SYS, "3", &["d"]);
        let local_set = "http://example.org/local";
        let local = value_set(local_set, json!({"include": [{"system": SYS}]}));
        let sets = [holder[1].1.clone(), bare[0].1.clone()];
        live.apply_scoped(
            &super::TerminologyChange {
                upserted: vec![newest.clone(), local.clone()],
                removed: Vec::new(),
            },
            3,
            &[(1, &sets[0]), (4, &sets[1])],
        );
        let rebuilt = scoped(&together(&[
            &holder,
            &bare,
            &package(3, vec![newest, local]),
        ]));
        for (set, code, expected) in [
            (SET, "a", true), // its own package's version, still
            (SET, "d", false),
            (bare_set, "d", true), // holds none: the latest, which is now version 3
            (bare_set, "a", false),
            (local_set, "d", true),
        ] {
            assert_eq!(
                has(&live, set, SYS, code),
                expected,
                "{set} {code}, incremental"
            );
            assert_eq!(
                has(&rebuilt, set, SYS, code),
                expected,
                "{set} {code}, rebuilt"
            );
        }
        // What the local package wrote goes again, and the other packages keep their copies.
        live.apply_scoped(
            &super::TerminologyChange {
                upserted: Vec::new(),
                removed: vec![("CodeSystem".into(), format!("{SYS}|3"))],
            },
            3,
            &[(1, &sets[0]), (4, &sets[1])],
        );
        assert!(has(&live, SET, SYS, "a"));
        assert!(has(&live, bare_set, SYS, "a") && !has(&live, bare_set, SYS, "d"));
    }

    const R4_SYSTEM: &str = "http://terminology.hl7.org/CodeSystem/x";
    const R5_SYSTEM: &str = "http://hl7.org/fhir/x";

    fn versioned_set(url: &str, version: &str, system: &str) -> serde_json::Value {
        json!({"resourceType": "ValueSet", "url": url, "version": version,
               "compose": {"include": [{"system": system}]}})
    }

    /// Resources that are not told where they came from, read for `release`.
    fn flat_for(resources: &[serde_json::Value], release: Option<&str>) -> MemoryTerminology {
        let tagged: Vec<(super::Scope, &serde_json::Value)> =
            resources.iter().map(|resource| (0, resource)).collect();
        MemoryTerminology::from_resources_scoped(&tagged, release)
    }

    fn flat_documents(
        resources: &[serde_json::Value],
        release: Option<&str>,
    ) -> Vec<serde_json::Value> {
        let tagged: Vec<(super::Scope, &serde_json::Value)> =
            resources.iter().map(|resource| (0, resource)).collect();
        MemoryTerminology::expansion_documents_scoped(&tagged, release)
    }

    fn scoped_for(
        all: &[(super::Scope, serde_json::Value)],
        release: Option<&str>,
    ) -> MemoryTerminology {
        let tagged: Vec<(super::Scope, &serde_json::Value)> = all
            .iter()
            .map(|(scope, resource)| (*scope, resource))
            .collect();
        MemoryTerminology::from_resources_scoped(&tagged, release)
    }

    /// The R4 package and a package that carries an R5-style copy of the same value set: another
    /// version, over another code system URL (as `hl7.fhir.uv.extensions.r4` does).
    fn r4_and_its_r5_style_copy() -> (
        Vec<(super::Scope, serde_json::Value)>,
        Vec<(super::Scope, serde_json::Value)>,
    ) {
        (
            package(
                1,
                vec![
                    versioned_system(R4_SYSTEM, "4.0.1", &["a", "b"]),
                    versioned_set(SET, "4.0.1", R4_SYSTEM),
                ],
            ),
            package(
                2,
                vec![
                    versioned_system(R5_SYSTEM, "5.3.0-ballot-tc1", &["a", "b", "c"]),
                    versioned_set(SET, "5.3.0-ballot-tc1", R5_SYSTEM),
                ],
            ),
        )
    }

    #[test]
    fn the_definition_at_the_release_is_the_one_in_use() {
        let (r4, copy) = r4_and_its_r5_style_copy();
        // Whichever package was installed last, an R4 server means R4's value set: its code
        // system URL, not the copy's.
        for all in [together(&[&r4, &copy]), together(&[&copy, &r4])] {
            let terms = scoped_for(&all, Some("4.0.1"));
            assert!(has(&terms, SET, R4_SYSTEM, "a") && has(&terms, SET, R4_SYSTEM, "b"));
            assert!(!has(&terms, SET, R5_SYSTEM, "c"));
        }
        // With no release to go by, the definition loaded last is in use, as it always was.
        let terms = scoped_for(&together(&[&r4, &copy]), None);
        assert!(has(&terms, SET, R5_SYSTEM, "c") && !has(&terms, SET, R4_SYSTEM, "a"));
        // The same when the release matches none of them.
        let terms = scoped_for(&together(&[&r4, &copy]), Some("4.3.0"));
        assert!(has(&terms, SET, R5_SYSTEM, "c"));

        // Of the definitions at the release, the one loaded last is in use.
        let again = package(
            3,
            vec![
                versioned_system(R4_SYSTEM, "4.0.1", &["a", "b", "z"]),
                versioned_set(SET, "4.0.1", R4_SYSTEM),
            ],
        );
        let terms = scoped_for(&together(&[&r4, &again, &copy]), Some("4.0.1"));
        assert!(has(&terms, SET, R4_SYSTEM, "z"));

        let tagged: Vec<(super::Scope, &serde_json::Value)> = vec![(1, &r4[1].1), (2, &copy[1].1)];
        assert_eq!(super::winning_scope(&tagged, Some("4.0.1")), Some(1));
        assert_eq!(super::winning_scope(&tagged, None), Some(2));
        assert_eq!(super::winning_scope(&[], Some("4.0.1")), None);
    }

    #[test]
    fn versions_that_do_not_name_the_release_are_not_compared() {
        // Core's `2018-08-12` and terminology's `1.0.0` do not order against each other (a text
        // comparison puts the date first), and `1.0.0` is the later publication. Neither names
        // the release, so what was installed last is in use, which is right here.
        let older = package(
            1,
            vec![
                versioned_system(R4_SYSTEM, "2018-08-12", &["old"]),
                versioned_set(SET, "2018-08-12", R4_SYSTEM),
            ],
        );
        let newer = package(
            2,
            vec![
                versioned_system(R5_SYSTEM, "1.0.0", &["new"]),
                versioned_set(SET, "1.0.0", R5_SYSTEM),
            ],
        );
        let terms = scoped_for(&together(&[&older, &newer]), Some("4.0.1"));
        assert!(has(&terms, SET, R5_SYSTEM, "new") && !has(&terms, SET, R4_SYSTEM, "old"));
        let terms = scoped_for(&together(&[&newer, &older]), Some("4.0.1"));
        assert!(has(&terms, SET, R4_SYSTEM, "old"));
    }

    #[test]
    fn a_start_loads_the_expansion_of_the_definition_in_use() {
        let (r4, copy) = r4_and_its_r5_style_copy();
        let all = together(&[&r4, &copy]);
        let tagged: Vec<(super::Scope, &serde_json::Value)> = all
            .iter()
            .map(|(scope, resource)| (*scope, resource))
            .collect();
        // What installing each package stored: each expanded on its own, the copy's last.
        let r4_resources: Vec<serde_json::Value> = r4.iter().map(|(_, r)| r.clone()).collect();
        let copy_resources: Vec<serde_json::Value> = copy.iter().map(|(_, r)| r.clone()).collect();
        let r4_docs = MemoryTerminology::expansion_documents(&r4_resources);
        let copy_docs = MemoryTerminology::expansion_documents(&copy_resources);
        let stored: Vec<(super::Scope, &serde_json::Value)> = r4_docs
            .iter()
            .map(|d| (1, d))
            .chain(copy_docs.iter().map(|d| (2, d)))
            .collect();
        let terms = MemoryTerminology::from_stored_scoped(&tagged, &stored, Some("4.0.1"));
        assert!(has(&terms, SET, R4_SYSTEM, "a") && !has(&terms, SET, R5_SYSTEM, "c"));
        // With no release it is the package installed last, as before.
        let terms = MemoryTerminology::from_stored_scoped(&tagged, &stored, None);
        assert!(has(&terms, SET, R5_SYSTEM, "c") && !has(&terms, SET, R4_SYSTEM, "a"));

        // When the definition in use has no stored expansion, the next one that has one answers.
        let only_copy: Vec<(super::Scope, &serde_json::Value)> =
            copy_docs.iter().map(|d| (2, d)).collect();
        let terms = MemoryTerminology::from_stored_scoped(&tagged, &only_copy, Some("4.0.1"));
        assert!(has(&terms, SET, R5_SYSTEM, "c"));
        // And an expansion stored for no package at all still answers.
        let unowned: Vec<(super::Scope, &serde_json::Value)> =
            r4_docs.iter().map(|d| (9, d)).collect();
        let terms = MemoryTerminology::from_stored_scoped(&tagged, &unowned, Some("4.0.1"));
        assert!(has(&terms, SET, R4_SYSTEM, "a"));
    }

    #[test]
    fn an_incremental_change_uses_the_definition_a_rebuild_uses() {
        let (r4, copy) = r4_and_its_r5_style_copy();
        let all = together(&[&r4, &copy]);
        let rebuilt = scoped_for(&all, Some("4.0.1"));
        let sets = [r4[1].1.clone(), copy[1].1.clone()];
        let nothing = super::TerminologyChange {
            upserted: Vec::new(),
            removed: Vec::new(),
        };
        // The value set is read again from the stored rows of every package that defines it, in
        // either order.
        for dependents in [
            [(1, &sets[0]), (2, &sets[1])],
            [(2, &sets[1]), (1, &sets[0])],
        ] {
            let mut live = scoped_for(&all, Some("4.0.1"));
            live.apply_scoped(&nothing, 3, &dependents);
            for (system, code, expected) in [
                (R4_SYSTEM, "a", true),
                (R5_SYSTEM, "c", false),
                (R5_SYSTEM, "a", false),
            ] {
                assert_eq!(has(&live, SET, system, code), expected, "{system} {code}");
                assert_eq!(has(&rebuilt, SET, system, code), expected);
            }
        }
    }

    #[test]
    fn a_definition_that_cannot_expand_leaves_the_answer_to_the_next_one() {
        // An older package's value set expands. A newer package's, which is the one in use,
        // includes a code system nothing carries (terminology's `v2-0227` includes
        // `hl7.org/fhir/sid/mvx`), so it cannot.
        let older = package(
            1,
            vec![
                versioned_system(SYS, "1", &["a", "b"]),
                versioned_set(SET, "1", SYS),
            ],
        );
        let missing = "http://example.org/not-installed";
        let newer = package(2, vec![versioned_set(SET, "2", missing)]);
        let all = together(&[&older, &newer]);

        // A rebuild answers with the older definition's expansion, as a start does.
        let rebuilt = scoped_for(&all, None);
        assert!(has(&rebuilt, SET, SYS, "a") && has(&rebuilt, SET, SYS, "b"));
        assert!(rebuilt.why_not_expanded(SET).is_none());
        let tagged: Vec<(super::Scope, &serde_json::Value)> = all
            .iter()
            .map(|(scope, resource)| (*scope, resource))
            .collect();
        let older_resources: Vec<serde_json::Value> =
            older.iter().map(|(_, r)| r.clone()).collect();
        let stored = MemoryTerminology::expansion_documents(&older_resources);
        let stored: Vec<(super::Scope, &serde_json::Value)> =
            stored.iter().map(|d| (1, d)).collect();
        let started = MemoryTerminology::from_stored_scoped(&tagged, &stored, None);
        assert!(has(&started, SET, SYS, "a") && started.why_not_expanded(SET).is_none());

        // And so does a change that reads it again.
        let sets = [older[1].1.clone(), newer[0].1.clone()];
        let nothing = super::TerminologyChange {
            upserted: Vec::new(),
            removed: Vec::new(),
        };
        let mut live = scoped_for(&all, None);
        live.apply_scoped(&nothing, 3, &[(1, &sets[0]), (2, &sets[1])]);
        assert!(has(&live, SET, SYS, "a") && live.why_not_expanded(SET).is_none());

        // When no definition can expand, the one in use is the one that is explained.
        let alone = scoped_for(&package(2, vec![versioned_set(SET, "2", missing)]), None);
        let reasons = alone.why_not_expanded(SET).unwrap();
        assert!(messages(&reasons)[0].contains(missing), "{reasons:?}");
    }

    #[test]
    fn every_definition_at_the_release_is_tried_before_one_that_is_not() {
        // The release's value set is in two packages. The first holds its code system. The second
        // (an examples package) is only a copy of the value set, so it cannot be expanded alone.
        // A third carries an R5-style copy that can. When the one in use cannot be expanded, the
        // one at the release that can must answer, not the R5-style copy.
        let core = package(
            1,
            vec![
                versioned_system(R4_SYSTEM, "4.0.1", &["a", "b"]),
                versioned_set(SET, "4.0.1", R4_SYSTEM),
            ],
        );
        let examples = package(2, vec![versioned_set(SET, "4.0.1", R4_SYSTEM)]);
        let ext = package(
            3,
            vec![
                versioned_system(R5_SYSTEM, "5.3.0-ballot-tc1", &["a", "b", "c"]),
                versioned_set(SET, "5.3.0-ballot-tc1", R5_SYSTEM),
            ],
        );
        let terms = scoped_for(&together(&[&core, &examples, &ext]), Some("4.0.1"));
        assert!(has(&terms, SET, R4_SYSTEM, "a") && !has(&terms, SET, R5_SYSTEM, "c"));
    }

    #[test]
    fn a_definition_is_tried_alone_before_the_rest_of_the_store_is_read() {
        // An older package holds a code system and a value set over all of it. A newer package's
        // value set filters it down to one code, and the system is not its own: installing it alone
        // could not expand it, so a start serves the older package's expansion. A rebuild must
        // agree, not expand the newer definition through the older package's code system.
        let older = package(
            1,
            vec![
                versioned_system(SYS, "1", &["a", "b"]),
                versioned_set(SET, "1", SYS),
            ],
        );
        let newer = package(
            2,
            vec![
                json!({"resourceType": "ValueSet", "url": SET, "version": "2",
                        "compose": {"include": [{"system": SYS, "filter": [
                            {"property": "concept", "op": "is-a", "value": "a"}]}]}}),
            ],
        );
        let all = together(&[&older, &newer]);
        let rebuilt = scoped_for(&all, None);
        assert!(has(&rebuilt, SET, SYS, "a") && has(&rebuilt, SET, SYS, "b"));
        // What a start has to go on is what installing the older package stored.
        let older_resources: Vec<serde_json::Value> =
            older.iter().map(|(_, r)| r.clone()).collect();
        let stored = MemoryTerminology::expansion_documents(&older_resources);
        let stored: Vec<(super::Scope, &serde_json::Value)> =
            stored.iter().map(|d| (1, d)).collect();
        let tagged: Vec<(super::Scope, &serde_json::Value)> = all
            .iter()
            .map(|(scope, resource)| (*scope, resource))
            .collect();
        let started = MemoryTerminology::from_stored_scoped(&tagged, &stored, None);
        assert!(has(&started, SET, SYS, "b"));
        // With no older definition that expands, the rest of the store is read: it is how a
        // value set that waits on another package's code system gets its expansion.
        let alone = scoped_for(
            &together(&[
                &newer,
                &package(3, vec![versioned_system(SYS, "1", &["a", "b"])]),
            ]),
            None,
        );
        assert!(has(&alone, SET, SYS, "a") && !has(&alone, SET, SYS, "b"));
    }

    #[test]
    fn a_code_is_described_from_the_content_its_expansion_was_made_from() {
        // Two packages hold one version of a system with different content, and each has a value
        // set that includes it.
        let named = |scope, display: &str, set: &str| {
            package(
                scope,
                vec![
                    json!({"resourceType": "CodeSystem", "url": SYS, "version": "1",
                           "content": "complete", "concept": [{"code": "a", "display": display}]}),
                    versioned_set(set, "1", SYS),
                ],
            )
        };
        let one = named(1, "One", "http://example.org/vs1");
        let two = named(2, "Two", "http://example.org/vs2");
        let terms = scoped_for(&together(&[&one, &two]), None);
        let display = |set: &str| {
            terms
                .describe_bound(set, Some(SYS), "a")
                .and_then(|described| described.display)
        };
        assert_eq!(display("http://example.org/vs1").as_deref(), Some("One"));
        assert_eq!(display("http://example.org/vs2").as_deref(), Some("Two"));
    }

    #[test]
    fn a_change_to_what_any_definition_includes_makes_the_value_set_a_dependent() {
        // The definition in use includes one system, an older one another. Whichever answers, a
        // change to either can alter the answer.
        let other = "http://example.org/other";
        let older = package(
            1,
            vec![
                versioned_system(SYS, "1", &["a"]),
                versioned_set(SET, "1", SYS),
            ],
        );
        let newer = package(2, vec![versioned_set(SET, "2", other)]);
        let terms = scoped_for(&together(&[&older, &newer]), None);
        assert_eq!(terms.dependents_of(&[SYS.to_string()]), [SET.to_string()]);
        assert_eq!(terms.dependents_of(&[other.to_string()]), [SET.to_string()]);
    }

    #[test]
    fn the_winner_rule_counts_a_package_by_its_last_definition() {
        // Package 1 holds two definitions of the URL (a version and a later one); only the last
        // is what the index keeps, so only it can be at the release.
        let at = versioned_set(SET, "4.0.1", SYS);
        let later = versioned_set(SET, "5.0.0", SYS);
        let other = versioned_set(SET, "5.3.0", SYS);
        let held: Vec<(super::Scope, &serde_json::Value)> =
            vec![(1, &at), (1, &later), (2, &other)];
        assert_eq!(super::winning_scope(&held, Some("4.0.1")), Some(2));
        // The same grouped by URL, ignoring what is not a value set.
        let system = versioned_system(SYS, "4.0.1", &["a"]);
        let all: Vec<(super::Scope, &serde_json::Value)> =
            vec![(1, &at), (1, &later), (2, &other), (3, &system)];
        let winners = super::winning_scopes(&all, Some("4.0.1"));
        assert_eq!(winners.len(), 1);
        assert_eq!(winners.get(SET), Some(&2));
    }

    #[test]
    fn stored_expansions_say_which_rules_they_were_made_under() {
        let resources = vec![
            versioned_system(SYS, "1", &["a"]),
            versioned_set(SET, "1", SYS),
        ];
        let documents = MemoryTerminology::expansion_documents(&resources);
        assert!(!documents.is_empty());
        assert!(documents.iter().all(super::expansion_is_current));
        let mut old = documents[0].clone();
        old.as_object_mut().unwrap().remove("rules");
        assert!(!super::expansion_is_current(&old));
        old["rules"] = json!(1);
        assert!(!super::expansion_is_current(&old));
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
