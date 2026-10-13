//! Persistence contract.
//!
//! Convex: `crates/common/src/persistence/mod.rs`
//! `Persistence::write`, `PersistenceReader::index_scan`, `previous_revisions`,
//! `load_documents`, `get_persistence_global`.
//!
//! Streams in Convex are `Vec` here. The rows and the timestamp rules are the same.
//! `DocumentLogEntry::value` is JSON text, not `ResolvedDocument`.

mod pool;

pub mod migrate;

pub use pool::{Pool, PoolGuard, PoolStats};

use std::sync::Arc;

use refhir_types::{DocumentId, IndexId, Interval, Order, TabletId, Timestamp};
use serde_json::Value as JsonValue;

/// How `Persistence::write` treats an existing primary key.
/// Convex `ConflictStrategy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictStrategy {
    Error,
    Overwrite,
}

/// Convex `PersistenceGlobalKey`. Retention keys are not written yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistenceGlobalKey {
    /// Highest persisted timestamp. Written at persist time, so it can run ahead of what
    /// has been published: never use it as a read bound for another process.
    MaxRepeatableTimestamp,
    /// Epoch of the process holding the writer lease. Bumped each time a lease is taken.
    WriterEpoch,
    /// Highest timestamp the writer has published, in order, with no gaps before it.
    /// Readers in other processes may serve every snapshot at or below it.
    PublishedTimestamp,
    /// Moves when the catalog is in a state a reader may use: it is not bumped while search
    /// parameters are still waiting for their backfill, so a replica never loads a parameter
    /// whose index is not complete. Replicas watch this one.
    CatalogVersion,
    /// Moves on every change to the conformance tables, including ones that leave parameters
    /// pending. A standby watches this one: when it takes over it may have to finish a backfill,
    /// so it needs to know about parameters a replica must not see yet.
    CatalogRevision,
}

impl PersistenceGlobalKey {
    pub fn as_str(self) -> &'static str {
        match self {
            PersistenceGlobalKey::MaxRepeatableTimestamp => "max_repeatable_ts",
            PersistenceGlobalKey::WriterEpoch => "writer_epoch",
            PersistenceGlobalKey::PublishedTimestamp => "published_ts",
            PersistenceGlobalKey::CatalogVersion => "catalog_version",
            PersistenceGlobalKey::CatalogRevision => "catalog_revision",
        }
    }
}

/// One document-log row. `value: None` is a tombstone.
/// Convex `DocumentLogEntry`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentLogEntry {
    pub ts: Timestamp,
    pub id: DocumentId,
    pub value: Option<String>,
    pub prev_ts: Option<Timestamp>,
}

impl DocumentLogEntry {
    pub fn deleted(&self) -> bool {
        self.value.is_none()
    }
}

/// One index-log row. `value: None` is a tombstone.
/// Convex `PersistenceIndexEntry` without backfill mode or `prev` pointers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistenceIndexEntry {
    pub ts: Timestamp,
    pub index_id: IndexId,
    pub key: Vec<u8>,
    pub value: Option<DocumentId>,
}

/// A live document returned by `index_scan`. Convex `LatestDocument`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatestDocument {
    pub ts: Timestamp,
    pub id: DocumentId,
    pub value: String,
    pub prev_ts: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexScanRow {
    pub key: Vec<u8>,
    pub document: LatestDocument,
}

/// A row of `index_scan_ids`: the key and the document it points at, without the document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexScanEntry {
    pub key: Vec<u8>,
    pub id: DocumentId,
}

/// The document an index row points at, from the two columns a backend stores it in.
pub fn index_target(table_id: Vec<u8>, document_id: Vec<u8>) -> Result<DocumentId, PersistError> {
    let tablet = TabletId::from_bytes(&table_id)
        .map_err(|_| PersistError::Definite("table_id is not utf-8".into()))?;
    Ok(DocumentId::new(tablet, document_id))
}

/// Reduces the stored versions of an index range to its live entries: the latest version of each
/// key, unless that version is a tombstone.
///
/// A backend feeds it rows read straight from its index table in key order, which needs no join
/// and no grouping in SQL, and can stop reading as soon as `push` says so. Versions of one key
/// arrive oldest first when scanning `Order::Asc` and newest first when scanning `Order::Desc`,
/// the order the table's primary key yields them in.
pub struct LiveEntries {
    order: Order,
    limit: usize,
    /// The latest version seen so far of the key being read: `Some` for a put, `None` for a tombstone.
    held: Option<(Vec<u8>, Option<DocumentId>)>,
    entries: Vec<IndexScanEntry>,
}

impl LiveEntries {
    pub fn new(order: Order, limit: Option<usize>) -> Self {
        Self {
            order,
            limit: limit.unwrap_or(usize::MAX),
            held: None,
            entries: Vec::new(),
        }
    }

    /// Feeds the next stored version (`id` is `None` for a tombstone). Returns `true` when `limit`
    /// entries are settled and nothing more needs reading.
    pub fn push(&mut self, key: Vec<u8>, id: Option<DocumentId>) -> bool {
        if let Some((held_key, held_id)) = &mut self.held {
            if *held_key == key {
                if self.order == Order::Asc {
                    *held_id = id;
                }
                return false;
            }
        }
        self.settle();
        if self.entries.len() >= self.limit {
            return true;
        }
        self.held = Some((key, id));
        false
    }

    fn settle(&mut self) {
        if let Some((key, Some(id))) = self.held.take() {
            self.entries.push(IndexScanEntry { key, id });
        }
    }

    /// A chunk of the range has been read and more remains. The key being read may have more
    /// versions in the next chunk, so it is not settled: it is returned, for the next chunk to
    /// start from, and forgotten here.
    pub fn end_chunk(&mut self) -> Option<Vec<u8>> {
        self.held.take().map(|(key, _)| key)
    }

    /// The entries, once every version of the range (or of what is wanted of it) has been fed.
    pub fn finish(mut self) -> Vec<IndexScanEntry> {
        self.settle();
        self.entries.truncate(self.limit);
        self.entries
    }
}

/// Inclusive start, exclusive end. Convex `TimestampRange`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimestampRange {
    pub start_inclusive: Timestamp,
    pub end_exclusive: Timestamp,
}

/// Failure of a persistence call.
///
/// `Uncertain` means the process cannot tell whether the SQL commit landed.
/// Convex `track_and_write_to_persistence` treats that as fatal.
#[derive(Debug)]
pub enum PersistError {
    Definite(String),
    Uncertain(String),
    /// A newer writer holds the lease. Nothing was written, and this process must stop
    /// writing: it is a stale primary.
    Fenced(String),
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistError::Definite(msg) => write!(f, "{msg}"),
            PersistError::Uncertain(msg) => write!(f, "uncertain persistence write: {msg}"),
            PersistError::Fenced(msg) => write!(f, "fenced by a newer writer: {msg}"),
        }
    }
}

impl std::error::Error for PersistError {}

pub fn document_from_parts(
    id: Vec<u8>,
    ts: i64,
    table_id: Vec<u8>,
    json_value: Option<String>,
    deleted: bool,
    prev_ts: Option<i64>,
) -> Result<DocumentLogEntry, PersistError> {
    let ts = Timestamp::from_i64(ts).map_err(|_| PersistError::Definite(format!("bad ts {ts}")))?;
    let tablet = TabletId::from_bytes(&table_id)
        .map_err(|_| PersistError::Definite("table_id is not utf-8".into()))?;
    let prev_ts = match prev_ts {
        Some(value) => Some(
            Timestamp::from_i64(value)
                .map_err(|_| PersistError::Definite(format!("bad prev_ts {value}")))?,
        ),
        None => None,
    };
    let value = if deleted {
        None
    } else {
        Some(json_value.ok_or_else(|| {
            PersistError::Definite(format!("live document {} at {ts} has no json", tablet))
        })?)
    };
    Ok(DocumentLogEntry {
        ts,
        id: DocumentId::new(tablet, id),
        value,
        prev_ts,
    })
}

pub trait PersistenceReader: Send + Sync {
    fn load_documents(
        &self,
        range: TimestampRange,
        order: Order,
    ) -> Result<Vec<DocumentLogEntry>, PersistError>;

    fn latest_document(
        &self,
        id: &DocumentId,
        snapshot: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError>;

    /// Convex `previous_revisions`: newest row with `ts < at`.
    fn previous_revision(
        &self,
        id: &DocumentId,
        at: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError>;

    /// Convex `previous_revisions_of_documents` / `EXACT_REV_QUERY`.
    fn exact_revision(
        &self,
        id: &DocumentId,
        ts: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError>;

    fn document_history(
        &self,
        id: &DocumentId,
        snapshot: Timestamp,
    ) -> Result<Vec<DocumentLogEntry>, PersistError>;

    /// Latest index row per key with `ts <= read_timestamp`, skipping tombstones,
    /// joined to the document written in the same commit.
    /// Convex `PersistenceReader::index_scan`.
    fn index_scan(
        &self,
        index_id: &IndexId,
        read_timestamp: Timestamp,
        interval: &Interval,
        order: Order,
    ) -> Result<Vec<IndexScanRow>, PersistError>;

    /// The live rows of an index range as keys and document ids, in key order (`order`), at most
    /// `limit` of them: for each key the latest row at `read_timestamp`, unless that row is a
    /// tombstone. These are the rows `index_scan` returns, without the document each one is joined
    /// to, so a search that needs a page of the matches reads that page's documents with
    /// `latest_document` instead of every document in the range. A tombstone is written whenever a
    /// key stops applying to a document (it moved or the document was deleted), so a live row
    /// means a live document.
    ///
    /// The default derives the rows from `index_scan`, which is correct for any backend and saves
    /// nothing; a backend that can read its index table directly should override it, using
    /// `LiveEntries`.
    fn index_scan_ids(
        &self,
        index_id: &IndexId,
        read_timestamp: Timestamp,
        interval: &Interval,
        order: Order,
        limit: Option<usize>,
    ) -> Result<Vec<IndexScanEntry>, PersistError> {
        let mut rows: Vec<IndexScanEntry> = self
            .index_scan(index_id, read_timestamp, interval, order)?
            .into_iter()
            .map(|row| IndexScanEntry {
                key: row.key,
                id: row.document.id,
            })
            .collect();
        if let Some(limit) = limit {
            rows.truncate(limit);
        }
        Ok(rows)
    }

    fn get_persistence_global(
        &self,
        key: PersistenceGlobalKey,
    ) -> Result<Option<JsonValue>, PersistError>;

    fn max_ts(&self) -> Result<Option<Timestamp>, PersistError>;

    /// Every index row written in `range`, in timestamp order. This is what a commit put
    /// into the write log, which a reader in another process needs to invalidate live
    /// queries. Index-only commits (backfills) have rows here and no document.
    fn load_index_writes(
        &self,
        range: TimestampRange,
    ) -> Result<Vec<PersistenceIndexEntry>, PersistError>;
}

/// Outcome of asking to become the single writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lease {
    /// This process is the writer now, at this epoch.
    Held { epoch: u64 },
    /// Another process holds it.
    Busy,
}

pub trait Persistence: PersistenceReader {
    fn is_fresh(&self) -> bool;

    /// Tries once to become the only writer. A backend with nothing to coordinate with (an
    /// in-memory database) always succeeds with epoch 0, which fences nothing.
    ///
    /// On success the backend keeps the lease for the life of the process, bumps the writer
    /// epoch, and from then on rejects its own writes with `PersistError::Fenced` if a newer
    /// epoch appears. If the lease is lost it exits the process.
    fn try_acquire_writer_lease(&self) -> Result<Lease, PersistError> {
        Ok(Lease::Held { epoch: 0 })
    }

    /// Records that every timestamp up to `ts` is published. Never moves backwards.
    fn advance_published_ts(&self, _ts: Timestamp) -> Result<(), PersistError> {
        Ok(())
    }

    /// Connection pool counters, for backends that have one.
    fn pool_stats(&self) -> Option<PoolStats> {
        None
    }

    /// Waits up to `timeout` for the writer to say it published a commit, and returns whether
    /// it did. A reader calls this between refreshes so it need not poll quickly: it wakes as
    /// soon as the writer announces something, and still looks every `timeout` in case an
    /// announcement was missed. A backend with no such channel just sleeps.
    fn wait_for_publish(&self, timeout: std::time::Duration) -> bool {
        std::thread::sleep(timeout);
        false
    }

    /// Whether `wait_for_publish` is woken by the writer rather than only timing out.
    fn publishes_notifications(&self) -> bool {
        false
    }

    fn reader(&self) -> Arc<dyn PersistenceReader>;

    /// Documents and index rows in one SQL transaction.
    /// Convex `Persistence::write`.
    fn write(
        &self,
        documents: &[DocumentLogEntry],
        indexes: &[PersistenceIndexEntry],
        conflict_strategy: ConflictStrategy,
    ) -> Result<(), PersistError>;

    fn write_persistence_global(
        &self,
        key: PersistenceGlobalKey,
        value: &JsonValue,
    ) -> Result<(), PersistError>;
}

/// Search parameter row state. `ready` is what search may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamStatus {
    Pending,
    Ready,
    Unsupported,
}

impl ParamStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ParamStatus::Pending => "pending",
            ParamStatus::Ready => "ready",
            ParamStatus::Unsupported => "unsupported",
        }
    }

    pub fn parse(text: &str) -> Result<Self, PersistError> {
        match text {
            "pending" => Ok(ParamStatus::Pending),
            "ready" => Ok(ParamStatus::Ready),
            "unsupported" => Ok(ParamStatus::Unsupported),
            other => Err(PersistError::Definite(format!(
                "unknown search parameter status {other}"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPackage {
    pub id: i64,
    pub name: String,
    pub version: String,
    pub fhir_version: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredStructureDefinition {
    pub package_id: i64,
    pub url: String,
    pub type_name: String,
    pub kind: String,
    pub is_abstract: bool,
    pub json: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSearchParameter {
    pub package_id: i64,
    pub url: String,
    pub base: String,
    pub code: String,
    pub param_type: String,
    pub expression: Option<String>,
    pub status: ParamStatus,
    pub json: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredBackfill {
    pub id: i64,
    pub package_id: i64,
    pub watermark_ts: i64,
    pub tablet: String,
    pub cursor_id: Option<String>,
    pub done: bool,
    /// Writer epoch that last claimed the job: the process that is running it, or that died
    /// running it. `None` until the first claim.
    pub claimed_epoch: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct NewStructureDefinition {
    pub url: String,
    pub type_name: String,
    pub kind: String,
    pub is_abstract: bool,
    pub json: String,
}

#[derive(Clone, Debug)]
pub struct NewSearchParameter {
    pub url: String,
    pub base: String,
    pub code: String,
    pub param_type: String,
    pub expression: Option<String>,
    pub status: ParamStatus,
    pub json: String,
}

/// Whether two search parameter expressions for `base` mean the same thing. Packages write
/// the same path with or without the resource type (`Questionnaire.subjectType` in core,
/// `subjectType` in SDC), and union branches may be spaced differently.
pub fn same_expression(base: &str, left: &str, right: &str) -> bool {
    let normalize = |expression: &str| {
        let prefix = format!("{base}.");
        expression
            .split('|')
            .map(|branch| {
                let branch = branch.trim();
                branch
                    .strip_prefix(prefix.as_str())
                    .unwrap_or(branch)
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(" | ")
    };
    normalize(left) == normalize(right)
}

/// What to do when an incoming search parameter has the same base and code as an installed
/// one but a different expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpressionClash {
    /// Refuse the package.
    Conflict,
    /// Drop the incoming row; the installed one is the real definition.
    KeepInstalled,
    /// Remove the installed row; the incoming one is the real definition.
    ReplaceInstalled,
}

/// Experimental parameters are samples and drafts (FHIR: "for testing purposes, not real
/// usage"), so they yield to a non-experimental parameter whichever package is installed first.
/// Anything else with a different expression is a real conflict.
pub fn resolve_expression_clash(installed_json: &str, incoming_json: &str) -> ExpressionClash {
    let experimental = |json: &str| {
        serde_json::from_str::<JsonValue>(json)
            .ok()
            .and_then(|value| value.get("experimental").and_then(JsonValue::as_bool))
            == Some(true)
    };
    match (experimental(installed_json), experimental(incoming_json)) {
        (true, false) => ExpressionClash::ReplaceInstalled,
        (false, true) => ExpressionClash::KeepInstalled,
        _ => ExpressionClash::Conflict,
    }
}

/// An installed search parameter row for one base and code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledParam {
    pub package_id: i64,
    pub url: String,
    pub expression: String,
    pub status: String,
    pub json: String,
}

/// What installing a search parameter should do given the rows already stored for its base
/// and code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamPlan {
    /// Store the incoming row. `status` is the status of an installed row with the same
    /// expression, if there is one. `remove` lists installed rows to delete first.
    Insert {
        status: Option<ParamStatus>,
        remove: Vec<(i64, String)>,
    },
    /// Leave the installed definition alone and do not store the incoming row.
    Skip,
    /// Installed and incoming definitions disagree and neither may yield.
    Conflict,
}

/// Decide how `param` meets the `installed` rows that share its base and code. Only the rows
/// that actually disagree with it and are experimental are ever removed.
pub fn plan_search_parameter(
    param: &NewSearchParameter,
    installed: &[InstalledParam],
) -> Result<ParamPlan, PersistError> {
    let incoming = param.expression.clone().unwrap_or_default();
    let mut status = None;
    let mut remove = Vec::new();
    let (mut keep, mut conflict) = (false, false);
    for row in installed {
        if same_expression(&param.base, &row.expression, &incoming) {
            if status.is_none() {
                status = Some(ParamStatus::parse(&row.status)?);
            }
            continue;
        }
        match resolve_expression_clash(&row.json, &param.json) {
            ExpressionClash::Conflict => conflict = true,
            ExpressionClash::KeepInstalled => keep = true,
            ExpressionClash::ReplaceInstalled => remove.push((row.package_id, row.url.clone())),
        }
    }
    // Whatever order the rows come in, a real installed definition outranks the incoming
    // draft, and only then is a disagreement an error.
    Ok(if keep {
        ParamPlan::Skip
    } else if conflict {
        ParamPlan::Conflict
    } else {
        ParamPlan::Insert { status, remove }
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredTerminology {
    pub package_id: i64,
    pub resource_type: String,
    pub url: String,
    pub json: String,
}

#[derive(Clone, Debug)]
pub struct NewTerminology {
    pub resource_type: String,
    pub url: String,
    pub json: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredExpansion {
    pub package_id: i64,
    pub url: String,
    pub json: String,
}

#[derive(Clone, Debug)]
pub struct NewExpansion {
    pub url: String,
    pub json: String,
}

/// A change to the stored expansion of one value set.
#[derive(Clone, Debug)]
pub struct ExpansionChange {
    pub package_id: i64,
    pub url: String,
    /// The expansion document to store, or `None` to remove the stored one.
    pub json: Option<String>,
}

#[derive(Clone, Debug)]
pub struct NewPackage {
    pub name: String,
    pub version: String,
    pub fhir_version: String,
    pub structure_definitions: Vec<NewStructureDefinition>,
    pub search_parameters: Vec<NewSearchParameter>,
    pub terminology: Vec<NewTerminology>,
    pub expansions: Vec<NewExpansion>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingParam {
    pub base: String,
    pub code: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallOutcome {
    pub package_id: i64,
    pub inserted: bool,
    pub newly_pending: Vec<PendingParam>,
}

#[derive(Debug)]
pub enum InstallError {
    FhirVersion { existing: String, incoming: String },
    Expression { base: String, code: String },
    Persist(PersistError),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::FhirVersion { existing, incoming } => {
                write!(f, "fhirVersion {incoming} does not match {existing}")
            }
            InstallError::Expression { base, code } => {
                write!(f, "search parameter {base}.{code} expression differs")
            }
            InstallError::Persist(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for InstallError {}

impl From<PersistError> for InstallError {
    fn from(err: PersistError) -> Self {
        InstallError::Persist(err)
    }
}

/// Direct SQL for packages, structure definitions, and search parameters.
/// Resource transactions do not use this trait.
pub trait ConformanceStore: Send + Sync {
    /// The counter every conformance change bumps. A process that serves from an in-memory
    /// catalog compares it with the value it loaded to know when to reload.
    fn catalog_version(&self) -> Result<u64, PersistError>;

    /// Adds one to the catalog version and returns the new value.
    fn bump_catalog_version(&self) -> Result<u64, PersistError>;

    /// The counter every conformance change bumps, including ones that leave search parameters
    /// pending. A standby follows this, not the version.
    fn catalog_revision(&self) -> Result<u64, PersistError>;

    /// Adds one to the catalog revision and returns the new value.
    fn bump_catalog_revision(&self) -> Result<u64, PersistError>;

    fn load_packages(&self) -> Result<Vec<StoredPackage>, PersistError>;

    fn load_structure_definitions(&self) -> Result<Vec<StoredStructureDefinition>, PersistError>;

    fn load_search_parameters(&self) -> Result<Vec<StoredSearchParameter>, PersistError>;

    fn load_terminology(&self) -> Result<Vec<StoredTerminology>, PersistError>;

    fn load_expansions(&self) -> Result<Vec<StoredExpansion>, PersistError>;

    /// The terminology rows of one package. The full load returns every installed package's
    /// JSON (the whole R4 core is thousands of resources); a caller that only wants the rows
    /// written through the API, which are few, should not pay for that. Backends override this
    /// with a query that filters in the database.
    fn load_terminology_of(&self, package_id: i64) -> Result<Vec<StoredTerminology>, PersistError> {
        Ok(self
            .load_terminology()?
            .into_iter()
            .filter(|row| row.package_id == package_id)
            .collect())
    }

    /// The search parameter rows of one package. See [`ConformanceStore::load_terminology_of`].
    fn load_search_parameters_of(
        &self,
        package_id: i64,
    ) -> Result<Vec<StoredSearchParameter>, PersistError> {
        Ok(self
            .load_search_parameters()?
            .into_iter()
            .filter(|row| row.package_id == package_id)
            .collect())
    }
    /// The terminology rows for these URLs, from every package. A URL matches a row stored
    /// under exactly that key, and a bare URL also matches the versions of it (`url|version`),
    /// since two versions of a code system are two rows. A write checks one URL, so it reads a
    /// few rows, not every code system and value set installed.
    fn load_terminology_for(&self, urls: &[String])
        -> Result<Vec<StoredTerminology>, PersistError>;

    /// Apply changes to stored expansions in one transaction, leaving every other expansion
    /// alone. `replace_expansions` rewrites a whole package.
    fn apply_expansions(&self, changes: &[ExpansionChange]) -> Result<(), PersistError>;

    /// Package row for resources authored through the FHIR API.
    fn ensure_local_package(&self, fhir_version: &str) -> Result<i64, PersistError>;

    /// Returns whether the index must be rebuilt. An unchanged expression keeps its status.
    fn upsert_search_parameter(
        &self,
        package_id: i64,
        param: &NewSearchParameter,
    ) -> Result<bool, PersistError>;

    fn delete_search_parameter(&self, package_id: i64, url: &str) -> Result<(), PersistError>;

    fn upsert_terminology(&self, package_id: i64, row: &NewTerminology)
        -> Result<(), PersistError>;

    fn delete_terminology(&self, package_id: i64, url: &str) -> Result<(), PersistError>;

    fn replace_expansions(
        &self,
        package_id: i64,
        expansions: &[NewExpansion],
    ) -> Result<(), PersistError>;

    fn install_package(&self, package: &NewPackage) -> Result<InstallOutcome, InstallError>;

    fn mark_base_ready(&self, package_id: i64, base: &str) -> Result<(), PersistError>;

    /// Marks the parameters snapshotted onto this backfill ready.
    /// A backfill with no snapshot marks every pending parameter on `base`.
    fn complete_backfill(
        &self,
        backfill_id: i64,
        package_id: i64,
        base: &str,
    ) -> Result<(), PersistError>;

    fn set_search_parameter_status(
        &self,
        package_id: i64,
        url: &str,
        base: &str,
        status: ParamStatus,
    ) -> Result<(), PersistError>;

    /// `urls` limits which parameters this job may mark ready.
    /// An empty list snapshots every parameter that is pending for `tablet`.
    fn insert_backfill(
        &self,
        package_id: i64,
        watermark_ts: i64,
        tablet: &str,
        urls: &[String],
    ) -> Result<i64, PersistError>;

    fn update_backfill(
        &self,
        id: i64,
        cursor_id: Option<&str>,
        done: bool,
    ) -> Result<(), PersistError>;

    /// Records that this writer is the one running backfill `id`, taking it over from an
    /// earlier writer that died with it half done. Returns false if the job is finished and
    /// there is nothing to run. Fails with `PersistError::Fenced` if a *newer* writer has
    /// already claimed it: this process is the stale one.
    fn claim_backfill(&self, id: i64) -> Result<bool, PersistError>;

    fn load_backfills(&self) -> Result<Vec<StoredBackfill>, PersistError>;
}

#[cfg(test)]
mod clash_tests {
    use super::*;

    #[test]
    fn expressions_match_with_or_without_the_resource_type() {
        assert!(same_expression(
            "Questionnaire",
            "Questionnaire.subjectType",
            "subjectType"
        ));
        assert!(same_expression(
            "Patient",
            "Patient.name|Patient.alias",
            "name | alias"
        ));
        assert!(!same_expression("Patient", "Patient.name", "Patient.alias"));
        assert!(!same_expression("Patient", "Observation.name", "name"));
    }

    fn param(expression: &str, experimental: bool) -> NewSearchParameter {
        NewSearchParameter {
            url: "http://example.org/new".into(),
            base: "Patient".into(),
            code: "maiden".into(),
            param_type: "string".into(),
            expression: Some(expression.into()),
            status: ParamStatus::Pending,
            json: format!(r#"{{"experimental":{experimental}}}"#),
        }
    }

    fn row(package_id: i64, expression: &str, experimental: bool) -> InstalledParam {
        InstalledParam {
            package_id,
            url: format!("http://example.org/{package_id}"),
            expression: expression.into(),
            status: "ready".into(),
            json: format!(r#"{{"experimental":{experimental}}}"#),
        }
    }

    #[test]
    fn only_experimental_rows_that_disagree_are_removed() {
        let incoming = param("Patient.real", false);
        let installed = [
            row(1, "Patient.draft", true),
            row(2, "Patient.real", false),
            row(3, "Patient.other", true),
        ];
        match plan_search_parameter(&incoming, &installed).unwrap() {
            ParamPlan::Insert { status, remove } => {
                assert_eq!(status, Some(ParamStatus::Ready));
                assert_eq!(remove.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [1, 3]);
            }
            other => panic!("{other:?}"),
        }
        let real_installed = [row(1, "Patient.draft", true), row(2, "Patient.real", false)];
        let experimental = param("Patient.draft2", true);
        assert_eq!(
            plan_search_parameter(&experimental, &real_installed).unwrap(),
            ParamPlan::Skip
        );
        let clash = [row(1, "Patient.a", false)];
        assert_eq!(
            plan_search_parameter(&param("Patient.b", false), &clash).unwrap(),
            ParamPlan::Conflict
        );
    }

    #[test]
    fn experimental_parameters_yield_to_real_ones() {
        let sample = r#"{"experimental":true}"#;
        let real = r#"{"status":"active"}"#;
        assert_eq!(
            resolve_expression_clash(sample, real),
            ExpressionClash::ReplaceInstalled
        );
        assert_eq!(
            resolve_expression_clash(real, sample),
            ExpressionClash::KeepInstalled
        );
        assert_eq!(
            resolve_expression_clash(real, real),
            ExpressionClash::Conflict
        );
        assert_eq!(
            resolve_expression_clash(sample, sample),
            ExpressionClash::Conflict
        );
        assert_eq!(
            resolve_expression_clash("not json", real),
            ExpressionClash::Conflict
        );
    }
}

/// Rows of the SMART authorization server: clients, codes, tokens, login sessions.
///
/// These live in their own table, outside the FHIR document log, so they never appear in
/// `_history` or `/sync` and a delete really removes the row. `kind` names the row type and
/// `id` is unique within it. `exp` is a Unix time in seconds; rows at or past it are dead.
pub trait AuthStore: Send + Sync {
    /// Insert or replace a row.
    fn auth_put(&self, kind: &str, id: &str, json: &str, exp: u64) -> Result<(), PersistError>;
    /// Insert a row unless a live one already exists. Returns whether it was inserted.
    fn auth_insert(
        &self,
        kind: &str,
        id: &str,
        json: &str,
        exp: u64,
        now: u64,
    ) -> Result<bool, PersistError>;
    fn auth_get(&self, kind: &str, id: &str, now: u64) -> Result<Option<String>, PersistError>;
    /// Delete a row and return it if it was live. Of concurrent callers, at most one gets it.
    fn auth_take(&self, kind: &str, id: &str, now: u64) -> Result<Option<String>, PersistError>;
    /// Delete a row only if it still holds exactly `json`. Returns whether it was deleted, so a
    /// caller that read a bad row cannot remove a good one another process wrote since.
    fn auth_delete_if(&self, kind: &str, id: &str, json: &str) -> Result<bool, PersistError>;
    /// Every live row of `kind`.
    fn auth_list(&self, kind: &str, now: u64) -> Result<Vec<String>, PersistError>;
    /// Delete every dead row.
    fn auth_prune(&self, now: u64) -> Result<(), PersistError>;
}

/// SQL `exp` column value: Unix seconds, clamped to the signed 64-bit range.
pub fn auth_exp(exp: u64) -> i64 {
    i64::try_from(exp).unwrap_or(i64::MAX)
}

/// [`AuthStore`] held in process memory, for tests and embedders without a database.
#[derive(Default)]
pub struct MemoryAuthStore {
    rows: std::sync::Mutex<std::collections::HashMap<(String, String), (String, u64)>>,
}

impl AuthStore for MemoryAuthStore {
    fn auth_put(&self, kind: &str, id: &str, json: &str, exp: u64) -> Result<(), PersistError> {
        let mut rows = self.rows.lock().expect("auth");
        rows.insert((kind.into(), id.into()), (json.into(), exp));
        Ok(())
    }

    fn auth_insert(
        &self,
        kind: &str,
        id: &str,
        json: &str,
        exp: u64,
        now: u64,
    ) -> Result<bool, PersistError> {
        let mut rows = self.rows.lock().expect("auth");
        let key = (kind.to_string(), id.to_string());
        if rows.get(&key).is_some_and(|(_, live)| *live > now) {
            return Ok(false);
        }
        rows.insert(key, (json.into(), exp));
        Ok(true)
    }

    fn auth_get(&self, kind: &str, id: &str, now: u64) -> Result<Option<String>, PersistError> {
        let rows = self.rows.lock().expect("auth");
        Ok(rows
            .get(&(kind.to_string(), id.to_string()))
            .filter(|(_, exp)| *exp > now)
            .map(|(json, _)| json.clone()))
    }

    fn auth_take(&self, kind: &str, id: &str, now: u64) -> Result<Option<String>, PersistError> {
        let mut rows = self.rows.lock().expect("auth");
        Ok(rows
            .remove(&(kind.to_string(), id.to_string()))
            .filter(|(_, exp)| *exp > now)
            .map(|(json, _)| json))
    }

    fn auth_delete_if(&self, kind: &str, id: &str, json: &str) -> Result<bool, PersistError> {
        let mut rows = self.rows.lock().expect("auth");
        let key = (kind.to_string(), id.to_string());
        if rows.get(&key).is_some_and(|(stored, _)| stored == json) {
            rows.remove(&key);
            return Ok(true);
        }
        Ok(false)
    }

    fn auth_list(&self, kind: &str, now: u64) -> Result<Vec<String>, PersistError> {
        let rows = self.rows.lock().expect("auth");
        Ok(rows
            .iter()
            .filter(|((row_kind, _), (_, exp))| row_kind == kind && *exp > now)
            .map(|(_, (json, _))| json.clone())
            .collect())
    }

    fn auth_prune(&self, now: u64) -> Result<(), PersistError> {
        self.rows
            .lock()
            .expect("auth")
            .retain(|_, (_, exp)| *exp > now);
        Ok(())
    }
}

/// Shared checks every [`AuthStore`] backend must pass.
pub mod auth {
    use super::AuthStore;

    pub fn exercise(store: &dyn AuthStore) {
        // A row is deleted by value only if it still holds that value.
        store
            .auth_put("delete-if", "d1", "json-a", u64::MAX)
            .unwrap();
        assert!(!store.auth_delete_if("delete-if", "d1", "json-b").unwrap());
        assert_eq!(
            store.auth_get("delete-if", "d1", 0).unwrap().as_deref(),
            Some("json-a")
        );
        assert!(store.auth_delete_if("delete-if", "d1", "json-a").unwrap());
        assert!(store.auth_get("delete-if", "d1", 0).unwrap().is_none());
        assert!(!store.auth_delete_if("delete-if", "d1", "json-a").unwrap());

        store.auth_put("code", "c1", "{\"a\":1}", 100).unwrap();
        assert_eq!(
            store.auth_get("code", "c1", 50).unwrap().as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(store.auth_get("code", "c1", 100).unwrap(), None, "expired");
        assert_eq!(
            store.auth_get("token", "c1", 50).unwrap(),
            None,
            "kinds are separate"
        );

        store.auth_put("code", "c1", "{\"a\":2}", 100).unwrap();
        assert_eq!(
            store.auth_take("code", "c1", 50).unwrap().as_deref(),
            Some("{\"a\":2}")
        );
        assert_eq!(
            store.auth_take("code", "c1", 50).unwrap(),
            None,
            "taken once"
        );

        store.auth_put("code", "old", "{}", 10).unwrap();
        assert_eq!(
            store.auth_take("code", "old", 50).unwrap(),
            None,
            "expired take"
        );

        assert!(store.auth_insert("jti", "j1", "{}", 100, 50).unwrap());
        assert!(
            !store.auth_insert("jti", "j1", "{}", 100, 50).unwrap(),
            "live duplicate"
        );
        assert!(
            store.auth_insert("jti", "j1", "{}", 300, 150).unwrap(),
            "expired is replaced"
        );

        store.auth_put("client", "a", "\"a\"", u64::MAX).unwrap();
        store.auth_put("client", "b", "\"b\"", u64::MAX).unwrap();
        let mut clients = store.auth_list("client", 50).unwrap();
        clients.sort();
        assert_eq!(clients, vec!["\"a\"", "\"b\""]);

        store.auth_put("token", "t", "{}", 60).unwrap();
        store.auth_prune(70).unwrap();
        assert_eq!(store.auth_get("token", "t", 0).unwrap(), None, "pruned");
        assert!(
            store.auth_get("client", "a", 70).unwrap().is_some(),
            "live rows survive"
        );
    }

    #[test]
    fn memory_store() {
        exercise(&super::MemoryAuthStore::default());
    }
}

/// Where a [`StoredJob`] is in its life. `Accepted` and `Running` are unfinished; `Completed`
/// and `Failed` are final. Which process is working on an unfinished job is not the status but
/// the lease (see [`JobStore`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStatus {
    Accepted,
    Running,
    Completed,
    Failed,
}

impl JobStatus {
    /// The text stored in the `status` column.
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Accepted => "accepted",
            JobStatus::Running => "running",
            JobStatus::Completed => "completed",
            JobStatus::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "accepted" => Some(JobStatus::Accepted),
            "running" => Some(JobStatus::Running),
            "completed" => Some(JobStatus::Completed),
            "failed" => Some(JobStatus::Failed),
            _ => None,
        }
    }

    pub fn finished(self) -> bool {
        matches!(self, JobStatus::Completed | JobStatus::Failed)
    }
}

/// One row of the `jobs` table. Times are Unix milliseconds supplied by the caller.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredJob {
    pub id: String,
    /// What the job is, for example `export`. The runner picks a handler by this.
    pub kind: String,
    pub status: JobStatus,
    /// Everything needed to run the job again from the start.
    pub request: JsonValue,
    /// Set when the job completes.
    pub result: Option<JsonValue>,
    /// Set when the job fails.
    pub error: Option<String>,
    /// How many times the job has been started. A job that keeps killing the process shows up
    /// here.
    pub attempts: u32,
    pub created_ms: u64,
    /// Set when the job completes or fails.
    pub completed_ms: Option<u64>,
    /// The process that holds the job, while it is unfinished and held.
    pub lease_owner: Option<String>,
    /// When that hold ends unless its owner renews it.
    pub lease_until_ms: Option<u64>,
}

impl StoredJob {
    /// Whether `owner` may take this job at `now_ms`: it is unfinished, and nobody holds it, or
    /// the hold has run out.
    pub fn claimable(&self, now_ms: u64) -> bool {
        !self.status.finished()
            && match (&self.lease_owner, self.lease_until_ms) {
                (Some(_), Some(until)) => until <= now_ms,
                _ => true,
            }
    }
}

/// A hold on a job: `owner` has it for `ms` from the moment it is taken.
#[derive(Clone, Copy, Debug)]
pub struct JobLease<'a> {
    pub owner: &'a str,
    pub ms: u64,
}

/// Background jobs that outlive the request that asked for them, and the process that ran them.
///
/// Rows live in their own `jobs` table, outside the FHIR document log. The store records state
/// and arbitrates who may work on a job; it does not run anything and does not read the clock,
/// so the caller passes `now_ms`.
///
/// **A job is worked on by whoever holds its lease.** A lease is an owner and an expiry. It is
/// taken in a single atomic step, so of any number of servers sharing one database only one gets
/// a job; it is kept alive by renewing it while the work goes on; and if its holder dies it
/// simply runs out, after which another server may take the job. Only the holder can record how
/// a job ended, so a worker that lost its lease cannot overwrite the result of the one that took
/// over. No lock is held while the work runs.
pub trait JobStore: Send + Sync {
    /// Insert a job as `accepted` with no attempts. Fails if `id` is already taken. With a
    /// `lease`, the job is held by its creator from the start, so no other server can take a job
    /// that was just made for this one.
    fn job_create(
        &self,
        id: &str,
        kind: &str,
        request: &JsonValue,
        now_ms: u64,
        lease: Option<JobLease<'_>>,
    ) -> Result<(), PersistError>;

    /// Take up to `limit` unfinished jobs of these `kinds` that nobody holds, oldest first, each
    /// for `lease_ms`. A job whose lease has run out is held by nobody. The jobs come back as
    /// they now are: `running`, one more attempt, leased to `owner`. Concurrent callers never
    /// get the same job.
    fn job_claim(
        &self,
        owner: &str,
        kinds: &[&str],
        now_ms: u64,
        lease_ms: u64,
        limit: u32,
    ) -> Result<Vec<StoredJob>, PersistError>;

    /// Start a job that `owner` already holds (one it created with a lease): `running`, one more
    /// attempt, the lease extended by `lease_ms`. `None` if the job is unknown, finished, or
    /// held by someone else.
    fn job_start(
        &self,
        id: &str,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<StoredJob>, PersistError>;

    /// Extend the lease on a job `owner` holds to `now_ms + lease_ms`. `false` means it is no
    /// longer held by `owner`: it finished, or the lease ran out and someone else took it.
    fn job_renew(
        &self,
        id: &str,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<bool, PersistError>;

    /// Record the end of a job `owner` holds: `Completed` with a `result`, or `Failed` with an
    /// `error`. The lease is released. `false` if the job is unknown, already finished, or held
    /// by someone else. Any other `status` is an error.
    fn job_finish(
        &self,
        id: &str,
        owner: &str,
        status: JobStatus,
        result: Option<&JsonValue>,
        error: Option<&str>,
        now_ms: u64,
    ) -> Result<bool, PersistError>;

    fn job_get(&self, id: &str) -> Result<Option<StoredJob>, PersistError>;

    /// The store's own clock in Unix milliseconds, when it has one that every server sharing it
    /// reads alike, such as a database server's. A lease is written by one server and judged by
    /// another, so the caller measures lease times on this clock rather than its own, which may
    /// not agree with the others'. `None` means the store has none, and the caller's will do.
    fn clock_ms(&self) -> Result<Option<u64>, PersistError> {
        Ok(None)
    }
}

/// SQL value for a job time: Unix milliseconds, clamped to the signed 64-bit range.
pub fn job_ms(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}

/// When a lease taken at `now_ms` for `lease_ms` runs out, as the SQL value.
pub fn lease_until(now_ms: u64, lease_ms: u64) -> i64 {
    job_ms(now_ms.saturating_add(lease_ms))
}

/// What [`JobStore::job_finish`] writes for the end of a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobEnd {
    pub status: &'static str,
    pub result: Option<String>,
    pub error: Option<String>,
    pub completed_ms: i64,
}

pub fn plan_job_end(
    status: JobStatus,
    result: Option<&JsonValue>,
    error: Option<&str>,
    now_ms: u64,
) -> Result<JobEnd, PersistError> {
    if !status.finished() {
        return Err(PersistError::Definite(
            "a job ends as completed or failed".into(),
        ));
    }
    Ok(JobEnd {
        status: status.as_str(),
        result: result.map(JsonValue::to_string),
        error: error.map(str::to_string),
        completed_ms: job_ms(now_ms),
    })
}

/// Build a [`StoredJob`] from the columns of a `jobs` row.
#[allow(clippy::too_many_arguments)]
pub fn job_from_parts(
    id: String,
    kind: String,
    status: &str,
    request: &str,
    result: Option<String>,
    error: Option<String>,
    attempts: i64,
    created_ms: i64,
    completed_ms: Option<i64>,
    lease_owner: Option<String>,
    lease_until_ms: Option<i64>,
) -> Result<StoredJob, PersistError> {
    let bad = |what: &str| PersistError::Definite(format!("job {id}: bad {what}"));
    let parse = |text: &str, what: &str| {
        serde_json::from_str::<JsonValue>(text)
            .map_err(|err| PersistError::Definite(format!("job {id}: {what} is not json: {err}")))
    };
    Ok(StoredJob {
        status: JobStatus::parse(status).ok_or_else(|| bad("status"))?,
        request: parse(request, "request")?,
        result: result
            .as_deref()
            .map(|text| parse(text, "result"))
            .transpose()?,
        error,
        attempts: u32::try_from(attempts).map_err(|_| bad("attempts"))?,
        created_ms: u64::try_from(created_ms).map_err(|_| bad("created_at"))?,
        completed_ms: completed_ms
            .map(|ms| u64::try_from(ms).map_err(|_| bad("completed_at")))
            .transpose()?,
        lease_owner,
        lease_until_ms: lease_until_ms
            .map(|ms| u64::try_from(ms).map_err(|_| bad("lease_until")))
            .transpose()?,
        kind,
        id,
    })
}

/// [`JobStore`] held in process memory. For tests and embedders without a database: nothing
/// in it survives the process, so it gives none of the restart recovery the real stores do.
#[derive(Default)]
pub struct MemoryJobStore {
    jobs: std::sync::Mutex<std::collections::BTreeMap<String, StoredJob>>,
}

impl JobStore for MemoryJobStore {
    fn job_create(
        &self,
        id: &str,
        kind: &str,
        request: &JsonValue,
        now_ms: u64,
        lease: Option<JobLease<'_>>,
    ) -> Result<(), PersistError> {
        let mut jobs = self.jobs.lock().expect("jobs");
        if jobs.contains_key(id) {
            return Err(PersistError::Definite(format!("job {id} already exists")));
        }
        jobs.insert(
            id.to_string(),
            StoredJob {
                id: id.to_string(),
                kind: kind.to_string(),
                status: JobStatus::Accepted,
                request: request.clone(),
                result: None,
                error: None,
                attempts: 0,
                created_ms: now_ms,
                completed_ms: None,
                lease_owner: lease.map(|lease| lease.owner.to_string()),
                lease_until_ms: lease.map(|lease| now_ms.saturating_add(lease.ms)),
            },
        );
        Ok(())
    }

    fn job_claim(
        &self,
        owner: &str,
        kinds: &[&str],
        now_ms: u64,
        lease_ms: u64,
        limit: u32,
    ) -> Result<Vec<StoredJob>, PersistError> {
        let mut jobs = self.jobs.lock().expect("jobs");
        let mut ids: Vec<(u64, String)> = jobs
            .values()
            .filter(|job| job.claimable(now_ms) && kinds.contains(&job.kind.as_str()))
            .map(|job| (job.created_ms, job.id.clone()))
            .collect();
        ids.sort();
        ids.truncate(limit as usize);
        let mut claimed = Vec::new();
        for (_, id) in ids {
            let job = jobs.get_mut(&id).expect("listed above");
            job.status = JobStatus::Running;
            job.attempts += 1;
            job.lease_owner = Some(owner.to_string());
            job.lease_until_ms = Some(now_ms.saturating_add(lease_ms));
            claimed.push(job.clone());
        }
        Ok(claimed)
    }

    fn job_start(
        &self,
        id: &str,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<StoredJob>, PersistError> {
        let mut jobs = self.jobs.lock().expect("jobs");
        let Some(job) = jobs
            .get_mut(id)
            .filter(|job| !job.status.finished() && job.lease_owner.as_deref() == Some(owner))
        else {
            return Ok(None);
        };
        job.status = JobStatus::Running;
        job.attempts += 1;
        job.lease_until_ms = Some(now_ms.saturating_add(lease_ms));
        Ok(Some(job.clone()))
    }

    fn job_renew(
        &self,
        id: &str,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<bool, PersistError> {
        let mut jobs = self.jobs.lock().expect("jobs");
        let Some(job) = jobs
            .get_mut(id)
            .filter(|job| !job.status.finished() && job.lease_owner.as_deref() == Some(owner))
        else {
            return Ok(false);
        };
        job.lease_until_ms = Some(now_ms.saturating_add(lease_ms));
        Ok(true)
    }

    fn job_finish(
        &self,
        id: &str,
        owner: &str,
        status: JobStatus,
        result: Option<&JsonValue>,
        error: Option<&str>,
        now_ms: u64,
    ) -> Result<bool, PersistError> {
        plan_job_end(status, result, error, now_ms)?;
        let mut jobs = self.jobs.lock().expect("jobs");
        let Some(job) = jobs
            .get_mut(id)
            .filter(|job| !job.status.finished() && job.lease_owner.as_deref() == Some(owner))
        else {
            return Ok(false);
        };
        job.status = status;
        job.result = result.cloned();
        job.error = error.map(str::to_string);
        job.completed_ms = Some(now_ms);
        job.lease_owner = None;
        job.lease_until_ms = None;
        Ok(true)
    }

    fn job_get(&self, id: &str) -> Result<Option<StoredJob>, PersistError> {
        Ok(self.jobs.lock().expect("jobs").get(id).cloned())
    }
}

/// Shared checks every [`JobStore`] backend must pass.
pub mod jobs {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use serde_json::json;

    use super::{JobLease, JobStatus, JobStore, StoredJob};

    /// The lifecycle of a job and its lease. `tag` keeps this run's jobs apart on a database
    /// that already holds others: every job here has a kind of its own, and only that kind is
    /// ever claimed.
    pub fn exercise(store: &dyn JobStore, tag: &str) {
        let id = |name: &str| format!("{tag}-{name}");
        let kind = format!("{tag}-kind");
        let other = format!("{tag}-other");
        let kinds = [kind.as_str()];
        let claim = |owner: &str, now: u64, lease: u64, limit: u32| {
            store.job_claim(owner, &kinds, now, lease, limit).unwrap()
        };

        // A new job is accepted, unheld, with nothing recorded yet.
        let request = json!({"type": "Patient", "id": null, "nested": {"n": [1, 2.5, "\u{e9}"]}});
        store
            .job_create(&id("a"), &kind, &request, 1000, None)
            .unwrap();
        let job = store.job_get(&id("a")).unwrap().expect("created");
        assert_eq!(job.kind, kind);
        assert_eq!(job.status, JobStatus::Accepted);
        assert_eq!(job.request, request);
        assert_eq!((job.result.clone(), job.error.clone()), (None, None));
        assert_eq!((job.attempts, job.created_ms), (0, 1000));
        assert_eq!(
            (job.completed_ms, job.lease_owner, job.lease_until_ms),
            (None, None, None)
        );
        assert!(
            store
                .job_create(&id("a"), &kind, &json!({}), 1001, None)
                .is_err(),
            "ids are unique"
        );
        assert_eq!(store.job_get(&id("nope")).unwrap(), None);

        // Claiming takes it: running, one attempt, held until the lease runs out.
        let got = claim("A", 1000, 100, 10);
        assert_eq!(got.len(), 1);
        let job = &got[0];
        assert_eq!(job.id, id("a"));
        assert_eq!((job.status, job.attempts), (JobStatus::Running, 1));
        assert_eq!(job.lease_owner.as_deref(), Some("A"));
        assert_eq!(job.lease_until_ms, Some(1100));
        assert_eq!(store.job_get(&id("a")).unwrap().unwrap(), *job);

        // While A holds it nobody else can take it, up to the moment the lease runs out.
        assert!(claim("B", 1000, 100, 10).is_empty(), "held");
        assert!(
            claim("B", 1099, 100, 10).is_empty(),
            "held until the last millisecond"
        );
        assert!(
            claim("A", 1050, 100, 10).is_empty(),
            "not even by its own holder again"
        );
        let got = claim("B", 1100, 100, 10);
        assert_eq!(got.len(), 1, "an expired lease is nobody's");
        assert_eq!(
            (got[0].attempts, got[0].lease_owner.as_deref()),
            (2, Some("B"))
        );

        // Only the holder renews, and only the holder can end it.
        assert!(
            !store.job_renew(&id("a"), "A", 1120, 100).unwrap(),
            "A lost it"
        );
        assert!(store.job_renew(&id("a"), "B", 1150, 100).unwrap());
        assert_eq!(
            store.job_get(&id("a")).unwrap().unwrap().lease_until_ms,
            Some(1250)
        );
        assert!(
            claim("A", 1200, 100, 10).is_empty(),
            "renewing kept it from A"
        );
        let result = json!({"file": "a.ndjson"});
        assert!(
            !store
                .job_finish(
                    &id("a"),
                    "A",
                    JobStatus::Completed,
                    Some(&result),
                    None,
                    1300
                )
                .unwrap(),
            "a worker that lost its lease cannot record an outcome"
        );
        assert_eq!(
            store.job_get(&id("a")).unwrap().unwrap().status,
            JobStatus::Running
        );
        assert!(store
            .job_finish(
                &id("a"),
                "B",
                JobStatus::Completed,
                Some(&result),
                None,
                1300
            )
            .unwrap());
        let job = store.job_get(&id("a")).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Completed);
        assert_eq!(job.result, Some(result.clone()));
        assert_eq!(
            (job.error.clone(), job.attempts, job.completed_ms),
            (None, 2, Some(1300))
        );
        assert_eq!(
            (job.lease_owner.clone(), job.lease_until_ms),
            (None, None),
            "released"
        );

        // A finished job is never changed, renewed, started or claimed again.
        assert!(!store
            .job_finish(&id("a"), "B", JobStatus::Failed, None, Some("late"), 1400)
            .unwrap());
        assert!(!store.job_renew(&id("a"), "B", 1400, 100).unwrap());
        assert_eq!(store.job_start(&id("a"), "B", 1400, 100).unwrap(), None);
        assert!(claim("A", 10_000, 100, 10).is_empty());
        let job = store.job_get(&id("a")).unwrap().unwrap();
        assert_eq!(
            (job.status, job.result, job.attempts),
            (JobStatus::Completed, Some(result), 2)
        );

        // A job made for its creator is held from the start and only the creator starts it.
        store
            .job_create(
                &id("b"),
                &kind,
                &json!({}),
                2000,
                Some(JobLease { owner: "C", ms: 50 }),
            )
            .unwrap();
        let made = store.job_get(&id("b")).unwrap().unwrap();
        assert_eq!((made.status, made.attempts), (JobStatus::Accepted, 0));
        assert_eq!(
            (made.lease_owner.as_deref(), made.lease_until_ms),
            (Some("C"), Some(2050))
        );
        assert!(
            claim("A", 2010, 100, 10).is_empty(),
            "nobody takes a job made for another"
        );
        assert_eq!(store.job_start(&id("b"), "A", 2010, 100).unwrap(), None);
        let started = store
            .job_start(&id("b"), "C", 2020, 100)
            .unwrap()
            .expect("C started it");
        assert_eq!((started.status, started.attempts), (JobStatus::Running, 1));
        assert_eq!(started.lease_until_ms, Some(2120));
        // If its creator never gets to it, the lease runs out and another server does.
        let got = claim("A", 2200, 100, 10);
        assert_eq!(
            (got.len(), got[0].attempts, got[0].lease_owner.as_deref()),
            (1, 2, Some("A"))
        );

        // Failing keeps the error and no result; other statuses are refused.
        assert!(store
            .job_finish(&id("b"), "A", JobStatus::Failed, None, Some("boom"), 2300)
            .unwrap());
        let job = store.job_get(&id("b")).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Failed);
        assert_eq!(
            (job.error.as_deref(), job.result, job.completed_ms),
            (Some("boom"), None, Some(2300))
        );
        store
            .job_create(&id("c"), &kind, &json!({}), 3000, None)
            .unwrap();
        assert!(claim("A", 3000, 100, 10).len() == 1);
        for status in [JobStatus::Running, JobStatus::Accepted] {
            assert!(store
                .job_finish(&id("c"), "A", status, None, None, 3001)
                .is_err());
        }
        assert!(store
            .job_finish(&id("c"), "A", JobStatus::Failed, None, Some("done"), 3002)
            .unwrap());

        // Unknown ids change nothing.
        assert!(!store.job_renew(&id("nope"), "A", 1, 1).unwrap());
        assert_eq!(store.job_start(&id("nope"), "A", 1, 1).unwrap(), None);
        assert!(!store
            .job_finish(&id("nope"), "A", JobStatus::Failed, None, Some("x"), 1)
            .unwrap());

        // Only the kinds asked for are taken, oldest first, up to the limit.
        store
            .job_create(&id("x"), &other, &json!({}), 3100, None)
            .unwrap();
        for (name, at) in [("d1", 4000), ("d2", 4001), ("d3", 4002)] {
            store
                .job_create(&id(name), &kind, &json!({}), at, None)
                .unwrap();
        }
        let first: Vec<String> = claim("D", 5000, 100, 2)
            .into_iter()
            .map(|job| job.id)
            .collect();
        assert_eq!(first, vec![id("d1"), id("d2")]);
        let rest: Vec<String> = claim("D", 5000, 100, 2)
            .into_iter()
            .map(|job| job.id)
            .collect();
        assert_eq!(rest, vec![id("d3")]);
        assert_eq!(
            store.job_get(&id("x")).unwrap().unwrap().status,
            JobStatus::Accepted
        );
        let got = store
            .job_claim("E", &[other.as_str()], 5000, 100, 10)
            .unwrap();
        assert_eq!(
            got.iter().map(|job| job.id.clone()).collect::<Vec<_>>(),
            vec![id("x")]
        );

        // Leave nothing unfinished behind on a shared database.
        for (name, owner) in [("d1", "D"), ("d2", "D"), ("d3", "D")] {
            assert!(store
                .job_finish(
                    &id(name),
                    owner,
                    JobStatus::Failed,
                    None,
                    Some("cleanup"),
                    9000
                )
                .unwrap());
        }
        assert!(store
            .job_finish(
                &id("x"),
                "E",
                JobStatus::Failed,
                None,
                Some("cleanup"),
                9000
            )
            .unwrap());
    }

    /// Several servers claiming from one table at once: every job goes to exactly one of them.
    /// `open` makes a separate handle to the same table each time it is called, as a second
    /// server would have; for a database with row locking, that is what exercises `SKIP LOCKED`.
    pub fn exercise_competing_claimers(open: &(dyn Fn() -> Arc<dyn JobStore> + Sync), tag: &str) {
        const JOBS: usize = 40;
        const SERVERS: usize = 6;
        let kind = format!("{tag}-race");
        {
            let store = open();
            for index in 0..JOBS {
                store
                    .job_create(
                        &format!("{tag}-race-{index:03}"),
                        &kind,
                        &json!({"n": index}),
                        index as u64,
                        None,
                    )
                    .unwrap();
            }
        }
        let taken: Vec<(String, Vec<StoredJob>)> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..SERVERS)
                .map(|server| {
                    let kind = kind.clone();
                    scope.spawn(move || {
                        let store = open();
                        let owner = format!("{tag}-server-{server}");
                        let mut mine = Vec::new();
                        loop {
                            let got = store
                                .job_claim(&owner, &[kind.as_str()], 1_000_000, 1_000_000, 3)
                                .unwrap();
                            if got.is_empty() {
                                break;
                            }
                            mine.extend(got);
                        }
                        (owner, mine)
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect()
        });
        let mut owners: BTreeMap<String, String> = BTreeMap::new();
        for (owner, jobs) in &taken {
            for job in jobs {
                assert_eq!(job.attempts, 1, "{} was claimed more than once", job.id);
                assert!(
                    owners.insert(job.id.clone(), owner.clone()).is_none(),
                    "{} went to two servers",
                    job.id
                );
            }
        }
        assert_eq!(owners.len(), JOBS, "every job went to one server");
        assert!(
            taken.iter().filter(|(_, jobs)| !jobs.is_empty()).count() >= 1,
            "at least one server took work"
        );
        // And the table agrees about who has each.
        let store = open();
        for (id, owner) in &owners {
            let row = store.job_get(id).unwrap().unwrap();
            assert_eq!(row.lease_owner.as_deref(), Some(owner.as_str()), "{id}");
            assert!(store
                .job_finish(id, owner, JobStatus::Completed, None, None, 2_000_000)
                .unwrap());
        }
    }

    #[test]
    fn memory_store() {
        exercise(&super::MemoryJobStore::default(), "memory");
    }

    #[test]
    fn memory_store_competing_claimers() {
        let store: Arc<dyn JobStore> = Arc::new(super::MemoryJobStore::default());
        exercise_competing_claimers(&|| store.clone(), "memory");
    }
}

pub mod conformance {
    use super::*;

    /// Reading terminology by URL and changing one stored expansion at a time. `tag` keeps this
    /// run's URLs and package names apart on a database that already holds earlier runs.
    pub fn exercise_terminology_changes(store: &dyn ConformanceStore, tag: &str) {
        let url = |name: &str| format!("http://example.org/{tag}/{name}");
        let row = |name: &str, kind: &str| NewTerminology {
            resource_type: kind.into(),
            url: url(name),
            json: format!(r#"{{"resourceType":"{kind}","url":"{}"}}"#, url(name)),
        };
        let package = |name: &str, terminology: Vec<NewTerminology>| NewPackage {
            name: format!("{name}.{tag}"),
            version: "1.0.0".into(),
            fhir_version: "4.0.1".into(),
            structure_definitions: Vec::new(),
            search_parameters: Vec::new(),
            terminology,
            expansions: Vec::new(),
        };
        let first = store
            .install_package(&package(
                "one",
                vec![
                    row("alpha", "ValueSet"),
                    row("beta", "CodeSystem"),
                    row("gamma|1", "CodeSystem"),
                    row("gamma|2", "CodeSystem"),
                ],
            ))
            .unwrap();
        assert!(first.inserted);
        let first = first.package_id;
        let second = store
            .install_package(&package("two", vec![row("alpha", "ValueSet")]))
            .unwrap();
        assert!(second.inserted);
        let second = second.package_id;

        // Only the URLs asked for, from every package that holds them, and none that do not exist.
        let found = store
            .load_terminology_for(&[url("alpha"), url("beta"), url("nowhere")])
            .unwrap();
        let mut got: Vec<(i64, String)> = found
            .iter()
            .map(|stored| (stored.package_id, stored.url.clone()))
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                (first, url("alpha")),
                (first, url("beta")),
                (second, url("alpha"))
            ]
        );
        assert_eq!(found[0].json, row("alpha", "ValueSet").json);
        // A bare URL finds every version of it; a versioned one finds that version only.
        let versions = |urls: &[String]| -> Vec<String> {
            store
                .load_terminology_for(urls)
                .unwrap()
                .into_iter()
                .map(|stored| stored.url)
                .collect()
        };
        assert_eq!(
            versions(&[url("gamma")]),
            vec![url("gamma|1"), url("gamma|2")]
        );
        assert_eq!(versions(&[url("gamma|2")]), vec![url("gamma|2")]);
        assert!(versions(&[url("gamma|3")]).is_empty());
        assert!(store.load_terminology_for(&[]).unwrap().is_empty());
        // A long list is fine: one query cannot take an unbounded number of parameters.
        let many: Vec<String> = (0..1200)
            .map(|index| url(&format!("filler-{index}")))
            .chain([url("beta")])
            .collect();
        let found = store.load_terminology_for(&many).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].url, url("beta"));

        // Expansion changes: store, replace, remove, and leave the rest alone.
        let mine = |store: &dyn ConformanceStore| -> Vec<(i64, String, String)> {
            store
                .load_expansions()
                .unwrap()
                .into_iter()
                .filter(|row| row.url.contains(&format!("/{tag}/")))
                .map(|row| (row.package_id, row.url, row.json))
                .collect()
        };
        assert!(mine(store).is_empty());
        let change = |package_id: i64, name: &str, json: Option<&str>| ExpansionChange {
            package_id,
            url: url(name),
            json: json.map(str::to_string),
        };
        store.apply_expansions(&[]).unwrap();
        store
            .apply_expansions(&[
                change(first, "alpha", Some("one")),
                change(second, "alpha", Some("two")),
                change(first, "gamma", Some("three")),
            ])
            .unwrap();
        let mut rows = mine(store);
        rows.sort();
        assert_eq!(
            rows,
            vec![
                (first, url("alpha"), "one".to_string()),
                (first, url("gamma"), "three".to_string()),
                (second, url("alpha"), "two".to_string()),
            ]
        );
        // The same key is replaced, not duplicated; a removal takes only its own package's row;
        // removing what is not there is not an error.
        store
            .apply_expansions(&[
                change(first, "alpha", Some("replaced")),
                change(second, "alpha", None),
                change(first, "never-stored", None),
            ])
            .unwrap();
        let mut rows = mine(store);
        rows.sort();
        assert_eq!(
            rows,
            vec![
                (first, url("alpha"), "replaced".to_string()),
                (first, url("gamma"), "three".to_string()),
            ]
        );
    }

    /// Search parameters that meet in different packages: a draft yields to a real definition
    /// in either install order, and two real ones that disagree are refused. `tag` keeps
    /// package names unique on a database that already holds earlier runs.
    pub fn exercise_parameter_clashes(store: &dyn ConformanceStore, tag: &str) {
        let package = |name: &str, expression: &str, experimental: bool| NewPackage {
            name: format!("{name}.{tag}"),
            version: "1.0.0".into(),
            fhir_version: "4.0.1".into(),
            structure_definitions: Vec::new(),
            search_parameters: vec![NewSearchParameter {
                url: format!("http://example.org/sp/{name}.{tag}"),
                base: "Patient".into(),
                code: format!("maiden-{tag}"),
                param_type: "string".into(),
                expression: Some(expression.into()),
                status: ParamStatus::Pending,
                json: format!(r#"{{"experimental":{experimental}}}"#),
            }],
            terminology: Vec::new(),
            expansions: Vec::new(),
        };
        let stored = || -> Vec<StoredSearchParameter> {
            store
                .load_search_parameters()
                .unwrap()
                .into_iter()
                .filter(|row| row.code == format!("maiden-{tag}"))
                .collect()
        };
        // Draft first, then the real definition replaces it.
        store
            .install_package(&package("draft", "Patient.extension('d')", true))
            .unwrap();
        store
            .install_package(&package("real", "Patient.extension('r').value", false))
            .unwrap();
        let rows = stored();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            rows[0].expression.as_deref(),
            Some("Patient.extension('r').value")
        );
        // A later draft yields to the installed real one.
        store
            .install_package(&package("draft2", "Patient.extension('x')", true))
            .unwrap();
        assert_eq!(stored().len(), 1);
        // The same expression without the resource type is the same definition.
        store
            .install_package(&package("same", "extension('r').value", false))
            .unwrap();
        assert!(stored()
            .iter()
            .all(|row| row.expression.as_deref() != Some("Patient.other")));
        // Two real definitions that disagree are refused.
        let err = store
            .install_package(&package("clash", "Patient.other", false))
            .unwrap_err();
        assert!(matches!(err, InstallError::Expression { .. }), "{err}");
    }
    use refhir_types::TabletId;

    /// Feature 1: a revision and its index row are visible at that timestamp,
    /// and a later tombstone hides them without erasing the earlier snapshot.
    pub fn exercise(p: &dyn Persistence) {
        assert!(p.is_fresh());
        let tablet = TabletId::new("fixture");
        let id = DocumentId::new(tablet.clone(), b"doc-1".as_slice());
        let index = IndexId::new("fixture/by_id");
        let key = b"doc-1".to_vec();
        let ts1 = Timestamp::from_u64(1).unwrap();
        let ts2 = Timestamp::from_u64(2).unwrap();

        p.write(
            &[DocumentLogEntry {
                ts: ts1,
                id: id.clone(),
                value: Some("{\"hello\":\"world\"}".into()),
                prev_ts: None,
            }],
            &[PersistenceIndexEntry {
                ts: ts1,
                index_id: index.clone(),
                key: key.clone(),
                value: Some(id.clone()),
            }],
            ConflictStrategy::Error,
        )
        .expect("insert");

        assert!(p
            .index_scan(&index, Timestamp::ZERO, &Interval::all(), Order::Asc)
            .unwrap()
            .is_empty());

        let at_1 = p
            .index_scan(&index, ts1, &Interval::all(), Order::Asc)
            .unwrap();
        assert_eq!(at_1.len(), 1);
        assert_eq!(at_1[0].key, key);
        assert_eq!(at_1[0].document.value, "{\"hello\":\"world\"}");
        assert_eq!(at_1[0].document.ts, ts1);

        p.write(
            &[DocumentLogEntry {
                ts: ts2,
                id: id.clone(),
                value: None,
                prev_ts: Some(ts1),
            }],
            &[PersistenceIndexEntry {
                ts: ts2,
                index_id: index.clone(),
                key: key.clone(),
                value: None,
            }],
            ConflictStrategy::Error,
        )
        .expect("tombstone");

        assert!(p
            .index_scan(&index, ts2, &Interval::all(), Order::Asc)
            .unwrap()
            .is_empty());
        let still = p
            .index_scan(&index, ts1, &Interval::all(), Order::Asc)
            .unwrap();
        assert_eq!(still.len(), 1);
        assert_eq!(still[0].document.ts, ts1);

        let prev = p.previous_revision(&id, ts2).unwrap().expect("prev");
        assert_eq!(prev.ts, ts1);
        assert_eq!(prev.value.as_deref(), Some("{\"hello\":\"world\"}"));

        let exact = p.exact_revision(&id, ts2).unwrap().expect("tombstone row");
        assert!(exact.deleted());
        assert_eq!(exact.prev_ts, Some(ts1));

        let history = p.document_history(&id, ts2).unwrap();
        assert_eq!(history.len(), 2);
        assert!(history[0].deleted());
        assert_eq!(history[1].ts, ts1);

        let loaded = p
            .load_documents(
                TimestampRange {
                    start_inclusive: ts1,
                    end_exclusive: Timestamp::from_u64(3).unwrap(),
                },
                Order::Asc,
            )
            .unwrap();
        assert_eq!(loaded.len(), 2);

        // What each commit wrote to the index, in order, tombstones included: this is what a
        // reader in another process turns into write-log entries.
        let writes = p
            .load_index_writes(TimestampRange {
                start_inclusive: ts1,
                end_exclusive: Timestamp::from_u64(3).unwrap(),
            })
            .unwrap();
        assert_eq!(writes.len(), 2, "{writes:?}");
        assert_eq!(
            (writes[0].ts, writes[0].value.clone()),
            (ts1, Some(id.clone()))
        );
        assert_eq!((writes[1].ts, writes[1].value.clone()), (ts2, None));
        assert_eq!(writes[1].key, key);
        assert!(p
            .load_index_writes(TimestampRange {
                start_inclusive: Timestamp::from_u64(3).unwrap(),
                end_exclusive: Timestamp::from_u64(9).unwrap(),
            })
            .unwrap()
            .is_empty());

        // The published watermark only moves forward.
        let published = |p: &dyn Persistence| {
            p.get_persistence_global(PersistenceGlobalKey::PublishedTimestamp)
                .unwrap()
                .and_then(|value| value.as_u64())
        };
        p.advance_published_ts(ts2).unwrap();
        assert_eq!(published(p), Some(2));
        p.advance_published_ts(ts1).unwrap();
        assert_eq!(published(p), Some(2));

        assert_eq!(p.max_ts().unwrap(), Some(ts2));
        assert!(!p.is_fresh());
    }

    /// `index_scan_ids` lists exactly the rows `index_scan` does (as keys and document ids), in the
    /// same order, for any interval, read timestamp, order and limit, including after a key moved
    /// to another value, after a document was deleted and created again under the same key, and
    /// for a key rewritten many times, so that a backend that reads in chunks has to resume in the
    /// middle of one. Run after `exercise`, on the same database.
    pub fn exercise_index_scan_ids(p: &dyn Persistence) {
        let tablet = TabletId::new("scan_ids");
        let index = IndexId::new("scan_ids/by_name");
        let doc = |n: u32| DocumentId::new(tablet.clone(), format!("d{n}").into_bytes());
        let at = |n: u64| Timestamp::from_u64(n).unwrap();
        let put = |ts: u64, n: u32, key: &str, prev: Option<u64>| {
            (
                DocumentLogEntry {
                    ts: at(ts),
                    id: doc(n),
                    value: Some(format!("{{\"n\":{n},\"v\":{ts}}}")),
                    prev_ts: prev.map(at),
                },
                PersistenceIndexEntry {
                    ts: at(ts),
                    index_id: index.clone(),
                    key: key.as_bytes().to_vec(),
                    value: Some(doc(n)),
                },
            )
        };
        let gone = |ts: u64, n: u32, key: &str, prev: u64| {
            (
                DocumentLogEntry {
                    ts: at(ts),
                    id: doc(n),
                    value: None,
                    prev_ts: Some(at(prev)),
                },
                PersistenceIndexEntry {
                    ts: at(ts),
                    index_id: index.clone(),
                    key: key.as_bytes().to_vec(),
                    value: None,
                },
            )
        };
        let commit = |rows: Vec<(DocumentLogEntry, PersistenceIndexEntry)>| {
            let (documents, indexes): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
            p.write(&documents, &indexes, ConflictStrategy::Error)
                .expect("write");
        };

        // 10: four documents. 11: d2 changes the key it is found under. 12: d3 is deleted.
        // 13: d1 is rewritten under the same key. 14: d3 is created again under its old key.
        commit(vec![
            put(10, 1, "a|d1", None),
            put(10, 2, "b|d2", None),
            put(10, 3, "b|d3", None),
            put(10, 4, "e|d4", None),
        ]);
        let (moved_doc, new_key) = put(11, 2, "c|d2", Some(10));
        let (_, old_key) = gone(11, 2, "b|d2", 10);
        p.write(&[moved_doc], &[old_key, new_key], ConflictStrategy::Error)
            .expect("move");
        commit(vec![gone(12, 3, "b|d3", 10)]);
        commit(vec![put(13, 1, "a|d1", Some(10))]);
        commit(vec![put(14, 3, "b|d3", Some(12))]);
        // d5 is rewritten under one key thirty times (ts 15..=44), and forty documents each have a
        // key of their own (ts 45): more rows than any small chunk holds, in the middle of a key.
        for ts in 15..=44 {
            commit(vec![put(
                ts,
                5,
                "h|d5",
                if ts == 15 { None } else { Some(ts - 1) },
            )]);
        }
        commit(
            (10..50)
                .map(|n| put(45, n, &format!("k|{n:02}"), None))
                .collect(),
        );

        let intervals = [
            Interval::all(),
            Interval::prefix(b"b".to_vec()),
            Interval::prefix(b"k".to_vec()),
            Interval {
                start: b"a|d1".to_vec(),
                end: Some(b"c".to_vec()),
            },
            Interval {
                start: b"h".to_vec(),
                end: Some(b"i".to_vec()),
            },
            Interval {
                start: b"zzz".to_vec(),
                end: None,
            },
        ];
        let mut listed = 0;
        for read_at in [9, 10, 11, 12, 13, 14, 20, 30, 44, 45, 60] {
            for interval in &intervals {
                for order in [Order::Asc, Order::Desc] {
                    let full: Vec<(Vec<u8>, DocumentId)> = p
                        .index_scan(&index, at(read_at), interval, order)
                        .unwrap()
                        .into_iter()
                        .map(|row| (row.key, row.document.id))
                        .collect();
                    for limit in [
                        None,
                        Some(0),
                        Some(1),
                        Some(3),
                        Some(25),
                        Some(1000),
                        Some(usize::MAX),
                    ] {
                        let ids: Vec<(Vec<u8>, DocumentId)> = p
                            .index_scan_ids(&index, at(read_at), interval, order, limit)
                            .unwrap()
                            .into_iter()
                            .map(|row| (row.key, row.id))
                            .collect();
                        let mut want = full.clone();
                        if let Some(limit) = limit {
                            want.truncate(limit);
                        }
                        listed += want.len();
                        assert_eq!(
                            ids, want,
                            "at {read_at}, {interval:?}, {order:?}, limit {limit:?}: the id-only scan lists other rows"
                        );
                    }
                }
            }
        }
        assert!(listed > 1000, "the comparison listed only {listed} rows");

        // And the rows are what they should be, so the comparison above is not between two
        // lists that are wrong in the same way.
        let keys = |read_at: u64, limit: Option<usize>| -> Vec<String> {
            p.index_scan_ids(
                &index,
                at(read_at),
                &Interval::prefix(b"a".to_vec()),
                Order::Asc,
                limit,
            )
            .unwrap()
            .into_iter()
            .chain(
                p.index_scan_ids(
                    &index,
                    at(read_at),
                    &Interval {
                        start: b"b".to_vec(),
                        end: Some(b"f".to_vec()),
                    },
                    Order::Asc,
                    limit,
                )
                .unwrap(),
            )
            .map(|row| String::from_utf8(row.key).unwrap())
            .collect()
        };
        assert!(keys(9, None).is_empty());
        assert_eq!(keys(10, None), ["a|d1", "b|d2", "b|d3", "e|d4"]);
        assert_eq!(keys(11, None), ["a|d1", "b|d3", "c|d2", "e|d4"], "d2 moved");
        assert_eq!(keys(12, None), ["a|d1", "c|d2", "e|d4"], "d3 was deleted");
        assert_eq!(
            keys(14, None),
            ["a|d1", "b|d3", "c|d2", "e|d4"],
            "d3 is back"
        );
        let hot = p
            .index_scan_ids(
                &index,
                at(60),
                &Interval::prefix(b"h".to_vec()),
                Order::Asc,
                Some(1),
            )
            .unwrap();
        assert_eq!(hot.len(), 1, "the thirty rows of one key are one entry");
        let all_keys = p
            .index_scan_ids(&index, at(60), &Interval::all(), Order::Asc, None)
            .unwrap();
        assert_eq!(
            all_keys.len(),
            4 + 1 + 40,
            "a, b, c, e plus the hot key plus forty"
        );
    }
}

#[cfg(test)]
mod live_entries_tests {
    use super::*;

    fn id(n: u32) -> Option<DocumentId> {
        Some(DocumentId::new(
            TabletId::new("t"),
            format!("d{n}").into_bytes(),
        ))
    }

    fn keys(entries: &[IndexScanEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| String::from_utf8(entry.key.clone()).unwrap())
            .collect()
    }

    #[test]
    fn an_ascending_scan_takes_the_newest_version_of_each_key_and_drops_tombstones() {
        let mut live = LiveEntries::new(Order::Asc, None);
        // a: put, then removed. b: put, then put again. c: put.
        for (key, target) in [
            ("a", id(1)),
            ("a", None),
            ("b", id(2)),
            ("b", id(3)),
            ("c", id(4)),
        ] {
            assert!(!live.push(key.as_bytes().to_vec(), target));
        }
        let entries = live.finish();
        assert_eq!(keys(&entries), ["b", "c"]);
        assert_eq!(entries[0].id, id(3).unwrap(), "the newer version of b");
    }

    #[test]
    fn a_descending_scan_takes_the_first_version_it_meets_of_each_key() {
        let mut live = LiveEntries::new(Order::Desc, None);
        // Newest first: c, then b (newer then older), then a, whose newest version is a tombstone.
        for (key, target) in [
            ("c", id(4)),
            ("b", id(3)),
            ("b", id(2)),
            ("a", None),
            ("a", id(1)),
        ] {
            assert!(!live.push(key.as_bytes().to_vec(), target));
        }
        let entries = live.finish();
        assert_eq!(keys(&entries), ["c", "b"]);
        assert_eq!(entries[1].id, id(3).unwrap());
    }

    #[test]
    fn it_says_stop_as_soon_as_the_limit_is_settled_and_not_before() {
        let mut live = LiveEntries::new(Order::Asc, Some(2));
        assert!(!live.push(b"a".to_vec(), id(1)));
        assert!(
            !live.push(b"a".to_vec(), id(2)),
            "the same key again settles nothing"
        );
        assert!(!live.push(b"b".to_vec(), id(3)), "a is settled, one of two");
        assert!(
            live.push(b"c".to_vec(), id(4)),
            "b is settled: two entries, so the first row of the next key is the last one read"
        );
        assert_eq!(keys(&live.finish()), ["a", "b"]);

        // A tombstone settles nothing, so it does not count towards the limit.
        let mut live = LiveEntries::new(Order::Asc, Some(1));
        assert!(!live.push(b"a".to_vec(), None));
        assert!(!live.push(b"b".to_vec(), id(1)));
        assert!(live.push(b"c".to_vec(), id(2)));
        assert_eq!(keys(&live.finish()), ["b"]);

        // A limit of nothing wants nothing.
        let mut live = LiveEntries::new(Order::Asc, Some(0));
        assert!(live.push(b"a".to_vec(), id(1)));
        assert!(live.finish().is_empty());
    }

    #[test]
    fn a_chunk_that_ends_inside_a_key_leaves_that_key_for_the_next_chunk() {
        let mut live = LiveEntries::new(Order::Asc, Some(5));
        assert!(!live.push(b"a".to_vec(), id(1)));
        assert!(!live.push(b"b".to_vec(), id(2)));
        assert!(
            !live.push(b"b".to_vec(), id(3)),
            "the chunk ends in the middle of b"
        );
        assert_eq!(live.end_chunk(), Some(b"b".to_vec()));
        // The next chunk starts again at b and sees all of it.
        assert!(!live.push(b"b".to_vec(), id(2)));
        assert!(!live.push(b"b".to_vec(), id(3)));
        assert!(!live.push(b"c".to_vec(), id(4)));
        let entries = live.finish();
        assert_eq!(keys(&entries), ["a", "b", "c"], "b is listed once");
        assert_eq!(entries[1].id, id(3).unwrap());
    }
}
