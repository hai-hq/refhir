use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{json, Value as Json};

use crate::cache::Cache;

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

#[derive(Clone, Debug, Default, PartialEq, Hash)]
struct Concept {
    display: Option<String>,
    definition: Option<String>,
    inactive: bool,
    parents: Vec<String>,
    properties: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Hash)]
struct SystemIndex {
    /// A complete system is closed. A missing code is really absent.
    closed: bool,
    /// The CodeSystem's `content` mode: `complete`, `example`, `fragment`, `not-present` or
    /// `supplement`.
    content: String,
    name: Option<String>,
    codes: BTreeMap<String, Concept>,
}

impl SystemIndex {
    /// A number that is the same for two indexes with the same content, and that no two with
    /// different content are likely to share (128 bits). It says whether two packages hold the
    /// same copy of a system without the copies having to be at hand together.
    fn digest(&self) -> u128 {
        let mut low = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut low);
        let mut high = std::collections::hash_map::DefaultHasher::new();
        0x5a_u8.hash(&mut high);
        self.hash(&mut high);
        (u128::from(high.finish()) << 64) | u128::from(low.finish())
    }

    /// Bytes this index takes, roughly: what a cache weighs it by.
    fn approx_bytes(&self) -> usize {
        let text = |text: &Option<String>| text.as_ref().map_or(0, String::len);
        self.codes
            .iter()
            .map(|(code, concept)| {
                std::mem::size_of::<Concept>()
                    + 64
                    + code.len()
                    + text(&concept.display)
                    + text(&concept.definition)
                    + concept
                        .parents
                        .iter()
                        .map(|parent| parent.len() + 24)
                        .sum::<usize>()
                    + concept
                        .properties
                        .iter()
                        .map(|(name, value)| name.len() + value.len() + 48)
                        .sum::<usize>()
            })
            .sum::<usize>()
            + self.content.len()
            + 128
    }
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
    /// The packages that hold exactly this content, each with the key its row is stored under
    /// (empty for content that is held).
    holders: BTreeMap<Scope, String>,
    /// [`SystemIndex::digest`] of the content: what says whether another copy is the same.
    digest: u128,
    /// The content, or `None` when it is read from the store when used.
    index: Option<Arc<SystemIndex>>,
}

impl Variant {
    fn held_by(&self, scope: Scope) -> bool {
        self.holders.contains_key(&scope)
    }
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
    /// Where a variant that is not held is read from.
    loader: Option<Loading>,
}

/// Where in `variants` the variant `scope` holds is, or the one loaded last when it holds none.
fn variant_position(variants: &[Variant], scope: Option<Scope>) -> Option<usize> {
    scope
        .and_then(|scope| variants.iter().rposition(|v| v.held_by(scope)))
        .or(variants.len().checked_sub(1))
}

/// The variant `scope` holds, or the one loaded last when it holds none.
fn variant_for(variants: &[Variant], scope: Option<Scope>) -> Option<&Variant> {
    variants.get(variant_position(variants, scope)?)
}

fn held_by(variants: &[Variant], scope: Scope) -> bool {
    variants.iter().any(|variant| variant.held_by(scope))
}

impl Systems {
    /// What `scope` held under this URL and version is replaced by content with this `digest`,
    /// stored under `key` (`index` is that content when it is held).
    fn insert(
        &mut self,
        scope: Scope,
        url: &str,
        version: &str,
        key: &str,
        digest: u128,
        index: Option<Arc<SystemIndex>>,
    ) {
        let variants = self
            .by_url
            .entry(url.to_string())
            .or_default()
            .entry(version.to_string())
            .or_default();
        for variant in variants.iter_mut() {
            variant.holders.remove(&scope);
        }
        variants.retain(|variant| !variant.holders.is_empty());
        // The variant loaded last answers a lookup that names no package, so one that matches
        // moves to the end rather than staying where it was.
        let mut variant = match variants.iter().position(|held| held.digest == digest) {
            Some(position) => variants.remove(position),
            None => Variant {
                holders: BTreeMap::new(),
                digest,
                index,
            },
        };
        variant.holders.insert(scope, key.to_string());
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
                variant.holders.remove(&scope);
            }
            variants.retain(|variant| !variant.holders.is_empty());
        }
        versions.retain(|_, variants| !variants.is_empty());
        if versions.is_empty() {
            self.by_url.remove(url);
        }
    }

    /// The version asked for, or, when none is, the installed version that is the server's
    /// release, or else the latest installed. Returns the version found, and its content.
    fn resolve(&self, url: &str, version: Option<&str>) -> Option<(&str, Arc<SystemIndex>)> {
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
    ) -> Option<(&str, Arc<SystemIndex>)> {
        let (version, variant) = self.pick_in(scope, strict, url, version)?;
        Some((version, self.content(variant)?))
    }

    /// [`Self::resolve_in`] as far as saying which version answers, and which variant of it:
    /// the content is not read. A caller that only needs the version, or whether there is one,
    /// does not make a stored system be read.
    fn pick_in(
        &self,
        scope: Option<Scope>,
        strict: bool,
        url: &str,
        version: Option<&str>,
    ) -> Option<(&str, &Variant)> {
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

    /// The content of `variant`: held, or read from the store now (or taken from what was read
    /// lately). `None` when it cannot be read.
    fn content(&self, variant: &Variant) -> Option<Arc<SystemIndex>> {
        match &variant.index {
            Some(index) => Some(index.clone()),
            None => {
                let (scope, key) = variant.holders.iter().next()?;
                self.loader.as_ref()?.0.system(*scope, key)
            }
        }
    }

    /// `system` or `system|version` as a reference, resolved the same way.
    fn find(&self, system: &str, version: Option<&str>) -> Option<Arc<SystemIndex>> {
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

    /// Read the content that [`Self::resolve_in`] gives for this reference and keep it, so a
    /// lookup of it never waits for a read again. What is already held is left as it is.
    fn hold(&mut self, scope: Option<Scope>, url: &str, version: Option<&str>) {
        let Some((found, variant)) = self.pick_in(scope, false, url, version) else {
            return;
        };
        if variant.index.is_some() {
            return;
        }
        let found = found.to_string();
        let Some(content) = self.content(variant) else {
            return;
        };
        let Some(variants) = self.by_url.get_mut(url).and_then(|v| v.get_mut(&found)) else {
            return;
        };
        if let Some(position) = variant_position(variants, scope) {
            variants[position].index = Some(content);
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Expansion {
    systems: BTreeSet<(String, String)>,
    codes: BTreeMap<(String, String), Option<String>>,
    /// The package whose definition of the value set this was expanded from, when known. It says
    /// which content of a code system version two packages hold differently the codes came from.
    scope: Option<Scope>,
    /// The code systems this expansion includes whole although they hold only some of their
    /// concepts, by `(url, version)`, each with the `content` mode that says so (`example`). The
    /// expansion lists the concepts that are installed, which a client must not read as all
    /// there are.
    incomplete: BTreeMap<(String, String), String>,
    /// The codes the expansion lists for navigation and that cannot be selected as a value:
    /// their code system marks them `notSelectable`. R4 calls them `abstract`.
    abstract_codes: BTreeSet<(String, String)>,
}

impl Expansion {
    /// Bytes this expansion takes, roughly: what a cache weighs it by.
    fn approx_bytes(&self) -> usize {
        self.incomplete
            .iter()
            .map(|((system, version), content)| 72 + system.len() + version.len() + content.len())
            .sum::<usize>()
            + self
                .abstract_codes
                .iter()
                .map(|(system, code)| 48 + system.len() + code.len())
                .sum::<usize>()
            + self
                .codes
                .iter()
                .map(|((system, code), display)| {
                    96 + system.len() + code.len() + display.as_ref().map_or(0, String::len)
                })
                .sum::<usize>()
            + self
                .systems
                .iter()
                .map(|(system, version)| 48 + system.len() + version.len())
                .sum::<usize>()
            + 64
    }
}

/// Where the codes of an expansion that is not held are stored. A key that is the URL of the
/// value set is not kept, because the value set is what the expansion is looked up by.
#[derive(Clone, Debug)]
enum Origin {
    /// An expansion document, stored for `package` under `key`.
    Expansion { package: Scope, key: Option<String> },
    /// The `expansion` inside the value set that `package` stores under `key`.
    Definition { package: Scope, key: Option<String> },
}

/// What answers for a value set: its expansion, held, or what says where to read it from. What
/// is asked of an expansion without its codes (the package it was made for, the code systems it
/// drew on) is kept with the pointer, so it is answered without a read.
#[derive(Clone, Debug)]
enum Answer {
    Held(Arc<Expansion>),
    Stored {
        scope: Option<Scope>,
        /// The code systems the expansion drew on, with versions, in order.
        systems: Box<[(String, String)]>,
        origin: Origin,
    },
}

impl Answer {
    fn held(expansion: Expansion) -> Self {
        Answer::Held(Arc::new(expansion))
    }

    /// The pointer to an expansion that is stored, keeping what is asked without its codes.
    fn stored(expansion: &Expansion, origin: Origin) -> Self {
        Answer::Stored {
            scope: expansion.scope,
            systems: expansion.systems.iter().cloned().collect(),
            origin,
        }
    }

    fn scope(&self) -> Option<Scope> {
        match self {
            Answer::Held(expansion) => expansion.scope,
            Answer::Stored { scope, .. } => *scope,
        }
    }

    /// The version of the code system `url` that the expansion was made from, when it drew on it.
    fn version_of(&self, url: &str) -> Option<&str> {
        fn find<'a>(
            mut systems: impl Iterator<Item = &'a (String, String)>,
            url: &str,
        ) -> Option<&'a str> {
            systems
                .find(|(used, _)| used == url)
                .map(|(_, version)| version.as_str())
        }
        match self {
            Answer::Held(expansion) => find(expansion.systems.iter(), url),
            Answer::Stored { systems, .. } => find(systems.iter(), url),
        }
    }
}

/// Where the terminology a [`MemoryTerminology`] holds only an index of is read from, a row at
/// a time. A key is the one the row is stored under, which is not always the canonical URL: a
/// code system with a version is `url|version`.
pub trait TermSource: Send + Sync {
    /// The CodeSystem, ValueSet or ConceptMap stored under `key` for package `package`.
    fn resource(&self, package: Scope, key: &str) -> Option<Json>;

    /// The expansion document stored under `key` for package `package`.
    fn expansion(&self, package: Scope, key: &str) -> Option<Json>;
}

/// What a concept map says, by source system and code: the targets, in the order they were read.
type MapTargets = BTreeMap<(String, String), Vec<Target>>;

/// What the caches of terminology that is read when used may weigh, each. The systems and
/// expansions a request touches are a few hundred kilobytes; the largest single ones in the R4
/// packages are a few megabytes and are kept while they are the most recently used.
const SYSTEM_CACHE_BYTES: usize = 8 << 20;
const EXPANSION_CACHE_BYTES: usize = 8 << 20;
const DEFINITION_CACHE_BYTES: usize = 2 << 20;
const MAP_CACHE_BYTES: usize = 2 << 20;

/// Reads terminology from a [`TermSource`] and keeps what it read, up to a budget.
struct Loader {
    source: Arc<dyn TermSource>,
    systems: Cache<(Scope, String), SystemIndex>,
    expansions: Cache<(Scope, String, bool), Expansion>,
    definitions: Cache<(Scope, String), Json>,
    maps: Cache<(Scope, String), MapTargets>,
    /// Why a value set that has no expansion has none, once worked out.
    reasons: Mutex<BTreeMap<String, Vec<ExpansionFailure>>>,
}

impl Loader {
    fn new(source: Arc<dyn TermSource>) -> Self {
        Self {
            source,
            systems: Cache::new(SYSTEM_CACHE_BYTES),
            expansions: Cache::new(EXPANSION_CACHE_BYTES),
            definitions: Cache::new(DEFINITION_CACHE_BYTES),
            maps: Cache::new(MAP_CACHE_BYTES),
            reasons: Mutex::new(BTreeMap::new()),
        }
    }

    /// Forget what was read: the rows it came from may have changed.
    fn clear(&self) {
        self.systems.forget(|_| true);
        self.expansions.forget(|_| true);
        self.definitions.forget(|_| true);
        self.maps.forget(|_| true);
        self.reasons
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// The rows read lately and still kept, least recently read first.
    fn resident(&self) -> Resident {
        Resident {
            systems: self.systems.keys(),
            expansions: self.expansions.keys(),
            definitions: self.definitions.keys(),
            maps: self.maps.keys(),
        }
    }

    /// Read every row `resident` lists that is still there.
    fn warm(&self, resident: &Resident) {
        for (package, key) in &resident.systems {
            let _ = self.system(*package, key);
        }
        for (package, key, inline) in &resident.expansions {
            let origin = if *inline {
                Origin::Definition {
                    package: *package,
                    key: Some(key.clone()),
                }
            } else {
                Origin::Expansion {
                    package: *package,
                    key: Some(key.clone()),
                }
            };
            let _ = self.expansion(key, &origin);
        }
        for (package, key) in &resident.definitions {
            let _ = self.definition(*package, key);
        }
        for (package, key) in &resident.maps {
            let _ = self.map(*package, key);
        }
    }

    fn system(&self, package: Scope, key: &str) -> Option<Arc<SystemIndex>> {
        self.systems.get_or_load(
            &(package, key.to_string()),
            |held| held.clone(),
            || {
                let resource = self.source.resource(package, key)?;
                let (_, _, index) = system_index(&resource)?;
                let weight = index.approx_bytes();
                Some((index, weight))
            },
        )
    }

    /// The expansion of the value set `url`, read from where `origin` says.
    fn expansion(&self, url: &str, origin: &Origin) -> Option<Arc<Expansion>> {
        let (package, key, inline) = match origin {
            Origin::Expansion { package, key } => (*package, key, false),
            Origin::Definition { package, key } => (*package, key, true),
        };
        let key = key.as_deref().unwrap_or(url);
        self.expansions.get_or_load(
            &(package, key.to_string(), inline),
            |held| held.clone(),
            || {
                let mut expansion = if inline {
                    let resource = self.source.resource(package, key)?;
                    let contains = resource
                        .pointer("/expansion/contains")
                        .and_then(|value| value.as_array())?;
                    expansion_from_contains(contains)
                } else {
                    let document = self.source.expansion(package, key)?;
                    expansion_from_document(&document)?.1
                };
                expansion.scope = Some(package);
                let weight = expansion.approx_bytes();
                Some((expansion, weight))
            },
        )
    }

    /// The value set stored under `key` for `package`, with only what expanding it reads.
    fn definition(&self, package: Scope, key: &str) -> Option<Arc<Json>> {
        self.definitions.get_or_load(
            &(package, key.to_string()),
            |held| held.clone(),
            || {
                let definition = slim_value_set(self.source.resource(package, key)?);
                let weight = json_weight(&definition);
                Some((definition, weight))
            },
        )
    }

    fn map(&self, package: Scope, key: &str) -> Option<Arc<MapTargets>> {
        self.maps.get_or_load(
            &(package, key.to_string()),
            |held| held.clone(),
            || {
                let resource = self.source.resource(package, key)?;
                let targets = map_targets(&resource);
                let weight = map_weight(&targets);
                Some((targets, weight))
            },
        )
    }
}

/// The rows a terminology that reads when used has read lately and still keeps, as the package and
/// key each is stored under: what a terminology that replaces it reads at once, so that a reload
/// does not send the requests that were being answered back to waiting for reads.
#[derive(Clone, Debug, Default)]
pub struct Resident {
    systems: Vec<(Scope, String)>,
    expansions: Vec<(Scope, String, bool)>,
    definitions: Vec<(Scope, String)>,
    maps: Vec<(Scope, String)>,
}

/// A handle to the [`Loader`] of a terminology. A copy of a terminology reads from the same
/// source with caches of its own, so what one has read cannot be taken for what the other's
/// index describes.
struct Loading(Arc<Loader>);

impl Clone for Loading {
    fn clone(&self) -> Self {
        Loading(Arc::new(Loader::new(self.0.source.clone())))
    }
}

impl std::fmt::Debug for Loading {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Loading")
    }
}

#[derive(Clone, Debug)]
struct Target {
    map_url: String,
    system: String,
    code: String,
    equivalence: String,
}

/// One concept map, as the terminology holds it.
#[derive(Clone, Debug)]
struct MapRef {
    scope: Scope,
    /// Its canonical URL.
    url: String,
    /// The systems its groups translate from, which says whether a translation needs it read.
    sources: BTreeSet<String>,
    body: MapBody,
}

#[derive(Clone, Debug)]
enum MapBody {
    Held(Arc<MapTargets>),
    /// Read from the store, under this key, when used.
    Stored {
        key: String,
    },
}

/// A definition of a value set that has no expansion: held, or read when used.
#[derive(Clone, Debug)]
enum Def {
    Held(Arc<Json>),
    /// Read from the store, under this key, when used.
    Stored(String),
}

/// The value sets that have no expansion, each with every definition of it and the package it is
/// from, ordered for use like [`Definitions`].
type Unexpanded = BTreeMap<String, Vec<(Scope, Def)>>;

#[derive(Debug, Default)]
pub struct MemoryTerminology {
    systems: Systems,
    expansions: BTreeMap<String, Answer>,
    /// Concept maps, in the order they were read, which is the order their translations come in.
    maps: Vec<MapRef>,
    /// Value sets that have no expansion, kept so a refusal can say why.
    unexpanded: Unexpanded,
    /// Why each of those has none. Worked out when the terminology is built or changed, so that
    /// a check against one of them is a lookup, not an expansion. Terminology that is read when
    /// used leaves it to the loader, which works one out when it is first asked for.
    reasons: BTreeMap<String, Vec<ExpansionFailure>>,
    /// For each value set, the code systems and value sets its `compose` draws on. It says which
    /// expansions a change to a code system or value set can alter.
    uses: BTreeMap<String, BTreeSet<String>>,
    /// Where what is not held is read from, for terminology that holds an index.
    loader: Option<Loading>,
}

impl Clone for MemoryTerminology {
    fn clone(&self) -> Self {
        let loader = self.loader.clone();
        let mut systems = self.systems.clone();
        systems.loader = loader.clone();
        Self {
            systems,
            expansions: self.expansions.clone(),
            maps: self.maps.clone(),
            unexpanded: self.unexpanded.clone(),
            reasons: self.reasons.clone(),
            uses: self.uses.clone(),
            loader,
        }
    }
}

/// Builds a [`MemoryTerminology`] from resources handed over one at a time, so that a caller that
/// reads them one by one does not have to keep them all parsed at once. A parsed code system is
/// several times the size of its text and the index keeps a fraction of it: each code system and
/// concept map is indexed as it comes and can be dropped by the caller, and each value set is
/// moved in (not copied) because it has to wait for every code system before it can be expanded.
///
/// There are two ways to finish, chosen by the constructor: [`Self::for_stored`] for terminology
/// loaded as stored, where the expansions already made are on offer, and [`Self::for_package`]
/// for the resources of a package being installed, where every value set is expanded.
pub struct TerminologyBuilder {
    index: Index,
    /// For each value set, the expansions on offer, with the package each came from: those inside
    /// the value sets, then the stored ones. A package has one per value set, the last it offered,
    /// because that is the only one of its own that is ever chosen. `None` when value sets are to
    /// be expanded instead.
    offered: Option<BTreeMap<String, Vec<(Scope, Answer)>>>,
    /// The value sets of terminology that is loaded as stored, in the form that holds them
    /// cheaply (see [`Held`]). Value sets to be expanded are held by `index`, parsed.
    held: BTreeMap<String, Vec<(Scope, Held)>>,
    /// Where what the terminology does not hold is read from, when it is built to be read when
    /// used (see [`Self::for_source`]).
    source: Option<Arc<dyn TermSource>>,
}

/// A value set definition held by terminology that is loaded as stored. Nothing is expanded
/// there: the expansions are already made, so a definition is read for three things only. Which
/// of its package's definitions is in use, which says its `version`; what it draws on, which
/// is kept as `references`; and, for one that has no expansion, its `compose`, to say why. A
/// parsed tree is several times the size of its text, and there are thousands of value sets, so
/// the definition is held as text, and only the few that cannot be expanded are parsed again.
struct Held {
    version: Option<String>,
    references: BTreeSet<String>,
    text: Text,
}

enum Text {
    /// The definition, without the expansion inside it: that is on offer, or absent.
    Inline(String),
    /// The key the definition is stored under: it is read from the store when used.
    Stored(String),
}

impl TerminologyBuilder {
    /// Terminology as stored: value sets are answered by the expansions already made.
    ///
    /// A value set that several packages define, and expand, is answered by the expansion of the
    /// definition in use (see [`winning_scope`]), or of the next one that has one: not by
    /// whichever package was installed last.
    pub fn for_stored(release: Option<&str>) -> Self {
        Self {
            index: Index::new(release, None),
            offered: Some(BTreeMap::new()),
            held: BTreeMap::new(),
            source: None,
        }
    }

    /// [`Self::for_stored`], for terminology that keeps an index of what is stored and reads
    /// each code system, expansion, value set and concept map from `source` when it is used.
    /// Resources and expansions are added with [`Self::add_stored_resource`] and
    /// [`Self::add_stored_expansion`], which say what each is stored under, and nothing of them
    /// is kept but what answers a question without reading them.
    pub fn for_source(release: Option<&str>, source: Arc<dyn TermSource>) -> Self {
        Self {
            index: Index::new(release, Some(source.clone())),
            offered: Some(BTreeMap::new()),
            held: BTreeMap::new(),
            source: Some(source),
        }
    }

    /// The resources of a package: every value set is expanded from its definition, each with the
    /// code system versions its own package holds.
    pub fn for_package(release: Option<&str>) -> Self {
        Self {
            index: Index::new(release, None),
            offered: None,
            held: BTreeMap::new(),
            source: None,
        }
    }

    /// Set the FHIR release the terminology is read for, when it is learned after the first
    /// resources were added.
    pub fn set_release(&mut self, release: Option<&str>) {
        self.index.set_release(release);
    }

    /// Add a resource of package `scope`. Only code systems, concept maps and value sets count.
    pub fn add_resource(&mut self, scope: Scope, resource: Json) {
        if resource_type(&resource) == Some("ValueSet") {
            self.offer_inline(scope, &resource, None);
            self.add_value_set(scope, slim_value_set(resource));
        } else {
            self.index.add_indexed(scope, &resource);
        }
    }

    /// [`Self::add_resource`] for a resource the caller keeps: only what is held of a value set
    /// is copied.
    pub fn add_resource_ref(&mut self, scope: Scope, resource: &Json) {
        if resource_type(resource) == Some("ValueSet") {
            self.offer_inline(scope, resource, None);
            self.add_value_set(scope, slim_value_set_copy(resource));
        } else {
            self.index.add_indexed(scope, resource);
        }
    }

    /// Add a resource that package `scope` stores under `key`. On a builder made by
    /// [`Self::for_source`] only what says where the resource is, and what is asked of it
    /// without reading it, is kept; on any other it is added as [`Self::add_resource`] adds it.
    pub fn add_stored_resource(&mut self, scope: Scope, key: &str, resource: Json) {
        if self.source.is_none() {
            return self.add_resource(scope, resource);
        }
        if resource_type(&resource) == Some("ValueSet") {
            self.offer_inline(scope, &resource, Some(key));
            self.add_stored_value_set(scope, key, &resource);
        } else {
            self.index.add_indexed_stored(scope, key, &resource);
        }
    }

    fn add_value_set(&mut self, scope: Scope, mut value_set: Json) {
        if self.offered.is_none() {
            self.index.add_value_set(scope, value_set);
            return;
        }
        let Some(url) = value_set
            .get("url")
            .and_then(|value| value.as_str())
            .map(|url| canonical(url).to_string())
        else {
            return;
        };
        if let Json::Object(fields) = &mut value_set {
            fields.remove("expansion");
        }
        let held = Held {
            version: value_set
                .get("version")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            references: compose_references(&value_set),
            text: Text::Inline(value_set.to_string()),
        };
        self.hold(scope, url, held);
    }

    /// A value set that package `scope` stores under `key`, to be read again when it is used.
    fn add_stored_value_set(&mut self, scope: Scope, key: &str, value_set: &Json) {
        let Some(url) = value_set
            .get("url")
            .and_then(|value| value.as_str())
            .map(|url| canonical(url).to_string())
        else {
            return;
        };
        let held = Held {
            version: value_set
                .get("version")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            references: compose_references(value_set),
            text: Text::Stored(key.to_string()),
        };
        self.hold(scope, url, held);
    }

    /// What this package defined before is replaced; the definition loaded last stays last.
    fn hold(&mut self, scope: Scope, url: String, held: Held) {
        let definitions = self.held.entry(url).or_default();
        definitions.retain(|(owner, _)| *owner != scope);
        definitions.push((scope, held));
    }

    /// Add a stored expansion document of package `scope`. It is read now, so the caller can
    /// drop the document.
    pub fn add_expansion(&mut self, scope: Scope, document: &Json) {
        let Some(offered) = self.offered.as_mut() else {
            return;
        };
        let Some((url, mut expansion)) = expansion_from_document(document) else {
            return;
        };
        expansion.scope = Some(scope);
        offer(offered, url, scope, Answer::held(expansion));
    }

    /// [`Self::add_expansion`] for a document that package `scope` stores under `key`. On a
    /// builder made by [`Self::for_source`] the codes are not kept: they are read from `source`
    /// when the expansion is used.
    pub fn add_stored_expansion(&mut self, scope: Scope, key: &str, document: &Json) {
        if self.source.is_none() {
            return self.add_expansion(scope, document);
        }
        let Some(offered) = self.offered.as_mut() else {
            return;
        };
        let Some((url, mut expansion)) = expansion_from_document(document) else {
            return;
        };
        expansion.scope = Some(scope);
        let origin = Origin::Expansion {
            package: scope,
            key: Some(key).filter(|key| *key != url).map(str::to_string),
        };
        offer(offered, url, scope, Answer::stored(&expansion, origin));
    }

    /// The expansion inside a value set's own definition. `key` is what the definition is
    /// stored under, when it is to be read from there again and not held.
    fn offer_inline(&mut self, scope: Scope, resource: &Json, key: Option<&str>) {
        let Some(offered) = self.offered.as_mut() else {
            return;
        };
        let Some(contains) = resource
            .pointer("/expansion/contains")
            .and_then(|value| value.as_array())
        else {
            return;
        };
        let Some(url) = resource
            .get("url")
            .and_then(|value| value.as_str())
            .filter(|url| !url.is_empty())
        else {
            return;
        };
        let mut expansion = expansion_from_contains(contains);
        expansion.scope = Some(scope);
        let url = canonical(url);
        let answer = match key {
            Some(key) => Answer::stored(
                &expansion,
                Origin::Definition {
                    package: scope,
                    key: Some(key).filter(|key| *key != url).map(str::to_string),
                },
            ),
            None => Answer::held(expansion),
        };
        offer(offered, url.to_string(), scope, answer);
    }

    pub fn finish(self) -> MemoryTerminology {
        let Self {
            index,
            offered,
            mut held,
            source: _,
        } = self;
        let release = index.release.clone();
        let (mut terms, value_sets) = index.finish();
        match offered {
            None => {
                terms.expand_every_value_set(&value_sets);
                terms.keep_unexpanded(value_sets);
            }
            Some(offered) => {
                let owners = order_held(&mut held, release.as_deref());
                terms.choose_offered(&owners, offered);
                terms.keep_unexpanded_held(held);
            }
        }
        terms
    }

    /// The expansion document of every value set that has an expansion, ordered by URL, made one
    /// at a time as the iterator is read, so a caller that stores each as it comes never holds
    /// them all. What is only needed to answer questions about the terminology afterwards (which
    /// value sets cannot be expanded, and why) is not worked out.
    pub fn expansion_documents(self) -> impl Iterator<Item = Json> {
        let Self {
            index,
            offered,
            mut held,
            source: _,
        } = self;
        let release = index.release.clone();
        let (mut terms, value_sets) = index.finish();
        match offered {
            None => terms.expand_every_value_set(&value_sets),
            Some(offered) => {
                let owners = order_held(&mut held, release.as_deref());
                terms.choose_offered(&owners, offered);
            }
        }
        drop((value_sets, held));
        let MemoryTerminology { expansions, .. } = terms;
        expansions
            .into_iter()
            .filter_map(|(url, answer)| match answer {
                Answer::Held(expansion) => Some(expansion_document(&url, &expansion)),
                // A terminology that is read when used is never turned into documents: they
                // are what it reads.
                Answer::Stored { .. } => None,
            })
    }
}

/// Order the held definitions of each value set for use (see `order_for_use`), and say which
/// packages they are from, the one in use last.
fn order_held(
    held: &mut BTreeMap<String, Vec<(Scope, Held)>>,
    release: Option<&str>,
) -> BTreeMap<String, Vec<Scope>> {
    held.iter_mut()
        .map(|(url, definitions)| {
            order_for_use(definitions, |definition| {
                release.is_some() && definition.version.as_deref() == release
            });
            (
                url.clone(),
                definitions.iter().map(|(scope, _)| *scope).collect(),
            )
        })
        .collect()
}

/// What expanding a value set, ordering its definitions and tracking what it draws on read from
/// it. A value set is held until every code system is in, and for good when it cannot be
/// expanded, so everything else (the narrative, the descriptions, contacts and usage notes) is
/// left out of what is held: the narrative alone is more than half the text of the value sets in
/// the R4 packages.
const VALUE_SET_FIELDS: [&str; 5] = ["resourceType", "url", "version", "compose", "expansion"];

fn slim_value_set(mut value_set: Json) -> Json {
    if let Json::Object(fields) = &mut value_set {
        fields.retain(|name, _| VALUE_SET_FIELDS.contains(&name.as_str()));
    }
    value_set
}

fn slim_value_set_copy(value_set: &Json) -> Json {
    match value_set {
        Json::Object(fields) => Json::Object(
            fields
                .iter()
                .filter(|(name, _)| VALUE_SET_FIELDS.contains(&name.as_str()))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Record `answer` as what package `scope` offers for `url`, in place of any it offered before.
fn offer(
    offered: &mut BTreeMap<String, Vec<(Scope, Answer)>>,
    url: String,
    scope: Scope,
    answer: Answer,
) {
    let candidates = offered.entry(url).or_default();
    candidates.retain(|(owner, _)| *owner != scope);
    candidates.push((scope, answer));
}

fn resource_type(resource: &Json) -> Option<&str> {
    resource
        .get("resourceType")
        .and_then(|value| value.as_str())
}

/// Every definition of each value set URL, with the package it came from, ordered for use (see
/// `order_for_use`): the one in use is last, which is the one at the server's release, or with
/// none the one loaded last. It answers a reference that comes from nowhere in particular; a
/// reference from a package prefers that package's own definition, as installing the package did.
type Definitions = BTreeMap<String, Vec<(Scope, Arc<Json>)>>;

impl MemoryTerminology {
    pub fn from_resources(resources: &[Json]) -> Self {
        Self::from_resources_scoped(&unscoped(resources), None)
    }

    /// Like [`Self::from_resources`], for a server that serves FHIR `release` and resources that
    /// each come from a package. A value set is expanded with the versions of a code system that
    /// its own package holds; only a system it does not hold is read from the others. With no
    /// version named, the release's version comes first.
    pub fn from_resources_scoped(resources: &[(Scope, &Json)], release: Option<&str>) -> Self {
        let mut builder = TerminologyBuilder::for_package(release);
        for (scope, resource) in resources {
            builder.add_resource_ref(*scope, resource);
        }
        builder.finish()
    }

    /// Expand each value set from its definition.
    fn expand_every_value_set(&mut self, value_sets: &Definitions) {
        let known = BTreeMap::new();
        let mut expander = Expander {
            systems: &self.systems,
            value_sets,
            scope: None,
            strict: false,
            known: &known,
            cache: BTreeMap::new(),
            stack: Vec::new(),
        };
        let mut expanded = Vec::new();
        for url in value_sets.keys() {
            if let Ok(expansion) = expander.expand_in_use(url) {
                expanded.push((url.clone(), expansion));
            }
        }
        for (url, expansion) in expanded {
            self.expansions.insert(url, Answer::held(expansion));
        }
    }

    /// Answer each value set with the expansion of the definition in use, or of the next one that
    /// has one. `owners` are the packages that define each value set, the one in use last.
    fn choose_offered(
        &mut self,
        owners: &BTreeMap<String, Vec<Scope>>,
        offered: BTreeMap<String, Vec<(Scope, Answer)>>,
    ) {
        for (url, mut candidates) in offered {
            let chosen = owners
                .get(&url)
                .and_then(|owners| {
                    owners
                        .iter()
                        .rev()
                        .find_map(|owner| candidates.iter().rposition(|(scope, _)| scope == owner))
                })
                .unwrap_or(candidates.len() - 1);
            let (_, answer) = candidates.swap_remove(chosen);
            self.expansions.insert(url, answer);
        }
    }

    /// [`Self::keep_unexpanded`] for value sets that were held as text. Only the ones that have
    /// no expansion are parsed again, and when the terminology is read when used not even those:
    /// each is read when it is asked about, and why it has no expansion is worked out then.
    fn keep_unexpanded_held(&mut self, held: BTreeMap<String, Vec<(Scope, Held)>>) {
        self.uses = held
            .iter()
            .map(|(url, definitions)| {
                let references = definitions
                    .iter()
                    .flat_map(|(_, definition)| definition.references.iter().cloned())
                    .collect();
                (url.clone(), references)
            })
            .collect();
        self.unexpanded = held
            .into_iter()
            .filter(|(url, _)| !self.expansions.contains_key(url))
            .map(|(url, definitions)| {
                let parsed = definitions
                    .into_iter()
                    .filter_map(|(scope, definition)| {
                        let definition = match definition.text {
                            Text::Inline(text) => {
                                Def::Held(Arc::new(serde_json::from_str(&text).ok()?))
                            }
                            Text::Stored(key) => Def::Stored(key),
                        };
                        Some((scope, definition))
                    })
                    .collect();
                (url, parsed)
            })
            .collect();
        if self.loader.is_none() {
            self.reasons = self
                .unexpanded
                .keys()
                .map(|url| (url.clone(), self.explain(url)))
                .collect();
        }
    }

    fn keep_unexpanded(&mut self, value_sets: Definitions) {
        self.record_references(&value_sets);
        self.unexpanded = value_sets
            .into_iter()
            .filter(|(url, _)| !self.expansions.contains_key(url))
            .map(|(url, definitions)| {
                let definitions = definitions
                    .into_iter()
                    .map(|(scope, definition)| (scope, Def::Held(definition)))
                    .collect();
                (url, definitions)
            })
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
        let pool = UnexpandedPool(self);
        let mut expander = Expander {
            systems: &self.systems,
            value_sets: &pool,
            scope: None,
            strict: false,
            known: self,
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

    /// The expansion that answers for `url`: held, or read now.
    fn expansion_of(&self, url: &str) -> Option<Arc<Expansion>> {
        match self.expansions.get(url)? {
            Answer::Held(expansion) => Some(expansion.clone()),
            Answer::Stored { origin, .. } => self.loader.as_ref()?.0.expansion(url, origin),
        }
    }

    /// A definition of a value set that has no expansion.
    fn definition_of(&self, package: Scope, definition: &Def) -> Option<Arc<Json>> {
        match definition {
            Def::Held(json) => Some(json.clone()),
            Def::Stored(key) => self.loader.as_ref()?.0.definition(package, key),
        }
    }

    /// The URLs of the value sets that have no expansion: every one `unexpandable` lists, without
    /// working out why.
    pub fn unexpandable_urls(&self) -> Vec<String> {
        self.unexpanded.keys().cloned().collect()
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
        let mut builder = TerminologyBuilder::for_stored(release);
        for (scope, resource) in resources {
            builder.add_resource_ref(*scope, resource);
        }
        for (scope, document) in expansions {
            builder.add_expansion(*scope, document);
        }
        builder.finish()
    }

    /// Read the expansions of these value sets, and the code systems they were made from, and
    /// keep them, so a request that checks a code against one of them never waits for a read.
    /// Terminology that holds everything already does, and nothing is done.
    ///
    /// This is for the value sets the base types bind (see `Schema::checked_value_sets`): a few
    /// hundred expansions of a few kilobytes and the code systems behind them, megabytes in all.
    /// What a request uses beyond them is read when it is used, and kept while it is among the
    /// most recently used.
    pub fn hold(&mut self, value_sets: &BTreeSet<String>) {
        let Some(loader) = self.loader.as_ref().map(|loading| loading.0.clone()) else {
            return;
        };
        let mut drawn: BTreeSet<(Option<Scope>, String, String)> = BTreeSet::new();
        let mut read = Vec::new();
        for url in value_sets {
            let url = canonical(url);
            let Some(Answer::Stored {
                origin,
                scope,
                systems,
            }) = self.expansions.get(url).cloned()
            else {
                continue;
            };
            let Some(expansion) = loader.expansion(url, &origin) else {
                continue;
            };
            for (system, version) in systems.iter() {
                drawn.insert((scope, system.clone(), version.clone()));
            }
            read.push((url.to_string(), expansion));
        }
        for (url, expansion) in read {
            self.expansions.insert(url, Answer::Held(expansion));
        }
        // The version of a system an expansion was made from is what describes its codes, and the
        // latest installed is what describes a code that no version names.
        for (scope, system, version) in drawn {
            if !version.is_empty() {
                self.systems.hold(scope, &system, Some(&version));
            }
            self.systems.hold(None, &system, None);
        }
        // What was read to be kept is kept where it is, not a second time in the caches.
        loader.clear();
    }

    /// What has been read from the store lately and is still kept: what a terminology that
    /// replaces this one can be told to read at once (see [`Self::warm`]). Terminology that holds
    /// everything has nothing of the kind.
    pub fn resident(&self) -> Resident {
        match &self.loader {
            Some(loading) => loading.0.resident(),
            None => Resident::default(),
        }
    }

    /// Read what `resident` lists, so that the requests that used it before this terminology
    /// replaced the one they were read for do not wait for it again. A row that is no longer
    /// there is passed over.
    pub fn warm(&self, resident: &Resident) {
        if let Some(loading) = &self.loader {
            loading.0.warm(resident);
        }
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
        if let Some(reasons) = self.reasons.get(url) {
            return Some(reasons.clone());
        }
        let Some(loader) = &self.loader else {
            return Some(self.explain(url));
        };
        // Terminology that is read when used works the reasons out when first asked, and keeps
        // them: a check against a value set that has no expansion is then a lookup.
        let remembered = loader
            .0
            .reasons
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(url)
            .cloned();
        if let Some(reasons) = remembered {
            return Some(reasons);
        }
        let reasons = self.explain(url);
        loader
            .0
            .reasons
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(url.to_string(), reasons.clone());
        Some(reasons)
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
        let mut builder = TerminologyBuilder::for_package(release);
        for (scope, resource) in resources {
            builder.add_resource_ref(*scope, resource);
        }
        builder.expansion_documents().collect()
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
    #[cfg(test)]
    fn documents(&self) -> Vec<Json> {
        self.expansions
            .keys()
            .filter_map(|url| {
                let expansion = self.expansion_of(url)?;
                Some(expansion_document(url, &expansion))
            })
            .collect()
    }

    pub fn expand(&self, url: &str, text_filter: Option<&str>) -> Option<Json> {
        let expansion = self.expansion_of(canonical(url))?;
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
            if expansion
                .abstract_codes
                .contains(&(system.clone(), code.clone()))
            {
                entry["abstract"] = json!(true);
            }
            contains.push(entry);
        }
        let mut body = json!({"contains": contains});
        // R4 has an expansion say which code systems it drew on only in part: a parameter named
        // `[canonical]#CodeSystem.content` whose value is the system's content mode.
        let parameters: Vec<Json> = expansion
            .incomplete
            .iter()
            .map(|((system, version), content)| {
                json!({
                    "name": format!("{}#CodeSystem.content", versioned_url(system, version)),
                    "valueCode": content
                })
            })
            .collect();
        if !parameters.is_empty() {
            body["parameter"] = Json::Array(parameters);
        }
        Some(json!({
            "resourceType": "ValueSet",
            "url": canonical(url),
            "expansion": body
        }))
    }

    /// Whether `code` is among the codes `value_set` expands to. `None` when it has no expansion.
    fn membership(&self, value_set: &str, system: Option<&str>, code: &str) -> Option<Membership> {
        let expansion = self.expansion_of(canonical(value_set))?;
        let system = system.map(canonical);
        let found = match system {
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
        Some(if found {
            Membership::In
        } else if expansion
            .incomplete
            .keys()
            .any(|(incomplete, _)| system.is_none_or(|system| system == incomplete))
        {
            Membership::NotListed
        } else {
            Membership::Out
        })
    }

    /// Whether the expansion of `value_set` lists `code` as abstract.
    fn is_abstract_in(&self, value_set: &str, system: Option<&str>, code: &str) -> bool {
        let Some(expansion) = self.expansion_of(canonical(value_set)) else {
            return false;
        };
        let system = system.map(canonical);
        expansion.abstract_codes.iter().any(|(listed, item)| {
            item == code && system.is_none_or(|system| system == listed || listed.is_empty())
        })
    }

    /// What an expansion that lists only some of a code system's concepts leaves unsaid, for
    /// `value_set`; `None` when its expansion is complete or there is none.
    fn incompleteness(&self, value_set: &str) -> Option<String> {
        let expansion = self.expansion_of(canonical(value_set))?;
        if expansion.incomplete.is_empty() {
            return None;
        }
        let systems: Vec<String> = expansion
            .incomplete
            .iter()
            .map(|((system, version), content)| {
                format!("{} (content '{content}')", versioned_url(system, version))
            })
            .collect();
        Some(format!(
            "only some of the concepts of code system {} are installed, so a code that is not \
             among them cannot be ruled out",
            systems.join(", ")
        ))
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
        self.validate_code_as(value_set, system, version, code, display, None)
    }

    /// [`Self::validate_code`] with R4's `abstract` input: `Some(true)` says the caller is in a
    /// context where a code that is only a grouping concept (`notSelectable` in its code system)
    /// may be used, and `Some(false)` says it is not, which makes such a code invalid. Without it
    /// the code is valid, as it is a member of the value set, and the answer's `message` says
    /// that it is abstract, which R4 allows a valid answer to carry as a hint.
    pub fn validate_code_as(
        &self,
        value_set: Option<&str>,
        system: Option<&str>,
        version: Option<&str>,
        code: &str,
        display: Option<&str>,
        abstract_allowed: Option<bool>,
    ) -> Json {
        if let Some(url) = value_set {
            if let (Some(system), Some(requested)) =
                (system, version.filter(|version| !version.is_empty()))
            {
                let base = canonical(split_system(system).0);
                if self
                    .systems
                    .pick_in(None, false, base, Some(requested))
                    .is_none()
                {
                    return self.system_not_loaded(base, Some(requested));
                }
            }
            return match self.membership(url, system, code) {
                Some(Membership::In) if self.is_abstract_in(url, system, code) => {
                    match abstract_allowed {
                        Some(false) => code_parameters(
                            false,
                            None,
                            Some(&format!(
                                "code {code} is abstract: a grouping concept that is not \
                                 selectable as a value"
                            )),
                        ),
                        allowed => {
                            let mut answer = code_parameters(
                                true,
                                display_of_bound(self, url, system, code, display),
                                None,
                            );
                            // Said unless the caller has said it is where such a code belongs.
                            if allowed.is_none() {
                                answer["parameter"]
                                    .as_array_mut()
                                    .expect("parameters")
                                    .push(json!({"name": "message", "valueString": format!(
                                        "code {code} is abstract: a grouping concept that is not \
                                         meant to be selected as a value"
                                    )}));
                            }
                            answer
                        }
                    }
                }
                Some(Membership::In) => code_parameters(
                    true,
                    display_of_bound(self, url, system, code, display),
                    None,
                ),
                Some(Membership::Out) => {
                    code_parameters(false, None, Some("code is not in the value set"))
                }
                // The answer agrees with `$expand`: the code is not among the ones it lists. It
                // says why that is not the end of it.
                Some(Membership::NotListed) => {
                    let why = self.incompleteness(url).unwrap_or_default();
                    code_parameters(
                        false,
                        None,
                        Some(&format!(
                            "code is not among the concepts listed for the value set: {why}"
                        )),
                    )
                }
                None => json!({
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
                display_message(describe_index(&index, code), display),
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
        is_a(&index, code, ancestor)
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
        let mut matches = Vec::new();
        // The maps in the order they were read, so the matches come in the order the targets of
        // the code were added.
        for map in &self.maps {
            if !map.sources.contains(&key.0) {
                continue;
            }
            if map_url.is_some_and(|url| canonical(url) != map.url) {
                continue;
            }
            let Some(targets) = self.targets_of(map) else {
                continue;
            };
            for target in targets.get(&key).map(Vec::as_slice).unwrap_or(&[]) {
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
        }
        let mut parameter = vec![json!({"name": "result", "valueBoolean": !matches.is_empty()})];
        parameter.extend(matches);
        json!({"resourceType": "Parameters", "parameter": parameter})
    }

    /// What a concept map translates: held, or read now.
    fn targets_of(&self, map: &MapRef) -> Option<Arc<MapTargets>> {
        match &map.body {
            MapBody::Held(targets) => Some(targets.clone()),
            MapBody::Stored { key } => self.loader.as_ref()?.0.map(map.scope, key),
        }
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
        for map in &self.maps {
            if map.url != map_url {
                continue;
            }
            if system.is_some_and(|item| !map.sources.contains(canonical(item))) {
                continue;
            }
            let Some(targets) = self.targets_of(map) else {
                continue;
            };
            for ((source_system, stored), targets) in targets.iter() {
                if stored != code {
                    continue;
                }
                if system.is_some_and(|item| canonical(item) != source_system) {
                    continue;
                }
                for target in targets {
                    if !usable_equivalence(&target.equivalence) {
                        continue;
                    }
                    if !codes.iter().any(|found| found == &target.code) {
                        codes.push(target.code.clone());
                    }
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
        match self.membership(value_set, system, code) {
            None => TermAnswer::Unknown,
            Some(Membership::In) => TermAnswer::Yes,
            Some(Membership::Out) => TermAnswer::No,
            // A code missing from a sample says nothing about the system it was taken from, and
            // `No` would have a `required` binding reject real codes.
            Some(Membership::NotListed) => TermAnswer::Unknown,
        }
    }

    fn expansion_problem(&self, value_set: &str) -> Option<String> {
        if let Some(reason) = self.incompleteness(value_set) {
            return Some(reason);
        }
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
        let index = self.systems.find(system, None)?;
        describe_index(&index, code)
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
        let expansion = self.expansion_of(canonical(value_set))?;
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
        // Which version of the system it was expanded from, and for whom, is kept with the
        // answer: no expansion is read for it.
        let expansion = self.expansions.get(canonical(value_set));
        let version = expansion.and_then(|expansion| expansion.version_of(url));
        let made_from = expansion.and_then(Answer::scope);
        match version
            .filter(|version| !version.is_empty())
            .and_then(|version| {
                self.systems
                    .resolve_in(made_from, false, url, Some(version))
            }) {
            Some((_, index)) => describe_index(&index, code),
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
        /// How many of its concepts are installed.
        concepts: usize,
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
                concepts,
                filter,
            } => {
                let system = versioned(system, version);
                let held = match content.as_str() {
                    "not-present" => "none of its concepts are installed".to_string(),
                    "example" if *concepts == 0 => "none of its concepts are installed".to_string(),
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
fn references_of(definitions: &[(Scope, Arc<Json>)]) -> BTreeSet<String> {
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
        // What was read may be what this changes, so it is read again.
        if let Some(loader) = &self.loader {
            loader.0.clear();
        }
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
                Some("CodeSystem") => self.add_written_system(resource, scope),
                Some("ConceptMap") => {
                    self.remove_map(&url);
                    self.add_written_map(resource, scope);
                }
                Some("ValueSet") => {
                    gone.remove(&url);
                    let definitions = changed.entry(url).or_default();
                    definitions.retain(|(owner, _)| *owner != scope);
                    definitions.push((scope, Arc::new(resource.clone())));
                }
                _ => {}
            }
        }
        for (owner, resource) in dependents {
            if let Some(url) = resource.get("url").and_then(|value| value.as_str()) {
                let definitions = changed.entry(canonical(url).to_string()).or_default();
                definitions.retain(|(held, _)| held != owner);
                definitions.push((*owner, Arc::new((*resource).clone())));
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
        let results: Vec<(String, Result<Expansion, Failures>)> = {
            let pool = Layered {
                changed: &changed,
                terms: self,
            };
            let mut expander = Expander {
                systems: &self.systems,
                value_sets: &pool,
                scope: None,
                strict: false,
                known: self,
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
                    self.expansions.insert(url, Answer::held(expansion));
                }
                Err(failures) => {
                    if let Some(definitions) = changed.get(&url) {
                        let definitions = definitions
                            .iter()
                            .map(|(owner, definition)| (*owner, Def::Held(definition.clone())))
                            .collect();
                        self.unexpanded.insert(url.clone(), definitions);
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
        self.maps.retain(|map| map.url != map_url);
    }
}

/// Where `code` stands against an expansion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Membership {
    In,
    Out,
    /// Not among the codes listed, but the expansion lists only some of what one of its code
    /// systems holds, so the code may still belong.
    NotListed,
}

/// `system|version`, or the system alone when the version is not known.
fn versioned_url(system: &str, version: &str) -> String {
    if version.is_empty() {
        system.to_string()
    } else {
        format!("{system}|{version}")
    }
}

fn versioned(system: &str, version: &Option<String>) -> String {
    match version {
        Some(version) => format!("{system}|{version}"),
        None => system.to_string(),
    }
}

type Failures = Vec<ExpansionFailure>;

/// The definitions of value sets an [`Expander`] may read, by URL.
trait Pool {
    /// The packages that define `url`, in the order for use: the one in use is last.
    fn owners(&self, url: &str) -> Vec<Scope>;

    /// `url` as package `owner` defines it, with only what expanding it reads.
    fn definition(&self, url: &str, owner: Scope) -> Option<Arc<Json>>;
}

impl Pool for Definitions {
    fn owners(&self, url: &str) -> Vec<Scope> {
        self.get(url)
            .map(|definitions| definitions.iter().map(|(owner, _)| *owner).collect())
            .unwrap_or_default()
    }

    fn definition(&self, url: &str, owner: Scope) -> Option<Arc<Json>> {
        self.get(url)?
            .iter()
            .find(|(held, _)| *held == owner)
            .map(|(_, definition)| definition.clone())
    }
}

/// The value sets of a terminology that have no expansion, which is what a value set that
/// cannot be expanded is explained from.
struct UnexpandedPool<'a>(&'a MemoryTerminology);

impl Pool for UnexpandedPool<'_> {
    fn owners(&self, url: &str) -> Vec<Scope> {
        self.0
            .unexpanded
            .get(url)
            .map(|definitions| definitions.iter().map(|(owner, _)| *owner).collect())
            .unwrap_or_default()
    }

    fn definition(&self, url: &str, owner: Scope) -> Option<Arc<Json>> {
        let (_, definition) = self
            .0
            .unexpanded
            .get(url)?
            .iter()
            .find(|(held, _)| *held == owner)?;
        self.0.definition_of(owner, definition)
    }
}

/// The definitions a change reads again, and for any other value set the ones that have no
/// expansion.
struct Layered<'a> {
    changed: &'a Definitions,
    terms: &'a MemoryTerminology,
}

impl Pool for Layered<'_> {
    fn owners(&self, url: &str) -> Vec<Scope> {
        if self.changed.contains_key(url) {
            self.changed.owners(url)
        } else {
            UnexpandedPool(self.terms).owners(url)
        }
    }

    fn definition(&self, url: &str, owner: Scope) -> Option<Arc<Json>> {
        if self.changed.contains_key(url) {
            self.changed.definition(url, owner)
        } else {
            UnexpandedPool(self.terms).definition(url, owner)
        }
    }
}

/// The expansions that already exist, which an [`Expander`] uses as they are.
trait Known {
    fn known(&self, url: &str) -> Option<Expansion>;
}

impl Known for BTreeMap<String, Expansion> {
    fn known(&self, url: &str) -> Option<Expansion> {
        self.get(url).cloned()
    }
}

impl Known for MemoryTerminology {
    fn known(&self, url: &str) -> Option<Expansion> {
        self.expansion_of(url)
            .map(|expansion| Expansion::clone(&expansion))
    }
}

struct Expander<'a> {
    systems: &'a Systems,
    value_sets: &'a dyn Pool,
    /// The scope of the value set being expanded, which says whose code system versions and
    /// value set definitions come first. At the top level, where it is not known, it is `None`.
    scope: Option<Scope>,
    /// Read each value set against what its own package holds and nothing else, as installing
    /// the package alone did. Expansions that already exist are not used either.
    strict: bool,
    /// Expansions that already exist. They are used as they are rather than recomputed.
    known: &'a dyn Known,
    cache: BTreeMap<(String, Scope, bool), Result<Expansion, Failures>>,
    stack: Vec<String>,
}

impl Expander<'_> {
    /// A system, read as the value set being expanded reads it.
    fn resolve(&self, system: &str, version: Option<&str>) -> Option<(&str, Arc<SystemIndex>)> {
        self.systems
            .resolve_in(self.scope, self.strict, system, version)
    }

    /// The version of a system, read as the value set being expanded reads it, without reading
    /// the system.
    fn version_of(&self, system: &str, version: Option<&str>) -> Option<String> {
        self.systems
            .pick_in(self.scope, self.strict, system, version)
            .map(|(version, _)| version.to_string())
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
        let mut owners = self.value_sets.owners(&url);
        owners.reverse();
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
            if let Some(known) = self.known.known(&url) {
                return Ok(known);
            }
        }
        // The definition of a value set named from a package is that package's own, when it has
        // one; otherwise the one in use, which is the last. A package read alone has only its
        // own.
        let owners = self.value_sets.owners(&url);
        let own = self.scope.filter(|scope| owners.contains(scope));
        let owner = if self.strict && self.scope.is_some() {
            own
        } else {
            own.or_else(|| owners.last().copied())
        };
        let Some(owner) = owner else {
            return Err(vec![ExpansionFailure::NotLoaded { value_set: url }]);
        };
        let Some(value_set) = self.value_sets.definition(&url, owner) else {
            return Err(vec![ExpansionFailure::NotLoaded { value_set: url }]);
        };
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
        let mut expansion = expand_value_set(self, &url, &value_set);
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
                        expansion.abstract_codes.remove(code);
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
        let described = system.as_ref().and_then(|system| {
            expander
                .resolve(system, version.as_deref())
                .map(|(_, index)| index)
        });
        for concept in concepts {
            let Some(code) = concept.get("code").and_then(|value| value.as_str()) else {
                continue;
            };
            let display = concept
                .get("display")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
                .or_else(|| {
                    described.as_ref().and_then(|index| {
                        index
                            .codes
                            .get(code)
                            .and_then(|concept| concept.display.clone())
                    })
                });
            let system_name = system.clone().unwrap_or_default();
            if described
                .as_ref()
                .is_some_and(|index| not_selectable(index, code))
            {
                listed
                    .abstract_codes
                    .insert((system_name.clone(), code.to_string()));
            }
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
            concepts: index.codes.len(),
            filter,
        }]
    };
    note_system(&mut expansion, expander, &system, version.as_deref());
    if let Some(filters) = filters {
        if !index.closed {
            return Err(incomplete(Some(describe_filters(filters))));
        }
        let matched = filter_codes(&index, filters);
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
                if not_selectable(&index, &code) {
                    expansion
                        .abstract_codes
                        .insert((system.clone(), code.clone()));
                }
                expansion.codes.insert((system.clone(), code), display);
            }
        }
        return Ok(expansion);
    }
    if !expansion.codes.is_empty() {
        return Ok(expansion);
    }
    if !index.closed {
        // A whole `example` system lists the few concepts it holds, and says so: HL7 publishes
        // value sets (`service-category`, `service-type`) that are nothing but that include,
        // offered as choices. Only an include can be completed that way. A `fragment` has no
        // telling which concepts are missing, an exclusion of a sample would leave what it could
        // not name, and a system with no concepts would be an empty list that looks like an
        // answer.
        if at.section != "include" || index.content != "example" || index.codes.is_empty() {
            return Err(incomplete(None));
        }
        let read = expander
            .version_of(&system, version.as_deref())
            .unwrap_or_else(|| version.clone().unwrap_or_default());
        expansion
            .incomplete
            .insert((system.clone(), read), index.content.clone());
    }
    for (code, concept) in &index.codes {
        if is_not_selectable(concept) {
            expansion
                .abstract_codes
                .insert((system.clone(), code.clone()));
        }
        expansion
            .codes
            .insert((system.clone(), code.clone()), concept.display.clone());
    }
    Ok(expansion)
}

/// Whether the code system marks `code` as one that is not to be selected as a value
/// (`notSelectable`, the concept property HL7 code systems use for grouping concepts).
fn not_selectable(index: &SystemIndex, code: &str) -> bool {
    index.codes.get(code).is_some_and(is_not_selectable)
}

fn is_not_selectable(concept: &Concept) -> bool {
    concept
        .properties
        .iter()
        .any(|(name, value)| name == "notSelectable" && value == "true")
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
        .version_of(system, requested)
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
    into.incomplete.extend(other.incomplete);
    into.abstract_codes.extend(other.abstract_codes);
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

fn prefer_release(definitions: &mut Vec<(Scope, Arc<Json>)>, release: Option<&str>) {
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

/// What indexing resources builds up: code systems and concept maps are indexed as they are
/// handed over, and value sets are kept, to be expanded once every code system is in.
struct Index {
    terms: MemoryTerminology,
    value_sets: Definitions,
    release: Option<String>,
}

impl Index {
    /// With a `source`, code systems and concept maps are indexed to be read from it when used.
    fn new(release: Option<&str>, source: Option<Arc<dyn TermSource>>) -> Self {
        let mut terms = match source {
            Some(source) => MemoryTerminology::reading_from(source),
            None => MemoryTerminology::default(),
        };
        terms.set_release(release);
        Self {
            terms,
            value_sets: Definitions::new(),
            release: release.map(str::to_string),
        }
    }

    fn set_release(&mut self, release: Option<&str>) {
        self.terms.set_release(release);
        self.release = release.map(str::to_string);
    }

    /// A code system or concept map: indexed, nothing of it is kept. Anything else is ignored.
    fn add_indexed(&mut self, scope: Scope, resource: &Json) {
        match resource_type(resource) {
            Some("CodeSystem") => self.terms.add_system(resource, scope, None),
            Some("ConceptMap") => self.terms.add_map(resource, scope, None),
            _ => {}
        }
    }

    /// [`Self::add_indexed`] for a resource stored under `key`, which is read again from there
    /// when it is used.
    fn add_indexed_stored(&mut self, scope: Scope, key: &str, resource: &Json) {
        match resource_type(resource) {
            Some("CodeSystem") => self.terms.add_system(resource, scope, Some(key)),
            Some("ConceptMap") => self.terms.add_map(resource, scope, Some(key)),
            _ => {}
        }
    }

    fn add_value_set(&mut self, scope: Scope, resource: Json) {
        let Some(url) = resource
            .get("url")
            .and_then(|value| value.as_str())
            .map(|url| canonical(url).to_string())
        else {
            return;
        };
        // What this package defined before is replaced; the definition loaded last stays last.
        let definitions = self.value_sets.entry(url).or_default();
        definitions.retain(|(owner, _)| *owner != scope);
        definitions.push((scope, Arc::new(resource)));
    }

    fn finish(mut self) -> (MemoryTerminology, Definitions) {
        for definitions in self.value_sets.values_mut() {
            prefer_release(definitions, self.release.as_deref());
        }
        (self.terms, self.value_sets)
    }
}

/// The index a CodeSystem makes: its URL and version, and its concepts.
fn system_index(resource: &Json) -> Option<(String, String, SystemIndex)> {
    let (url, version) = code_system_key(resource)?;
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
    Some((
        url,
        version,
        SystemIndex {
            closed,
            content,
            name: resource
                .get("name")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string()),
            codes,
        },
    ))
}

/// What a ConceptMap translates: its canonical URL, and its targets by source system and code.
/// Nothing comes of a map that has no URL, no group or no target.
fn map_of(resource: &Json) -> Option<(String, MapTargets)> {
    let url = canonical(resource.get("url").and_then(|value| value.as_str())?).to_string();
    let groups = resource.get("group").and_then(|value| value.as_array())?;
    let mut targets = MapTargets::new();
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
            let Some(listed) = element.get("target").and_then(|value| value.as_array()) else {
                continue;
            };
            for target in listed {
                let Some(target_code) = target.get("code").and_then(|value| value.as_str()) else {
                    continue;
                };
                let equivalence = target
                    .get("equivalence")
                    .and_then(|value| value.as_str())
                    .unwrap_or("equivalent");
                targets
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
    Some((url, targets))
}

/// What a concept map's targets take, roughly: what a cache weighs them by.
fn map_weight(targets: &MapTargets) -> usize {
    targets
        .iter()
        .map(|((system, code), list)| {
            96 + system.len()
                + code.len()
                + list
                    .iter()
                    .map(|target| {
                        128 + target.map_url.len()
                            + target.system.len()
                            + target.code.len()
                            + target.equivalence.len()
                    })
                    .sum::<usize>()
        })
        .sum::<usize>()
        + 64
}

fn map_targets(resource: &Json) -> MapTargets {
    map_of(resource)
        .map(|(_, targets)| targets)
        .unwrap_or_default()
}

fn json_weight(value: &Json) -> usize {
    crate::schema::json_bytes(value)
}

impl MemoryTerminology {
    /// Terminology that keeps an index and reads what it does not hold from `source`.
    fn reading_from(source: Arc<dyn TermSource>) -> Self {
        let loader = Loading(Arc::new(Loader::new(source)));
        Self {
            systems: Systems {
                loader: Some(Loading(loader.0.clone())),
                ..Systems::default()
            },
            loader: Some(loader),
            ..Self::default()
        }
    }

    /// Give the caches budgets so small that almost nothing is kept, so that what is read is
    /// read again. Only for checking that a second read gives what the first did.
    #[cfg(test)]
    fn squeeze(&mut self) {
        if let Some(loading) = &self.loader {
            let source = loading.0.source.clone();
            let mut loader = Loader::new(source);
            loader.systems = Cache::new(1);
            loader.expansions = Cache::new(1);
            loader.definitions = Cache::new(1);
            loader.maps = Cache::new(1);
            let loading = Loading(Arc::new(loader));
            self.systems.loader = Some(Loading(loading.0.clone()));
            self.loader = Some(loading);
        }
    }

    /// Add a code system. `stored` is the key it is stored under when it is to be read again from
    /// there when used, and then none of it is kept but its digest.
    fn add_system(&mut self, resource: &Json, scope: Scope, stored: Option<&str>) {
        let Some((url, version, index)) = system_index(resource) else {
            return;
        };
        let digest = index.digest();
        match stored {
            Some(key) => self
                .systems
                .insert(scope, &url, &version, key, digest, None),
            None => self
                .systems
                .insert(scope, &url, &version, "", digest, Some(Arc::new(index))),
        }
    }

    /// A code system that was just written. Terminology that is read when used has it in the
    /// store already, under the key a write stores it under, and keeps what was just made of it
    /// for the next lookup.
    fn add_written_system(&mut self, resource: &Json, scope: Scope) {
        let Some(loader) = self.loader.as_ref().map(|loading| loading.0.clone()) else {
            return self.add_system(resource, scope, None);
        };
        let (Some((url, version, index)), Some(key)) =
            (system_index(resource), code_system_row_key(resource))
        else {
            return;
        };
        let digest = index.digest();
        let weight = index.approx_bytes();
        loader
            .systems
            .insert((scope, key.clone()), Arc::new(index), weight);
        self.systems
            .insert(scope, &url, &version, &key, digest, None);
    }

    /// Add a concept map. `stored` is as for [`Self::add_system`].
    fn add_map(&mut self, resource: &Json, scope: Scope, stored: Option<&str>) {
        let Some((url, targets)) = map_of(resource) else {
            return;
        };
        if targets.is_empty() {
            return;
        }
        let sources = targets.keys().map(|(source, _)| source.clone()).collect();
        let body = match stored {
            Some(key) => MapBody::Stored {
                key: key.to_string(),
            },
            None => MapBody::Held(Arc::new(targets)),
        };
        self.maps.push(MapRef {
            scope,
            url,
            sources,
            body,
        });
    }

    /// A concept map that was just written: see [`Self::add_written_system`].
    fn add_written_map(&mut self, resource: &Json, scope: Scope) {
        let Some(loader) = self.loader.as_ref().map(|loading| loading.0.clone()) else {
            return self.add_map(resource, scope, None);
        };
        let Some(key) = resource.get("url").and_then(|value| value.as_str()) else {
            return;
        };
        let Some((url, targets)) = map_of(resource) else {
            return;
        };
        if targets.is_empty() {
            return;
        }
        let sources = targets.keys().map(|(source, _)| source.clone()).collect();
        let weight = map_weight(&targets);
        loader
            .maps
            .insert((scope, key.to_string()), Arc::new(targets), weight);
        self.maps.push(MapRef {
            scope,
            url,
            sources,
            body: MapBody::Stored {
                key: key.to_string(),
            },
        });
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
            if item.get("abstract").and_then(|value| value.as_bool()) == Some(true) {
                expansion
                    .abstract_codes
                    .insert((system.clone(), code.to_string()));
            }
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
    if let Some(incomplete) = document
        .get("incomplete")
        .and_then(|value| value.as_array())
    {
        for system in incomplete {
            let text = |name: &str| system.get(name).and_then(|value| value.as_str());
            let Some(url) = text("url") else {
                continue;
            };
            expansion.incomplete.insert(
                (
                    canonical(url).to_string(),
                    text("version").unwrap_or("").to_string(),
                ),
                text("content").unwrap_or("example").to_string(),
            );
        }
    }
    if let Some(contains) = document.get("contains").and_then(|value| value.as_array()) {
        collect_expansion(contains, &mut expansion, !systems_listed);
    }
    Some((canonical(url).to_string(), expansion))
}

/// The rules stored expansions are made under. A document without this number, or with another,
/// was made by code that chose a code system version or a value set definition differently (one
/// pool for every package, the package installed last), or that refused a value set the current
/// code expands (a whole `example` system, listed and flagged), or listed a code its system
/// marks `notSelectable` without saying it is abstract, so it is rebuilt once rather than read.
pub const EXPANSION_RULES: u32 = 3;

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
            if expansion
                .abstract_codes
                .contains(&(system.clone(), code.clone()))
            {
                entry["abstract"] = json!(true);
            }
            entry
        })
        .collect();
    let mut document =
        json!({"url": url, "systems": systems, "contains": contains, "rules": EXPANSION_RULES});
    if !expansion.incomplete.is_empty() {
        document["incomplete"] = expansion
            .incomplete
            .iter()
            .map(|((system, version), content)| {
                json!({"url": system, "version": version, "content": content})
            })
            .collect();
    }
    document
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
            code_system("http://example.org/examples", "fragment", &["a"]),
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
            text[1].contains("'fragment'") && text[1].contains("included whole"),
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
            code_system("http://example.org/examples", "fragment", &["a"]),
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
        assert!(text.contains("'fragment'"), "{text}");
        let missing = terms
            .why_not_expanded("http://example.org/missing-dependency")
            .unwrap();
        assert!(messages(&missing)[0].contains("http://example.org/nowhere is not loaded"));
    }

    fn parameters_of(expanded: &serde_json::Value) -> Vec<String> {
        expanded["expansion"]["parameter"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        format!(
                            "{} = {}",
                            item["name"].as_str().unwrap(),
                            item["valueCode"].as_str().unwrap()
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn a_whole_example_system_is_listed_and_flagged_and_a_miss_is_never_ruled_on() {
        let (sample, full) = ("http://example.org/sample", "http://example.org/full");
        let resources = [
            code_system(sample, "example", &["a", "b"]),
            code_system(full, "complete", &["x"]),
            value_set(
                "http://example.org/only",
                json!({"include": [{"system": sample}]}),
            ),
            value_set(
                "http://example.org/mixed",
                json!({"include": [{"system": sample}, {"system": full}]}),
            ),
            value_set(
                "http://example.org/nested",
                json!({"include": [{"valueSet": ["http://example.org/only"]}]}),
            ),
            value_set(
                "http://example.org/listed",
                json!({"include": [{"system": sample, "concept": [{"code": "a"}]}]}),
            ),
        ];
        let live = MemoryTerminology::from_resources(&resources);
        // What a server reads back from the expansions it stored answers the same.
        let documents = MemoryTerminology::expansion_documents(&resources);
        let stored = MemoryTerminology::from_stored(&resources, &documents);
        for terms in [&live, &stored] {
            let flag = format!("{sample}#CodeSystem.content = example");
            let only = terms.expand("http://example.org/only", None).unwrap();
            assert_eq!(only["expansion"]["contains"].as_array().unwrap().len(), 2);
            assert_eq!(parameters_of(&only), [flag.clone()]);
            // An include that names its concepts is complete, however sparse its system is, and
            // an include of a value set that is not complete is not either.
            let listed = terms.expand("http://example.org/listed", None).unwrap();
            assert!(parameters_of(&listed).is_empty());
            let nested = terms.expand("http://example.org/nested", None).unwrap();
            assert_eq!(parameters_of(&nested), [flag.clone()]);
            let mixed = terms.expand("http://example.org/mixed", None).unwrap();
            assert_eq!(mixed["expansion"]["contains"].as_array().unwrap().len(), 3);
            assert_eq!(parameters_of(&mixed), [flag]);

            // A code the sample does not list may still be in the system, so the answer to a
            // validator is "cannot say", while a complete system's miss is a no.
            let ask = |value_set: &str, system: Option<&str>, code: &str| {
                terms.contains(value_set, system, code)
            };
            assert_eq!(
                ask("http://example.org/only", Some(sample), "a"),
                TermAnswer::Yes
            );
            assert_eq!(
                ask("http://example.org/only", Some(sample), "zz"),
                TermAnswer::Unknown
            );
            assert_eq!(
                ask("http://example.org/only", None, "zz"),
                TermAnswer::Unknown
            );
            assert_eq!(
                ask("http://example.org/mixed", Some(full), "zz"),
                TermAnswer::No
            );
            assert_eq!(
                ask("http://example.org/mixed", Some(sample), "zz"),
                TermAnswer::Unknown
            );
            assert_eq!(
                ask("http://example.org/mixed", Some(full), "x"),
                TermAnswer::Yes
            );
            assert_eq!(
                ask("http://example.org/nested", Some(sample), "zz"),
                TermAnswer::Unknown
            );
            assert_eq!(
                ask("http://example.org/listed", Some(sample), "b"),
                TermAnswer::No
            );
            let reason = terms.expansion_problem("http://example.org/only").unwrap();
            assert!(
                reason.contains("only some of the concepts of code system")
                    && reason.contains(sample)
                    && reason.contains("'example'"),
                "{reason}"
            );
            assert_eq!(terms.expansion_problem("http://example.org/listed"), None);

            // `$validate-code` answers as `$expand` lists, and says why a miss is not final.
            let validate = |value_set: &str, system: &str, code: &str| {
                terms.validate_code(Some(value_set), Some(system), None, code, None)
            };
            let result = |outcome: &serde_json::Value| {
                outcome["parameter"][0]["valueBoolean"].as_bool().unwrap()
            };
            assert!(result(&validate("http://example.org/only", sample, "a")));
            let miss = validate("http://example.org/only", sample, "zz");
            assert!(!result(&miss));
            assert!(miss["parameter"][1]["valueString"]
                .as_str()
                .unwrap()
                .contains("only some of the concepts"));
            let miss = validate("http://example.org/mixed", full, "zz");
            assert!(!result(&miss));
            assert_eq!(
                miss["parameter"][1]["valueString"],
                "code is not in the value set"
            );
        }
    }

    #[test]
    fn only_an_include_of_a_whole_example_system_with_concepts_is_listed() {
        let (sample, fragment, bare) = (
            "http://example.org/sample",
            "http://example.org/fragment",
            "http://example.org/bare",
        );
        let terms = MemoryTerminology::from_resources(&[
            code_system(sample, "example", &["a"]),
            code_system(fragment, "fragment", &["a"]),
            code_system(bare, "example", &[]),
            code_system("http://example.org/full", "complete", &["a"]),
            value_set(
                "http://example.org/fragment-whole",
                json!({"include": [{"system": fragment}]}),
            ),
            value_set(
                "http://example.org/bare-whole",
                json!({"include": [{"system": bare}]}),
            ),
            value_set(
                "http://example.org/sample-filtered",
                json!({"include": [{"system": sample,
                    "filter": [{"property": "concept", "op": "is-a", "value": "a"}]}]}),
            ),
            value_set(
                "http://example.org/sample-excluded",
                json!({"include": [{"system": "http://example.org/full"}],
                       "exclude": [{"system": sample}]}),
            ),
        ]);
        let refused = |url: &str| {
            let failures = terms.why_not_expanded(url).expect(url);
            assert_eq!(failures.len(), 1, "{failures:?}");
            assert_eq!(failures[0].reason(), "code-system-incomplete");
            (failures[0].message(), failures[0].expression().unwrap())
        };
        let (text, at) = refused("http://example.org/fragment-whole");
        assert!(
            text.contains("cannot be included whole: its content is 'fragment', not complete"),
            "{text}"
        );
        assert_eq!(at, "ValueSet.compose.include[0]");
        // An example system that holds no concept has nothing to list: an empty answer would look
        // as complete as any other.
        let (text, _) = refused("http://example.org/bare-whole");
        assert!(
            text.contains("cannot be included whole: none of its concepts are installed"),
            "{text}"
        );
        // Which of its concepts a filter picks cannot be told from a sample.
        let (text, _) = refused("http://example.org/sample-filtered");
        assert!(
            text.contains(
                "cannot be filtered by concept is-a a: its content is 'example', not complete"
            ),
            "{text}"
        );
        // What an exclusion leaves out cannot either.
        let (text, at) = refused("http://example.org/sample-excluded");
        assert!(text.contains("cannot be included whole"), "{text}");
        assert_eq!(at, "ValueSet.compose.exclude[0]");
        for url in [
            "http://example.org/fragment-whole",
            "http://example.org/bare-whole",
            "http://example.org/sample-filtered",
            "http://example.org/sample-excluded",
        ] {
            assert!(terms.expand(url, None).is_none(), "{url}");
        }
    }

    fn abstract_codes_of(expanded: &serde_json::Value) -> Vec<String> {
        expanded["expansion"]["contains"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["abstract"] == true)
            .map(|entry| entry["code"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_code_its_system_marks_not_selectable_is_listed_as_abstract() {
        let (groups, plain) = ("http://example.org/groups", "http://example.org/plain");
        let selectable = |value: bool| json!([{"code": "notSelectable", "valueBoolean": value}]);
        let resources = [
            // `_group` is a grouping concept with two selectable children, one of which says
            // outright that it is selectable; `_other` is a second group, deeper down.
            json!({"resourceType": "CodeSystem", "url": groups, "content": "complete",
            "property": [{"code": "notSelectable", "type": "boolean"}],
            "concept": [
                {"code": "_group", "property": selectable(true), "concept": [
                    {"code": "a"},
                    {"code": "b", "property": selectable(false)},
                    {"code": "_other", "property": selectable(true), "concept": [
                        {"code": "c"}
                    ]}
                ]}
            ]}),
            code_system(plain, "complete", &["x", "y"]),
            value_set(
                "http://example.org/whole",
                json!({"include": [{"system": groups}]}),
            ),
            value_set(
                "http://example.org/filtered",
                json!({"include": [{"system": groups,
                    "filter": [{"property": "concept", "op": "is-a", "value": "_other"}]}]}),
            ),
            value_set(
                "http://example.org/listed",
                json!({"include": [{"system": groups,
                    "concept": [{"code": "_group"}, {"code": "a"}]}]}),
            ),
            value_set(
                "http://example.org/without",
                json!({"include": [{"system": groups}],
                       "exclude": [{"system": groups, "concept": [{"code": "_group"}]}]}),
            ),
            value_set(
                "http://example.org/plain",
                json!({"include": [{"system": plain}]}),
            ),
            value_set(
                "http://example.org/nested",
                json!({"include": [{"valueSet": ["http://example.org/filtered"]},
                                   {"system": plain}]}),
            ),
        ];
        let live = MemoryTerminology::from_resources(&resources);
        let documents = MemoryTerminology::expansion_documents(&resources);
        let stored = MemoryTerminology::from_stored(&resources, &documents);
        for terms in [&live, &stored] {
            let abstract_of = |url: &str| {
                abstract_codes_of(&terms.expand(url, None).unwrap_or_else(|| panic!("{url}")))
            };
            assert_eq!(
                abstract_of("http://example.org/whole"),
                ["_group", "_other"]
            );
            // A filter, an explicit list and an included value set carry it the same way.
            assert_eq!(abstract_of("http://example.org/filtered"), ["_other"]);
            assert_eq!(abstract_of("http://example.org/listed"), ["_group"]);
            assert_eq!(abstract_of("http://example.org/nested"), ["_other"]);
            // What is excluded is not there to be flagged.
            assert_eq!(abstract_of("http://example.org/without"), ["_other"]);
            // A system with no such property flags nothing, and says nothing of it.
            assert!(abstract_of("http://example.org/plain").is_empty());
            let plain_entry = terms.expand("http://example.org/plain", None).unwrap();
            assert!(!plain_entry.to_string().contains("abstract"));
            // `notSelectable` false is not abstract, and it is not listed as if it were.
            let whole = terms.expand("http://example.org/whole", None).unwrap();
            let entry = |code: &str| {
                whole["expansion"]["contains"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|entry| entry["code"] == code)
                    .unwrap()
                    .clone()
            };
            assert!(entry("b").get("abstract").is_none());
            assert!(entry("a").get("abstract").is_none());

            // `$validate-code` takes R4's `abstract`: without it the code is valid and says it is
            // a grouping concept, with it true it is valid, with it false it is not.
            let validate = |code: &str, allowed: Option<bool>| {
                terms.validate_code_as(
                    Some("http://example.org/whole"),
                    Some(groups),
                    None,
                    code,
                    None,
                    allowed,
                )
            };
            let result = |outcome: &serde_json::Value| {
                outcome["parameter"][0]["valueBoolean"].as_bool().unwrap()
            };
            let message = |outcome: &serde_json::Value| {
                outcome["parameter"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| item["name"] == "message")
                    .map(|item| item["valueString"].as_str().unwrap().to_string())
            };
            let said_nothing = validate("_group", None);
            assert!(result(&said_nothing));
            assert!(message(&said_nothing)
                .unwrap()
                .contains("_group is abstract"));
            let allowed = validate("_group", Some(true));
            assert!(result(&allowed));
            assert_eq!(message(&allowed), None);
            let refused = validate("_group", Some(false));
            assert!(!result(&refused));
            assert!(message(&refused).unwrap().contains("_group is abstract"));
            for allowed in [None, Some(true), Some(false)] {
                let outcome = validate("a", allowed);
                assert!(result(&outcome));
                assert_eq!(message(&outcome), None);
            }
            // Asked without a system, the code is found the same way.
            let bare = terms.validate_code_as(
                Some("http://example.org/whole"),
                None,
                None,
                "_other",
                None,
                Some(false),
            );
            assert!(!result(&bare));
        }
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

    /// What a package holds, each resource with the scope it was installed under.
    type Definitions = Vec<(super::Scope, serde_json::Value)>;

    /// The R4 package and a package that carries an R5-style copy of the same value set: another
    /// version, over another code system URL (as `hl7.fhir.uv.extensions.r4` does).
    fn r4_and_its_r5_style_copy() -> (Definitions, Definitions) {
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

    /// `package` again under another scope, each resource with generated narrative text, which is
    /// how `hl7.fhir.r4.examples` carries what `hl7.fhir.r4.core` does.
    fn with_narrative(scope: super::Scope, package: &Definitions) -> Definitions {
        package
            .iter()
            .map(|(_, resource)| {
                let mut resource = resource.clone();
                resource["text"] = json!({"status": "generated", "div": "<div>narrative</div>"});
                (scope, resource)
            })
            .collect()
    }

    #[test]
    fn a_copy_of_the_release_with_other_narrative_moves_who_holds_a_value_set_and_nothing_it_means()
    {
        // R4 core (scope 1), an extensions package with an R5-style copy of the value set
        // (scope 2), and the examples package (scope 3): core's definitions again, with narrative.
        // sdc depended on it; it is no longer installed for sdc. Whichever order the rest of the
        // tree is installed in, the value set means what it meant, and only the package that holds
        // the definition in use differs.
        let (core, ext) = r4_and_its_r5_style_copy();
        let examples = with_narrative(3, &core);
        let release = Some("4.0.1");
        let expansion = |all: &Definitions| -> serde_json::Value {
            let tagged: Vec<(super::Scope, &serde_json::Value)> = all
                .iter()
                .map(|(scope, resource)| (*scope, resource))
                .collect();
            MemoryTerminology::expansion_documents_scoped(&tagged, release)
                .into_iter()
                .find(|document| document["url"] == SET)
                .unwrap()
        };
        let owner = |all: &Definitions| -> super::Scope {
            let tagged: Vec<(super::Scope, &serde_json::Value)> = all
                .iter()
                .map(|(scope, resource)| (*scope, resource))
                .collect();
            super::winning_scopes(&tagged, release)[SET]
        };

        let without = together(&[&core, &ext]);
        for with in [
            together(&[&core, &examples, &ext]),
            together(&[&core, &ext, &examples]),
        ] {
            // The same answers, and the same expansion document.
            for all in [&without, &with] {
                let terms = scoped_for(all, release);
                assert!(has(&terms, SET, R4_SYSTEM, "a") && has(&terms, SET, R4_SYSTEM, "b"));
                assert!(
                    !has(&terms, SET, R5_SYSTEM, "c"),
                    "the R5-style copy took over"
                );
            }
            assert_eq!(expansion(&without), expansion(&with));
            // The definition in use is the last one at the release: core's alone, the examples
            // package's when it is there. This is the one thing that moves.
            assert_eq!(owner(&without), 1);
            assert_eq!(owner(&with), 3);
        }

        // A code system that two packages carry with the same content is one copy that both hold,
        // and when the examples package is left out it is core's alone.
        let held = |all: &Definitions| -> Vec<Vec<super::Scope>> {
            let terms = scoped_for(all, release);
            terms.systems.by_url[R4_SYSTEM]["4.0.1"]
                .iter()
                .map(|variant| variant.holders.keys().copied().collect())
                .collect()
        };
        assert_eq!(held(&without), [vec![1]]);
        assert_eq!(held(&together(&[&core, &examples, &ext])), [vec![1, 3]]);

        // A start reads the expansion the owner stored, and answers the same either way.
        for (all, stored_under) in [
            (together(&[&core, &examples, &ext]), 3),
            (without.clone(), 1),
        ] {
            let resources: Vec<serde_json::Value> = all
                .iter()
                .filter(|(scope, _)| *scope == stored_under)
                .map(|(_, r)| r.clone())
                .collect();
            let documents = MemoryTerminology::expansion_documents(&resources);
            let stored: Vec<(super::Scope, &serde_json::Value)> = documents
                .iter()
                .map(|document| (stored_under, document))
                .collect();
            let tagged: Vec<(super::Scope, &serde_json::Value)> = all
                .iter()
                .map(|(scope, resource)| (*scope, resource))
                .collect();
            let started = MemoryTerminology::from_stored_scoped(&tagged, &stored, release);
            assert!(has(&started, SET, R4_SYSTEM, "a") && !has(&started, SET, R5_SYSTEM, "c"));
        }
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
    /// expansion, on why each value set cannot expand, and on concept map translation. So does
    /// terminology that reads what it does not hold from a store, when the change is applied to
    /// it after the store has the rows it writes.
    fn assert_matches_a_rebuild(base: &[serde_json::Value], change: TerminologyChange) {
        let (inc, rows) = incremental(base, &change);
        let final_resources = after(base, &change);
        let full = MemoryTerminology::from_resources(&final_resources);
        assert_eq!(inc.documents(), full.documents(), "expansions differ");
        assert_eq!(rows, full.documents(), "rows to store differ");
        let lazy = lazily_changed(base, &change);
        for (what, terms) in [
            ("incremental", &inc),
            ("incremental, read when used", &lazy),
        ] {
            assert_eq!(
                terms.documents(),
                full.documents(),
                "{what}: expansions differ"
            );
            let reasons = |terms: &MemoryTerminology| -> Vec<(String, Vec<String>)> {
                terms
                    .unexpandable()
                    .into_iter()
                    .map(|(url, failures)| (url, failures.iter().map(|f| f.message()).collect()))
                    .collect()
            };
            assert_eq!(reasons(terms), reasons(&full), "{what}: reasons differ");
            for resource in &final_resources {
                if resource["resourceType"] == "ConceptMap" {
                    for code in ["a", "b", "c"] {
                        assert_eq!(
                            terms.translate(
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
                            "{what}: translation of {code} differs"
                        );
                    }
                }
            }
            // The same answer comes back for membership.
            for resource in &final_resources {
                if resource["resourceType"] == "ValueSet" {
                    for code in ["a", "b", "c", "x"] {
                        assert_eq!(
                            terms.contains(&url_of(resource), None, code),
                            full.contains(&url_of(resource), None, code),
                            "{what}: membership of {code} differs"
                        );
                    }
                }
            }
        }
    }

    /// `base` as stored, read when used, with `change` written to the store and then applied.
    fn lazily_changed(base: &[serde_json::Value], change: &TerminologyChange) -> MemoryTerminology {
        let documents = MemoryTerminology::expansion_documents(base);
        let resources: Vec<(super::Scope, serde_json::Value)> =
            base.iter().map(|resource| (0, resource.clone())).collect();
        let stored: Vec<(super::Scope, serde_json::Value)> = documents
            .iter()
            .map(|document| (0, document.clone()))
            .collect();
        let (mut terms, shelf) = lazily(&resources, &stored, None);
        for (_, key) in &change.removed {
            shelf.resources.lock().unwrap().remove(&(0, key.clone()));
        }
        for resource in &change.upserted {
            shelf
                .resources
                .lock()
                .unwrap()
                .insert((0, row_key(resource)), resource.clone());
        }
        let dependents = terms.dependents_of(&change.urls());
        let loaded: Vec<serde_json::Value> = base
            .iter()
            .filter(|resource| {
                resource["resourceType"] == "ValueSet" && dependents.contains(&url_of(resource))
            })
            .cloned()
            .collect();
        terms.apply(change, &loaded);
        terms
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

    #[test]
    fn a_value_set_is_held_without_what_expanding_it_never_reads() {
        let full = json!({
            "resourceType": "ValueSet", "id": "x", "url": SET, "version": "1",
            "text": {"status": "generated", "div": "<div>a long narrative</div>"},
            "description": "words", "contact": [{"name": "someone"}],
            "compose": {"include": [{"system": SYS}]},
            "expansion": {"contains": [{"system": SYS, "code": "a"}]}
        });
        let slim = json!({
            "resourceType": "ValueSet", "url": SET, "version": "1",
            "compose": {"include": [{"system": SYS}]},
            "expansion": {"contains": [{"system": SYS, "code": "a"}]}
        });
        assert_eq!(super::slim_value_set(full.clone()), slim);
        assert_eq!(super::slim_value_set_copy(&full), slim);

        // What is held is enough to expand it: from its definition, or from the expansion inside
        // it, which comes first, and to say why one cannot be.
        let system = json!({"resourceType": "CodeSystem", "url": SYS, "content": "complete",
                            "concept": [{"code": "a"}, {"code": "b"}]});
        let terms = MemoryTerminology::from_resources(&[system.clone(), full.clone()]);
        assert_eq!(terms.contains(SET, Some(SYS), "a"), TermAnswer::Yes);
        assert_eq!(terms.contains(SET, Some(SYS), "b"), TermAnswer::No);
        let mut composed = full.clone();
        composed.as_object_mut().unwrap().remove("expansion");
        let terms = MemoryTerminology::from_resources(&[system.clone(), composed]);
        assert_eq!(terms.contains(SET, Some(SYS), "b"), TermAnswer::Yes);
        let missing = json!({"resourceType": "ValueSet", "url": "http://example.org/gone",
                             "description": "words",
                             "compose": {"include": [{"system": "http://example.org/nowhere"}]}});
        let terms = MemoryTerminology::from_resources(&[system, missing]);
        let reasons = terms.why_not_expanded("http://example.org/gone");
        assert!(reasons.is_some_and(|reasons| !reasons.is_empty()));
    }

    /// The codes of the expansion a terminology answers `url` with.
    fn expansion_codes(terms: &MemoryTerminology, url: &str) -> Vec<String> {
        terms
            .expand(url, None)
            .map(|expansion| {
                expansion["expansion"]["contains"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item["code"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn the_expansion_a_stored_terminology_answers_with_is_the_definition_in_uses() {
        let defined = |inline: Option<&str>| {
            let mut set = json!({"resourceType": "ValueSet", "url": SET});
            if let Some(code) = inline {
                set["expansion"] = json!({"contains": [{"code": code}]});
            }
            set
        };
        let stored = |codes: &[&str]| {
            json!({"url": SET, "systems": [],
                   "contains": codes.iter().map(|code| json!({"code": code})).collect::<Vec<_>>()})
        };
        let read = |resources: &[(super::Scope, serde_json::Value)],
                    expansions: &[(super::Scope, serde_json::Value)]| {
            let resources: Vec<(super::Scope, &serde_json::Value)> = resources
                .iter()
                .map(|(scope, json)| (*scope, json))
                .collect();
            let expansions: Vec<(super::Scope, &serde_json::Value)> = expansions
                .iter()
                .map(|(scope, json)| (*scope, json))
                .collect();
            expansion_codes(
                &MemoryTerminology::from_stored_scoped(&resources, &expansions, None),
                SET,
            )
        };

        // A package's own stored expansion is the later of what it offered: it replaces the one
        // inside its definition.
        assert_eq!(
            read(&[(1, defined(Some("a")))], &[(1, stored(&["b"]))]),
            ["b"]
        );
        // The definition in use is the one loaded last, and its package's expansion answers,
        // whatever another package stored.
        assert_eq!(
            read(
                &[(1, defined(Some("a"))), (2, defined(Some("c")))],
                &[(1, stored(&["b"]))]
            ),
            ["c"]
        );
        // A package that does not define the value set does not answer for it, even when it was
        // installed last: the definition in use is package 1's.
        assert_eq!(
            read(
                &[(1, defined(None))],
                &[(1, stored(&["b"])), (2, stored(&["c"]))]
            ),
            ["b"]
        );
        // With no definition at all, the expansion stored last is what there is.
        assert_eq!(
            read(&[], &[(1, stored(&["b"])), (2, stored(&["c"]))]),
            ["c"]
        );
    }

    // Terminology that is read when used ---------------------------------------------------

    use std::collections::{BTreeMap as Rows, BTreeSet as Names};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    use std::sync::{Arc, Mutex};

    use super::{code_system_row_key, Scope, TermSource, TerminologyBuilder};

    /// The terminology rows of a store, and how many were read from it.
    #[derive(Default)]
    struct Shelf {
        resources: Mutex<Rows<(Scope, String), serde_json::Value>>,
        expansions: Mutex<Rows<(Scope, String), serde_json::Value>>,
        reads: AtomicUsize,
    }

    impl TermSource for Shelf {
        fn resource(&self, package: Scope, key: &str) -> Option<serde_json::Value> {
            self.reads.fetch_add(1, Relaxed);
            self.resources
                .lock()
                .unwrap()
                .get(&(package, key.to_string()))
                .cloned()
        }

        fn expansion(&self, package: Scope, key: &str) -> Option<serde_json::Value> {
            self.reads.fetch_add(1, Relaxed);
            self.expansions
                .lock()
                .unwrap()
                .get(&(package, key.to_string()))
                .cloned()
        }
    }

    /// The key a resource is stored under: a code system by its URL and version.
    fn row_key(resource: &serde_json::Value) -> String {
        if resource["resourceType"] == "CodeSystem" {
            code_system_row_key(resource).unwrap()
        } else {
            url_of(resource)
        }
    }

    /// Terminology that holds an index and reads the rest from a store holding `resources` and
    /// `expansions`, each with the package it is stored for.
    fn lazily(
        resources: &[(Scope, serde_json::Value)],
        expansions: &[(Scope, serde_json::Value)],
        release: Option<&str>,
    ) -> (MemoryTerminology, Arc<Shelf>) {
        let shelf = Arc::new(Shelf::default());
        let mut builder = TerminologyBuilder::for_source(release, shelf.clone());
        for (scope, resource) in resources {
            let key = row_key(resource);
            shelf
                .resources
                .lock()
                .unwrap()
                .insert((*scope, key.clone()), resource.clone());
            builder.add_stored_resource(*scope, &key, resource.clone());
        }
        for (scope, document) in expansions {
            let key = url_of(document);
            shelf
                .expansions
                .lock()
                .unwrap()
                .insert((*scope, key.clone()), document.clone());
            builder.add_stored_expansion(*scope, &key, document);
        }
        (builder.finish(), shelf)
    }

    /// The same terminology, every resource and expansion held.
    fn held(
        resources: &[(Scope, serde_json::Value)],
        expansions: &[(Scope, serde_json::Value)],
        release: Option<&str>,
    ) -> MemoryTerminology {
        let resources: Vec<(Scope, &serde_json::Value)> = resources
            .iter()
            .map(|(scope, item)| (*scope, item))
            .collect();
        let expansions: Vec<(Scope, &serde_json::Value)> = expansions
            .iter()
            .map(|(scope, item)| (*scope, item))
            .collect();
        MemoryTerminology::from_stored_scoped(&resources, &expansions, release)
    }

    /// What installing the packages stores: the expansion of every value set, for the package
    /// whose definition of it is in use.
    fn stored_expansions(
        resources: &[(Scope, serde_json::Value)],
        release: Option<&str>,
    ) -> Vec<(Scope, serde_json::Value)> {
        let tagged: Vec<(Scope, &serde_json::Value)> = resources
            .iter()
            .map(|(scope, item)| (*scope, item))
            .collect();
        let owners = super::winning_scopes(&tagged, release);
        MemoryTerminology::expansion_documents_scoped(&tagged, release)
            .into_iter()
            .filter_map(|document| {
                let owner = *owners.get(document["url"].as_str()?)?;
                Some((owner, document))
            })
            .collect()
    }

    /// Every question terminology answers, asked of both, and the answers compared.
    fn assert_alike(held: &MemoryTerminology, lazy: &MemoryTerminology) {
        let reasons = |terms: &MemoryTerminology| -> Vec<(String, Vec<String>)> {
            terms
                .unexpandable()
                .into_iter()
                .map(|(url, failures)| (url, messages(&failures)))
                .collect()
        };
        assert_eq!(held.documents(), lazy.documents(), "expansions differ");
        assert_eq!(reasons(held), reasons(lazy), "reasons differ");
        assert_eq!(held.unexpandable_urls(), lazy.unexpandable_urls());
        let mut urls: Names<String> = held
            .expansions
            .keys()
            .chain(held.unexpanded.keys())
            .cloned()
            .collect();
        urls.insert("http://example.org/not-loaded".to_string());
        let systems: Vec<(String, Vec<String>)> = held
            .systems
            .by_url
            .iter()
            .map(|(url, versions)| (url.clone(), versions.keys().cloned().collect()))
            .collect();
        let mut codes: Names<String> = held
            .systems
            .by_url
            .values()
            .flat_map(|versions| versions.values().flatten())
            .flat_map(|variant| {
                variant
                    .index
                    .iter()
                    .flat_map(|index| index.codes.keys().cloned())
            })
            .collect();
        codes.insert("no-such-code".to_string());
        for url in &urls {
            assert_eq!(held.expand(url, None), lazy.expand(url, None), "{url}");
            assert_eq!(
                held.expand_page(url, Some("a"), 0, Some(2)),
                lazy.expand_page(url, Some("a"), 0, Some(2)),
                "{url}"
            );
            assert_eq!(
                held.why_not_expanded(url)
                    .map(|failures| messages(&failures)),
                lazy.why_not_expanded(url)
                    .map(|failures| messages(&failures)),
                "{url}"
            );
            assert_eq!(
                held.expansion_problem(url),
                lazy.expansion_problem(url),
                "{url}"
            );
            for code in &codes {
                assert_eq!(
                    held.contains(url, None, code),
                    lazy.contains(url, None, code)
                );
                assert_eq!(
                    held.describe_bound(url, None, code),
                    lazy.describe_bound(url, None, code),
                    "{url} {code}"
                );
                for (system, _) in &systems {
                    assert_eq!(
                        held.contains(url, Some(system), code),
                        lazy.contains(url, Some(system), code),
                        "{url} {system} {code}"
                    );
                    assert_eq!(
                        held.describe_bound(url, Some(system), code),
                        lazy.describe_bound(url, Some(system), code),
                        "{url} {system} {code}"
                    );
                    assert_eq!(
                        held.validate_code(Some(url), Some(system), None, code, None),
                        lazy.validate_code(Some(url), Some(system), None, code, None),
                        "{url} {system} {code}"
                    );
                }
            }
        }
        for (system, versions) in &systems {
            for code in &codes {
                assert_eq!(held.describe(system, code), lazy.describe(system, code));
                assert_eq!(
                    held.validate_code(None, Some(system), None, code, Some("display")),
                    lazy.validate_code(None, Some(system), None, code, Some("display")),
                    "{system} {code}"
                );
                assert_eq!(
                    held.lookup(system, None, code),
                    lazy.lookup(system, None, code),
                    "{system} {code}"
                );
                for version in versions {
                    assert_eq!(
                        held.lookup(system, Some(version), code),
                        lazy.lookup(system, Some(version), code),
                        "{system} {version} {code}"
                    );
                    assert_eq!(
                        held.validate_code(None, Some(system), Some(version), code, None),
                        lazy.validate_code(None, Some(system), Some(version), code, None),
                        "{system} {version} {code}"
                    );
                }
                for other in &codes {
                    assert_eq!(
                        held.subsumes(system, None, other, code),
                        lazy.subsumes(system, None, other, code),
                        "{system} {other} {code}"
                    );
                }
            }
            assert_eq!(
                held.validate_code(None, Some(system), Some("9.9.9"), "a", None),
                lazy.validate_code(None, Some(system), Some("9.9.9"), "a", None)
            );
        }
        for map in &held.maps {
            for (system, code) in map
                .sources
                .iter()
                .flat_map(|system| codes.iter().map(move |code| (system.clone(), code.clone())))
            {
                for map_url in [None, Some(map.url.as_str())] {
                    assert_eq!(
                        held.translate(map_url, &system, &code, None),
                        lazy.translate(map_url, &system, &code, None),
                        "{system} {code}"
                    );
                }
                for system in [None, Some(system.as_str())] {
                    assert_eq!(
                        held.translate_coding(&map.url, system, &code),
                        lazy.translate_coding(&map.url, system, &code),
                        "{system:?} {code}"
                    );
                }
            }
        }
    }

    /// The resources of some packages, each with its package, and the release they are read for.
    type Scenario = (Vec<(Scope, serde_json::Value)>, Option<&'static str>);

    /// Terminology of the kinds the rest of these tests build, each as the packages that make it.
    fn lazy_scenarios() -> Vec<Scenario> {
        let tree = "http://example.org/tree";
        let mut scenarios: Vec<Scenario> = Vec::new();
        scenarios.push((catalog().into_iter().map(|item| (0, item)).collect(), None));
        let (r4, copy) = r4_and_its_r5_style_copy();
        scenarios.push((together(&[&r4, &copy]), Some("4.0.1")));
        scenarios.push((together(&[&r4, &copy]), None));
        scenarios.push((together(&[&copy, &r4]), Some("4.0.1")));
        let own = package(
            1,
            vec![
                versioned_system(SYS, "1", &["a", "b"]),
                value_set(SET, json!({"include": [{"system": SYS}]})),
            ],
        );
        let newer = package(2, vec![versioned_system(SYS, "2", &["c"])]);
        scenarios.push((together(&[&own, &newer]), None));
        scenarios.push((together(&[&newer, &own]), Some("1")));
        // Two packages that hold one version of a system, with the same content and with not.
        let same = package(2, vec![versioned_system(SYS, "1", &["a", "b"])]);
        scenarios.push((together(&[&own, &same]), None));
        let differ = package(3, vec![versioned_system(SYS, "1", &["a", "z"])]);
        scenarios.push((together(&[&own, &same, &differ]), Some("1")));
        // A hierarchy and the filters over it, a value set with its expansion inside it, a cycle,
        // a filter that is not supported, and concept maps two packages hold.
        let hierarchy = package(
            1,
            vec![
                json!({"resourceType": "CodeSystem", "url": tree, "content": "complete",
                       "name": "Tree",
                       "concept": [{"code": "root", "display": "Root", "concept": [
                           {"code": "leaf", "display": "Leaf"},
                           {"code": "twig", "concept": [{"code": "bud"}]}]}]}),
                value_set(
                    "http://example.org/under-twig",
                    json!({"include": [{"system": tree, "filter": [
                        {"property": "concept", "op": "is-a", "value": "twig"}]}]}),
                ),
                json!({"resourceType": "ValueSet", "url": "http://example.org/inline",
                       "expansion": {"contains": [
                           {"system": tree, "code": "leaf", "display": "Leaf"}]}}),
                value_set(
                    "http://example.org/c1",
                    json!({"include": [{"valueSet": ["http://example.org/c2"]}]}),
                ),
                value_set(
                    "http://example.org/c2",
                    json!({"include": [{"valueSet": ["http://example.org/c1"]}]}),
                ),
                value_set(
                    "http://example.org/odd",
                    json!({"include": [{"system": tree, "filter": [
                        {"property": "p", "op": "regex", "value": "x"}]}]}),
                ),
                json!({"resourceType": "ConceptMap", "url": "http://example.org/map",
                       "group": [{"source": tree, "target": "http://example.org/dst",
                                  "element": [{"code": "leaf", "target": [
                                      {"code": "x", "equivalence": "equivalent"},
                                      {"code": "y", "equivalence": "wider"}]}]}]}),
            ],
        );
        let again = package(
            2,
            vec![
                json!({"resourceType": "ConceptMap", "url": "http://example.org/map",
                        "group": [{"source": tree, "target": "http://example.org/dst",
                                   "element": [{"code": "leaf", "target": [
                                       {"code": "z", "equivalence": "equivalent"}]}]}]}),
            ],
        );
        scenarios.push((together(&[&hierarchy, &again]), None));
        scenarios
    }

    #[test]
    fn terminology_read_when_used_answers_what_held_terminology_answers() {
        for (resources, release) in lazy_scenarios() {
            let stored = stored_expansions(&resources, release);
            let held = held(&resources, &stored, release);
            let (mut lazy, shelf) = lazily(&resources, &stored, release);
            // Building it reads no row: what it needs is handed to it.
            assert_eq!(shelf.reads.load(Relaxed), 0);
            assert_alike(&held, &lazy);
            assert_alike(&held, &lazy);
            // With caches that keep almost nothing, everything is read again each time.
            lazy.squeeze();
            assert_alike(&held, &lazy);
            assert_alike(&held, &lazy);
        }
    }

    #[test]
    fn a_question_reads_only_what_it_needs() {
        let (r4, _) = r4_and_its_r5_style_copy();
        let extra = package(
            1,
            vec![
                versioned_system("http://example.org/other", "1", &["q"]),
                value_set(
                    "http://example.org/other-set",
                    json!({"include": [{"system": "http://example.org/other"}]}),
                ),
            ],
        );
        let resources = together(&[&r4, &extra]);
        let stored = stored_expansions(&resources, Some("4.0.1"));
        let (lazy, shelf) = lazily(&resources, &stored, Some("4.0.1"));
        let reads = || shelf.reads.load(Relaxed);
        // Membership reads that value set's expansion, once.
        assert_eq!(lazy.contains(SET, None, "a"), TermAnswer::Yes);
        assert_eq!(reads(), 1);
        assert_eq!(lazy.contains(SET, None, "b"), TermAnswer::Yes);
        assert_eq!(reads(), 1);
        // Which version of a system an expansion was made from is known without reading it, so
        // describing a code reads the system and nothing more.
        assert!(lazy
            .describe_bound(SET, Some(R4_SYSTEM), "a")
            .is_some_and(|described| described.display.is_none()));
        assert_eq!(reads(), 2);
        // A version that is not installed is refused without reading the system.
        let refused = lazy.validate_code(
            Some(SET),
            Some("http://example.org/other"),
            Some("9"),
            "q",
            None,
        );
        assert_eq!(refused["issue"][0]["code"], "not-found");
        assert_eq!(reads(), 2);
        // A value set no question was asked about is not read.
        assert_eq!(
            lazy.contains("http://example.org/other-set", None, "q"),
            TermAnswer::Yes
        );
        assert_eq!(reads(), 3);
        // Nor is the reason a value set has no expansion, until it is asked for.
        assert_eq!(lazy.unexpandable_urls(), Vec::<String>::new());
        assert_eq!(reads(), 3);
    }

    #[test]
    fn packages_that_hold_the_same_system_share_one_copy_read_when_used_as_well() {
        let own = package(1, vec![versioned_system(SYS, "1", &["a", "b"])]);
        let same = package(2, vec![versioned_system(SYS, "1", &["a", "b"])]);
        let differ = package(3, vec![versioned_system(SYS, "1", &["a", "z"])]);
        let resources = together(&[&own, &same, &differ]);
        let (lazy, _) = lazily(&resources, &[], None);
        let variants = &lazy.systems.by_url[SYS]["1"];
        assert_eq!(variants.len(), 2);
        let holders: Vec<Vec<Scope>> = variants
            .iter()
            .map(|variant| variant.holders.keys().copied().collect())
            .collect();
        assert!(holders.contains(&vec![1, 2]) && holders.contains(&vec![3]));
        // Held and read-when-used terminology agree on which package gets which content.
        let held = held(&resources, &[], None);
        for scope in [1, 2, 3] {
            let by_scope = |terms: &MemoryTerminology| {
                terms
                    .systems
                    .resolve_in(Some(scope), false, SYS, Some("1"))
                    .map(|(_, index)| index.codes.keys().cloned().collect::<Vec<_>>())
            };
            assert_eq!(by_scope(&held), by_scope(&lazy), "package {scope}");
        }
    }

    #[test]
    fn a_copy_of_terminology_reads_from_the_same_store_with_caches_of_its_own() {
        let (r4, _) = r4_and_its_r5_style_copy();
        let stored = stored_expansions(&r4, None);
        let (lazy, shelf) = lazily(&r4, &stored, None);
        assert_eq!(lazy.contains(SET, None, "a"), TermAnswer::Yes);
        assert_eq!(shelf.reads.load(Relaxed), 1);
        let copy = lazy.clone();
        assert_eq!(copy.contains(SET, None, "a"), TermAnswer::Yes);
        assert_eq!(shelf.reads.load(Relaxed), 2);
        assert_eq!(copy.documents(), lazy.documents());
    }

    #[test]
    fn terminology_that_holds_what_a_request_needs_answers_as_before() {
        for (resources, release) in lazy_scenarios() {
            let stored = stored_expansions(&resources, release);
            let held = held(&resources, &stored, release);
            let (mut lazy, shelf) = lazily(&resources, &stored, release);
            let every: Names<String> = resources
                .iter()
                .filter(|(_, item)| item["resourceType"] == "ValueSet")
                .map(|(_, item)| url_of(item))
                .collect();
            lazy.hold(&every);
            assert_alike(&held, &lazy);
            // The same, with everything read again each time except what is held.
            lazy.squeeze();
            assert_alike(&held, &lazy);
            assert!(shelf.reads.load(Relaxed) > 0);
        }
    }

    #[test]
    fn what_is_held_is_read_once_and_a_request_for_it_reads_nothing() {
        let (r4, _) = r4_and_its_r5_style_copy();
        let extra = package(
            1,
            vec![
                versioned_system("http://example.org/other", "1", &["q"]),
                value_set(
                    "http://example.org/other-set",
                    json!({"include": [{"system": "http://example.org/other"}]}),
                ),
            ],
        );
        let resources = together(&[&r4, &extra]);
        let stored = stored_expansions(&resources, Some("4.0.1"));
        let (mut lazy, shelf) = lazily(&resources, &stored, Some("4.0.1"));
        let reads = || shelf.reads.load(Relaxed);
        lazy.hold(&Names::from([
            SET.to_string(),
            "http://example.org/not-a-value-set".to_string(),
        ]));
        // The expansion, and the one version of its code system, were each read once.
        assert_eq!(reads(), 2);
        let after_hold = reads();
        // Checking a code, describing it from the version the set was made from, and describing
        // it as the latest of its system: nothing is read.
        assert_eq!(lazy.contains(SET, None, "a"), TermAnswer::Yes);
        assert_eq!(lazy.contains(SET, Some(R4_SYSTEM), "b"), TermAnswer::Yes);
        assert!(lazy.describe_bound(SET, Some(R4_SYSTEM), "a").is_some());
        assert!(lazy.describe(R4_SYSTEM, "a").is_some());
        assert_eq!(reads(), after_hold);
        // What was not held is read when used, as before.
        assert_eq!(
            lazy.contains("http://example.org/other-set", None, "q"),
            TermAnswer::Yes
        );
        assert_eq!(reads(), after_hold + 1);
        // Terminology that holds everything has nothing to hold, and a hold does nothing to it.
        let mut everything = held(&resources, &stored, Some("4.0.1"));
        let before = everything.documents();
        everything.hold(&Names::from([SET.to_string()]));
        assert_eq!(everything.documents(), before);
        // A copy that is made reads what it is asked, and keeps what was held.
        let copy = lazy.clone();
        assert_eq!(copy.contains(SET, None, "a"), TermAnswer::Yes);
        assert_eq!(reads(), after_hold + 1);
    }

    #[test]
    fn what_a_terminology_read_lately_a_replacing_one_reads_at_once() {
        let (r4, _) = r4_and_its_r5_style_copy();
        let stored = stored_expansions(&r4, None);
        let (old, shelf) = lazily(&r4, &stored, None);
        assert_eq!(old.contains(SET, None, "a"), TermAnswer::Yes);
        assert!(old.describe_bound(SET, Some(R4_SYSTEM), "a").is_some());
        let reads = shelf.reads.load(Relaxed);
        assert!(reads >= 2);
        // The one that replaces it is built from the same store, and knows nothing of what was read.
        let (new, new_shelf) = lazily(&r4, &stored, None);
        assert_eq!(new_shelf.reads.load(Relaxed), 0);
        new.warm(&old.resident());
        assert_eq!(new_shelf.reads.load(Relaxed), reads);
        // What was read is there: the same questions read nothing more.
        assert_eq!(new.contains(SET, None, "a"), TermAnswer::Yes);
        assert!(new.describe_bound(SET, Some(R4_SYSTEM), "a").is_some());
        assert_eq!(new_shelf.reads.load(Relaxed), reads);
        // A row that is no longer in the store is passed over.
        let (empty, empty_shelf) = lazily(&[], &[], None);
        empty.warm(&old.resident());
        assert_eq!(empty_shelf.reads.load(Relaxed), reads);
        // Terminology that holds everything has nothing to warm from or to.
        let everything = held(&r4, &stored, None);
        new.warm(&everything.resident());
        assert_eq!(new_shelf.reads.load(Relaxed), reads);
    }
}
