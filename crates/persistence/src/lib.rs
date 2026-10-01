//! Persistence contract.
//!
//! Convex: `crates/common/src/persistence/mod.rs`
//! `Persistence::write`, `PersistenceReader::index_scan`, `previous_revisions`,
//! `load_documents`, `get_persistence_global`.
//!
//! Streams in Convex are `Vec` here. The rows and the timestamp rules are the same.
//! `DocumentLogEntry::value` is JSON text, not `ResolvedDocument`.

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
    MaxRepeatableTimestamp,
}

impl PersistenceGlobalKey {
    pub fn as_str(self) -> &'static str {
        match self {
            PersistenceGlobalKey::MaxRepeatableTimestamp => "max_repeatable_ts",
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
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistError::Definite(msg) => write!(f, "{msg}"),
            PersistError::Uncertain(msg) => write!(f, "uncertain persistence write: {msg}"),
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

    fn get_persistence_global(
        &self,
        key: PersistenceGlobalKey,
    ) -> Result<Option<JsonValue>, PersistError>;

    fn max_ts(&self) -> Result<Option<Timestamp>, PersistError>;
}

pub trait Persistence: PersistenceReader {
    fn is_fresh(&self) -> bool;

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

#[derive(Clone, Debug)]
pub struct NewPackage {
    pub name: String,
    pub version: String,
    pub fhir_version: String,
    pub structure_definitions: Vec<NewStructureDefinition>,
    pub search_parameters: Vec<NewSearchParameter>,
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
    fn load_packages(&self) -> Result<Vec<StoredPackage>, PersistError>;

    fn load_structure_definitions(&self) -> Result<Vec<StoredStructureDefinition>, PersistError>;

    fn load_search_parameters(&self) -> Result<Vec<StoredSearchParameter>, PersistError>;

    fn install_package(&self, package: &NewPackage) -> Result<InstallOutcome, InstallError>;

    fn mark_base_ready(&self, package_id: i64, base: &str) -> Result<(), PersistError>;

    fn set_search_parameter_status(
        &self,
        package_id: i64,
        url: &str,
        base: &str,
        status: ParamStatus,
    ) -> Result<(), PersistError>;

    fn insert_backfill(
        &self,
        package_id: i64,
        watermark_ts: i64,
        tablet: &str,
    ) -> Result<i64, PersistError>;

    fn update_backfill(
        &self,
        id: i64,
        cursor_id: Option<&str>,
        done: bool,
    ) -> Result<(), PersistError>;

    fn load_backfills(&self) -> Result<Vec<StoredBackfill>, PersistError>;
}

pub mod conformance {
    use super::*;
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

        let global = p
            .get_persistence_global(PersistenceGlobalKey::MaxRepeatableTimestamp)
            .unwrap()
            .expect("max repeatable");
        assert_eq!(global.as_u64(), Some(2));
        assert_eq!(p.max_ts().unwrap(), Some(ts2));
        assert!(!p.is_fresh());
    }
}
