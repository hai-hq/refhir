//! SQLite document log, index log, and persistence globals.
//!
//! Convex: `crates/sqlite/src/lib.rs` (`DOCUMENTS_INIT`, `INDEXES_INIT`,
//! `PERSISTENCE_GLOBALS_INIT`, `write`, `index_scan`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use refhir_persistence::{
    document_from_parts, index_target, ConflictStrategy, DocumentLogEntry, IndexScanEntry,
    IndexScanRow, LatestDocument, Lease, LiveEntries, PersistError, Persistence,
    PersistenceGlobalKey, PersistenceIndexEntry, PersistenceReader, Pool, PoolGuard, PoolStats,
    TimestampRange,
};
use refhir_types::{DocumentId, IndexId, Interval, Order, TabletId, Timestamp};
use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use serde_json::Value as JsonValue;

mod lease;

/// Convex `DOCUMENTS_INIT`.
const DOCUMENTS_INIT: &str = r#"
CREATE TABLE IF NOT EXISTS documents (
    id BLOB NOT NULL,
    ts INTEGER NOT NULL,
    table_id BLOB NOT NULL,
    json_value TEXT NULL,
    deleted INTEGER NOT NULL,
    prev_ts INTEGER,
    PRIMARY KEY (ts, table_id, id)
);
CREATE INDEX IF NOT EXISTS documents_by_table_and_id ON documents (table_id, id, ts);
"#;

/// Convex `INDEXES_INIT`.
const INDEXES_INIT: &str = r#"
CREATE TABLE IF NOT EXISTS indexes (
    index_id BLOB NOT NULL,
    ts INTEGER NOT NULL,
    key BLOB NOT NULL,
    deleted INTEGER NOT NULL,
    table_id BLOB NULL,
    document_id BLOB NULL,
    PRIMARY KEY (index_id, key, ts)
);
-- A reader in another process learns what a commit wrote by scanning index rows by ts.
CREATE INDEX IF NOT EXISTS indexes_by_ts ON indexes (ts);
"#;

/// Convex `PERSISTENCE_GLOBALS_INIT`.
const PERSISTENCE_GLOBALS_INIT: &str = r#"
CREATE TABLE IF NOT EXISTS persistence_globals (
    key TEXT NOT NULL,
    json_value TEXT NOT NULL,
    PRIMARY KEY (key)
);
"#;

const CONFORMANCE_INIT: &str = r#"
CREATE TABLE IF NOT EXISTS packages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    fhir_version TEXT NOT NULL,
    UNIQUE (name, version)
);
CREATE TABLE IF NOT EXISTS structure_definitions (
    package_id INTEGER NOT NULL,
    url TEXT NOT NULL,
    type_name TEXT NOT NULL,
    kind TEXT NOT NULL,
    is_abstract INTEGER NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, url)
);
CREATE TABLE IF NOT EXISTS search_parameters (
    package_id INTEGER NOT NULL,
    url TEXT NOT NULL,
    base TEXT NOT NULL,
    code TEXT NOT NULL,
    param_type TEXT NOT NULL,
    expression TEXT,
    status TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, url, base)
);
-- Installing a parameter looks up the rows that share its base and code.
CREATE INDEX IF NOT EXISTS search_parameters_by_base_code ON search_parameters (base, code);
CREATE TABLE IF NOT EXISTS index_backfills (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    package_id INTEGER NOT NULL,
    watermark_ts INTEGER NOT NULL,
    tablet TEXT NOT NULL,
    cursor_id TEXT,
    done INTEGER NOT NULL,
    claimed_epoch INTEGER
);
CREATE TABLE IF NOT EXISTS terminology (
    package_id INTEGER NOT NULL,
    resource_type TEXT NOT NULL,
    url TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, url)
);
CREATE TABLE IF NOT EXISTS value_set_expansion (
    package_id INTEGER NOT NULL,
    url TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, url)
);
CREATE TABLE IF NOT EXISTS backfill_parameter (
    backfill_id INTEGER NOT NULL,
    url TEXT NOT NULL,
    PRIMARY KEY (backfill_id, url)
);
"#;

const INSERT_DOCUMENT: &str = "INSERT INTO documents (id, ts, table_id, json_value, deleted, \
     prev_ts) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
const INSERT_OVERWRITE_DOCUMENT: &str =
    "INSERT OR REPLACE INTO documents (id, ts, table_id, json_value, deleted, \
     prev_ts) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
const INSERT_INDEX: &str = "INSERT INTO indexes \
     (index_id, ts, key, deleted, table_id, document_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
const INSERT_OVERWRITE_INDEX: &str = "INSERT OR REPLACE INTO indexes \
     (index_id, ts, key, deleted, table_id, document_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

pub(crate) struct Inner {
    pub(crate) connection: Connection,
}

/// How a file database is opened.
#[derive(Clone, Debug)]
pub struct SqliteOptions {
    /// Read-only connections next to the single writer. A file database in WAL mode lets them
    /// read while the writer commits. An in-memory database has one connection only.
    pub read_connections: usize,
}

impl Default for SqliteOptions {
    fn default() -> Self {
        Self {
            read_connections: 4,
        }
    }
}

#[derive(Clone)]
pub struct SqlitePersistence {
    /// The one connection that writes, and runs the conformance and auth statements.
    inner: Arc<Pool<Inner>>,
    /// Read-only connections. `None` for an in-memory database, where reads use `inner`.
    readers: Option<Arc<Pool<Inner>>>,
    lease: Arc<lease::LeaseState>,
    path: Option<Arc<PathBuf>>,
}

impl SqlitePersistence {
    pub fn open(path: &Path) -> Result<Self, PersistError> {
        Self::open_with(path, &SqliteOptions::default())
    }

    pub fn open_with(path: &Path, options: &SqliteOptions) -> Result<Self, PersistError> {
        let connection = Connection::open(path).map_err(definite)?;
        let mut this = Self::from_connection(connection)?;
        this.path = Some(Arc::new(path.to_path_buf()));
        if options.read_connections > 0 {
            let reader_path = path.to_path_buf();
            let make = move || open_reader(&reader_path);
            let first = make()?;
            this.readers = Some(Arc::new(Pool::new(
                options.read_connections,
                vec![first],
                make,
                |_| true,
            )));
        }
        Ok(this)
    }

    pub fn open_in_memory() -> Result<Self, PersistError> {
        let connection = Connection::open_in_memory().map_err(definite)?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self, PersistError> {
        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .or_else(|_| connection.execute_batch("PRAGMA busy_timeout=5000;"))
            .map_err(definite)?;
        connection
            .execute_batch(DOCUMENTS_INIT)
            .and_then(|_| connection.execute_batch(INDEXES_INIT))
            .and_then(|_| connection.execute_batch(PERSISTENCE_GLOBALS_INIT))
            .and_then(|_| connection.execute_batch(CONFORMANCE_INIT))
            .and_then(|_| connection.execute_batch(auth_store::AUTH_INIT))
            .and_then(|_| connection.execute_batch(job_store::JOBS_INIT))
            .and_then(|_| job_store::migrate(&connection))
            .map_err(definite)?;
        // Databases made before a writer epoch was recorded on each backfill.
        let has_claim: bool = connection
            .prepare(
                "SELECT 1 FROM pragma_table_info('index_backfills') WHERE name = 'claimed_epoch'",
            )
            .and_then(|mut stmt| stmt.exists([]))
            .map_err(definite)?;
        if !has_claim {
            connection
                .execute_batch("ALTER TABLE index_backfills ADD COLUMN claimed_epoch INTEGER")
                .map_err(definite)?;
        }
        Ok(Self {
            inner: Arc::new(Pool::new(
                1,
                vec![Inner { connection }],
                || {
                    Err(PersistError::Definite(
                        "the writer connection is gone".into(),
                    ))
                },
                |_| true,
            )),
            readers: None,
            lease: Arc::new(lease::LeaseState::default()),
            path: None,
        })
    }

    /// Gives up the writer lease (the lock file is closed). For an orderly shutdown, and for a
    /// test that is about to remove the database.
    pub fn release_writer_lease(&self) {
        self.lease.release();
    }

    /// A connection for a read. Reads use a reader when the database has them, so they do not
    /// queue behind a commit; otherwise they share the writer.
    fn read(&self) -> Result<PoolGuard<'_, Inner>, PersistError> {
        match &self.readers {
            Some(readers) => readers.get(),
            None => self.inner.get(),
        }
    }
}

fn open_reader(path: &Path) -> Result<Inner, PersistError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(definite)?;
    // `query_only` makes a stray write on a reader an error instead of a second writer.
    connection
        .execute_batch("PRAGMA busy_timeout=5000; PRAGMA query_only=ON;")
        .map_err(definite)?;
    Ok(Inner { connection })
}

impl PersistenceReader for SqlitePersistence {
    fn load_documents(
        &self,
        range: TimestampRange,
        order: Order,
    ) -> Result<Vec<DocumentLogEntry>, PersistError> {
        let order_sql = match order {
            Order::Asc => "ASC",
            Order::Desc => "DESC",
        };
        let sql = format!(
            "SELECT id, ts, table_id, json_value, deleted, prev_ts FROM documents \
             WHERE ts >= ?1 AND ts < ?2 ORDER BY ts {order_sql}, table_id {order_sql}, id {order_sql}"
        );
        let guard = self.read()?;
        let mut stmt = guard.connection.prepare(&sql).map_err(definite)?;
        let rows = stmt
            .query_map(
                params![i64_ts(range.start_inclusive)?, i64_ts(range.end_exclusive)?],
                read_document_row,
            )
            .map_err(definite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(definite)?
            .into_iter()
            .collect()
    }

    fn latest_document(
        &self,
        id: &DocumentId,
        snapshot: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        self.one_document(
            "SELECT id, ts, table_id, json_value, deleted, prev_ts FROM documents \
             WHERE table_id = ?1 AND id = ?2 AND ts <= ?3 ORDER BY ts DESC LIMIT 1",
            id,
            snapshot,
        )
    }

    fn previous_revision(
        &self,
        id: &DocumentId,
        at: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        // Convex `PREV_REV_QUERY`.
        self.one_document(
            "SELECT id, ts, table_id, json_value, deleted, prev_ts FROM documents \
             WHERE table_id = ?1 AND id = ?2 AND ts < ?3 ORDER BY ts DESC LIMIT 1",
            id,
            at,
        )
    }

    fn exact_revision(
        &self,
        id: &DocumentId,
        ts: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        // Convex `EXACT_REV_QUERY`.
        self.one_document(
            "SELECT id, ts, table_id, json_value, deleted, prev_ts FROM documents \
             WHERE table_id = ?1 AND id = ?2 AND ts = ?3",
            id,
            ts,
        )
    }

    fn document_history(
        &self,
        id: &DocumentId,
        snapshot: Timestamp,
    ) -> Result<Vec<DocumentLogEntry>, PersistError> {
        let guard = self.read()?;
        let mut stmt = guard
            .connection
            .prepare(
                "SELECT id, ts, table_id, json_value, deleted, prev_ts FROM documents \
                 WHERE table_id = ?1 AND id = ?2 AND ts <= ?3 ORDER BY ts DESC",
            )
            .map_err(definite)?;
        let rows = stmt
            .query_map(
                params![id.tablet.as_bytes(), id.id, i64_ts(snapshot)?],
                read_document_row,
            )
            .map_err(definite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(definite)?
            .into_iter()
            .collect()
    }

    fn index_scan(
        &self,
        index_id: &IndexId,
        read_timestamp: Timestamp,
        interval: &Interval,
        order: Order,
    ) -> Result<Vec<IndexScanRow>, PersistError> {
        // Convex `SqlitePersistence::_index_scan_inner`.
        // Index-only backfill rows use a later commit than the document, so the
        // document join is the latest live revision at the read timestamp.
        let upper = if interval.end.is_some() {
            " AND key < ?4"
        } else {
            ""
        };
        let ord = match order {
            Order::Asc => "ASC",
            Order::Desc => "DESC",
        };
        let sql = format!(
            r#"
SELECT B.key, C.ts, B.document_id, C.table_id, C.json_value, C.prev_ts
FROM (
    SELECT index_id, key, MAX(ts) as max_ts
    FROM indexes
    WHERE index_id = ?1 AND ts <= ?2 AND key >= ?3{upper}
    GROUP BY index_id, key
) A
JOIN indexes B
  ON B.deleted = 0
 AND A.index_id = B.index_id
 AND A.key = B.key
 AND A.max_ts = B.ts
JOIN documents C
  ON C.table_id = B.table_id
 AND C.id = B.document_id
 AND C.deleted = 0
 AND C.ts = (
    SELECT MAX(D.ts) FROM documents D
    WHERE D.table_id = B.table_id AND D.id = B.document_id AND D.ts <= ?2
 )
ORDER BY B.key {ord}
"#
        );
        let guard = self.read()?;
        let mut stmt = guard.connection.prepare(&sql).map_err(definite)?;
        let ts = i64_ts(read_timestamp)?;
        let map_row = |row: &rusqlite::Row<'_>| {
            let key: Vec<u8> = row.get(0)?;
            let row_ts: i64 = row.get(1)?;
            let document_id: Vec<u8> = row.get(2)?;
            let table_id: Vec<u8> = row.get(3)?;
            let json_value: Option<String> = row.get(4)?;
            let prev_ts: Option<i64> = row.get(5)?;
            Ok((key, row_ts, document_id, table_id, json_value, prev_ts))
        };
        let raw = if let Some(end) = &interval.end {
            stmt.query_map(
                params![index_id.as_bytes(), ts, interval.start, end],
                map_row,
            )
            .map_err(definite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(definite)?
        } else {
            stmt.query_map(params![index_id.as_bytes(), ts, interval.start], map_row)
                .map_err(definite)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(definite)?
        };
        let mut out = Vec::with_capacity(raw.len());
        for (key, row_ts, document_id, table_id, json_value, prev_ts) in raw {
            let json_value = json_value.ok_or_else(|| {
                PersistError::Definite(format!(
                    "index {} points at a document with no json at ts {row_ts}",
                    index_id.as_str()
                ))
            })?;
            let entry = document_from_parts(
                document_id,
                row_ts,
                table_id,
                Some(json_value),
                false,
                prev_ts,
            )?;
            let value = entry.value.expect("live document");
            out.push(IndexScanRow {
                key,
                document: LatestDocument {
                    ts: entry.ts,
                    id: entry.id,
                    value,
                    prev_ts: entry.prev_ts,
                },
            });
        }
        Ok(out)
    }

    fn index_scan_ids(
        &self,
        index_id: &IndexId,
        read_timestamp: Timestamp,
        interval: &Interval,
        order: Order,
        limit: Option<usize>,
    ) -> Result<Vec<IndexScanEntry>, PersistError> {
        if limit == Some(0) {
            return Ok(Vec::new());
        }
        // The index table's primary key is (index_id, key, ts), so reading it ordered by key and
        // ts needs no sort, no join and no grouping, and the rows can be taken as they come and
        // the rest left unread once `limit` entries are settled. Tombstones are rows too: the
        // latest version of each key is picked in `LiveEntries`.
        let upper = if interval.end.is_some() {
            " AND key < ?4"
        } else {
            ""
        };
        let ord = match order {
            Order::Asc => "ASC",
            Order::Desc => "DESC",
        };
        let sql = format!(
            "SELECT key, deleted, table_id, document_id FROM indexes \
             WHERE index_id = ?1 AND ts <= ?2 AND key >= ?3{upper} \
             ORDER BY key {ord}, ts {ord}"
        );
        let guard = self.read()?;
        let mut stmt = guard.connection.prepare(&sql).map_err(definite)?;
        let ts = i64_ts(read_timestamp)?;
        let index_bytes = index_id.as_bytes();
        let mut args: Vec<&dyn rusqlite::ToSql> = vec![&index_bytes, &ts, &interval.start];
        if let Some(end) = &interval.end {
            args.push(end);
        }
        let mut rows = stmt.query(args.as_slice()).map_err(definite)?;
        let mut live = LiveEntries::new(order, limit);
        while let Some(row) = rows.next().map_err(definite)? {
            let key: Vec<u8> = row.get(0).map_err(definite)?;
            let deleted: i64 = row.get(1).map_err(definite)?;
            let id = if deleted == 0 {
                let table_id: Option<Vec<u8>> = row.get(2).map_err(definite)?;
                let document_id: Option<Vec<u8>> = row.get(3).map_err(definite)?;
                match (table_id, document_id) {
                    (Some(table_id), Some(document_id)) => {
                        Some(index_target(table_id, document_id)?)
                    }
                    _ => {
                        return Err(PersistError::Definite(format!(
                            "index {} has a live row that points at no document",
                            index_id.as_str()
                        )))
                    }
                }
            } else {
                None
            };
            if live.push(key, id) {
                break;
            }
        }
        Ok(live.finish())
    }

    fn get_persistence_global(
        &self,
        key: PersistenceGlobalKey,
    ) -> Result<Option<JsonValue>, PersistError> {
        let guard = self.read()?;
        read_global(&guard.connection, key)
    }

    fn load_index_writes(
        &self,
        range: TimestampRange,
    ) -> Result<Vec<PersistenceIndexEntry>, PersistError> {
        let guard = self.read()?;
        let mut stmt = guard
            .connection
            .prepare(
                "SELECT index_id, ts, key, deleted, table_id, document_id FROM indexes \
                 WHERE ts >= ?1 AND ts < ?2 ORDER BY ts, index_id, key",
            )
            .map_err(definite)?;
        let rows = stmt
            .query_map(
                params![i64_ts(range.start_inclusive)?, i64_ts(range.end_exclusive)?],
                |row| {
                    let index_id: Vec<u8> = row.get(0)?;
                    let ts: i64 = row.get(1)?;
                    let key: Vec<u8> = row.get(2)?;
                    let deleted: i64 = row.get(3)?;
                    let table_id: Option<Vec<u8>> = row.get(4)?;
                    let document_id: Option<Vec<u8>> = row.get(5)?;
                    Ok((index_id, ts, key, deleted, table_id, document_id))
                },
            )
            .map_err(definite)?;
        let mut out = Vec::new();
        for row in rows {
            let (index_id, ts, key, deleted, table_id, document_id) = row.map_err(definite)?;
            let value = match (deleted != 0, table_id, document_id) {
                (false, Some(table_id), Some(document_id)) => Some(DocumentId::new(
                    TabletId::from_bytes(&table_id)
                        .map_err(|_| PersistError::Definite("table_id is not utf-8".into()))?,
                    document_id,
                )),
                (false, _, _) => {
                    return Err(PersistError::Definite(format!(
                        "live index row at ts {ts} has no document"
                    )))
                }
                (true, _, _) => None,
            };
            out.push(PersistenceIndexEntry {
                ts: Timestamp::from_i64(ts)
                    .map_err(|_| PersistError::Definite(format!("bad index ts {ts}")))?,
                index_id: IndexId::from_bytes(&index_id)
                    .map_err(|_| PersistError::Definite("index_id is not utf-8".into()))?,
                key,
                value,
            });
        }
        Ok(out)
    }

    fn max_ts(&self) -> Result<Option<Timestamp>, PersistError> {
        let guard = self.read()?;
        // One statement, so the document log and the global are read from the same state. The
        // global holds the newest index-only commit; a value that is not a number is ignored.
        // SQLite's two-argument MAX is NULL if either side is, so absent values become -1.
        let newest: i64 = guard
            .connection
            .query_row(
                "SELECT MAX(
                     COALESCE((SELECT MAX(ts) FROM documents), -1),
                     COALESCE((SELECT CAST(json_value AS INTEGER) FROM persistence_globals
                               WHERE key = ?1 AND json_value GLOB '[0-9]*'), -1)
                 )",
                params![PersistenceGlobalKey::MaxRepeatableTimestamp.as_str()],
                |row| row.get(0),
            )
            .map_err(definite)?;
        if newest < 0 {
            return Ok(None);
        }
        Timestamp::from_i64(newest)
            .map(Some)
            .map_err(|_| PersistError::Definite(format!("bad max ts {newest}")))
    }
}

impl SqlitePersistence {
    fn one_document(
        &self,
        sql: &str,
        id: &DocumentId,
        ts: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        let guard = self.read()?;
        let mut stmt = guard.connection.prepare(sql).map_err(definite)?;
        let row = stmt
            .query_row(
                params![id.tablet.as_bytes(), id.id, i64_ts(ts)?],
                read_document_row,
            )
            .optional()
            .map_err(definite)?;
        row.transpose()
    }
}

impl Persistence for SqlitePersistence {
    fn is_fresh(&self) -> bool {
        let Ok(guard) = self.read() else {
            return false;
        };
        // One statement, so both counts come from the same state.
        guard
            .connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM documents) = 0 \
                 AND (SELECT COUNT(*) FROM persistence_globals) = 0",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false)
    }

    fn reader(&self) -> Arc<dyn PersistenceReader> {
        Arc::new(self.clone())
    }

    fn write(
        &self,
        documents: &[DocumentLogEntry],
        indexes: &[PersistenceIndexEntry],
        conflict_strategy: ConflictStrategy,
    ) -> Result<(), PersistError> {
        let mut guard = self.inner.lock()?;
        let epoch = self.lease.epoch();
        // With a lease, take the write lock first so the epoch check and the writes see one
        // state; without one the transaction stays deferred as before.
        let behavior = if epoch.is_some() {
            TransactionBehavior::Immediate
        } else {
            TransactionBehavior::Deferred
        };
        let tx = guard
            .connection
            .transaction_with_behavior(behavior)
            .map_err(definite)?;
        if let Some(epoch) = epoch {
            if let Err(err) = lease::check_epoch(&tx, epoch) {
                let _ = tx.rollback();
                return Err(err);
            }
        }
        let written = write_in_tx(&tx, documents, indexes, conflict_strategy);
        if let Err(err) = written {
            let _ = tx.rollback();
            return Err(err);
        }
        tx.commit().map_err(uncertain)
    }

    fn try_acquire_writer_lease(&self) -> Result<Lease, PersistError> {
        let lock_path = self.path.as_ref().map(|path| {
            let mut name = path.as_os_str().to_owned();
            name.push(".lock");
            PathBuf::from(name)
        });
        self.lease.acquire(lock_path.as_deref(), &self.inner)
    }

    fn advance_published_ts(&self, ts: Timestamp) -> Result<(), PersistError> {
        let guard = self.inner.lock()?;
        guard
            .connection
            .execute(
                "INSERT INTO persistence_globals (key, json_value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET json_value = MAX(
                     CAST(json_value AS INTEGER), CAST(excluded.json_value AS INTEGER))",
                params![
                    PersistenceGlobalKey::PublishedTimestamp.as_str(),
                    ts.as_u64().to_string()
                ],
            )
            .map_err(definite)?;
        Ok(())
    }

    fn pool_stats(&self) -> Option<PoolStats> {
        let writer = self.inner.stats();
        let Some(readers) = &self.readers else {
            return Some(writer);
        };
        let read = readers.stats();
        Some(PoolStats {
            max: writer.max + read.max,
            open: writer.open + read.open,
            in_use: writer.in_use + read.in_use,
            peak_in_use: writer.peak_in_use.max(read.peak_in_use),
            acquires: writer.acquires + read.acquires,
            waits: writer.waits + read.waits,
            wait_ns: writer.wait_ns + read.wait_ns,
        })
    }

    fn write_persistence_global(
        &self,
        key: PersistenceGlobalKey,
        value: &JsonValue,
    ) -> Result<(), PersistError> {
        let guard = self.inner.lock()?;
        guard
            .connection
            .execute(
                "INSERT OR REPLACE INTO persistence_globals (key, json_value) VALUES (?1, ?2)",
                params![key.as_str(), value.to_string()],
            )
            .map_err(definite)?;
        Ok(())
    }
}

fn write_in_tx(
    tx: &Transaction<'_>,
    documents: &[DocumentLogEntry],
    indexes: &[PersistenceIndexEntry],
    conflict_strategy: ConflictStrategy,
) -> Result<(), PersistError> {
    let document_sql = match conflict_strategy {
        ConflictStrategy::Error => INSERT_DOCUMENT,
        ConflictStrategy::Overwrite => INSERT_OVERWRITE_DOCUMENT,
    };
    let index_sql = match conflict_strategy {
        ConflictStrategy::Error => INSERT_INDEX,
        ConflictStrategy::Overwrite => INSERT_OVERWRITE_INDEX,
    };
    {
        let mut stmt = tx.prepare(document_sql).map_err(definite)?;
        for document in documents {
            let deleted: i64 = if document.value.is_none() { 1 } else { 0 };
            stmt.execute(params![
                document.id.id,
                i64_ts(document.ts)?,
                document.id.tablet.as_bytes(),
                document.value,
                deleted,
                document.prev_ts.map(|ts| ts.to_i64().ok()).flatten(),
            ])
            .map_err(definite)?;
        }
    }
    {
        let mut stmt = tx.prepare(index_sql).map_err(definite)?;
        for index in indexes {
            let (deleted, table_id, document_id): (i64, Option<&[u8]>, Option<&[u8]>) =
                match &index.value {
                    Some(id) => (0, Some(id.tablet.as_bytes()), Some(id.id.as_slice())),
                    None => (1, None, None),
                };
            stmt.execute(params![
                index.index_id.as_bytes(),
                i64_ts(index.ts)?,
                index.key,
                deleted,
                table_id,
                document_id,
            ])
            .map_err(definite)?;
        }
    }
    if let Some(ts) = commit_timestamp(documents, indexes) {
        let current = read_global(tx, PersistenceGlobalKey::MaxRepeatableTimestamp)?;
        let current_ts = current
            .and_then(|value| value.as_u64())
            .and_then(|value| Timestamp::from_u64(value).ok());
        let stored = match current_ts {
            Some(existing) if existing > ts => existing,
            _ => ts,
        };
        tx.execute(
            "INSERT OR REPLACE INTO persistence_globals (key, json_value) VALUES (?1, ?2)",
            params![
                PersistenceGlobalKey::MaxRepeatableTimestamp.as_str(),
                serde_json::json!(stored.as_u64()).to_string()
            ],
        )
        .map_err(definite)?;
    }
    Ok(())
}

fn commit_timestamp(
    documents: &[DocumentLogEntry],
    indexes: &[PersistenceIndexEntry],
) -> Option<Timestamp> {
    documents
        .iter()
        .map(|document| document.ts)
        .chain(indexes.iter().map(|index| index.ts))
        .max()
}

fn read_global(
    conn: &Connection,
    key: PersistenceGlobalKey,
) -> Result<Option<JsonValue>, PersistError> {
    let text: Option<String> = conn
        .query_row(
            "SELECT json_value FROM persistence_globals WHERE key = ?1",
            params![key.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(definite)?;
    match text {
        Some(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|err| PersistError::Definite(format!("bad global json: {err}"))),
        None => Ok(None),
    }
}

fn read_document_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<DocumentLogEntry, PersistError>> {
    let id: Vec<u8> = row.get(0)?;
    let ts: i64 = row.get(1)?;
    let table_id: Vec<u8> = row.get(2)?;
    let json_value: Option<String> = row.get(3)?;
    let deleted: i64 = row.get(4)?;
    let prev_ts: Option<i64> = row.get(5)?;
    Ok(document_from_parts(
        id,
        ts,
        table_id,
        json_value,
        deleted != 0,
        prev_ts,
    ))
}

fn i64_ts(ts: Timestamp) -> Result<i64, PersistError> {
    ts.to_i64()
        .map_err(|_| PersistError::Definite(format!("timestamp {ts} does not fit in i64")))
}

pub(crate) fn definite(err: impl ToString) -> PersistError {
    PersistError::Definite(err.to_string())
}

fn uncertain(err: impl ToString) -> PersistError {
    PersistError::Uncertain(err.to_string())
}

mod auth_store;
mod conformance_store;
mod job_store;

#[cfg(test)]
mod tests {
    use super::*;
    use refhir_persistence::conformance;
    use refhir_persistence::ConformanceStore as _;

    #[test]
    fn memory_log_roundtrip() {
        let persistence = SqlitePersistence::open_in_memory().unwrap();
        conformance::exercise(&persistence);
        conformance::exercise_index_scan_ids(&persistence);
    }

    fn temp_db(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "refhir-sqlite-{tag}-{}-{}.sqlite",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ))
    }

    fn remove_db(path: &Path) {
        for suffix in ["", "-wal", "-shm", ".lock"] {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            let _ = std::fs::remove_file(name);
        }
    }

    fn write_doc(p: &SqlitePersistence, ts: u64, name: &str) -> Result<(), PersistError> {
        let id = DocumentId::new(TabletId::new("lease"), name.as_bytes());
        let ts = Timestamp::from_u64(ts).unwrap();
        p.write(
            &[DocumentLogEntry {
                ts,
                id: id.clone(),
                value: Some("{}".into()),
                prev_ts: None,
            }],
            &[PersistenceIndexEntry {
                ts,
                index_id: IndexId::new("lease/by_id"),
                key: name.as_bytes().to_vec(),
                value: Some(id),
            }],
            ConflictStrategy::Error,
        )
    }

    #[test]
    fn only_one_process_holds_the_writer_lease_on_a_file() {
        let path = temp_db("lease");
        let first = SqlitePersistence::open(&path).unwrap();
        let second = SqlitePersistence::open(&path).unwrap();
        assert_eq!(
            first.try_acquire_writer_lease().unwrap(),
            Lease::Held { epoch: 1 }
        );
        assert_eq!(second.try_acquire_writer_lease().unwrap(), Lease::Busy);
        write_doc(&first, 1, "before").unwrap();

        // The holder lets go (as it does when its process dies); the next one takes over at a
        // higher epoch, and the first can no longer write.
        first.lease.release();
        assert_eq!(
            second.try_acquire_writer_lease().unwrap(),
            Lease::Held { epoch: 2 }
        );
        let stale = write_doc(&first, 2, "stale").unwrap_err();
        assert!(matches!(stale, PersistError::Fenced(_)), "{stale}");
        write_doc(&second, 2, "after").unwrap();

        // The tables the committer does not write are fenced too: a replaced primary cannot move
        // a backfill's cursor, mark it complete, or announce a catalog change.
        let fenced = |result: Result<(), PersistError>| {
            assert!(matches!(result, Err(PersistError::Fenced(_))), "{result:?}");
        };
        fenced(first.update_backfill(1, Some("p"), false));
        fenced(first.complete_backfill(1, 1, "Patient"));
        fenced(first.mark_base_ready(1, "Patient"));
        fenced(first.insert_backfill(1, 1, "Patient", &[]).map(drop));
        fenced(first.bump_catalog_version().map(drop));
        let version = second
            .bump_catalog_version()
            .expect("the new holder announces changes");
        assert_eq!(second.catalog_version().unwrap(), version);
        assert_eq!(second.max_ts().unwrap().map(|ts| ts.as_u64()), Some(2));
        remove_db(&path);
    }

    #[test]
    fn an_in_memory_database_has_nothing_to_lease() {
        let memory = SqlitePersistence::open_in_memory().unwrap();
        assert_eq!(
            memory.try_acquire_writer_lease().unwrap(),
            Lease::Held { epoch: 0 }
        );
        write_doc(&memory, 1, "unfenced").unwrap();
    }

    #[test]
    fn readers_see_committed_writes_on_their_own_connections() {
        let path = temp_db("readers");
        let p = SqlitePersistence::open_with(
            &path,
            &SqliteOptions {
                read_connections: 3,
            },
        )
        .unwrap();
        write_doc(&p, 1, "a").unwrap();
        write_doc(&p, 2, "b").unwrap();
        let range = TimestampRange {
            start_inclusive: Timestamp::from_u64(1).unwrap(),
            end_exclusive: Timestamp::from_u64(3).unwrap(),
        };
        // Reads go to reader connections, so the writer connection is untouched by them.
        let before = p.inner.stats().acquires;
        assert_eq!(p.load_documents(range, Order::Asc).unwrap().len(), 2);
        assert_eq!(p.inner.stats().acquires, before);
        assert!(p.pool_stats().unwrap().acquires > before);
        // A reader connection cannot write.
        let readers = p.readers.as_ref().expect("file databases have readers");
        let reader = readers.get().unwrap();
        assert!(reader
            .connection
            .execute("DELETE FROM documents", [])
            .is_err());
        drop(reader);
        remove_db(&path);
    }

    #[test]
    fn a_backfill_is_claimed_by_the_epoch_that_runs_it() {
        let path = temp_db("claims");
        let first = SqlitePersistence::open(&path).unwrap();
        assert_eq!(
            first.try_acquire_writer_lease().unwrap(),
            Lease::Held { epoch: 1 }
        );
        let job = first.insert_backfill(1, 5, "Patient", &[]).unwrap();
        let claim = |p: &SqlitePersistence| {
            p.load_backfills()
                .unwrap()
                .into_iter()
                .find(|row| row.id == job)
                .map(|row| row.claimed_epoch)
        };
        assert_eq!(claim(&first), Some(None), "a new job is unclaimed");
        assert!(first.claim_backfill(job).unwrap());
        assert_eq!(claim(&first), Some(Some(1)));
        assert!(
            first.claim_backfill(job).unwrap(),
            "claiming again is the same claim"
        );

        // The first writer dies. The next one takes the job over: the claim moves.
        first.lease.release();
        let second = SqlitePersistence::open(&path).unwrap();
        assert_eq!(
            second.try_acquire_writer_lease().unwrap(),
            Lease::Held { epoch: 2 }
        );
        assert!(second.claim_backfill(job).unwrap());
        assert_eq!(claim(&second), Some(Some(2)));

        // A process with an older (or no) epoch is refused a job a newer one holds.
        let stale = SqlitePersistence::open(&path).unwrap();
        let refused = stale.claim_backfill(job).unwrap_err();
        assert!(matches!(refused, PersistError::Fenced(_)), "{refused}");
        assert_eq!(claim(&second), Some(Some(2)), "and the claim is unchanged");

        // A finished job, and one that does not exist, have nothing to claim.
        second.update_backfill(job, Some("p9"), true).unwrap();
        assert!(!second.claim_backfill(job).unwrap());
        assert!(!second.claim_backfill(job + 100).unwrap());
        remove_db(&path);
    }

    #[test]
    fn a_database_from_before_claims_gets_the_column() {
        let path = temp_db("migrate");
        {
            // The table as an earlier version made it.
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE index_backfills (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        package_id INTEGER NOT NULL,
                        watermark_ts INTEGER NOT NULL,
                        tablet TEXT NOT NULL,
                        cursor_id TEXT,
                        done INTEGER NOT NULL
                    );
                    INSERT INTO index_backfills (package_id, watermark_ts, tablet, cursor_id, done)
                    VALUES (1, 7, 'Patient', 'p3', 0);",
                )
                .unwrap();
        }
        let p = SqlitePersistence::open(&path).unwrap();
        let jobs = p.load_backfills().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].cursor_id.as_deref(), Some("p3"));
        assert_eq!(jobs[0].claimed_epoch, None);
        assert!(p.claim_backfill(jobs[0].id).unwrap());
        // Opening it again does not try to add the column twice.
        drop(p);
        SqlitePersistence::open(&path).unwrap();
        remove_db(&path);
    }

    #[test]
    fn the_two_catalog_counters_count_separately() {
        let p = SqlitePersistence::open_in_memory().unwrap();
        assert_eq!(
            (p.catalog_version().unwrap(), p.catalog_revision().unwrap()),
            (0, 0)
        );
        assert_eq!(p.bump_catalog_revision().unwrap(), 1);
        assert_eq!(p.bump_catalog_revision().unwrap(), 2);
        assert_eq!(p.bump_catalog_version().unwrap(), 1);
        assert_eq!(
            (p.catalog_version().unwrap(), p.catalog_revision().unwrap()),
            (1, 2)
        );
    }

    #[test]
    fn file_reopen_keeps_the_log() {
        let path = std::env::temp_dir().join(format!(
            "refhir-sqlite-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let persistence = SqlitePersistence::open(&path).unwrap();
            conformance::exercise(&persistence);
        }
        let reopened = SqlitePersistence::open(&path).unwrap();
        assert!(!reopened.is_fresh());
        assert_eq!(reopened.max_ts().unwrap().map(|ts| ts.as_u64()), Some(2));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    }
}
