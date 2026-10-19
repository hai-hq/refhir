//! Fetch a FHIR NPM package and install it the same way a local directory is
//! installed. Network I/O stays outside the OCC transaction.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock, TryLockError};
use std::time::Duration;

use flate2::read::GzDecoder;
use refhir_database::{Database, ReadSet};
use refhir_persistence::ConformanceStore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::catalog::Catalog;
use crate::error::FhirError;
use crate::package::{
    finish_install, newer_version, note_newer_installed, package_identity, package_root,
    read_manifest, run_backfills, stage_package_deferred,
};
use crate::run::RunResult;
use crate::s3::S3Source;

const ENGINE: &str = "refhir_engine";
const DEFAULT_REGISTRY: &str = "https://packages.fhir.org";
const MAX_TARBALL: u64 = 100 * 1024 * 1024;
const MAX_UNCOMPRESSED: u64 = 512 * 1024 * 1024;
const MAX_ENTRIES: u64 = 20_000;
const COMPLETE: &str = ".complete";

fn install_lock() -> &'static Mutex<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    &LOCK
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageSpec {
    Dir(PathBuf),
    Tarball(PathBuf),
    Npm { name: String, version: String },
}

/// `Sync` because the packages one round of a dependency walk needs are fetched side by side.
pub trait PackageFetcher: Sync {
    fn fetch(&self, name: &str, version: &str, dest: &Path) -> Result<(), FhirError>;
}

/// Where packages are downloaded from: a FHIR package registry (`https://packages.fhir.org`),
/// any static host that serves `<base>/<name>/<version>`, or `s3://bucket/prefix`.
#[derive(Clone, Debug)]
pub struct NpmRegistry {
    pub base_url: String,
    pub cache_dir: PathBuf,
    s3: Option<S3Source>,
    /// Whether this registry may be reached at all. A server does not download packages unless
    /// it was told it may (see [`package_downloads_allowed`]).
    allow_download: bool,
}

/// The environment variable that lets a server download packages. `[conformance].package_download`
/// sets it.
pub const DOWNLOAD_VARIABLE: &str = "REFHIR_PACKAGE_DOWNLOAD";

/// Whether this server was told it may download packages. It may not unless it was: packages come
/// from a file prepared ahead of time (see `prepared.rs`), or from a directory or a `.tgz`.
pub fn package_downloads_allowed() -> bool {
    std::env::var(DOWNLOAD_VARIABLE)
        .ok()
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// What is said when a package would have to be downloaded and the server may not.
fn download_refused(name: &str, version: &str) -> FhirError {
    FhirError::Forbidden(format!(
        "{name}@{version} is not installed, and this server does not download packages. \
         Set [conformance].package_download = true (or {DOWNLOAD_VARIABLE}=true) to let it, \
         or install the package from a directory or a .tgz"
    ))
}

impl NpmRegistry {
    /// `REFHIR_PACKAGE_REGISTRY` (default packages.fhir.org) and `REFHIR_PACKAGE_CACHE`. It
    /// downloads only if the server may (see [`package_downloads_allowed`]); a package that is
    /// already in the cache is used either way.
    pub fn from_env() -> Result<Self, FhirError> {
        let base_url = std::env::var("REFHIR_PACKAGE_REGISTRY")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_REGISTRY.to_string());
        let cache_dir = std::env::var("REFHIR_PACKAGE_CACHE")
            .ok()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".refhir/packages"));
        Ok(Self::new(&base_url, cache_dir)?.allowing_download(package_downloads_allowed()))
    }

    /// A registry that downloads: building one says the caller wants to reach it.
    pub fn new(location: &str, cache_dir: PathBuf) -> Result<Self, FhirError> {
        let s3 = S3Source::from_location(location).transpose()?;
        Ok(Self {
            base_url: location.trim_end_matches('/').to_string(),
            cache_dir,
            s3,
            allow_download: true,
        })
    }

    pub fn allowing_download(mut self, allow: bool) -> Self {
        self.allow_download = allow;
        self
    }
}

impl PackageFetcher for NpmRegistry {
    fn fetch(&self, name: &str, version: &str, dest: &Path) -> Result<(), FhirError> {
        if !self.allow_download {
            return Err(download_refused(name, version));
        }
        let bytes = match &self.s3 {
            Some(s3) => {
                let object = s3.object(name, version);
                tracing::info!(target: ENGINE, name, version, url = object.url.as_str(), "downloading FHIR package from S3");
                download_tarball(&object.url, &|| s3.headers(&object, unix_now()))?
            }
            None => {
                let url = format!("{}/{name}/{version}", self.base_url.trim_end_matches('/'));
                tracing::info!(target: ENGINE, name, version, url = url.as_str(), "downloading FHIR package");
                download_tarball(&url, &Vec::new)?
            }
        };
        extract_tarball(&bytes, dest)?;
        write_complete(dest, &sha256_hex(&bytes))
    }
}

/// Install from a directory, `.tgz`, or `name@version` / `name#version`.
pub fn install_package(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    spec: &str,
) -> Result<(), FhirError> {
    let registry = NpmRegistry::from_env()?;
    install_with(store, db, catalog, spec, &registry, &registry.cache_dir)
}

pub fn install_with(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    spec: &str,
    fetcher: &dyn PackageFetcher,
    cache: &Path,
) -> Result<(), FhirError> {
    let parsed = parse_package_spec(spec)?;
    install_tree(store, db, catalog, parsed, fetcher, cache)?;
    Ok(())
}

/// Whether every `name@version` spec is in the database, or has been replaced there by a newer
/// version of the package, which is what installing it would leave.
pub fn packages_installed(store: &dyn ConformanceStore, specs: &[&str]) -> Result<bool, FhirError> {
    for spec in specs {
        match parse_package_spec(spec)? {
            PackageSpec::Npm { name, version } => {
                if !already_installed(store, &name, &version)?
                    && newer_installed(store, &name, &version)?.is_none()
                {
                    return Ok(false);
                }
            }
            _ => return Ok(false),
        }
    }
    Ok(true)
}

pub fn install_operation(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    args: &Value,
) -> Result<RunResult, FhirError> {
    let _guard = match install_lock().try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => {
            return Err(FhirError::Conflict(
                "another package install is running".into(),
            ));
        }
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
    };
    let (name, version) = npm_spec_from_args(args)?;
    let spec = format!("{name}@{version}");
    let fetcher = NpmRegistry::from_env()?;
    let before = unexpandable_value_sets(catalog);
    let newer = newer_installed(store, &name, &version)?;
    install_with(store, db, catalog, &spec, &fetcher, &fetcher.cache_dir)?;
    let mut issues = vec![json!({
        "severity": "information",
        "code": "informational",
        "diagnostics": match &newer {
            Some(newer) => format!(
                "{spec} was not installed: {name}@{newer}, a newer version of it, is installed"
            ),
            None => format!("installed {spec}"),
        }
    })];
    // A package can bring a ValueSet that includes content nothing here can supply. Say so now,
    // rather than when a client first asks for the expansion.
    let introduced: Vec<String> = unexpandable_value_sets(catalog)
        .difference(&before)
        .cloned()
        .collect();
    if !introduced.is_empty() {
        let listed: Vec<&str> = introduced.iter().take(20).map(String::as_str).collect();
        issues.push(json!({
            "severity": "warning",
            "code": "not-supported",
            "diagnostics": format!(
                "{} value set(s) from {spec} cannot be expanded: {}{}. Ask ValueSet/$expandability for the reasons.",
                introduced.len(),
                listed.join(", "),
                if introduced.len() > listed.len() { ", ..." } else { "" }
            )
        }));
    }
    Ok(RunResult {
        value: json!({"resourceType": "OperationOutcome", "issue": issues}),
        ts: db.latest_ts(),
        reads: ReadSet::default(),
        wrote: false,
        writes: Vec::new(),
        status: 200,
    })
}

/// URLs of the loaded value sets that have no expansion.
pub fn unexpandable_value_sets(catalog: &RwLock<Catalog>) -> std::collections::BTreeSet<String> {
    catalog
        .read()
        .expect("catalog")
        .terms()
        .unexpandable_urls()
        .into_iter()
        .collect()
}

pub fn parse_package_spec(text: &str) -> Result<PackageSpec, FhirError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(FhirError::Invalid("package is required".into()));
    }
    let path = PathBuf::from(text);
    if is_tarball(text) {
        return Ok(PackageSpec::Tarball(path));
    }
    if path.is_dir() {
        return Ok(PackageSpec::Dir(path));
    }
    if let Some((name, version)) = split_npm(text) {
        return Ok(PackageSpec::Npm { name, version });
    }
    if looks_like_path(text) {
        return Ok(PackageSpec::Dir(path));
    }
    Err(FhirError::Invalid(format!(
        "package {text} needs a version (name@version) or a local directory"
    )))
}

fn npm_spec_from_args(args: &Value) -> Result<(String, String), FhirError> {
    if optional(args, &["registry"]).is_some() {
        return Err(FhirError::Invalid(
            "$install does not accept a registry; set REFHIR_PACKAGE_REGISTRY".into(),
        ));
    }
    let (name, version) = if let Some(package) = optional(args, &["package", "npm"]) {
        split_npm(package).ok_or_else(|| {
            FhirError::Invalid(
                "$install only accepts name@version from the package registry".into(),
            )
        })?
    } else {
        match (optional(args, &["name"]), optional(args, &["version"])) {
            (Some(name), Some(version)) => (name.to_string(), version.to_string()),
            (Some(_), None) => return Err(FhirError::Invalid("version is required".into())),
            (None, Some(_)) => return Err(FhirError::Invalid("name is required".into())),
            (None, None) => {
                return Err(FhirError::Invalid(
                    "package or name and version is required".into(),
                ));
            }
        }
    };
    if looks_like_path(&name) || looks_like_path(&version) {
        return Err(FhirError::Invalid(
            "$install only accepts name@version from the package registry".into(),
        ));
    }
    Ok((safe_segment(&name)?, safe_segment(&version)?))
}

/// Install `spec` and everything it depends on, one version per package name.
///
/// The whole tree is resolved before anything is installed, so a conflict stops the install
/// with nothing half-applied. See [`Resolver`] for the rules.
///
/// A package that is installed already, or of which a newer version is, has nothing to install:
/// the newest version of a package is the one in force, and a server started again on an older
/// release of its package stays on what it has. Installing a newer version replaces the older ones
/// (see `ConformanceStore::install_package`).
fn install_tree(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    spec: PackageSpec,
    fetcher: &dyn PackageFetcher,
    cache: &Path,
) -> Result<(), FhirError> {
    if let PackageSpec::Npm { name, version } = &spec {
        if nothing_to_install(store, name, version)? {
            return run_backfills(store, db, catalog);
        }
    }
    let root = materialize(&spec, fetcher, cache)?;
    let (name, version) = package_identity(&root.dir)?;
    if nothing_to_install(store, &name, &version)? {
        return run_backfills(store, db, catalog);
    }
    let installed = store
        .load_packages()
        .map_err(FhirError::from)?
        .into_iter()
        .map(|package| (package.name, package.version));
    let resolving = std::time::Instant::now();
    let order = Resolver::new(installed, fetcher, cache).resolve(root, name, version)?;
    // Fetching and unpacking the tree is most of the time before the first package is stored.
    tracing::info!(
        target: ENGINE,
        "fetched {} package(s) to install in {:.1?}",
        order.len(),
        resolving.elapsed()
    );
    // Store every package first and rebuild the catalog once at the end: the rebuild grows
    // with the catalog, so doing it after each package of a large tree costs minutes.
    let mut staged = false;
    let mut pending = Vec::new();
    let mut result = Ok(());
    for node in order {
        if already_installed(store, &node.name, &node.version)? {
            continue;
        }
        staged = true;
        match stage_package_deferred(store, db, catalog, &node.staged.dir) {
            Ok(backfill) => pending.extend(backfill),
            Err(err) => {
                result = Err(err);
                break;
            }
        }
    }
    if !staged {
        // Nothing new, but an earlier attempt may have left backfills behind.
        return run_backfills(store, db, catalog);
    }
    finish_install(store, db, catalog, &pending, result)
}

/// A package that was downloaded (or read from the cache) during resolution.
pub(crate) struct Node {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) staged: Staged,
    deps: Vec<(String, String)>,
}

/// More rounds than any real package graph needs; reaching it means versions keep flipping.
const MAX_ROUNDS: usize = 64;

/// Packages fetched at once. Each is unpacked from memory to its own directory, so this is as
/// many cores as the unpacking can use, and as many tarballs as are held at the same time.
const PREFETCH_THREADS: usize = 4;

/// Picks one version per package name across a dependency tree.
///
/// - The highest version that a still-needed package asks for wins, like npm's dedupe. Each
///   round is recomputed from the packages reachable under the current choices, so requests
///   made by a version that was later replaced do not count.
/// - A version named by the caller is fixed: a dependency that needs something newer is an
///   error.
/// - An installed version at least as new as the request is used as is. A newer request replaces
///   the installed one (see `ConformanceStore::install_package`).
/// - Versions that are not `major.minor.patch` still order (see [`compare_versions`]).
pub(crate) struct Resolver<'a> {
    fetcher: &'a dyn PackageFetcher,
    cache: &'a Path,
    chosen: BTreeMap<String, String>,
    /// Highest installed version per package name.
    installed_latest: BTreeMap<String, String>,
    /// Installed (name, version) pairs, so their dependencies are not walked again.
    installed: BTreeSet<(String, String)>,
    /// Names the caller asked for.
    requested: BTreeSet<String>,
    nodes: BTreeMap<(String, String), Node>,
    /// Dependencies that are not installed, because they add nothing a server uses (see
    /// [`is_examples_package`]).
    skipped: BTreeSet<String>,
}

/// The examples packages of the FHIR releases: each holds the example resources of a release
/// (`hl7.fhir.r4.examples`) and, because it is built from the same source, a second copy of every
/// definition the core package has. A server loads neither, and a package such as SDC lists one as
/// a dependency, so it is left out when it is only a dependency. Asking for one by name still
/// installs it. The names are listed rather than matched by pattern, so that an implementation
/// guide's own package of examples, which may hold profiles it needs, is never dropped.
const EXAMPLES_PACKAGES: [&str; 3] = [
    "hl7.fhir.r4.examples",
    "hl7.fhir.r4b.examples",
    "hl7.fhir.r5.examples",
];

fn is_examples_package(name: &str) -> bool {
    EXAMPLES_PACKAGES.contains(&name)
}

impl<'a> Resolver<'a> {
    /// A resolver for what is already installed, as (name, version) pairs.
    pub(crate) fn new(
        installed_packages: impl IntoIterator<Item = (String, String)>,
        fetcher: &'a dyn PackageFetcher,
        cache: &'a Path,
    ) -> Self {
        let mut installed_latest: BTreeMap<String, String> = BTreeMap::new();
        let mut installed = BTreeSet::new();
        for (name, version) in installed_packages {
            installed.insert((name.clone(), version.clone()));
            let newer = installed_latest
                .get(&name)
                .is_none_or(|current| compare_versions(&version, current) == Ordering::Greater);
            if newer {
                installed_latest.insert(name, version);
            }
        }
        Self {
            fetcher,
            cache,
            chosen: BTreeMap::new(),
            installed_latest,
            installed,
            requested: BTreeSet::new(),
            nodes: BTreeMap::new(),
            skipped: BTreeSet::new(),
        }
    }

    pub(crate) fn resolve(
        mut self,
        root: Staged,
        name: String,
        version: String,
    ) -> Result<Vec<Node>, FhirError> {
        self.chosen.insert(name.clone(), version.clone());
        self.requested.insert(name.clone());
        let deps = dependencies_of(&root.dir)?;
        self.nodes.insert(
            (name.clone(), version.clone()),
            Node {
                name: name.clone(),
                version,
                staged: root,
                deps,
            },
        );
        for _ in 0..MAX_ROUNDS {
            let mut changed = false;
            let requests = self.best_requests(&name);
            self.prefetch(&requests);
            for (dep, (wanted, requester)) in requests {
                changed |= self.settle(&dep, &wanted, &requester)?;
            }
            if !changed {
                return self.finish(&name);
            }
        }
        Err(FhirError::Invalid(
            "package dependency versions do not settle on one version each".into(),
        ))
    }

    /// For every package reachable from `root` under the current choices, the highest version
    /// any reachable package asks for, and who asked.
    fn best_requests(&self, root: &str) -> BTreeMap<String, (String, String)> {
        let mut best: BTreeMap<String, (String, String)> = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let mut stack = vec![root.to_string()];
        while let Some(name) = stack.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(version) = self.chosen.get(&name) else {
                continue;
            };
            let Some(node) = self.nodes.get(&(name.clone(), version.clone())) else {
                continue;
            };
            for (dep, wanted) in &node.deps {
                let higher = best.get(dep).is_none_or(|(current, _)| {
                    compare_versions(wanted, current) == Ordering::Greater
                });
                if higher {
                    best.insert(
                        dep.clone(),
                        (wanted.clone(), format!("{name}@{}", node.version)),
                    );
                }
                stack.push(dep.clone());
            }
        }
        best
    }

    /// The version of `name` that a request of `wanted` settles on: the one asked for, unless a
    /// newer one is installed. `None` when the request is one that [`Resolver::settle`] refuses.
    fn target_for(&self, name: &str, wanted: &str) -> Option<String> {
        if self.requested.contains(name) {
            let current = self.chosen[name].clone();
            if compare_versions(wanted, &current) == Ordering::Greater {
                return None;
            }
            return Some(current);
        }
        Some(match self.installed_latest.get(name) {
            Some(latest) if compare_versions(latest, wanted) != Ordering::Less => latest.clone(),
            _ => wanted.to_string(),
        })
    }

    /// Fetch the packages this round will settle on that are not yet here, a few at a time. The
    /// time is in unpacking tens of thousands of files, and one package does not wait on
    /// another. The cache is how the results are handed on: [`Resolver::settle`] asks for each
    /// package as before and finds it complete. A package that cannot be fetched is left to
    /// `settle`, which tries again and reports the error.
    fn prefetch(&self, requests: &BTreeMap<String, (String, String)>) {
        let mut wanted: Vec<(String, String)> = Vec::new();
        for (name, (version, _)) in requests {
            let Some(target) = self.target_for(name, version) else {
                continue;
            };
            let key = (name.clone(), target);
            if !self.installed.contains(&key)
                && !self.nodes.contains_key(&key)
                && !wanted.contains(&key)
            {
                wanted.push(key);
            }
        }
        if wanted.len() < 2 {
            return;
        }
        let threads = wanted.len().min(PREFETCH_THREADS);
        let queue = std::sync::Mutex::new(wanted.into_iter());
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| loop {
                    let next = queue.lock().expect("fetch queue").next();
                    let Some((name, version)) = next else {
                        return;
                    };
                    let _ = materialize(
                        &PackageSpec::Npm { name, version },
                        self.fetcher,
                        self.cache,
                    );
                });
            }
        });
    }

    /// Choose the version of `name` for a request of `wanted`. True if the choice changed.
    fn settle(&mut self, name: &str, wanted: &str, requester: &str) -> Result<bool, FhirError> {
        let Some(target) = self.target_for(name, wanted) else {
            return Err(FhirError::Invalid(format!(
                "version conflict: {requester} needs {name}@{wanted}, but {name}@{} was requested",
                self.chosen[name]
            )));
        };
        if let Some(current) = self.chosen.get(name) {
            if compare_versions(current, &target) == Ordering::Equal {
                return Ok(false);
            }
        }
        self.chosen.insert(name.to_string(), target.clone());
        if !self.requested.contains(name) && is_examples_package(name) {
            tracing::info!(
                target: ENGINE,
                "not installing {name}@{target}, which {requester} lists: it holds examples and a copy of the core definitions"
            );
            self.skipped.insert(name.to_string());
            return Ok(true);
        }
        let key = (name.to_string(), target.clone());
        if !self.installed.contains(&key) && !self.nodes.contains_key(&key) {
            let staged = materialize(
                &PackageSpec::Npm {
                    name: name.to_string(),
                    version: target.clone(),
                },
                self.fetcher,
                self.cache,
            )?;
            let deps = dependencies_of(&staged.dir)?;
            self.nodes.insert(
                key,
                Node {
                    name: name.to_string(),
                    version: target,
                    staged,
                    deps,
                },
            );
        }
        Ok(true)
    }

    /// Log the overrides, then order the packages that will be installed.
    fn finish(mut self, root: &str) -> Result<Vec<Node>, FhirError> {
        for (dep, (wanted, requester)) in self.best_requests(root) {
            let chosen = &self.chosen[&dep];
            if compare_versions(&wanted, chosen) != Ordering::Equal {
                tracing::warn!(
                    target: ENGINE,
                    "{requester} wants {dep}@{wanted}; using {dep}@{chosen}"
                );
            }
        }
        let mut order = Vec::new();
        let mut visiting = BTreeSet::new();
        let mut done = BTreeSet::new();
        self.visit(root, &mut visiting, &mut done, &mut order)?;
        let mut nodes = std::mem::take(&mut self.nodes);
        Ok(order
            .into_iter()
            .filter_map(|key| nodes.remove(&key))
            .collect())
    }

    /// Depth-first over the chosen versions only, so packages reachable solely through an
    /// overridden version are dropped. Dependencies come before the packages that need them.
    fn visit(
        &self,
        name: &str,
        visiting: &mut BTreeSet<String>,
        done: &mut BTreeSet<String>,
        order: &mut Vec<(String, String)>,
    ) -> Result<(), FhirError> {
        if done.contains(name) {
            return Ok(());
        }
        if self.skipped.contains(name) {
            done.insert(name.to_string());
            return Ok(());
        }
        let version = self.chosen[name].clone();
        let key = (name.to_string(), version.clone());
        if self.installed.contains(&key) {
            done.insert(name.to_string());
            return Ok(());
        }
        if !visiting.insert(name.to_string()) {
            return Err(FhirError::Invalid(format!(
                "package {name}@{version} depends on itself"
            )));
        }
        for (dep, wanted) in &self.nodes[&key].deps {
            // A cycle that only exists because a dependency was swapped for the chosen version
            // (terminology 6.5.0 wanting extensions 5.2.0, extensions 5.3.0 wanting terminology
            // 6.5.0) is fine: skip that edge. A cycle in the versions as declared is an error.
            if visiting.contains(dep)
                && compare_versions(&self.chosen[dep], wanted) != Ordering::Equal
            {
                continue;
            }
            self.visit(dep, visiting, done, order)?;
        }
        visiting.remove(name);
        done.insert(name.to_string());
        order.push(key);
        Ok(())
    }
}

pub(crate) use refhir_persistence::compare_versions;

pub(crate) struct Staged {
    pub(crate) dir: PathBuf,
    _scratch: Option<PathBuf>,
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(path) = &self._scratch {
            let _ = fs::remove_dir_all(path);
        }
    }
}

pub(crate) fn materialize(
    spec: &PackageSpec,
    fetcher: &dyn PackageFetcher,
    cache: &Path,
) -> Result<Staged, FhirError> {
    match spec {
        PackageSpec::Dir(path) => Ok(Staged {
            dir: path.clone(),
            _scratch: None,
        }),
        PackageSpec::Tarball(path) => {
            let scratch = scratch_dir()?;
            let bytes = read_tarball(path)?;
            extract_tarball(&bytes, &scratch)?;
            Ok(Staged {
                dir: scratch.clone(),
                _scratch: Some(scratch),
            })
        }
        PackageSpec::Npm { name, version } => {
            let dest = cache.join(safe_segment(name)?).join(safe_segment(version)?);
            if cache_complete(&dest) {
                return Ok(Staged {
                    dir: dest,
                    _scratch: None,
                });
            }
            if dest.exists() {
                let _ = fs::remove_dir_all(&dest);
            }
            fs::create_dir_all(&dest).map_err(|err| {
                FhirError::Storage(format!("package cache {}: {err}", dest.display()))
            })?;
            match fetcher.fetch(name, version, &dest) {
                Ok(()) => {
                    if package_root(&dest).is_err() {
                        let _ = fs::remove_dir_all(&dest);
                        return Err(FhirError::Invalid(format!(
                            "downloaded {name}@{version} is not a FHIR package"
                        )));
                    }
                    if !dest.join(COMPLETE).is_file() {
                        write_complete(&dest, "ok")?;
                    }
                    Ok(Staged {
                        dir: dest,
                        _scratch: None,
                    })
                }
                Err(err) => {
                    let _ = fs::remove_dir_all(&dest);
                    Err(err)
                }
            }
        }
    }
}

fn dependencies_of(path: &Path) -> Result<Vec<(String, String)>, FhirError> {
    let root = package_root(path)?;
    let manifest = read_manifest(&root)?;
    let Some(deps) = manifest
        .get("dependencies")
        .and_then(|value| value.as_object())
    else {
        return Ok(Vec::new());
    };
    let mut rows = Vec::new();
    for (name, version) in deps {
        let version = version.as_str().ok_or_else(|| {
            FhirError::Invalid(format!("dependency {name} version must be a string"))
        })?;
        if !exact_version(version) {
            return Err(FhirError::Invalid(format!(
                "dependency {name} version {version} must be an exact version"
            )));
        }
        rows.push((name.clone(), version.to_string()));
    }
    Ok(rows)
}

/// The newest installed version of `name` when it is newer than `version`.
fn newer_installed(
    store: &dyn ConformanceStore,
    name: &str,
    version: &str,
) -> Result<Option<String>, FhirError> {
    let installed = store.load_packages().map_err(FhirError::from)?;
    Ok(newer_version(&installed, name, version).map(str::to_string))
}

/// Whether installing `name@version` has nothing to do: it is installed, or a newer version of the
/// package is, which says so in the log.
fn nothing_to_install(
    store: &dyn ConformanceStore,
    name: &str,
    version: &str,
) -> Result<bool, FhirError> {
    if already_installed(store, name, version)? {
        return Ok(true);
    }
    let Some(newer) = newer_installed(store, name, version)? else {
        return Ok(false);
    };
    note_newer_installed(name, version, &newer);
    Ok(true)
}

fn already_installed(
    store: &dyn ConformanceStore,
    name: &str,
    version: &str,
) -> Result<bool, FhirError> {
    Ok(store
        .load_packages()
        .map_err(FhirError::from)?
        .iter()
        .any(|package| package.name == name && package.version == version))
}

fn scratch_dir() -> Result<PathBuf, FhirError> {
    let path = std::env::temp_dir().join(format!("refhir-pkg-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&path)
        .map_err(|err| FhirError::Storage(format!("temp package dir: {err}")))?;
    Ok(path)
}

fn cache_complete(dest: &Path) -> bool {
    dest.join(COMPLETE).is_file() && package_root(dest).is_ok()
}

/// The SHA-256 of the tarball a package in the cache was downloaded from, when the cache recorded
/// it: a package from a directory, or one cached without a digest, has none.
pub(crate) fn source_digest(dir: &Path) -> Option<String> {
    let token = fs::read_to_string(dir.join(COMPLETE)).ok()?;
    let token = token.trim();
    (token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| token.to_string())
}

fn write_complete(dest: &Path, token: &str) -> Result<(), FhirError> {
    fs::write(dest.join(COMPLETE), token)
        .map_err(|err| FhirError::Storage(format!("package cache marker: {err}")))
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_tarball(path: &Path) -> Result<Vec<u8>, FhirError> {
    let file =
        File::open(path).map_err(|err| FhirError::Invalid(format!("{}: {err}", path.display())))?;
    let mut bytes = Vec::new();
    file.take(MAX_TARBALL + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| FhirError::Invalid(format!("{}: {err}", path.display())))?;
    if bytes.len() as u64 > MAX_TARBALL {
        return Err(FhirError::Invalid(format!(
            "{} is larger than {MAX_TARBALL} bytes",
            path.display()
        )));
    }
    Ok(bytes)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// `headers` is called for every attempt, so a signed request is never older than one try.
fn download_tarball(
    url: &str,
    headers: &dyn Fn() -> Vec<(String, String)>,
) -> Result<Vec<u8>, FhirError> {
    let mut last = FhirError::Storage(format!("download {url} failed"));
    for _ in 0..3 {
        match download_once(url, &headers()) {
            Ok(bytes) => return Ok(bytes),
            Err(err) if retries_download(&err) => last = err,
            Err(err) => return Err(err),
        }
    }
    Err(last)
}

fn retries_download(err: &FhirError) -> bool {
    matches!(err, FhirError::Storage(_))
}

fn download_once(url: &str, headers: &[(String, String)]) -> Result<Vec<u8>, FhirError> {
    let mut request = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(120))
        .build()
        .get(url)
        .set("User-Agent", "refhir/0.1");
    for (name, value) in headers {
        request = request.set(name, value);
    }
    let response = request
        .call()
        .map_err(|err| match err {
            ureq::Error::Status(404, _) => {
                FhirError::NotFound(format!("package not found at {url}"))
            }
            ureq::Error::Status(403, _) => FhirError::Invalid(format!(
                "download {url}: HTTP 403 (access denied: check the registry credentials, and for S3 the bucket policy; a missing object also reads as 403 without list permission)"
            )),
            ureq::Error::Status(code, _) if (400..500).contains(&code) => {
                FhirError::Invalid(format!("download {url}: HTTP {code}"))
            }
            other => FhirError::Storage(format!("download {url}: {other}")),
        })?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_TARBALL + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| FhirError::Storage(format!("download {url}: {err}")))?;
    if bytes.len() as u64 > MAX_TARBALL {
        return Err(FhirError::Invalid(format!(
            "package at {url} is larger than {MAX_TARBALL} bytes"
        )));
    }
    Ok(bytes)
}

struct BoundedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe) {
                Ok(0) => Ok(0),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "package tarball unpacks larger than the size limit",
                )),
                Err(err) => Err(err),
            };
        }
        let take = buf
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        let n = self.inner.read(&mut buf[..take])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

fn extract_tarball(bytes: &[u8], dest: &Path) -> Result<(), FhirError> {
    extract_tarball_limited(bytes, dest, MAX_UNCOMPRESSED, MAX_ENTRIES)
}

fn extract_tarball_limited(
    bytes: &[u8],
    dest: &Path,
    max_uncompressed: u64,
    max_entries: u64,
) -> Result<(), FhirError> {
    fs::create_dir_all(dest)
        .map_err(|err| FhirError::Storage(format!("extract {}: {err}", dest.display())))?;
    let decoder = BoundedReader {
        inner: GzDecoder::new(bytes),
        remaining: max_uncompressed,
    };
    let mut archive = tar::Archive::new(decoder);
    // Nothing reads the times of the unpacked files, and setting them is a call per file.
    archive.set_preserve_mtime(false);
    let entries = archive
        .entries()
        .map_err(|err| FhirError::Invalid(format!("invalid package tarball: {err}")))?;
    let mut count = 0u64;
    for entry in entries {
        let mut entry =
            entry.map_err(|err| FhirError::Invalid(format!("invalid package tarball: {err}")))?;
        let kind = entry.header().entry_type();
        if kind.is_symlink()
            || kind.is_hard_link()
            || kind.is_fifo()
            || kind.is_character_special()
            || kind.is_block_special()
        {
            return Err(FhirError::Invalid(
                "package tarball has a link or special file".into(),
            ));
        }
        let path = entry
            .path()
            .map_err(|err| FhirError::Invalid(format!("invalid package tarball: {err}")))?;
        if path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(FhirError::Invalid(
                "package tarball has an unsafe path".into(),
            ));
        }
        count += 1;
        if count > max_entries {
            return Err(FhirError::Invalid(
                "package tarball has too many files".into(),
            ));
        }
        let unpacked = entry
            .unpack_in(dest)
            .map_err(|err| FhirError::Invalid(format!("invalid package tarball: {err}")))?;
        if !unpacked {
            return Err(FhirError::Invalid(
                "package tarball has an unsafe path".into(),
            ));
        }
    }
    Ok(())
}

fn split_npm(text: &str) -> Option<(String, String)> {
    let (name, version) = text
        .rsplit_once('@')
        .or_else(|| text.rsplit_once('#'))
        .filter(|(name, version)| !name.is_empty() && !version.is_empty())?;
    if looks_like_path(name) {
        return None;
    }
    Some((name.to_string(), version.to_string()))
}

fn looks_like_path(text: &str) -> bool {
    text.starts_with('.')
        || text.starts_with('/')
        || text.contains('/')
        || text.contains('\\')
        || Path::new(text).is_dir()
}

fn is_tarball(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.ends_with(".tgz") || lower.ends_with(".tar.gz")
}

fn exact_version(version: &str) -> bool {
    !version.is_empty()
        && version != "latest"
        && !version.starts_with(['^', '~', '>', '<', '*'])
        && !version.contains(' ')
}

fn safe_segment(text: &str) -> Result<String, FhirError> {
    if text.is_empty()
        || text.contains("..")
        || !text
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
    {
        return Err(FhirError::Invalid(format!("invalid package id {text}")));
    }
    Ok(text.to_string())
}

fn optional<'a>(args: &'a Value, names: &[&str]) -> Option<&'a str> {
    for name in names {
        if let Some(text) = args.get(*name).and_then(|value| value.as_str()) {
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    let parameters = args
        .get("parameter")
        .and_then(|value| value.as_array())
        .or_else(|| {
            args.pointer("/resource/parameter")
                .and_then(|value| value.as_array())
        })?;
    for name in names {
        for parameter in parameters {
            if parameter.get("name").and_then(|value| value.as_str()) != Some(*name) {
                continue;
            }
            for key in [
                "valueUri",
                "valueUrl",
                "valueCanonical",
                "valueString",
                "valueCode",
            ] {
                if let Some(text) = parameter.get(key).and_then(|value| value.as_str()) {
                    if !text.is_empty() {
                        return Some(text);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_name_version_and_local_dir() {
        assert_eq!(
            parse_package_spec("hl7.fhir.us.core@6.1.0").unwrap(),
            PackageSpec::Npm {
                name: "hl7.fhir.us.core".into(),
                version: "6.1.0".into()
            }
        );
        assert_eq!(
            parse_package_spec("hl7.fhir.us.core#6.1.0").unwrap(),
            PackageSpec::Npm {
                name: "hl7.fhir.us.core".into(),
                version: "6.1.0".into()
            }
        );
        assert!(matches!(
            parse_package_spec("pkg.tgz").unwrap(),
            PackageSpec::Tarball(_)
        ));
        assert!(parse_package_spec("hl7.fhir.us.core").is_err());
        let extra = crate::package::extra_dir();
        assert_eq!(
            parse_package_spec(extra.to_str().unwrap()).unwrap(),
            PackageSpec::Dir(extra.to_path_buf())
        );
    }

    #[test]
    fn install_operation_only_accepts_registry_name_version() {
        let (name, version) = npm_spec_from_args(&json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "name", "valueString": "hl7.fhir.us.core"},
                {"name": "version", "valueString": "6.1.0"}
            ]
        }))
        .unwrap();
        assert_eq!(name, "hl7.fhir.us.core");
        assert_eq!(version, "6.1.0");
        assert_eq!(
            npm_spec_from_args(&json!({"package": "hl7.fhir.us.core@6.1.0"})).unwrap(),
            ("hl7.fhir.us.core".into(), "6.1.0".into())
        );
        assert!(npm_spec_from_args(&json!({"package": "/tmp/ig"})).is_err());
        assert!(npm_spec_from_args(&json!({"package": "pkg.tgz"})).is_err());
        assert!(npm_spec_from_args(&json!({
            "name": "hl7.fhir.us.core",
            "version": "6.1.0",
            "registry": "http://127.0.0.1"
        }))
        .is_err());
    }

    /// Serve one request with `body`; returns the base URL and the raw request it received.
    fn serve_once(body: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = io::Read::read(&mut stream, &mut chunk).unwrap();
                request.extend_from_slice(&chunk[..read]);
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            String::from_utf8_lossy(&request).to_string()
        });
        (base, handle)
    }

    fn package_tarball() -> Vec<u8> {
        gzipped_tar(|builder| {
            let data = br#"{"name":"example.pkg","version":"1.0.0"}"#;
            let mut header = tar::Header::new_gnu();
            header.set_path("package/package.json").unwrap();
            header.set_size(data.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, "package/package.json", data.as_slice())
                .unwrap();
        })
    }

    #[test]
    fn a_static_mirror_serves_packages_in_the_registry_layout() {
        let (base, request) = serve_once(package_tarball());
        let dest = std::env::temp_dir().join(format!("refhir-mirror-{}", uuid::Uuid::new_v4()));
        let registry = NpmRegistry::new(&format!("{base}/mirror/"), dest.clone()).unwrap();
        registry.fetch("example.pkg", "1.0.0", &dest).unwrap();
        assert!(dest.join("package/package.json").is_file());
        assert!(dest.join(COMPLETE).is_file());
        let request = request.join().unwrap().to_ascii_lowercase();
        assert!(
            request.starts_with("get /mirror/example.pkg/1.0.0 "),
            "{request}"
        );
        assert!(!request.contains("authorization:"), "{request}");
        let _ = fs::remove_dir_all(&dest);
    }

    #[test]
    fn an_s3_source_signs_the_request() {
        let (base, request) = serve_once(package_tarball());
        let dest = std::env::temp_dir().join(format!("refhir-s3-{}", uuid::Uuid::new_v4()));
        let registry = NpmRegistry {
            base_url: "s3://packages/fhir".into(),
            cache_dir: dest.clone(),
            s3: Some(S3Source::for_test(&base)),
            allow_download: true,
        };
        registry.fetch("example.pkg", "1.0.0", &dest).unwrap();
        assert!(dest.join("package/package.json").is_file());
        let request = request.join().unwrap().to_ascii_lowercase();
        assert!(
            request.starts_with("get /packages/fhir/example.pkg/1.0.0 "),
            "{request}"
        );
        assert!(
            request.contains("authorization: aws4-hmac-sha256 credential=testkey/"),
            "{request}"
        );
        assert!(request.contains("x-amz-date: "), "{request}");
        assert!(request.contains("x-amz-content-sha256: "), "{request}");
        let _ = fs::remove_dir_all(&dest);
    }

    #[test]
    fn a_registry_location_that_is_not_a_bucket_is_rejected() {
        assert!(NpmRegistry::new("s3://", PathBuf::from("x")).is_err());
        assert!(NpmRegistry::new("https://mirror.example.com", PathBuf::from("x")).is_ok());
    }

    #[test]
    fn extract_rejects_parent_paths_and_symlinks() {
        let dest = std::env::temp_dir().join(format!("refhir-tar-{}", uuid::Uuid::new_v4()));
        let parent = gzipped_tar(|builder| {
            let mut header = tar::Header::new_gnu();
            {
                let gnu = header.as_gnu_mut().unwrap();
                let name = b"../evil.json";
                gnu.name[..name.len()].copy_from_slice(name);
            }
            header.set_size(2);
            header.set_cksum();
            builder.append(&header, b"{}".as_ref()).unwrap();
        });
        assert!(extract_tarball(&parent, &dest).is_err());
        let _ = fs::remove_dir_all(&dest);

        let dest = std::env::temp_dir().join(format!("refhir-tar-{}", uuid::Uuid::new_v4()));
        let link = gzipped_tar(|builder| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_cksum();
            builder
                .append_link(&mut header, "link", "/etc/passwd")
                .unwrap();
        });
        assert!(extract_tarball(&link, &dest).is_err());
        let _ = fs::remove_dir_all(&dest);
    }

    #[test]
    fn extract_stops_on_decompressed_bytes_not_tar_headers() {
        let dest = std::env::temp_dir().join(format!("refhir-tar-{}", uuid::Uuid::new_v4()));
        let bytes = gzipped_tar(|builder| {
            let data = vec![b'x'; 2000];
            let mut header = tar::Header::new_gnu();
            header.set_path("package.json").unwrap();
            header.set_size(data.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, "package.json", data.as_slice())
                .unwrap();
        });
        let err = extract_tarball_limited(&bytes, &dest, 100, MAX_ENTRIES).unwrap_err();
        assert!(matches!(err, FhirError::Invalid(_)), "{err}");
        let _ = fs::remove_dir_all(&dest);
    }

    fn gzipped_tar(
        write: impl FnOnce(&mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>),
    ) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        write(&mut builder);
        builder.into_inner().unwrap().finish().unwrap()
    }
}
