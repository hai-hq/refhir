//! Load a FHIR package directory into the conformance tables, then backfill
//! search indexes for parameters that were not indexed yet.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use refhir_database::{CommitError, Database, IndexPut};
use refhir_persistence::{
    same_expression, ConformanceStore, InstallError, InstallOutcome, NewExpansion, NewPackage,
    NewSearchParameter, NewStructureDefinition, NewTerminology, PackageRows, ParamStatus,
};
use refhir_types::{IndexId, Interval, Order, Timestamp};
use refhir_validate::TerminologyBuilder;
use serde_json::Value;

use crate::catalog::CatalogClock;
use crate::catalog::{pending_index_keys, Catalog};
use crate::error::FhirError;
use crate::extract;
use crate::run::MAX_OCC_FAILURES;

const BATCH: usize = 16;

// The `*_dir` fixtures below are for the test suites only. The server binary carries no packages:
// it installs the release defaults from a prepared file (see `prepared.rs`) or, without one,
// downloads them from `[conformance].package_registry` (see `npm.rs`).

pub fn minir4_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/minir4"))
}

pub fn r4_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/r4"))
}

pub fn extra_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/extra"))
}

pub fn version_clash_dir() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/version-clash"
    ))
}

/// A package for the same FHIR release as `r4_dir` that carries another release's copy of one of
/// its code systems: `hl7.fhir.uv.xver-r5.r4`, which holds `5.0.0` of R4's `encounter-status`.
pub fn cross_version_dir() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/cross-version"
    ))
}

/// A package for the same FHIR release as `r4_dir` that carries a newer copy of a code system
/// the R4 package also carries: `hl7.terminology.r4`, with `v3-MaritalStatus` 4.0.0 (adds `C`).
pub fn newer_terminology_dir() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/newer-terminology"
    ))
}

/// A package for the same FHIR release as `r4_dir` that carries R5-style copies of its value sets:
/// `hl7.fhir.uv.extensions.r4`, whose `allerg-intol-substance-exp-risk` is another version over
/// another code system URL.
pub fn extension_copies_dir() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/extension-copies"
    ))
}

pub fn expression_clash_dir() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/expression-clash"
    ))
}

pub fn sdc_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/sdc"))
}

/// Default FHIR NPM specs for a release (`packages.fhir.org`), selected by
/// `[fhir].version`. Skipped when already in the conformance store; tarballs
/// are cached under `REFHIR_PACKAGE_CACHE`.
pub const R4_CORE_PACKAGE: &str = "hl7.fhir.r4.core@4.0.1";
pub const SDC_PACKAGE: &str = "hl7.fhir.uv.sdc@4.0.0";

pub fn default_r4_registry_specs() -> [&'static str; 2] {
    [R4_CORE_PACKAGE, SDC_PACKAGE]
}

pub fn install_package_dir(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    path: &Path,
) -> Result<(), FhirError> {
    stage_package_dir(store, db, catalog, path)?;
    let started = std::time::Instant::now();
    run_backfills(store, db, catalog)?;
    tracing::debug!(target: "refhir_engine", "backfills took {:.1?}", started.elapsed());
    Ok(())
}

pub fn stage_package_dir(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    path: &Path,
) -> Result<(), FhirError> {
    stage_package(store, db, catalog, path, true).map(|_| ())
}

/// Backfill work for a package whose search parameters are stored but not yet searchable.
pub(crate) struct PendingBackfill {
    package_id: i64,
    bases: BTreeSet<String>,
}

impl PendingBackfill {
    /// Queue the backfill jobs. Call this after the catalog that writers use has been
    /// published: the watermark is read here, and backfill leaves documents newer than it to
    /// writers, which only index a parameter once the catalog knows it.
    pub(crate) fn create(
        &self,
        store: &dyn ConformanceStore,
        db: &Database,
    ) -> Result<(), FhirError> {
        let watermark = db
            .latest_ts()
            .to_i64()
            .map_err(|_| FhirError::Storage("watermark timestamp does not fit".into()))?;
        for base in &self.bases {
            store
                .insert_backfill(self.package_id, watermark, base, &[])
                .map_err(FhirError::from)?;
        }
        Ok(())
    }
}

/// Store a package without rebuilding the catalog, for installing several in a row. The caller
/// must [`publish`], then [`PendingBackfill::create`] each returned item and run the backfills.
pub(crate) fn stage_package_deferred(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    path: &Path,
) -> Result<Option<PendingBackfill>, FhirError> {
    stage_package(store, db, catalog, path, false)
}

fn stage_package(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    path: &Path,
    publish_now: bool,
) -> Result<Option<PendingBackfill>, FhirError> {
    let (name, version) = package_identity(path)?;
    let installed = store.load_packages().map_err(FhirError::from)?;
    if let Some(package) = installed
        .iter()
        .find(|package| package.name == name && package.version == version)
    {
        reopen_unsupported(store, db, package.id)?;
        if publish_now {
            publish(store, catalog)?;
        }
        return Ok(None);
    }
    let started = std::time::Instant::now();
    let package = read_package(path)?;
    stage_new_package(
        store,
        db,
        catalog,
        package,
        started.elapsed(),
        "read",
        publish_now,
    )
}

/// Store a package that has been read, and (unless `publish_now` is false) publish it.
/// `read_time` and `read_label` are only for the log line: it says where the time went, whether
/// the package was read from a directory or from a prepared bundle.
pub(crate) fn stage_new_package(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    package: NewPackage,
    read_time: std::time::Duration,
    read_label: &str,
    publish_now: bool,
) -> Result<Option<PendingBackfill>, FhirError> {
    let started = std::time::Instant::now();
    let name = package.name.clone();
    let version = package.version.clone();
    let outcome = store.install_package(&package).map_err(install_error)?;
    let stored = started.elapsed();
    let counts = (
        package.structure_definitions.len(),
        package.search_parameters.len(),
        package.terminology.len(),
    );
    // The catalog rebuild below reads everything back from the store. The package's own copy of
    // it, which is as large as the package, is not needed any more, so it is not kept alive
    // under the rebuild.
    drop(package);
    crate::memory::release_unused_memory();
    finish_staging(
        store,
        db,
        catalog,
        (&name, &version),
        counts,
        &format!("{read_label} {read_time:.1?}, store {stored:.1?}"),
        (started, stored),
        outcome,
        publish_now,
    )
}

/// The name and version a package is stored under, and the release it is for.
pub(crate) struct PackageHead {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) fhir_version: String,
}

/// How many rows of each kind a package brought, counted as they pass.
struct Counting<'a> {
    rows: &'a mut dyn PackageRows,
    definitions: usize,
    parameters: usize,
    terminology: usize,
}

impl PackageRows for Counting<'_> {
    fn structure_definitions(
        &mut self,
        rows: &[NewStructureDefinition],
    ) -> Result<(), InstallError> {
        self.definitions += rows.len();
        self.rows.structure_definitions(rows)
    }

    fn search_parameters(&mut self, rows: &[NewSearchParameter]) -> Result<(), InstallError> {
        self.parameters += rows.len();
        self.rows.search_parameters(rows)
    }

    fn terminology(&mut self, rows: &[NewTerminology]) -> Result<(), InstallError> {
        self.terminology += rows.len();
        self.rows.terminology(rows)
    }

    fn expansions(&mut self, rows: &[NewExpansion]) -> Result<(), InstallError> {
        self.rows.expansions(rows)
    }
}

/// [`stage_new_package`] for a package that is not held whole: `feed` hands its rows over a few
/// at a time as the store asks for them, so storing the largest package takes the memory of a few
/// rows and not of the package. The package is stored in one transaction, as it is when it is held
/// whole, so it is there whole or not at all.
pub(crate) fn stage_package_in_pieces(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    head: &PackageHead,
    feed: &mut dyn FnMut(&mut dyn PackageRows) -> Result<(), InstallError>,
    publish_now: bool,
) -> Result<Option<PendingBackfill>, FhirError> {
    let started = std::time::Instant::now();
    let mut counts = (0, 0, 0);
    let outcome = store
        .install_package_in_pieces(&head.name, &head.version, &head.fhir_version, &mut |rows| {
            let mut counting = Counting {
                rows,
                definitions: 0,
                parameters: 0,
                terminology: 0,
            };
            let fed = feed(&mut counting);
            counts = (
                counting.definitions,
                counting.parameters,
                counting.terminology,
            );
            fed
        })
        .map_err(install_error)?;
    let stored = started.elapsed();
    crate::memory::release_unused_memory();
    finish_staging(
        store,
        db,
        catalog,
        (&head.name, &head.version),
        counts,
        &format!("read and store {stored:.1?}"),
        (started, stored),
        outcome,
        publish_now,
    )
}

/// What follows storing a package, whichever way it was stored: publish it, say where the time
/// went, and note the search parameters that still need their index built.
#[allow(clippy::too_many_arguments)]
fn finish_staging(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    (name, version): (&str, &str),
    counts: (usize, usize, usize),
    stored_note: &str,
    (started, stored): (std::time::Instant, std::time::Duration),
    outcome: InstallOutcome,
    publish_now: bool,
) -> Result<Option<PendingBackfill>, FhirError> {
    if publish_now {
        publish(store, catalog)?;
    }
    // Installing a large package takes a while; say where the time went. The readings are taken
    // in quick succession, and a monotonic clock on a virtual machine can read slightly earlier
    // the second time, so the differences saturate at zero instead of panicking over a log line.
    let catalog_time = if publish_now {
        format!(", catalog {:.1?}", started.elapsed().saturating_sub(stored))
    } else {
        String::new()
    };
    tracing::info!(
        target: "refhir_engine",
        "installed {name}@{version}: {} definitions, {} search parameters, {} terminology resources; {stored_note}{catalog_time}",
        counts.0,
        counts.1,
        counts.2,
    );
    if !outcome.inserted {
        return Ok(None);
    }
    let pending = PendingBackfill {
        package_id: outcome.package_id,
        bases: outcome
            .newly_pending
            .iter()
            .map(|param| param.base.clone())
            .collect(),
    };
    if publish_now {
        pending.create(store, db)?;
        return Ok(None);
    }
    Ok(Some(pending))
}

pub fn run_backfills(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
) -> Result<(), FhirError> {
    let jobs = store.load_backfills().map_err(FhirError::from)?;
    let mut refreshed = false;
    for job in jobs.into_iter().filter(|job| !job.done) {
        // This writer owns the job from here. If an earlier writer died with it half done, the
        // claim moves to this one and the cursor tells where to resume; if a newer writer has
        // already claimed it, this process is the stale one and is refused.
        if !store.claim_backfill(job.id).map_err(FhirError::from)? {
            continue;
        }
        let watermark = Timestamp::from_i64(job.watermark_ts)
            .map_err(|_| FhirError::Storage("bad backfill watermark".into()))?;
        let old_keys = index_keys_for_pending(db, catalog, &job.tablet)?;
        let mut ids = live_ids(db, &job.tablet)?;
        if let Some(cursor) = &job.cursor_id {
            ids.retain(|id| id.as_str() > cursor.as_str());
        }
        let mut start = 0;
        while start < ids.len() {
            let end = (start + BATCH).min(ids.len());
            let batch = &ids[start..end];
            match backfill_batch(db, catalog, &job.tablet, batch, watermark, &old_keys) {
                Ok(()) => {}
                Err(CommitError::Occ(_)) => {
                    for id in batch {
                        backfill_id(db, catalog, &job.tablet, id, watermark, &old_keys)?;
                    }
                }
                Err(err) => return Err(FhirError::Storage(err.to_string())),
            }
            store
                .update_backfill(job.id, Some(batch[batch.len() - 1].as_str()), false)
                .map_err(FhirError::from)?;
            start = end;
        }
        store
            .complete_backfill(job.id, job.package_id, &job.tablet)
            .map_err(FhirError::from)?;
        let cursor = ids.last().map(String::as_str).or(job.cursor_id.as_deref());
        store
            .update_backfill(job.id, cursor, true)
            .map_err(FhirError::from)?;
        refreshed = true;
    }
    if refreshed {
        publish(store, catalog)?;
    }
    Ok(())
}

/// Publish what was stored, queue the backfills for it, and run them. `result` is how storing the
/// packages went: what was stored reaches the catalog and gets its backfills even when a later
/// package failed, or its search parameters stay pending until the next restart, and then the
/// failure is returned. The backfill jobs are queued only after the publish, so the watermark they
/// record is not older than the catalog writers use.
pub(crate) fn finish_install(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    pending: &[PendingBackfill],
    result: Result<(), FhirError>,
) -> Result<(), FhirError> {
    let finish = || -> Result<(), FhirError> {
        let started = std::time::Instant::now();
        publish(store, catalog)?;
        tracing::info!(
            target: "refhir_engine",
            "rebuilt the catalog in {:.1?}",
            started.elapsed()
        );
        for backfill in pending {
            backfill.create(store, db)?;
        }
        run_backfills(store, db, catalog)
    };
    let finished = finish();
    result?;
    finished
}

pub(crate) fn publish(
    store: &dyn ConformanceStore,
    catalog: &RwLock<Catalog>,
) -> Result<(), FhirError> {
    let (policy, archiver, update_create, in_place) = {
        let current = catalog.read().expect("catalog");
        (
            current.reference_policy(),
            current.sdc_archiver(),
            current.update_create(),
            current.rebuild_in_place(),
        )
    };
    if in_place {
        *catalog.write().expect("catalog") = Catalog::empty();
    }
    // The tables are read a page at a time, so a write that lands in between would give a catalog
    // that mixes two states; the revision is checked before and after, and the load repeated.
    let (mut loaded, _) = Catalog::open_consistent(store, CatalogClock::Latest)?;
    loaded.set_rebuild_in_place(in_place);
    loaded.set_reference_policy(policy);
    loaded.set_sdc_archiver(archiver);
    let stable = !loaded.has_pending_parameters();
    loaded.set_update_create(update_create);
    *catalog.write().expect("catalog") = loaded;
    // Tell the other processes the conformance tables changed. Every change moves the revision,
    // which a standby follows. The version, which replicas follow, moves only when no search
    // parameter is waiting for its backfill: while one is, this publish is skipped, and the one
    // that follows the backfill's completion bumps it. So a replica never loads a parameter whose
    // index is not complete.
    store.bump_catalog_revision()?;
    if stable {
        store.bump_catalog_version()?;
    }
    Ok(())
}

/// Rebuild `catalog` from the store if its version moved since `loaded_version`, keeping the
/// settings that are not stored (reference policy, archiver mode). Returns the version the
/// catalog now reflects. A replica calls this when another process changed the catalog, and a
/// standby calls it once more when it takes over.
pub fn reload_catalog(
    store: &dyn ConformanceStore,
    catalog: &RwLock<Catalog>,
    loaded_version: u64,
    clock: CatalogClock,
) -> Result<u64, FhirError> {
    // Nothing to do, and nothing worth the seconds a full load takes, if the clock stands.
    if clock.read(store)? == loaded_version {
        return Ok(loaded_version);
    }
    let (fresh, version) = Catalog::open_consistent(store, clock)?;
    let mut fresh = fresh;
    {
        let current = catalog.read().expect("catalog");
        fresh.set_reference_policy(current.reference_policy());
        fresh.set_sdc_archiver(current.sdc_archiver());
        fresh.set_update_create(current.update_create());
        fresh.set_rebuild_in_place(current.rebuild_in_place());
    }
    *catalog.write().expect("catalog") = fresh;
    Ok(version)
}

/// The thread that keeps a catalog in step with another process. See [`watch_catalog`].
pub struct CatalogWatcher {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    loaded: std::sync::Arc<std::sync::atomic::AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CatalogWatcher {
    /// Stops watching and returns the catalog version the catalog was last loaded at. A standby
    /// that is about to become the primary does this, then reloads once, so from then on only
    /// its own changes touch the catalog.
    pub fn stop(mut self) -> u64 {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.loaded.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Keep `catalog` in step with changes another process makes: poll the catalog version and
/// reload in the background when it moves. The thread stops when the catalog is dropped or the
/// returned watcher is stopped.
pub fn watch_catalog(
    store: std::sync::Arc<dyn ConformanceStore>,
    catalog: &std::sync::Arc<RwLock<Catalog>>,
    loaded_version: u64,
    interval: std::time::Duration,
    clock: CatalogClock,
) -> CatalogWatcher {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    let weak = Arc::downgrade(catalog);
    let stop = Arc::new(AtomicBool::new(false));
    let loaded = Arc::new(AtomicU64::new(loaded_version));
    let (stopped, tracked) = (stop.clone(), loaded.clone());
    let spawned = std::thread::Builder::new()
        .name("refhir-catalog".into())
        .spawn(move || loop {
            // Sleep in slices so a stop request is honoured promptly.
            let mut slept = std::time::Duration::ZERO;
            while slept < interval {
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                let slice = std::time::Duration::from_millis(25).min(interval);
                std::thread::sleep(slice);
                slept += slice;
            }
            let Some(catalog) = weak.upgrade() else {
                return;
            };
            let known = tracked.load(Ordering::SeqCst);
            match clock.read(store.as_ref()) {
                Ok(version) if version != known => {
                    match reload_catalog(store.as_ref(), &catalog, known, clock) {
                        Ok(now) => {
                            tracing::info!(
                                target: "refhir",
                                "catalog reloaded at version {now} (was {known})"
                            );
                            tracked.store(now, Ordering::SeqCst);
                        }
                        Err(err) => {
                            tracing::warn!(target: "refhir", "catalog reload failed: {err}")
                        }
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(target: "refhir", "catalog {clock:?} clock unreadable: {err}")
                }
            }
        });
    let thread = match spawned {
        Ok(thread) => Some(thread),
        Err(err) => {
            tracing::error!(target: "refhir", "could not start the catalog watcher: {err}");
            None
        }
    };
    CatalogWatcher {
        stop,
        loaded,
        thread,
    }
}

pub(crate) fn live_ids(db: &Database, type_name: &str) -> Result<Vec<String>, FhirError> {
    let index = IndexId::new(format!("{type_name}/by_id"));
    let rows = db
        .reader()
        .index_scan(&index, db.latest_ts(), &Interval::all(), Order::Asc)?;
    let mut ids = Vec::new();
    for row in rows {
        if let Some(id) = row.document.id.id_str() {
            ids.push(id.to_string());
        }
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

fn same_keys(left: &[(IndexId, Vec<u8>)], right: &[(IndexId, Vec<u8>)]) -> bool {
    let left: BTreeSet<_> = left.iter().cloned().collect();
    let right: BTreeSet<_> = right.iter().cloned().collect();
    left == right
}

/// The index keys of each document of one type, by document id.
type KeysByDocument = HashMap<String, Vec<(IndexId, Vec<u8>)>>;

fn index_keys_for_pending(
    db: &Database,
    catalog: &RwLock<Catalog>,
    type_name: &str,
) -> Result<KeysByDocument, FhirError> {
    let guard = catalog.read().expect("catalog");
    let Ok(model) = guard.get(type_name) else {
        return Ok(HashMap::new());
    };
    let mut map: HashMap<String, Vec<(IndexId, Vec<u8>)>> = HashMap::new();
    for param in model.pending_params() {
        let index = model.search_index(&param.name);
        let rows = db
            .reader()
            .index_scan(&index, db.latest_ts(), &Interval::all(), Order::Asc)?;
        for row in rows {
            let Some(id) = row.document.id.id_str() else {
                continue;
            };
            map.entry(id.to_string())
                .or_default()
                .push((index.clone(), row.key));
        }
    }
    Ok(map)
}

fn backfill_batch(
    db: &Database,
    catalog: &RwLock<Catalog>,
    type_name: &str,
    ids: &[String],
    watermark: Timestamp,
    old_by_id: &HashMap<String, Vec<(IndexId, Vec<u8>)>>,
) -> Result<(), CommitError> {
    let mut tx = db.begin();
    let guard = catalog.read().expect("catalog");
    let model = guard
        .get(type_name)
        .map_err(|err| CommitError::Invalid(err.to_string()))?;
    let mut staged = false;
    for id in ids {
        let Some(stored) = tx
            .get(&model.tablet(), id, &model.by_id())
            .map_err(|err| CommitError::Invalid(err.to_string()))?
        else {
            continue;
        };
        let Some(ts) = stored.ts else {
            continue;
        };
        let keys = {
            let host = crate::resolve::TxHost::new(&mut tx, &guard);
            pending_index_keys(guard.path_model(), model, &stored.json, &host).unwrap_or_default()
        };
        let old_keys = old_by_id.get(id).cloned().unwrap_or_default();
        if ts > watermark && old_keys.is_empty() {
            continue;
        }
        if same_keys(&keys, &old_keys) {
            continue;
        }
        tx.stage_index(IndexPut {
            tablet: model.tablet(),
            id: id.clone(),
            keys,
            old_keys,
        })
        .map_err(|err| CommitError::Invalid(err.to_string()))?;
        staged = true;
    }
    drop(guard);
    if staged {
        db.commit(tx)?;
    }
    Ok(())
}

fn backfill_id(
    db: &Database,
    catalog: &RwLock<Catalog>,
    type_name: &str,
    id: &str,
    watermark: Timestamp,
    old_by_id: &HashMap<String, Vec<(IndexId, Vec<u8>)>>,
) -> Result<(), FhirError> {
    for attempt in 0..=MAX_OCC_FAILURES {
        match backfill_batch(
            db,
            catalog,
            type_name,
            &[id.to_string()],
            watermark,
            old_by_id,
        ) {
            Ok(()) => return Ok(()),
            Err(CommitError::Occ(_)) if attempt < MAX_OCC_FAILURES => continue,
            Err(CommitError::Occ(err)) => return Err(FhirError::Conflict(format!("{err:?}"))),
            Err(err) => return Err(FhirError::Storage(err.to_string())),
        }
    }
    Err(FhirError::Conflict("backfill OCC retries exhausted".into()))
}

pub(crate) fn read_manifest(root: &Path) -> Result<Value, FhirError> {
    let manifest_text = fs::read_to_string(root.join("package.json"))
        .map_err(|err| FhirError::Invalid(format!("package.json: {err}")))?;
    serde_json::from_str(&manifest_text)
        .map_err(|err| FhirError::Invalid(format!("package.json: {err}")))
}

pub(crate) fn package_identity(path: &Path) -> Result<(String, String), FhirError> {
    let root = package_root(path)?;
    let manifest = read_manifest(&root)?;
    let name = manifest
        .get("name")
        .and_then(|value| value.as_str())
        .ok_or_else(|| FhirError::Invalid("package.json name is required".into()))?
        .to_string();
    let version = manifest
        .get("version")
        .and_then(|value| value.as_str())
        .ok_or_else(|| FhirError::Invalid("package.json version is required".into()))?
        .to_string();
    Ok((name, version))
}

/// The resource types a package contributes to the conformance tables.
fn is_kept(resource_type: Option<&str>) -> bool {
    matches!(
        resource_type,
        Some("StructureDefinition")
            | Some("SearchParameter")
            | Some("CodeSystem")
            | Some("ValueSet")
            | Some("ConceptMap")
            | Some("CompartmentDefinition")
    )
}

/// `StructureDefinition-patient.json` and the like: the name says the file is kept. The type
/// inside the file still decides where it goes.
fn conventional_name(file: &Path) -> bool {
    file.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.split_once('-'))
        .is_some_and(|(prefix, _)| is_kept(Some(prefix)))
}

/// Just the type of a resource; every other field is skipped without being stored.
#[derive(serde::Deserialize)]
struct ResourceKind {
    #[serde(rename = "resourceType")]
    resource_type: Option<Value>,
}

pub(crate) fn read_package(path: &Path) -> Result<NewPackage, FhirError> {
    let root = package_root(path)?;
    let manifest = read_manifest(&root)?;
    let name = manifest
        .get("name")
        .and_then(|value| value.as_str())
        .ok_or_else(|| FhirError::Invalid("package.json name is required".into()))?
        .to_string();
    let version = manifest
        .get("version")
        .and_then(|value| value.as_str())
        .ok_or_else(|| FhirError::Invalid("package.json version is required".into()))?
        .to_string();
    // A parsed file is several times the size of its text and a package holds thousands, so each
    // StructureDefinition and each piece of terminology is reduced to its row, and to what is read
    // from it later, as soon as it is parsed. Search parameters are kept parsed: they are small,
    // and read together below.
    let mut structure_definitions = Vec::new();
    let mut scope = SearchScope::default();
    let mut definition_version = None;
    let mut definition_error = None;
    let mut parameters = Vec::new();
    let mut terminology_rows = Vec::new();
    let mut terminology_error = None;
    let mut expanded = TerminologyBuilder::for_package(None);
    for entry in fs::read_dir(&root).map_err(|err| FhirError::Invalid(err.to_string()))? {
        let entry = entry.map_err(|err| FhirError::Invalid(err.to_string()))?;
        let file = entry.path();
        if file.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        if file.file_name().and_then(|name| name.to_str()) == Some("package.json") {
            continue;
        }
        let text = fs::read_to_string(&file)
            .map_err(|err| FhirError::Invalid(format!("{}: {err}", file.display())))?;
        let parse = |text: &str| {
            serde_json::from_str::<Value>(text)
                .map_err(|err| FhirError::Invalid(format!("{}: {err}", file.display())))
        };
        // Most files in a package are examples. Files named `<Type>-<id>.json` for a type that
        // is kept are parsed once; any other file is first checked for its type without
        // building a value, so examples are skipped cheaply.
        let value = if conventional_name(&file) {
            parse(&text)?
        } else {
            match serde_json::from_str::<ResourceKind>(&text) {
                Ok(kind) if is_kept(kind.resource_type.as_ref().and_then(Value::as_str)) => {
                    parse(&text)?
                }
                Ok(_) => continue,
                // Not an object (an array, a number, null): never a resource. Invalid JSON is
                // still an error.
                Err(_) => {
                    parse(&text)?;
                    continue;
                }
            }
        };
        let bucket = match value.get("resourceType").and_then(Value::as_str) {
            Some("StructureDefinition") => {
                scope.note_definition(&value);
                if definition_version.is_none() {
                    definition_version = value
                        .get("fhirVersion")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                match structure_row(&value) {
                    Ok(row) => structure_definitions.push(row),
                    Err(err) => {
                        definition_error.get_or_insert(err);
                    }
                }
                continue;
            }
            Some("SearchParameter") => &mut parameters,
            Some("CodeSystem")
            | Some("ValueSet")
            | Some("ConceptMap")
            | Some("CompartmentDefinition") => {
                match terminology_row(&value) {
                    Ok(row) => terminology_rows.push(row),
                    Err(err) => {
                        terminology_error.get_or_insert(err);
                    }
                }
                expanded.add_resource(0, value);
                continue;
            }
            _ => continue,
        };
        bucket.push(value);
    }
    let fhir_version = fhir_version(&manifest, definition_version)?;
    if let Some(err) = definition_error {
        return Err(err);
    }
    for parameter in &parameters {
        scope.note_parameter(parameter);
    }
    let mut search_parameters = Vec::new();
    // Non-experimental parameters go first and win: the R4 core package ships a sample
    // `_id` parameter (`SearchParameter/example`) that disagrees with the real `Resource-id`.
    let is_experimental =
        |parameter: &Value| parameter.get("experimental").and_then(Value::as_bool) == Some(true);
    let mut ordered: Vec<&Value> = parameters.iter().collect();
    ordered.sort_by_key(|parameter| is_experimental(parameter));
    let mut defined: BTreeMap<(String, String), (String, bool)> = BTreeMap::new();
    for parameter in ordered {
        // The same package ships draft `*-extensions-*` parameters with no `base`. They
        // belong to no resource type, so there is nothing to index or search.
        if parameter.get("base").is_none() {
            continue;
        }
        let experimental = is_experimental(parameter);
        for row in parameter_rows(parameter, &scope)? {
            let key = (row.base.clone(), row.code.clone());
            let expression = row.expression.clone().unwrap_or_default();
            if let Some((first, first_experimental)) = defined.get(&key) {
                if !same_expression(&row.base, first, &expression) && experimental {
                    if !first_experimental {
                        // A draft or sample yields to the real definition.
                        continue;
                    }
                    return Err(FhirError::Invalid(format!(
                        "search parameter {}.{} is defined twice with different expressions",
                        row.base, row.code
                    )));
                }
            }
            defined.entry(key).or_insert((expression, experimental));
            search_parameters.push(row);
        }
    }
    if let Some(err) = terminology_error {
        return Err(err);
    }
    // Read for the release the package is for, as the catalog and a rebuild read it, so that what a
    // rebuild gives is what is stored here.
    expanded.set_release(Some(fhir_version.as_str()));
    let expansions = expanded
        .expansion_documents()
        .filter_map(|document| {
            let url = document.get("url")?.as_str()?.to_string();
            Some(NewExpansion {
                url,
                json: document.to_string(),
            })
        })
        .collect();
    Ok(NewPackage {
        name,
        version,
        fhir_version,
        structure_definitions,
        search_parameters,
        terminology: terminology_rows,
        expansions,
    })
}

pub(crate) fn package_root(path: &Path) -> Result<PathBuf, FhirError> {
    let nested = path.join("package").join("package.json");
    if nested.is_file() {
        return Ok(path.join("package"));
    }
    if path.join("package.json").is_file() {
        return Ok(path.to_path_buf());
    }
    Err(FhirError::Invalid(format!(
        "no package.json in {}",
        path.display()
    )))
}

/// The release a package is for: what its manifest says, or else what its first definition that
/// says (`definition_version`) does.
fn fhir_version(manifest: &Value, definition_version: Option<String>) -> Result<String, FhirError> {
    if let Some(version) = manifest.get("fhirVersion").and_then(|value| value.as_str()) {
        return Ok(version.to_string());
    }
    if let Some(version) = manifest
        .get("fhirVersions")
        .and_then(|value| value.as_array())
        .and_then(|items| items.first())
        .and_then(|value| value.as_str())
    {
        return Ok(version.to_string());
    }
    definition_version.ok_or_else(|| FhirError::Invalid("package fhirVersion is required".into()))
}

fn terminology_row(resource: &Value) -> Result<NewTerminology, FhirError> {
    Ok(NewTerminology {
        resource_type: required_str(resource, "resourceType")?,
        url: required_str(resource, "url")?,
        json: resource.to_string(),
    })
}

fn structure_row(definition: &Value) -> Result<NewStructureDefinition, FhirError> {
    let url = required_str(definition, "url")?;
    let kind = required_str(definition, "kind")?;
    let type_name = definition
        .get("type")
        .and_then(|value| value.as_str())
        .or_else(|| definition.get("name").and_then(|value| value.as_str()))
        .ok_or_else(|| FhirError::Invalid("StructureDefinition type is required".into()))?
        .to_string();
    Ok(NewStructureDefinition {
        url,
        type_name,
        kind,
        is_abstract: definition
            .get("abstract")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        json: definition.to_string(),
    })
}

fn reopen_unsupported(
    store: &dyn ConformanceStore,
    db: &Database,
    package_id: i64,
) -> Result<(), FhirError> {
    let parameters = store.load_search_parameters().map_err(FhirError::from)?;
    let scope = SearchScope::from_store(store)?;
    let mut bases = BTreeSet::new();
    for parameter in parameters.iter().filter(|parameter| {
        parameter.package_id == package_id && parameter.status == ParamStatus::Unsupported
    }) {
        let json = serde_json::from_str(&parameter.json).unwrap_or(Value::Null);
        if extract::compile_parameter(&json, &scope.kinds).is_none() {
            continue;
        }
        store
            .set_search_parameter_status(
                package_id,
                &parameter.url,
                &parameter.base,
                ParamStatus::Pending,
            )
            .map_err(FhirError::from)?;
        bases.insert(parameter.base.clone());
    }
    if bases.is_empty() {
        return Ok(());
    }
    let watermark = db
        .latest_ts()
        .to_i64()
        .map_err(|_| FhirError::Storage("watermark timestamp does not fit".into()))?;
    for base in bases {
        store
            .insert_backfill(package_id, watermark, &base, &[])
            .map_err(FhirError::from)?;
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SearchScope {
    pub(crate) kinds: BTreeMap<String, String>,
    parents: BTreeMap<String, String>,
    concrete: Vec<String>,
}

impl SearchScope {
    pub(crate) fn from_store(store: &dyn ConformanceStore) -> Result<Self, FhirError> {
        let mut scope = Self::default();
        for definition in store
            .load_structure_definitions()
            .map_err(FhirError::from)?
        {
            if definition.kind != "resource" {
                continue;
            }
            let json = serde_json::from_str(&definition.json).unwrap_or(Value::Null);
            scope.note_definition(&json);
        }
        for parameter in store.load_search_parameters().map_err(FhirError::from)? {
            scope
                .kinds
                .entry(parameter.url)
                .or_insert(parameter.param_type);
        }
        Ok(scope)
    }

    pub(crate) fn with_parameters(mut self, parameters: &[Value]) -> Self {
        for parameter in parameters {
            self.note_parameter(parameter);
        }
        self
    }

    fn note_parameter(&mut self, parameter: &Value) {
        let Some(url) = parameter.get("url").and_then(|value| value.as_str()) else {
            return;
        };
        let Some(param_type) = parameter.get("type").and_then(|value| value.as_str()) else {
            return;
        };
        self.kinds
            .entry(url.to_string())
            .or_insert_with(|| param_type.to_string());
    }

    fn note_definition(&mut self, definition: &Value) {
        if definition.get("kind").and_then(|value| value.as_str()) != Some("resource") {
            return;
        }
        let Some(name) = definition
            .get("type")
            .and_then(|value| value.as_str())
            .or_else(|| definition.get("name").and_then(|value| value.as_str()))
        else {
            return;
        };
        if let Some(parent) = definition
            .get("baseDefinition")
            .and_then(|value| value.as_str())
            .and_then(|url| url.rsplit('/').next())
        {
            if parent != name {
                self.parents
                    .entry(name.to_string())
                    .or_insert_with(|| parent.to_string());
            }
        }
        let abstract_type = definition
            .get("abstract")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let constraint = definition
            .get("derivation")
            .and_then(|value| value.as_str())
            == Some("constraint");
        if abstract_type || constraint {
            return;
        }
        if !self.concrete.iter().any(|item| item == name) {
            self.concrete.push(name.to_string());
        }
    }

    fn bases(&self, base: &str) -> Vec<String> {
        if self.concrete.is_empty() {
            return vec![base.to_string()];
        }
        let found: Vec<String> = self
            .concrete
            .iter()
            .filter(|name| inherits(name, base, &self.parents))
            .cloned()
            .collect();
        if found.is_empty() {
            vec![base.to_string()]
        } else {
            found
        }
    }
}

fn inherits(name: &str, target: &str, parents: &BTreeMap<String, String>) -> bool {
    let mut current = name.to_string();
    for _ in 0..24 {
        if current == target {
            return true;
        }
        match parents.get(&current) {
            Some(parent) => current = parent.clone(),
            None => return false,
        }
    }
    false
}

pub(crate) fn parameter_rows(
    parameter: &Value,
    scope: &SearchScope,
) -> Result<Vec<NewSearchParameter>, FhirError> {
    let url = required_str(parameter, "url")?;
    let code = required_str(parameter, "code")?;
    let param_type = required_str(parameter, "type")?;
    let expression = parameter
        .get("expression")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string());
    let bases = parameter
        .get("base")
        .and_then(|value| value.as_array())
        .ok_or_else(|| FhirError::Invalid(format!("SearchParameter {code} base is required")))?;
    let status = if extract::compile_parameter(parameter, &scope.kinds).is_some() {
        ParamStatus::Pending
    } else {
        ParamStatus::Unsupported
    };
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    for base in bases {
        let Some(base) = base.as_str() else {
            continue;
        };
        for base in scope.bases(base) {
            if !seen.insert(base.clone()) {
                continue;
            }
            rows.push(NewSearchParameter {
                url: url.clone(),
                base,
                code: code.clone(),
                param_type: param_type.clone(),
                expression: expression.clone(),
                status,
                json: parameter.to_string(),
            });
        }
    }
    Ok(rows)
}

fn required_str(value: &Value, field: &str) -> Result<String, FhirError> {
    value
        .get(field)
        .and_then(|item| item.as_str())
        .map(|item| item.to_string())
        .ok_or_else(|| FhirError::Invalid(format!("{field} is required")))
}

fn install_error(err: InstallError) -> FhirError {
    match err {
        InstallError::FhirVersion { existing, incoming } => FhirError::Invalid(format!(
            "package fhirVersion {incoming} does not match {existing}"
        )),
        InstallError::Expression { base, code } => FhirError::Invalid(format!(
            "search parameter {base}.{code} expression conflicts with an installed package"
        )),
        InstallError::Persist(err) => FhirError::Storage(err.to_string()),
    }
}
