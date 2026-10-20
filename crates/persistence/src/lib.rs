//! Persistence contract.
//!
//! `Persistence::write` appends documents and index rows in one SQL transaction.
//! `PersistenceReader` answers `index_scan`, `previous_revision` and `exact_revision` at a
//! timestamp. Row streams are `Vec`s. `DocumentLogEntry::value` is JSON text.

mod pool;
mod version;

pub mod migrate;

pub use pool::{Pool, PoolGuard, PoolStats};
pub use version::compare_versions;

use std::collections::HashMap;
use std::sync::Arc;

use refhir_types::{DocumentId, IndexId, Interval, Order, TabletId, Timestamp};
use serde_json::Value as JsonValue;

mod audit_chain;
mod governance;

pub use audit_chain::*;
pub use governance::*;

/// How `Persistence::write` treats an existing primary key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictStrategy {
    Error,
    Overwrite,
}

/// Keys of the persistence globals. Retention keys are not written yet.
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
    /// Moves when only the terminology changed (value sets, code systems, concept maps and the
    /// expansions kept for them) and the process that made the change has already applied it to
    /// its own catalog in place. A replica or a standby that sees only this one move reloads the
    /// terminology and nothing else, so it never loads a search parameter on the way.
    TerminologyVersion,
}

impl PersistenceGlobalKey {
    pub fn as_str(self) -> &'static str {
        match self {
            PersistenceGlobalKey::MaxRepeatableTimestamp => "max_repeatable_ts",
            PersistenceGlobalKey::WriterEpoch => "writer_epoch",
            PersistenceGlobalKey::PublishedTimestamp => "published_ts",
            PersistenceGlobalKey::CatalogVersion => "catalog_version",
            PersistenceGlobalKey::CatalogRevision => "catalog_revision",
            PersistenceGlobalKey::TerminologyVersion => "terminology_version",
        }
    }
}

/// One document-log row. `value: None` is a tombstone.
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistenceIndexEntry {
    pub ts: Timestamp,
    pub index_id: IndexId,
    pub key: Vec<u8>,
    pub value: Option<DocumentId>,
}

/// A live document returned by `index_scan`.
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

/// Inclusive start, exclusive end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimestampRange {
    pub start_inclusive: Timestamp,
    pub end_exclusive: Timestamp,
}

/// Failure of a persistence call.
///
/// `Uncertain` means the process cannot tell whether the SQL commit landed.
/// The committer treats that as fatal.
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
            PersistError::Definite(format!("live document {tablet} at {ts} has no json"))
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

    /// Newest row with `ts < at`.
    fn previous_revision(
        &self,
        id: &DocumentId,
        at: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError>;

    /// The row written at exactly `ts`.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct NewStructureDefinition {
    pub url: String,
    pub type_name: String,
    pub kind: String,
    pub is_abstract: bool,
    pub json: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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
    pub param_type: String,
    pub status: String,
    pub json: String,
    /// The row belongs to the version of the package that the install being planned replaces (its
    /// predecessor): it takes no part in a clash with the incoming definition, and it goes once the
    /// package is stored (see [`plan_search_parameter`]). A row of a package an earlier install
    /// replaced is not given to the planner at all (see [`superseded_package_ids`]).
    pub predecessor: bool,
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

/// Whether `row` indexes a document as `param` would, so that what it already indexed holds for
/// `param`: the same type and expression, and the same `target` and `component`, which are the
/// other parts of a definition that an index is made from. When this cannot be told apart the
/// answer is no, which costs a backfill that finds nothing to change. A composite is never the
/// same: its keys also depend on the types of the parameters its components name, which a row
/// does not say.
fn indexes_the_same(param: &NewSearchParameter, row: &InstalledParam) -> bool {
    let incoming = param.expression.clone().unwrap_or_default();
    // The parts of a definition besides its type and expression that decide what is indexed: the
    // resource types a reference may point at (as a set), and the components of a composite.
    let shape = |json: &str| {
        let value: JsonValue = serde_json::from_str(json).unwrap_or(JsonValue::Null);
        let targets: std::collections::BTreeSet<String> = value
            .get("target")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(|target| target.as_str().map(str::to_string))
            .collect();
        (targets, value.get("component").cloned())
    };
    param.param_type != "composite"
        && param.param_type == row.param_type
        && same_expression(&param.base, &row.expression, &incoming)
        && shape(&param.json) == shape(&row.json)
}

/// Decide how `param` meets the `installed` rows that share its base and code. Only the rows
/// that actually disagree with it and are experimental are ever removed.
///
/// A row of the version of the same package that this install replaces (`predecessor`) is neither
/// kept nor in conflict, whatever it says: the package that holds the incoming definition replaces
/// it. The incoming parameter takes its place with the status it had when it would index the same,
/// so that a parameter a new version leaves as it was is not indexed again, and as new when it
/// would not, which is also what a parameter whose backfill never finished is.
pub fn plan_search_parameter(
    param: &NewSearchParameter,
    installed: &[InstalledParam],
) -> Result<ParamPlan, PersistError> {
    let incoming = param.expression.clone().unwrap_or_default();
    let mut status = None;
    let mut remove = Vec::new();
    let (mut keep, mut conflict) = (false, false);
    for row in installed {
        if row.predecessor {
            if status.is_none()
                && ParamStatus::parse(&row.status)? == ParamStatus::Ready
                && indexes_the_same(param, row)
            {
                status = Some(ParamStatus::Ready);
            }
            continue;
        }
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

/// A stored search parameter row with the base and code it is looked up by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredParameterRow {
    pub base: String,
    pub code: String,
    pub row: InstalledParam,
}

/// A stored row that installing a package replaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterRemoval {
    pub package_id: i64,
    pub url: String,
    pub base: String,
    pub code: String,
}

/// What installing all the search parameters of one package comes to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageParameterPlan {
    /// The parameters to store, as positions in the package's list, with the status each is
    /// stored under, in the order they are stored.
    pub inserts: Vec<(usize, ParamStatus)>,
    /// Rows that were stored before the install and go away.
    pub removals: Vec<ParameterRemoval>,
    pub newly_pending: Vec<PendingParam>,
}

/// [`plan_search_parameter`] for every parameter of a package, in order, as if each one had been
/// stored before the next was looked at: a package may carry two definitions of one parameter,
/// and the later one then meets the earlier one. `stored` holds the rows that already exist for
/// the bases and codes of `parameters`, so a store reads them in one query rather than one per
/// parameter, then writes the result in a few statements.
pub fn plan_package_parameters(
    package_id: i64,
    parameters: &[NewSearchParameter],
    stored: Vec<StoredParameterRow>,
) -> Result<PackageParameterPlan, InstallError> {
    let mut by_key: HashMap<(String, String), Vec<InstalledParam>> = HashMap::new();
    for stored in stored {
        by_key
            .entry((stored.base, stored.code))
            .or_default()
            .push(stored.row);
    }
    let mut plan = PackageParameterPlan::default();
    for (position, param) in parameters.iter().enumerate() {
        let key = (param.base.clone(), param.code.clone());
        let installed = by_key.entry(key).or_default();
        let (existing, remove) = match plan_search_parameter(param, installed)? {
            ParamPlan::Conflict => {
                return Err(InstallError::Expression {
                    base: param.base.clone(),
                    code: param.code.clone(),
                });
            }
            ParamPlan::Skip => continue,
            ParamPlan::Insert { status, remove } => (status, remove),
        };
        for (owner, url) in remove {
            installed.retain(|row| !(row.package_id == owner && row.url == url));
            // A row this package inserted earlier in the plan was never stored.
            let earlier = (owner == package_id)
                .then(|| {
                    plan.inserts.iter().position(|(held, _)| {
                        let held = &parameters[*held];
                        held.url == url && held.base == param.base && held.code == param.code
                    })
                })
                .flatten();
            match earlier {
                Some(index) => {
                    plan.inserts.remove(index);
                }
                None => plan.removals.push(ParameterRemoval {
                    package_id: owner,
                    url,
                    base: param.base.clone(),
                    code: param.code.clone(),
                }),
            }
        }
        let is_new = existing.is_none();
        let status = existing.unwrap_or(param.status);
        if is_new && status == ParamStatus::Pending {
            plan.newly_pending.push(PendingParam {
                base: param.base.clone(),
                code: param.code.clone(),
            });
        }
        installed.push(InstalledParam {
            package_id,
            url: param.url.clone(),
            expression: param.expression.clone().unwrap_or_default(),
            param_type: param.param_type.clone(),
            status: status.as_str().to_string(),
            json: param.json.clone(),
            predecessor: false,
        });
        plan.inserts.push((position, status));
    }
    Ok(plan)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredTerminology {
    pub package_id: i64,
    pub resource_type: String,
    pub url: String,
    pub json: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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

/// A package as it is stored. The row types it holds are also what a prepared file of packages
/// holds, in their serde form: adding, removing or renaming a field of one of them changes the
/// bytes of every such file, so it needs a bump of `FORMAT` in `refhir-fhir`'s `prepared.rs`
/// (a test there fails when the bytes change).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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

/// A search parameter that an older version of a package had and no search parameter has now,
/// named like a pending one by its base and code.
pub type RetiredParam = PendingParam;

/// The condition `column NOT IN (…)` that leaves the rows of the packages `ids` out of a statement,
/// or nothing when there are none, so that a database without a replaced package reads what it
/// did. The ids are numbers, so they are written into the statement.
pub fn not_in_packages(column: &str, ids: &[i64]) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    let list: Vec<String> = ids.iter().map(i64::to_string).collect();
    Some(format!("{column} NOT IN ({})", list.join(", ")))
}

/// The ids of the packages in `packages` that a newer version of the same name has replaced: every
/// one but the newest of each name. Versions that [`compare_versions`] orders the same are told
/// apart by id, the one stored later being the newer, as a package that is stored replaces them.
/// A store leaves what these hold out of every list it gives, and keeps it until it is collected
/// (see [`ConformanceStore::collect_superseded`]).
pub fn superseded_package_ids(packages: &[StoredPackage]) -> Vec<i64> {
    let mut newest: HashMap<&str, &StoredPackage> = HashMap::new();
    for package in packages {
        let newer = newest.get(package.name.as_str()).is_none_or(|held| {
            compare_versions(&package.version, &held.version)
                .then(package.id.cmp(&held.id))
                .is_gt()
        });
        if newer {
            newest.insert(package.name.as_str(), package);
        }
    }
    packages
        .iter()
        .filter(|package| newest[package.name.as_str()].id != package.id)
        .map(|package| package.id)
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallOutcome {
    pub package_id: i64,
    pub inserted: bool,
    pub newly_pending: Vec<PendingParam>,
    /// The package was not stored: this store keeps one copy of what its tenants have in common,
    /// and the package is now the tenant's from that copy. Its rows were not read.
    pub shared: bool,
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

/// Where the rows of a package go when they are handed over a few at a time, by
/// [`ConformanceStore::install_package_in_pieces`]. They are stored as
/// [`ConformanceStore::install_package`] stores the same rows given at once, in the order they
/// are handed over: a package a store already has takes only terminology and expansions, as it does
/// there.
pub trait PackageRows {
    fn structure_definitions(
        &mut self,
        rows: &[NewStructureDefinition],
    ) -> Result<(), InstallError>;

    fn search_parameters(&mut self, rows: &[NewSearchParameter]) -> Result<(), InstallError>;

    fn terminology(&mut self, rows: &[NewTerminology]) -> Result<(), InstallError>;

    /// The expansions replace those the package already had, once, whatever the number of calls:
    /// a package with none is told so by never calling this.
    fn expansions(&mut self, rows: &[NewExpansion]) -> Result<(), InstallError>;
}

/// A package held whole is what [`ConformanceStore::install_package`] takes, so a store that does
/// not store in pieces can be given the pieces one after another and then the whole.
impl PackageRows for NewPackage {
    fn structure_definitions(
        &mut self,
        rows: &[NewStructureDefinition],
    ) -> Result<(), InstallError> {
        self.structure_definitions.extend_from_slice(rows);
        Ok(())
    }

    fn search_parameters(&mut self, rows: &[NewSearchParameter]) -> Result<(), InstallError> {
        self.search_parameters.extend_from_slice(rows);
        Ok(())
    }

    fn terminology(&mut self, rows: &[NewTerminology]) -> Result<(), InstallError> {
        self.terminology.extend_from_slice(rows);
        Ok(())
    }

    fn expansions(&mut self, rows: &[NewExpansion]) -> Result<(), InstallError> {
        self.expansions.extend_from_slice(rows);
        Ok(())
    }
}

/// How many rows a backend hands over at a time to [`ConformanceStore::visit_terminology`] and
/// its siblings.
pub const CONFORMANCE_PAGE_ROWS: usize = 128;

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

    /// The counter a change to the terminology alone bumps, when the process that made it has
    /// already applied it to its own catalog. Whoever follows the version or the revision also
    /// follows this one: if only it moved, only the terminology needs loading again.
    fn terminology_version(&self) -> Result<u64, PersistError>;

    /// Adds one to the terminology version and returns the new value.
    fn bump_terminology_version(&self) -> Result<u64, PersistError>;

    fn load_packages(&self) -> Result<Vec<StoredPackage>, PersistError>;

    fn load_structure_definitions(&self) -> Result<Vec<StoredStructureDefinition>, PersistError>;

    fn load_search_parameters(&self) -> Result<Vec<StoredSearchParameter>, PersistError>;

    fn load_terminology(&self) -> Result<Vec<StoredTerminology>, PersistError>;

    fn load_expansions(&self) -> Result<Vec<StoredExpansion>, PersistError>;

    /// [`ConformanceStore::load_structure_definitions`], handed over a page at a time, in the
    /// same order. A caller that compiles the definitions as they come never holds more than a
    /// page of them, where the full load holds every installed package's JSON at once. The store
    /// is not locked while `visit` runs. The default reads them all and hands them over in one
    /// page; a backend that can read page by page should override it.
    fn visit_structure_definitions(
        &self,
        visit: &mut dyn FnMut(Vec<StoredStructureDefinition>) -> Result<(), PersistError>,
    ) -> Result<(), PersistError> {
        visit(self.load_structure_definitions()?)
    }

    /// [`ConformanceStore::load_terminology`], a page at a time. See
    /// [`ConformanceStore::visit_structure_definitions`].
    fn visit_terminology(
        &self,
        visit: &mut dyn FnMut(Vec<StoredTerminology>) -> Result<(), PersistError>,
    ) -> Result<(), PersistError> {
        visit(self.load_terminology()?)
    }

    /// [`ConformanceStore::load_expansions`], a page at a time. See
    /// [`ConformanceStore::visit_structure_definitions`].
    fn visit_expansions(
        &self,
        visit: &mut dyn FnMut(Vec<StoredExpansion>) -> Result<(), PersistError>,
    ) -> Result<(), PersistError> {
        visit(self.load_expansions()?)
    }

    /// The JSON of the structure definition stored under `url` for package `package_id`, or
    /// `None` when there is no such row. A catalog that keeps an index of the definitions reads
    /// the one a request needs with this, when it needs it. The default reads them all; a
    /// backend should override it with a lookup by key.
    fn load_structure_definition_json(
        &self,
        package_id: i64,
        url: &str,
    ) -> Result<Option<String>, PersistError> {
        Ok(self
            .load_structure_definitions()?
            .into_iter()
            .find(|row| row.package_id == package_id && row.url == url)
            .map(|row| row.json))
    }

    /// The JSON of the terminology row stored under `url` for package `package_id`. See
    /// [`ConformanceStore::load_structure_definition_json`].
    fn load_terminology_json(
        &self,
        package_id: i64,
        url: &str,
    ) -> Result<Option<String>, PersistError> {
        Ok(self
            .load_terminology()?
            .into_iter()
            .find(|row| row.package_id == package_id && row.url == url)
            .map(|row| row.json))
    }

    /// The JSON of the stored expansion under `url` for package `package_id`. See
    /// [`ConformanceStore::load_structure_definition_json`].
    fn load_expansion_json(
        &self,
        package_id: i64,
        url: &str,
    ) -> Result<Option<String>, PersistError> {
        Ok(self
            .load_expansions()?
            .into_iter()
            .find(|row| row.package_id == package_id && row.url == url)
            .map(|row| row.json))
    }

    /// A handle to this store that a catalog can keep, to read from it later. `None` for a store
    /// that cannot give one, and a catalog then loads everything it needs when it is built.
    fn shared(&self) -> Option<std::sync::Arc<dyn ConformanceStore>> {
        None
    }

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

    /// Store a package: its definitions, search parameters, terminology and expansions go in one
    /// transaction, so it is there whole or not at all.
    ///
    /// The newest version of a package is the one in force. A package that is stored replaces the
    /// older versions of the same name (ordered by [`compare_versions`], so `0.10` replaces `0.10.0`
    /// as well, a version that orders the same and is written another way): from then on no list
    /// the store gives holds anything of theirs, not their package, definitions, search parameters,
    /// terminology or expansions. Their rows stay stored until [`ConformanceStore::collect_superseded`]
    /// removes them, because a process that serves from the catalog it loaded before may still read
    /// them by key. What the new version defines again takes the place of what it defined before.
    /// The older versions' search parameters take no part in a clash with the new ones. A search
    /// parameter keeps the status of the one it replaces (`Ready`: what was indexed holds) when its
    /// type, expression, `target` and `component` are the same, and is otherwise stored as the
    /// package gives it, to be backfilled. Packages with other names are never replaced.
    fn install_package(&self, package: &NewPackage) -> Result<InstallOutcome, InstallError>;

    /// The search parameters that only packages a newer version has replaced define: what is
    /// indexed under them is not used by the catalog in force, and
    /// [`ConformanceStore::collect_superseded`] is about to remove their rows, so a caller clears
    /// those indexes first. Worked out from what is stored, so it is the same after a crash. The
    /// default is none, for a store that never keeps a replaced package.
    fn retired_parameters(&self) -> Result<Vec<RetiredParam>, PersistError> {
        Ok(Vec::new())
    }

    /// Remove the packages that newer versions have replaced, with everything they hold:
    /// definitions, search parameters, terminology, expansions and the backfills queued for them.
    /// Call it when nothing waits for an index and a process that follows the catalog has had time
    /// to load the one that replaced them: the start of a process after the one that replaced them
    /// is late enough, and an install that follows the one that replaced them is not. A store that
    /// shares what its tenants have in common also removes a shared package that no tenant has any
    /// longer, in the same transaction in which the tenant gives it up. The default does nothing.
    fn collect_superseded(&self) -> Result<(), PersistError> {
        Ok(())
    }

    /// [`ConformanceStore::install_package`] for a package that is not held whole: `feed` is
    /// given the place the rows go and hands them over a few at a time, and the package is
    /// stored in one transaction, so it is there whole or not at all. Installing the largest R4
    /// package this way takes the memory of a few rows, not of the package. If `feed` fails, so
    /// does the install and nothing is stored. The default gathers the rows and stores them with
    /// [`ConformanceStore::install_package`]. `feed` is `Send` because a store that talks to its
    /// database from a thread of its own calls it there.
    fn install_package_in_pieces(
        &self,
        name: &str,
        version: &str,
        fhir_version: &str,
        feed: &mut (dyn FnMut(&mut dyn PackageRows) -> Result<(), InstallError> + Send),
    ) -> Result<InstallOutcome, InstallError> {
        let mut package = NewPackage {
            name: name.to_string(),
            version: version.to_string(),
            fhir_version: fhir_version.to_string(),
            structure_definitions: Vec::new(),
            search_parameters: Vec::new(),
            terminology: Vec::new(),
            expansions: Vec::new(),
        };
        feed(&mut package)?;
        self.install_package(&package)
    }

    /// [`ConformanceStore::install_package_in_pieces`] for a package that `content` (a digest of
    /// its source) identifies, so a store that keeps one copy of what its tenants have in common
    /// may keep one of this package for all of them: the first tenant to have it stores it, and
    /// each other tenant that asks for the same `content` is given it from that copy.
    ///
    /// A store that gives the package from a copy it already holds does **not call `feed`**, and the
    /// outcome says `shared`: a caller that reads the rows from a stream must pass over what `feed`
    /// would have read. A package of the same name and version with other content is stored for the
    /// tenant alone. The default stores the package as `install_package_in_pieces` does, calling
    /// `feed`, since a store without tenants has nobody to share with.
    fn install_shared_package_in_pieces(
        &self,
        name: &str,
        version: &str,
        fhir_version: &str,
        content: &str,
        feed: &mut (dyn FnMut(&mut dyn PackageRows) -> Result<(), InstallError> + Send),
    ) -> Result<InstallOutcome, InstallError> {
        let _ = content;
        self.install_package_in_pieces(name, version, fhir_version, feed)
    }

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

    /// [`ConformanceStore::insert_backfill`] for each of `tablets`, each snapshotting every
    /// parameter of the package that is pending for it. A package that adds parameters to
    /// a hundred resource types queues a hundred jobs; a store that makes this one step spares
    /// a round trip and a commit each.
    fn insert_backfills(
        &self,
        package_id: i64,
        watermark_ts: i64,
        tablets: &[String],
    ) -> Result<(), PersistError> {
        for tablet in tablets {
            self.insert_backfill(package_id, watermark_ts, tablet, &[])?;
        }
        Ok(())
    }

    /// Records that this writer is the one running backfill `id`, taking it over from an
    /// earlier writer that died with it half done. Returns false if the job is finished and
    /// there is nothing to run. Fails with `PersistError::Fenced` if a *newer* writer has
    /// already claimed it: this process is the stale one.
    fn claim_backfill(&self, id: i64) -> Result<bool, PersistError>;

    /// Claims, completes and marks done each of `jobs`, which the caller found nothing to index
    /// for: the resource type holds no document that is not already past the job's cursor. It
    /// is [`ConformanceStore::claim_backfill`], [`ConformanceStore::complete_backfill`] and
    /// [`ConformanceStore::update_backfill`] for each job in turn, except that a job that is
    /// already finished is passed over, and a store may do all of it in one step.
    fn complete_empty_backfills(&self, jobs: &[StoredBackfill]) -> Result<(), PersistError> {
        for job in jobs {
            if !self.claim_backfill(job.id)? {
                continue;
            }
            self.complete_backfill(job.id, job.package_id, &job.tablet)?;
            self.update_backfill(job.id, job.cursor_id.as_deref(), true)?;
        }
        Ok(())
    }

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
            param_type: "string".into(),
            status: "ready".into(),
            json: format!(r#"{{"experimental":{experimental}}}"#),
            predecessor: false,
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
    fn a_predecessor_composite_is_indexed_again_and_a_plain_parameter_is_not() {
        let predecessor = |param_type: &str| InstalledParam {
            predecessor: true,
            param_type: param_type.into(),
            ..row(1, "Patient.same", false)
        };
        let incoming = |param_type: &str| NewSearchParameter {
            param_type: param_type.into(),
            ..param("Patient.same", false)
        };
        let status = |param_type: &str| match plan_search_parameter(
            &incoming(param_type),
            &[predecessor(param_type)],
        )
        .unwrap()
        {
            ParamPlan::Insert { status, .. } => status,
            other => panic!("{other:?}"),
        };
        assert_eq!(status("string"), Some(ParamStatus::Ready));
        assert_eq!(status("composite"), None);
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

    fn incoming(url: &str, expression: &str, experimental: bool) -> NewSearchParameter {
        NewSearchParameter {
            url: url.into(),
            ..param(expression, experimental)
        }
    }

    type Rows = Vec<(i64, String, String)>;

    /// The rows left after storing `parameters` one at a time, each looked up against what the
    /// ones before it left: how a store that goes parameter by parameter ends up.
    fn one_at_a_time(
        package_id: i64,
        parameters: &[NewSearchParameter],
        mut rows: Vec<InstalledParam>,
    ) -> Result<(Rows, Vec<PendingParam>), InstallError> {
        let mut pending = Vec::new();
        for param in parameters {
            let (existing, remove) = match plan_search_parameter(param, &rows)? {
                ParamPlan::Conflict => {
                    return Err(InstallError::Expression {
                        base: param.base.clone(),
                        code: param.code.clone(),
                    })
                }
                ParamPlan::Skip => continue,
                ParamPlan::Insert { status, remove } => (status, remove),
            };
            for (owner, url) in remove {
                rows.retain(|row| !(row.package_id == owner && row.url == url));
            }
            let status = existing.unwrap_or(param.status);
            if existing.is_none() && status == ParamStatus::Pending {
                pending.push(PendingParam {
                    base: param.base.clone(),
                    code: param.code.clone(),
                });
            }
            rows.push(InstalledParam {
                package_id,
                url: param.url.clone(),
                expression: param.expression.clone().unwrap_or_default(),
                param_type: param.param_type.clone(),
                status: status.as_str().into(),
                json: param.json.clone(),
                predecessor: false,
            });
        }
        let mut rows: Rows = rows
            .into_iter()
            .map(|row| (row.package_id, row.url, row.status))
            .collect();
        rows.sort();
        Ok((rows, pending))
    }

    /// The same, from the plan a store gets for the whole package and applies in bulk.
    fn all_at_once(
        package_id: i64,
        parameters: &[NewSearchParameter],
        rows: Vec<InstalledParam>,
    ) -> Result<(Rows, Vec<PendingParam>), InstallError> {
        let stored = rows
            .iter()
            .cloned()
            .map(|row| StoredParameterRow {
                base: "Patient".into(),
                code: "maiden".into(),
                row,
            })
            .collect();
        let plan = plan_package_parameters(package_id, parameters, stored)?;
        let mut rows = rows;
        for removal in &plan.removals {
            rows.retain(|row| !(row.package_id == removal.package_id && row.url == removal.url));
        }
        for (position, status) in &plan.inserts {
            let param = &parameters[*position];
            rows.push(InstalledParam {
                package_id,
                url: param.url.clone(),
                expression: param.expression.clone().unwrap_or_default(),
                param_type: param.param_type.clone(),
                status: status.as_str().into(),
                json: param.json.clone(),
                predecessor: false,
            });
        }
        let mut rows: Rows = rows
            .into_iter()
            .map(|row| (row.package_id, row.url, row.status))
            .collect();
        rows.sort();
        Ok((rows, plan.newly_pending))
    }

    #[test]
    fn a_package_is_planned_as_if_its_parameters_were_stored_one_by_one() {
        // Rows of other packages that are already stored, and the definitions a package may
        // carry for one parameter: drafts and real ones, one of them twice under another URL.
        let stored: [Vec<InstalledParam>; 4] = [
            Vec::new(),
            vec![row(1, "Patient.draft", true)],
            vec![row(1, "Patient.real", false)],
            vec![
                row(1, "Patient.draft", true),
                row(2, "Patient.draft2", true),
            ],
        ];
        let definitions = [
            incoming("http://example.org/a", "Patient.draft", true),
            incoming("http://example.org/b", "Patient.draft2", true),
            incoming("http://example.org/c", "Patient.real", false),
            incoming("http://example.org/d", "Patient.real", false),
            incoming("http://example.org/e", "Patient.other", false),
        ];
        let mut compared = 0;
        for rows in &stored {
            for first in 0..definitions.len() {
                for second in 0..definitions.len() {
                    for third in 0..definitions.len() {
                        let package = [
                            definitions[first].clone(),
                            definitions[second].clone(),
                            definitions[third].clone(),
                        ];
                        let one = one_at_a_time(9, &package, rows.clone());
                        let all = all_at_once(9, &package, rows.clone());
                        match (one, all) {
                            (Ok(one), Ok(all)) => assert_eq!(one, all, "{rows:?} {package:?}"),
                            (Err(one), Err(all)) => {
                                assert_eq!(one.to_string(), all.to_string())
                            }
                            (one, all) => panic!("{rows:?} {package:?}: {one:?} against {all:?}"),
                        }
                        compared += 1;
                    }
                }
            }
        }
        assert_eq!(compared, 4 * 5 * 5 * 5);
    }

    #[test]
    fn a_definition_the_package_replaces_itself_is_never_stored() {
        let package = [
            incoming("http://example.org/draft", "Patient.draft", true),
            incoming("http://example.org/real", "Patient.real", false),
        ];
        let plan = plan_package_parameters(9, &package, Vec::new()).unwrap();
        assert_eq!(plan.inserts, [(1, ParamStatus::Pending)]);
        assert!(plan.removals.is_empty(), "{plan:?}");
        assert_eq!(plan.newly_pending.len(), 2);
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

    /// Reading structure definitions, terminology and expansions a page at a time gives the rows
    /// the full loads give, in the same order, in pages no larger than
    /// [`CONFORMANCE_PAGE_ROWS`], and leaves the store free while the caller works on a page. A
    /// row read by its key is the row the full load has, and a handle the store gives reads the
    /// same rows. `tag` keeps this run's rows apart on a database that already holds earlier
    /// runs.
    pub fn exercise_paged_reads(store: &dyn ConformanceStore, tag: &str) {
        let rows = CONFORMANCE_PAGE_ROWS * 2 + 5;
        let url = |kind: &str, index: usize| format!("http://example.org/{tag}/{kind}/{index:05}");
        let document =
            |kind: &str, url: &str| format!(r#"{{"resourceType":"{kind}","url":"{url}"}}"#);
        for name in ["paged-one", "paged-two"] {
            let package = NewPackage {
                name: format!("{name}.{tag}"),
                version: "1.0.0".into(),
                fhir_version: "4.0.1".into(),
                structure_definitions: (0..rows)
                    .map(|index| NewStructureDefinition {
                        url: url("definition", index),
                        type_name: "Thing".into(),
                        kind: "complex-type".into(),
                        is_abstract: index % 2 == 0,
                        json: document("StructureDefinition", &url("definition", index)),
                    })
                    .collect(),
                search_parameters: Vec::new(),
                terminology: (0..rows)
                    .map(|index| NewTerminology {
                        resource_type: "CodeSystem".into(),
                        url: url("system", index),
                        json: document("CodeSystem", &url("system", index)),
                    })
                    .collect(),
                expansions: (0..rows)
                    .map(|index| NewExpansion {
                        url: url("set", index),
                        json: document("ValueSet", &url("set", index)),
                    })
                    .collect(),
            };
            assert!(store.install_package(&package).unwrap().inserted);
        }
        let mine = |url: &str| url.contains(&format!("/{tag}/"));

        // One macro for the three tables, which differ only in their row type and loaders.
        macro_rules! check {
            ($load:ident, $visit:ident, $row:ident) => {{
                let full: Vec<_> = store
                    .$load()
                    .unwrap()
                    .into_iter()
                    .filter(|row| mine(&row.url))
                    .collect();
                assert_eq!(full.len(), rows * 2);
                let mut paged = Vec::new();
                let mut pages = 0;
                store
                    .$visit(&mut |page| {
                        assert!(page.len() <= CONFORMANCE_PAGE_ROWS);
                        // The store is not held while a page is worked on: another call goes
                        // straight through.
                        store.catalog_version().unwrap();
                        pages += 1;
                        paged.extend(page.into_iter().filter(|row| mine(&row.url)));
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(paged, full, "{} pages of {}", pages, stringify!($visit));
                assert!(pages >= (rows * 2).div_ceil(CONFORMANCE_PAGE_ROWS));
                // What the caller says stops the read, and comes back as it was said.
                let mut calls = 0;
                let stopped = store.$visit(&mut |_| {
                    calls += 1;
                    Err(PersistError::Definite("stop".into()))
                });
                assert!(matches!(stopped, Err(PersistError::Definite(text)) if text == "stop"));
                assert_eq!(calls, 1);
                // A row by its key is the row the full load has, and a key that is not a row
                // (a URL, or a package, that has none) is not found.
                let reader = store.shared();
                for row in [full.first().unwrap(), full.last().unwrap()] {
                    let found = store.$row(row.package_id, &row.url).unwrap();
                    assert_eq!(found.as_deref(), Some(row.json.as_str()));
                    if let Some(reader) = &reader {
                        let found = reader.$row(row.package_id, &row.url).unwrap();
                        assert_eq!(found.as_deref(), Some(row.json.as_str()));
                    }
                    assert_eq!(store.$row(row.package_id, "http://example.org/none").unwrap(), None);
                    assert_eq!(store.$row(i64::MAX, &row.url).unwrap(), None);
                }
            }};
        }
        check!(
            load_structure_definitions,
            visit_structure_definitions,
            load_structure_definition_json
        );
        check!(load_terminology, visit_terminology, load_terminology_json);
        check!(load_expansions, visit_expansions, load_expansion_json);
    }

    /// Installing a package a few rows at a time stores what installing it whole stores, is all
    /// or nothing, and tops up a package that is already there as installing it again does. `tag`
    /// keeps this run's URLs and package names apart on a database that already holds earlier
    /// runs.
    pub fn exercise_install_in_pieces(store: &dyn ConformanceStore, tag: &str) {
        let url = |kind: &str, index: usize| format!("http://example.org/{tag}/{kind}/{index:03}");
        let json = |kind: &str, url: &str| format!(r#"{{"resourceType":"{kind}","url":"{url}"}}"#);
        let definitions = |from: usize, to: usize| -> Vec<NewStructureDefinition> {
            (from..to)
                .map(|index| NewStructureDefinition {
                    url: url("definition", index),
                    type_name: "Thing".into(),
                    kind: "complex-type".into(),
                    is_abstract: index % 2 == 0,
                    json: json("StructureDefinition", &url("definition", index)),
                })
                .collect()
        };
        let parameters = |name: &str| -> Vec<NewSearchParameter> {
            (0..4)
                .map(|index| NewSearchParameter {
                    url: format!("http://example.org/{tag}/{name}/param/{index}"),
                    base: "Patient".into(),
                    code: format!("{tag}-{name}-{index}"),
                    param_type: "token".into(),
                    expression: Some(format!("Patient.{tag}{name}{index}")),
                    status: ParamStatus::Pending,
                    json: "{}".into(),
                })
                .collect()
        };
        let terminology = |from: usize, to: usize| -> Vec<NewTerminology> {
            (from..to)
                .map(|index| NewTerminology {
                    resource_type: "CodeSystem".into(),
                    url: url("system", index),
                    json: json("CodeSystem", &url("system", index)),
                })
                .collect()
        };
        let expansions = |from: usize, to: usize| -> Vec<NewExpansion> {
            (from..to)
                .map(|index| NewExpansion {
                    url: url("set", index),
                    json: json("ValueSet", &url("set", index)),
                })
                .collect()
        };
        let whole = |name: &str, definitions, terminology, expansions| NewPackage {
            name: format!("{name}.{tag}"),
            version: "1.0.0".into(),
            fhir_version: "4.0.1".into(),
            structure_definitions: definitions,
            search_parameters: parameters(name),
            terminology,
            expansions,
        };
        // What a package holds, apart from its id.
        let held = |name: &str| {
            let id = store
                .load_packages()
                .unwrap()
                .into_iter()
                .find(|package| package.name == format!("{name}.{tag}"))
                .map(|package| package.id)
                .unwrap_or_else(|| panic!("{name} is not installed"));
            let mut definitions: Vec<(String, String, bool)> = store
                .load_structure_definitions()
                .unwrap()
                .into_iter()
                .filter(|row| row.package_id == id)
                .map(|row| (row.url, row.json, row.is_abstract))
                .collect();
            definitions.sort();
            let mut terminology: Vec<(String, String)> = store
                .load_terminology()
                .unwrap()
                .into_iter()
                .filter(|row| row.package_id == id)
                .map(|row| (row.url, row.json))
                .collect();
            terminology.sort();
            let mut expansions: Vec<(String, String)> = store
                .load_expansions()
                .unwrap()
                .into_iter()
                .filter(|row| row.package_id == id)
                .map(|row| (row.url, row.json))
                .collect();
            expansions.sort();
            let parameters = store
                .load_search_parameters()
                .unwrap()
                .into_iter()
                .filter(|row| row.package_id == id)
                .count();
            (definitions, terminology, expansions, parameters)
        };

        // Whole, and in pieces that cut each kind of row in two, give the same rows and the
        // same search parameters to index.
        let at_once = store
            .install_package(&whole(
                "whole",
                definitions(0, 40),
                terminology(0, 30),
                expansions(0, 20),
            ))
            .unwrap();
        let all_parameters = parameters("pieces");
        let outcome = store
            .install_package_in_pieces(&format!("pieces.{tag}"), "1.0.0", "4.0.1", &mut |rows| {
                rows.structure_definitions(&definitions(0, 17))?;
                rows.structure_definitions(&definitions(17, 40))?;
                rows.search_parameters(&all_parameters[..2])?;
                rows.search_parameters(&all_parameters[2..])?;
                rows.terminology(&terminology(0, 11))?;
                rows.terminology(&terminology(11, 30))?;
                rows.expansions(&expansions(0, 9))?;
                rows.expansions(&expansions(9, 20))?;
                Ok(())
            })
            .unwrap();
        assert!(at_once.inserted && outcome.inserted);
        assert_eq!(at_once.newly_pending.len(), outcome.newly_pending.len());
        assert_eq!(held("whole"), held("pieces"));
        assert_eq!(held("pieces").3, 4);

        // All or nothing: a failure part of the way through leaves no package and no row.
        let failed = store.install_package_in_pieces(
            &format!("failed.{tag}"),
            "1.0.0",
            "4.0.1",
            &mut |rows| {
                rows.structure_definitions(&definitions(0, 5))?;
                rows.terminology(&terminology(0, 5))?;
                Err(InstallError::Persist(PersistError::Definite("stop".into())))
            },
        );
        assert!(
            matches!(failed, Err(InstallError::Persist(PersistError::Definite(text))) if text == "stop")
        );
        assert!(!store
            .load_packages()
            .unwrap()
            .iter()
            .any(|package| package.name == format!("failed.{tag}")));

        // A package that is there takes terminology on top of what it has (a row it already
        // has stays as it is) and its expansions replace the old ones, as installing it again
        // does.
        let again = whole(
            "whole",
            definitions(0, 3),
            terminology(25, 40),
            expansions(18, 26),
        );
        assert!(!store.install_package(&again).unwrap().inserted);
        let topped = store
            .install_package_in_pieces(&format!("pieces.{tag}"), "1.0.0", "4.0.1", &mut |rows| {
                rows.structure_definitions(&definitions(0, 3))?;
                rows.search_parameters(&parameters("pieces"))?;
                rows.terminology(&terminology(25, 33))?;
                rows.terminology(&terminology(33, 40))?;
                rows.expansions(&expansions(18, 22))?;
                rows.expansions(&expansions(22, 26))?;
                Ok(())
            })
            .unwrap();
        assert!(!topped.inserted);
        assert_eq!(held("whole"), held("pieces"));
        assert_eq!(held("pieces").1.len(), 40);
        assert_eq!(held("pieces").2.len(), 8);
        // One that brings no expansions leaves it with none, as installing it again does.
        assert!(
            !store
                .install_package(&whole("whole", Vec::new(), Vec::new(), Vec::new()))
                .unwrap()
                .inserted
        );
        assert!(
            !store
                .install_package_in_pieces(&format!("pieces.{tag}"), "1.0.0", "4.0.1", &mut |_| Ok(
                    ()
                ))
                .unwrap()
                .inserted
        );
        assert_eq!(held("whole"), held("pieces"));
        assert!(held("pieces").2.is_empty());

        // Search parameters that meet inside one package (a draft and the real definition that
        // replaces it, a draft that yields to a real one stored before it, two spellings of one
        // expression) end up as they do whole, wherever the pieces are cut, and report the same
        // ones as waiting for a backfill.
        let parameter = |name: &str, key: &str, code: &str, expression: Option<&str>, draft| {
            NewSearchParameter {
                url: format!("http://example.org/{tag}/{name}/param/{key}"),
                base: "Patient".into(),
                code: format!("{name}-{code}-{tag}"),
                param_type: "string".into(),
                expression: expression.map(str::to_string),
                status: ParamStatus::Pending,
                json: format!(r#"{{"experimental":{draft}}}"#),
            }
        };
        let meeting = |name: &str| -> Vec<NewSearchParameter> {
            vec![
                parameter(name, "bare", "bare", None, false),
                parameter(name, "draft", "replaced", Some("Patient.draft"), true),
                parameter(name, "real", "replaced", Some("Patient.real"), false),
                parameter(name, "plain", "plain", Some("Patient.plain"), false),
                parameter(name, "kept", "kept", Some("Patient.kept"), false),
                parameter(name, "sketch", "kept", Some("Patient.sketch"), true),
                parameter(name, "again", "plain", Some("plain"), false),
            ]
        };
        let pending = |name: &str, outcome: &InstallOutcome| -> Vec<String> {
            let mut codes: Vec<String> = outcome
                .newly_pending
                .iter()
                .map(|param| {
                    param
                        .code
                        .strip_prefix(&format!("{name}-"))
                        .and_then(|code| code.strip_suffix(&format!("-{tag}")))
                        .unwrap_or_else(|| panic!("{} is not {name}'s", param.code))
                        .to_string()
                })
                .collect();
            codes.sort();
            codes
        };
        let stored_parameters = |name: &str| -> Vec<(String, Option<String>, ParamStatus)> {
            let id = store
                .load_packages()
                .unwrap()
                .into_iter()
                .find(|package| package.name == format!("{name}.{tag}"))
                .map(|package| package.id)
                .unwrap_or_else(|| panic!("{name} is not installed"));
            let mut rows: Vec<(String, Option<String>, ParamStatus)> = store
                .load_search_parameters()
                .unwrap()
                .into_iter()
                .filter(|row| row.package_id == id)
                .map(|row| {
                    let key = row.url.rsplit('/').next().unwrap_or_default().to_string();
                    (key, row.expression, row.status)
                })
                .collect();
            rows.sort_by(|left, right| left.0.cmp(&right.0));
            rows
        };
        let package_of = |name: &str, parameters| NewPackage {
            name: format!("{name}.{tag}"),
            version: "1.0.0".into(),
            fhir_version: "4.0.1".into(),
            structure_definitions: Vec::new(),
            search_parameters: parameters,
            terminology: Vec::new(),
            expansions: Vec::new(),
        };
        let reference = store
            .install_package(&package_of("meetwhole", meeting("meetwhole")))
            .unwrap();
        let expected_pending = pending("meetwhole", &reference);
        assert_eq!(
            expected_pending,
            ["bare", "kept", "plain", "replaced", "replaced"]
        );
        let expected_rows = stored_parameters("meetwhole");
        let keys: Vec<&str> = expected_rows.iter().map(|row| row.0.as_str()).collect();
        assert_eq!(keys, ["again", "bare", "kept", "plain", "real"]);
        let count = meeting("shape").len();
        let mut cut_sets: Vec<Vec<usize>> = (0..=count).map(|cut| vec![cut]).collect();
        cut_sets.push((1..count).collect());
        for (number, cuts) in cut_sets.iter().enumerate() {
            let name = format!("meetpieces{number}");
            let all = meeting(&name);
            let outcome = store
                .install_package_in_pieces(
                    &format!("{name}.{tag}"),
                    "1.0.0",
                    "4.0.1",
                    &mut |rows| {
                        let mut from = 0;
                        for &to in cuts.iter().chain(&[all.len()]) {
                            rows.search_parameters(&all[from..to])?;
                            from = to;
                        }
                        Ok(())
                    },
                )
                .unwrap();
            assert_eq!(
                pending(&name, &outcome),
                expected_pending,
                "cut at {cuts:?}"
            );
            assert_eq!(stored_parameters(&name), expected_rows, "cut at {cuts:?}");
        }

        // Against a parameter another package has installed, in pieces: the same definition is
        // found there and is not waiting for a backfill, and one that disagrees refuses the
        // install after earlier pieces were stored, which leaves nothing behind.
        let shared = |name: &str, key: &str, expression: &str| NewSearchParameter {
            url: format!("http://example.org/{tag}/{name}/param/{key}"),
            base: "Patient".into(),
            code: format!("shared-{tag}"),
            param_type: "string".into(),
            expression: Some(format!("Patient.{expression}")),
            status: ParamStatus::Pending,
            json: r#"{"experimental":false}"#.into(),
        };
        let owner = store
            .install_package(&package_of(
                "owner",
                vec![shared("owner", "owned", "owned")],
            ))
            .unwrap();
        assert_eq!(owner.newly_pending.len(), 1);
        let agreeing = store
            .install_package_in_pieces(&format!("agreeing.{tag}"), "1.0.0", "4.0.1", &mut |rows| {
                rows.search_parameters(&[parameter(
                    "agreeing",
                    "other",
                    "other",
                    Some("o"),
                    false,
                )])?;
                rows.search_parameters(&[shared("agreeing", "owned", "owned")])?;
                Ok(())
            })
            .unwrap();
        assert_eq!(pending("agreeing", &agreeing), ["other"]);
        assert_eq!(stored_parameters("agreeing").len(), 2);
        let refused = store.install_package_in_pieces(
            &format!("disagreeing.{tag}"),
            "1.0.0",
            "4.0.1",
            &mut |rows| {
                rows.structure_definitions(&definitions(0, 2))?;
                rows.search_parameters(&[parameter(
                    "disagreeing",
                    "other",
                    "other",
                    Some("o"),
                    false,
                )])?;
                rows.search_parameters(&[shared("disagreeing", "elsewhere", "elsewhere")])?;
                Ok(())
            },
        );
        assert!(
            matches!(refused, Err(InstallError::Expression { .. })),
            "{refused:?}"
        );
        assert!(!store
            .load_packages()
            .unwrap()
            .iter()
            .any(|package| package.name == format!("disagreeing.{tag}")));
        assert!(!store
            .load_search_parameters()
            .unwrap()
            .iter()
            .any(|row| row.code == format!("disagreeing-other-{tag}")));
    }

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
    /// A package that carries several search parameters, a definition and the draft it replaces,
    /// a parameter with no expression, and two pieces of terminology for one URL goes in as the
    /// same rows a one-by-one install would leave, and a second package with the same
    /// parameters finds them there and inherits their status.
    pub fn exercise_package_install(store: &dyn ConformanceStore, tag: &str) {
        let parameter = |url: &str, code: &str, expression: Option<&str>, experimental: bool| {
            NewSearchParameter {
                url: format!("http://example.org/sp/{tag}/{url}"),
                base: "Patient".into(),
                code: format!("{code}-{tag}"),
                param_type: "string".into(),
                expression: expression.map(str::to_string),
                status: ParamStatus::Pending,
                json: format!(r#"{{"experimental":{experimental}}}"#),
            }
        };
        let package = |name: &str| NewPackage {
            name: format!("{name}.{tag}"),
            version: "1.0.0".into(),
            fhir_version: "4.0.1".into(),
            structure_definitions: ["one", "two"]
                .iter()
                .map(|id| NewStructureDefinition {
                    url: format!("http://example.org/sd/{tag}/{id}"),
                    type_name: "Thing".into(),
                    kind: "resource".into(),
                    is_abstract: *id == "two",
                    json: format!(r#"{{"id":"{id}"}}"#),
                })
                .collect(),
            search_parameters: vec![
                parameter("none", "bare", None, false),
                parameter("draft", "replaced", Some("Patient.draft"), true),
                parameter("real", "replaced", Some("Patient.real"), false),
                parameter("plain", "plain", Some("Patient.plain"), false),
            ],
            terminology: ["first", "second"]
                .iter()
                .map(|which| NewTerminology {
                    resource_type: "CodeSystem".into(),
                    url: format!("http://example.org/cs/{tag}"),
                    json: format!(r#"{{"which":"{which}"}}"#),
                })
                .collect(),
            expansions: Vec::new(),
        };
        let mine = |row: &StoredSearchParameter| row.code.ends_with(&format!("-{tag}"));
        let first = store.install_package(&package("bulk-one")).unwrap();
        assert!(first.inserted);
        let mut pending: Vec<String> = first
            .newly_pending
            .iter()
            .map(|param| param.code.clone())
            .collect();
        pending.sort();
        // The draft was new and pending when it went in, then the real one replaced it.
        assert_eq!(
            pending,
            [
                format!("bare-{tag}"),
                format!("plain-{tag}"),
                format!("replaced-{tag}"),
                format!("replaced-{tag}"),
            ]
        );
        let rows: Vec<StoredSearchParameter> = store
            .load_search_parameters()
            .unwrap()
            .into_iter()
            .filter(mine)
            .collect();
        let mut stored: Vec<(&str, Option<&str>, ParamStatus)> = rows
            .iter()
            .map(|row| (row.code.as_str(), row.expression.as_deref(), row.status))
            .collect();
        stored.sort_by_key(|row| row.0);
        let (bare, plain, replaced) = (
            format!("bare-{tag}"),
            format!("plain-{tag}"),
            format!("replaced-{tag}"),
        );
        assert_eq!(
            stored,
            [
                (bare.as_str(), None, ParamStatus::Pending),
                (plain.as_str(), Some("Patient.plain"), ParamStatus::Pending),
                (
                    replaced.as_str(),
                    Some("Patient.real"),
                    ParamStatus::Pending
                ),
            ]
        );
        let definitions: Vec<StoredStructureDefinition> = store
            .load_structure_definitions()
            .unwrap()
            .into_iter()
            .filter(|row| row.url.contains(&format!("/{tag}/")))
            .collect();
        assert_eq!(definitions.len(), 2);
        assert!(definitions.iter().any(|row| row.is_abstract));
        assert!(definitions.iter().any(|row| !row.is_abstract));
        let terminology: Vec<StoredTerminology> = store
            .load_terminology()
            .unwrap()
            .into_iter()
            .filter(|row| row.url.ends_with(&format!("/cs/{tag}")))
            .collect();
        assert_eq!(terminology.len(), 1);
        assert_eq!(terminology[0].json, r#"{"which":"first"}"#);

        // The package that follows carries the same parameters: they are found installed, keep
        // their status, and nothing in it is waiting for a backfill that is not queued already.
        let second = store.install_package(&package("bulk-two")).unwrap();
        assert!(second.inserted);
        assert!(
            second.newly_pending.is_empty(),
            "{:?}",
            second.newly_pending
        );
        let rows: Vec<StoredSearchParameter> = store
            .load_search_parameters()
            .unwrap()
            .into_iter()
            .filter(|row| mine(row) && row.package_id == second.package_id)
            .collect();
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(rows.iter().all(|row| row.status == ParamStatus::Pending));
    }

    /// Queueing and finishing backfill jobs in bulk does what doing it one at a time does: each
    /// job is queued over the parameters pending for its type, finishing jobs that had nothing to
    /// index marks exactly those parameters ready, a job that was limited to some parameters
    /// leaves the others pending, and one that is already finished is passed over.
    pub fn exercise_backfill_batches(store: &dyn ConformanceStore, tag: &str) {
        let parameter = |base: &str, code: &str| NewSearchParameter {
            url: format!("http://example.org/sp/{tag}/{base}-{code}"),
            base: base.into(),
            code: format!("{code}-{tag}"),
            param_type: "string".into(),
            expression: Some(format!("{base}.{code}")),
            status: ParamStatus::Pending,
            json: "{}".into(),
        };
        let package = NewPackage {
            name: format!("backfills.{tag}"),
            version: "1.0.0".into(),
            fhir_version: "4.0.1".into(),
            structure_definitions: Vec::new(),
            search_parameters: vec![
                parameter("Patient", "a"),
                parameter("Observation", "a"),
                parameter("Encounter", "a"),
                parameter("Encounter", "b"),
                parameter("Group", "a"),
                parameter("Group", "b"),
            ],
            terminology: Vec::new(),
            expansions: Vec::new(),
        };
        let package_id = store.install_package(&package).unwrap().package_id;
        let status_of = |base: &str, code: &str| -> ParamStatus {
            store
                .load_search_parameters()
                .unwrap()
                .into_iter()
                .find(|row| {
                    row.package_id == package_id && row.base == base && row.code.starts_with(code)
                })
                .unwrap()
                .status
        };
        let mine = || -> Vec<StoredBackfill> {
            store
                .load_backfills()
                .unwrap()
                .into_iter()
                .filter(|job| job.package_id == package_id)
                .collect()
        };

        let tablets: Vec<String> = ["Encounter", "Observation", "Patient"]
            .iter()
            .map(|tablet| tablet.to_string())
            .collect();
        store.insert_backfills(package_id, 7, &tablets).unwrap();
        let queued = mine();
        assert_eq!(
            queued
                .iter()
                .map(|job| job.tablet.as_str())
                .collect::<Vec<_>>(),
            ["Encounter", "Observation", "Patient"],
            "queued in the order given"
        );
        assert!(queued
            .iter()
            .all(|job| !job.done && job.cursor_id.is_none() && job.watermark_ts == 7));
        assert!(queued.windows(2).all(|pair| pair[0].id < pair[1].id));

        // The first two have nothing to index; the third is still to run.
        store.complete_empty_backfills(&queued[..2]).unwrap();
        let after = mine();
        assert!(
            after[0].done && after[1].done && !after[2].done,
            "{after:?}"
        );
        assert!(after[0].claimed_epoch.is_some() && after[1].claimed_epoch.is_some());
        assert_eq!(status_of("Encounter", "a"), ParamStatus::Ready);
        assert_eq!(status_of("Encounter", "b"), ParamStatus::Ready);
        assert_eq!(status_of("Observation", "a"), ParamStatus::Ready);
        assert_eq!(status_of("Patient", "a"), ParamStatus::Pending);

        // Finished jobs are passed over, so asking again for all three finishes only the last.
        store.complete_empty_backfills(&queued).unwrap();
        assert!(mine().iter().all(|job| job.done));
        assert_eq!(status_of("Patient", "a"), ParamStatus::Ready);

        // A job limited to one parameter of its type marks that one and no other.
        let limited = store
            .insert_backfill(
                package_id,
                7,
                "Group",
                &[format!("http://example.org/sp/{tag}/Group-a")],
            )
            .unwrap();
        let job = mine().into_iter().find(|job| job.id == limited).unwrap();
        store.complete_empty_backfills(&[job]).unwrap();
        assert!(mine().iter().find(|job| job.id == limited).unwrap().done);
        assert_eq!(status_of("Group", "a"), ParamStatus::Ready);
        assert_eq!(status_of("Group", "b"), ParamStatus::Pending);
    }

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

    /// A newer version of a package replaces its older versions: they are out of every list from
    /// the transaction that stores it, and gone, with everything they held, once collected. With
    /// `shared_links`, other tenants of the store have a link to every package it holds, so a
    /// package that tenant owns is kept whole when it is collected, and only what changes per
    /// tenant (expansions, search parameters, queued backfills) goes.
    pub fn exercise_package_versions(store: &dyn ConformanceStore, tag: &str, shared_links: bool) {
        let name = format!("upgrade.{tag}");
        let code = |short: &str| format!("{short}-{tag}");
        let param = |short: &str, expression: &str| NewSearchParameter {
            url: format!("http://example.org/sp/{tag}/{short}"),
            base: "Patient".into(),
            code: code(short),
            param_type: "string".into(),
            expression: Some(expression.into()),
            status: ParamStatus::Pending,
            json: r#"{"type":"string"}"#.into(),
        };
        // What a package holds besides its search parameters: a definition every version has, one
        // only its own version has, a code system and a value set expansion.
        let package = |name: &str, version: &str, parameters: Vec<NewSearchParameter>| NewPackage {
            name: name.into(),
            version: version.into(),
            fhir_version: "4.0.1".into(),
            structure_definitions: ["kept".to_string(), format!("only-{version}")]
                .iter()
                .map(|what| NewStructureDefinition {
                    url: format!("http://example.org/sd/{name}/{what}"),
                    type_name: "Patient".into(),
                    kind: "resource".into(),
                    is_abstract: false,
                    json: "{}".into(),
                })
                .collect(),
            search_parameters: parameters,
            terminology: vec![NewTerminology {
                resource_type: "CodeSystem".into(),
                url: format!("http://example.org/cs/{name}/{version}"),
                json: "{}".into(),
            }],
            expansions: vec![NewExpansion {
                url: format!("http://example.org/vs/{name}/{version}"),
                json: "{}".into(),
            }],
        };
        // The definitions, terminology and expansions of the package `owner`, as the parts of their
        // URLs after its name, and the id of the package they are stored under.
        let content = |owner: &str| -> (Vec<String>, Vec<String>, Vec<String>, Vec<i64>) {
            let part = |url: &str| {
                url.rsplit(&format!("/{owner}/"))
                    .next()
                    .unwrap()
                    .to_string()
            };
            let mut ids = Vec::new();
            let mut definitions: Vec<String> = store
                .load_structure_definitions()
                .unwrap()
                .into_iter()
                .filter(|row| row.url.contains(&format!("/{owner}/")))
                .map(|row| {
                    ids.push(row.package_id);
                    part(&row.url)
                })
                .collect();
            let mut terminology: Vec<String> = store
                .load_terminology()
                .unwrap()
                .into_iter()
                .filter(|row| row.url.contains(&format!("/{owner}/")))
                .map(|row| part(&row.url))
                .collect();
            let mut expansions: Vec<String> = store
                .load_expansions()
                .unwrap()
                .into_iter()
                .filter(|row| row.url.contains(&format!("/{owner}/")))
                .map(|row| part(&row.url))
                .collect();
            definitions.sort();
            terminology.sort();
            expansions.sort();
            ids.sort();
            ids.dedup();
            (definitions, terminology, expansions, ids)
        };
        // The same as `content`, read a page at a time the way a catalog is loaded.
        let visited = |owner: &str| -> (Vec<String>, Vec<String>, Vec<String>) {
            let (mut definitions, mut terminology, mut expansions) =
                (Vec::new(), Vec::new(), Vec::new());
            let marker = format!("/{owner}/");
            let part = |url: &str| url.rsplit(marker.as_str()).next().unwrap().to_string();
            store
                .visit_structure_definitions(&mut |page| {
                    definitions.extend(
                        page.iter()
                            .filter(|row| row.url.contains(marker.as_str()))
                            .map(|row| part(&row.url)),
                    );
                    Ok(())
                })
                .unwrap();
            store
                .visit_terminology(&mut |page| {
                    terminology.extend(
                        page.iter()
                            .filter(|row| row.url.contains(marker.as_str()))
                            .map(|row| part(&row.url)),
                    );
                    Ok(())
                })
                .unwrap();
            store
                .visit_expansions(&mut |page| {
                    expansions.extend(
                        page.iter()
                            .filter(|row| row.url.contains(marker.as_str()))
                            .map(|row| part(&row.url)),
                    );
                    Ok(())
                })
                .unwrap();
            definitions.sort();
            terminology.sort();
            expansions.sort();
            (definitions, terminology, expansions)
        };
        // This test's rows, as (code, expression, status, package id), by code.
        let rows = || -> Vec<(String, String, ParamStatus, i64)> {
            let mut rows: Vec<_> = store
                .load_search_parameters()
                .unwrap()
                .into_iter()
                .filter(|row| row.code.ends_with(&format!("-{tag}")))
                .map(|row| {
                    let short = row.code.trim_end_matches(&format!("-{tag}")).to_string();
                    (
                        short,
                        row.expression.unwrap_or_default(),
                        row.status,
                        row.package_id,
                    )
                })
                .collect();
            rows.sort_by(|left, right| left.0.cmp(&right.0));
            rows
        };
        let package_id = |version: &str| {
            store
                .load_packages()
                .unwrap()
                .into_iter()
                .find(|held| held.name == name && held.version == version)
                .map(|held| held.id)
        };
        let shorts = |list: &[PendingParam]| -> Vec<String> {
            let mut shorts: Vec<String> = list
                .iter()
                .map(|param| param.code.trim_end_matches(&format!("-{tag}")).to_string())
                .collect();
            shorts.sort();
            shorts
        };
        let retired_shorts = |list: &[RetiredParam]| -> Vec<String> {
            let mut shorts: Vec<String> = list
                .iter()
                .filter(|param| param.code.ends_with(&format!("-{tag}")))
                .map(|param| param.code.trim_end_matches(&format!("-{tag}")).to_string())
                .collect();
            shorts.sort();
            shorts
        };

        // 0.1.0 has four parameters, and a package that is not a version of it has one.
        let first = store
            .install_package(&package(
                &name,
                "0.1.0",
                vec![
                    param("keep", "Patient.keep"),
                    param("change", "Patient.change"),
                    param("drop", "Patient.drop"),
                    param("rename", "Patient.rename"),
                ],
            ))
            .unwrap();
        store.mark_base_ready(first.package_id, "Patient").unwrap();
        let bystander = format!("bystander.{tag}");
        let other = store
            .install_package(&package(
                &bystander,
                "1.0.0",
                vec![param("bystander", "Patient.bystander")],
            ))
            .unwrap();
        store.mark_base_ready(other.package_id, "Patient").unwrap();
        assert_eq!(content(&name).0, ["kept", "only-0.1.0"]);
        // Each package has a backfill queued, as an install that had stored parameters waiting for
        // their index would have left.
        let queued = |package_id: i64| {
            store
                .load_backfills()
                .unwrap()
                .into_iter()
                .filter(|job| job.package_id == package_id)
                .count()
        };
        // The first names the parameters it may mark ready, which is a row of its own to remove
        // with it; the other leaves the choice to the parameters that are pending.
        store
            .insert_backfill(
                first.package_id,
                1,
                "Patient",
                &[format!("http://example.org/sp/{tag}/keep")],
            )
            .unwrap();
        store
            .insert_backfill(other.package_id, 1, "Patient", &[])
            .unwrap();
        assert_eq!((queued(first.package_id), queued(other.package_id)), (1, 1));

        // 0.2.0 keeps one as it was, gives one another expression, leaves one out, moves one to
        // another code under the same URL, and adds one.
        let mut moved = param("rename", "Patient.rename");
        moved.code = code("renamed");
        let second = store
            .install_package(&package(
                &name,
                "0.2.0",
                vec![
                    param("keep", "Patient.keep"),
                    param("change", "Patient.changed"),
                    moved,
                    param("added", "Patient.added"),
                ],
            ))
            .unwrap();
        assert!(second.inserted);
        // What is already indexed is not indexed again; what changed or is new is.
        assert_eq!(
            shorts(&second.newly_pending),
            ["added", "change", "renamed"]
        );
        // What only the replaced version defines: the one left out, and the code that was moved from.
        assert_eq!(
            retired_shorts(&store.retired_parameters().unwrap()),
            ["drop", "rename"]
        );
        let now = rows();
        let held = [
            (
                "added",
                "Patient.added",
                ParamStatus::Pending,
                second.package_id,
            ),
            (
                "bystander",
                "Patient.bystander",
                ParamStatus::Ready,
                other.package_id,
            ),
            (
                "change",
                "Patient.changed",
                ParamStatus::Pending,
                second.package_id,
            ),
            (
                "keep",
                "Patient.keep",
                ParamStatus::Ready,
                second.package_id,
            ),
            (
                "renamed",
                "Patient.rename",
                ParamStatus::Pending,
                second.package_id,
            ),
        ]
        .map(|(short, expression, status, id)| {
            (short.to_string(), expression.to_string(), status, id)
        });
        assert_eq!(now, held);
        // The older version is out of every list: its definitions, its terminology, its expansions
        // and its place among the packages. What the new version holds, and what another package
        // holds, is there.
        assert_eq!(package_id("0.1.0"), None);
        // But what it holds is still stored, for a process that serves from the catalog it loaded
        // before the install and reads a definition, a code system or an expansion by its key.
        let (kept, system, expansion) = (
            format!("http://example.org/sd/{name}/kept"),
            format!("http://example.org/cs/{name}/0.1.0"),
            format!("http://example.org/vs/{name}/0.1.0"),
        );
        let reads = |package: i64| {
            (
                store
                    .load_structure_definition_json(package, &kept)
                    .unwrap(),
                store.load_terminology_json(package, &system).unwrap(),
                store.load_expansion_json(package, &expansion).unwrap(),
            )
        };
        let (definition, terminology, expanded) = reads(first.package_id);
        assert!(definition.is_some() && terminology.is_some() && expanded.is_some());
        assert_eq!((queued(first.package_id), queued(other.package_id)), (1, 1));
        assert_eq!(
            content(&name),
            (
                vec!["kept".to_string(), "only-0.2.0".to_string()],
                vec!["0.2.0".to_string()],
                vec!["0.2.0".to_string()],
                vec![second.package_id]
            )
        );
        assert_eq!(
            content(&bystander),
            (
                vec!["kept".to_string(), "only-1.0.0".to_string()],
                vec!["1.0.0".to_string()],
                vec!["1.0.0".to_string()],
                vec![other.package_id]
            )
        );

        let listed = content(&name);
        assert_eq!(visited(&name), (listed.0, listed.1, listed.2));
        // Collected, it is gone with everything it held, the backfills queued for it too, and
        // what another package holds is not.
        store.collect_superseded().unwrap();
        let collected = reads(first.package_id);
        assert_eq!(collected.2, None);
        if !shared_links {
            assert_eq!(collected, (None, None, None));
        }
        assert_eq!((queued(first.package_id), queued(other.package_id)), (0, 1));
        assert!(retired_shorts(&store.retired_parameters().unwrap()).is_empty());
        assert_eq!(rows(), now);
        assert_eq!(content(&bystander).0, ["kept", "only-1.0.0"]);

        // An install that fails changes nothing: the version that was in force still is, whether
        // the package was handed over whole or in pieces.
        let clash = |url: &str, expression: &str| NewSearchParameter {
            url: url.into(),
            ..param("added", expression)
        };
        let content_before = content(&name);
        let twice = package(
            &name,
            "0.3.0",
            vec![
                param("keep", "Patient.keep"),
                clash("http://example.org/sp/one", "Patient.one"),
                clash("http://example.org/sp/two", "Patient.two"),
            ],
        );
        let err = store.install_package(&twice).unwrap_err();
        assert!(matches!(err, InstallError::Expression { .. }), "{err}");
        let err = store
            .install_package_in_pieces(&name, "0.3.0", "4.0.1", &mut |rows| {
                rows.search_parameters(&twice.search_parameters)
            })
            .unwrap_err();
        assert!(matches!(err, InstallError::Expression { .. }), "{err}");
        let against_bystander = package(
            &name,
            "0.3.0",
            vec![
                param("keep", "Patient.keep"),
                param("bystander", "Patient.other"),
            ],
        );
        let err = store.install_package(&against_bystander).unwrap_err();
        assert!(matches!(err, InstallError::Expression { .. }), "{err}");
        let err = store
            .install_package_in_pieces(&name, "0.3.0", "4.0.1", &mut |rows| {
                rows.search_parameters(&[param("keep", "Patient.keep")])?;
                Err(InstallError::Persist(PersistError::Definite("stop".into())))
            })
            .unwrap_err();
        assert!(matches!(err, InstallError::Persist(_)), "{err}");
        assert_eq!(rows(), now);
        assert_eq!(content(&name), content_before);
        assert_eq!(package_id("0.3.0"), None);
        assert!(package_id("0.2.0").is_some());

        // In pieces, a newer version replaces them as well. `added` is still waiting for its
        // index, so its definition is new again; a `keep` of another type, or that points at other
        // resources, is not what was indexed; and `keep` as it was is.
        let third = store
            .install_package_in_pieces(&name, "0.3.0", "4.0.1", &mut |rows| {
                rows.search_parameters(&[
                    param("keep", "Patient.keep"),
                    param("added", "Patient.added"),
                ])
            })
            .unwrap();
        assert_eq!(shorts(&third.newly_pending), ["added"]);
        assert_eq!(
            retired_shorts(&store.retired_parameters().unwrap()),
            ["change", "renamed"]
        );
        store.collect_superseded().unwrap();
        let now = rows();
        assert_eq!(
            now.iter()
                .map(|(short, _, status, id)| (short.as_str(), *status, *id == third.package_id))
                .collect::<Vec<_>>(),
            [
                ("added", ParamStatus::Pending, true),
                ("bystander", ParamStatus::Ready, false),
                ("keep", ParamStatus::Ready, true),
            ]
        );
        store.mark_base_ready(third.package_id, "Patient").unwrap();
        let mut token = param("keep", "Patient.keep");
        token.param_type = "token".into();
        let mut narrowed = param("added", "Patient.added");
        narrowed.json = r#"{"type":"string","target":["Group"]}"#.into();
        let fourth = store
            .install_package(&package(&name, "0.4.0", vec![token, narrowed]))
            .unwrap();
        assert_eq!(shorts(&fourth.newly_pending), ["added", "keep"]);
        assert!(retired_shorts(&store.retired_parameters().unwrap()).is_empty());
        store.collect_superseded().unwrap();
        // Version 0.10.0 is newer than 0.4.0, as a version number and not as text.
        let fifth = store
            .install_package(&package(
                &name,
                "0.10.0",
                vec![param("keep", "Patient.keep")],
            ))
            .unwrap();
        assert_eq!(
            retired_shorts(&store.retired_parameters().unwrap()),
            ["added"]
        );
        store.collect_superseded().unwrap();
        assert!(rows()
            .iter()
            .all(|(short, _, _, id)| { short == "bystander" || *id == fifth.package_id }));
        // `0.10` is `0.10.0` written another way, and replaces it all the same, so the two are
        // never held side by side, where a changed parameter would clash with itself.
        let sixth = store
            .install_package(&package(
                &name,
                "0.10",
                vec![param("keep", "Patient.other")],
            ))
            .unwrap();
        assert!(sixth.inserted);
        assert_eq!(shorts(&sixth.newly_pending), ["keep"]);
        assert_eq!(package_id("0.10.0"), None);
        assert_eq!(package_id("0.10"), Some(sixth.package_id));
        store.collect_superseded().unwrap();

        // A package that a newer version replaced and that is not collected yet is not looked at
        // when another package's parameters are checked: here `zz` is defined by nothing in force,
        // and by a stale row with another expression.
        let stale = format!("stale.{tag}");
        store
            .install_package(&package(&stale, "1.0.0", vec![param("zz", "Patient.zz1")]))
            .unwrap();
        store
            .install_package(&package(&stale, "2.0.0", Vec::new()))
            .unwrap();
        let late = store
            .install_package(&package(
                &format!("late.{tag}"),
                "1.0.0",
                vec![param("zz", "Patient.zz2")],
            ))
            .unwrap();
        assert_eq!(shorts(&late.newly_pending), ["zz"]);
        assert!(retired_shorts(&store.retired_parameters().unwrap()).is_empty());
        store.collect_superseded().unwrap();
        assert!(rows()
            .iter()
            .any(|row| row.0 == "zz" && row.1 == "Patient.zz2"));
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
