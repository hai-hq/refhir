//! SQLite document log, index log, and persistence globals.
//!
//! Convex: `crates/sqlite/src/lib.rs` (`DOCUMENTS_INIT`, `INDEXES_INIT`,
//! `PERSISTENCE_GLOBALS_INIT`, `write`, `index_scan`).

use std::path::Path;
use std::sync::{Arc, Mutex};

use refhir_persistence::{
    document_from_parts, ConflictStrategy, DocumentLogEntry, IndexScanRow, LatestDocument,
    PersistError, Persistence, PersistenceGlobalKey, PersistenceIndexEntry, PersistenceReader,
    TimestampRange,
};
use refhir_types::{DocumentId, IndexId, Interval, Order, Timestamp};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::Value as JsonValue;

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
    done INTEGER NOT NULL
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

#[derive(Clone)]
pub struct SqlitePersistence {
    inner: Arc<Mutex<Inner>>,
}

impl SqlitePersistence {
    pub fn open(path: &Path) -> Result<Self, PersistError> {
        let connection = Connection::open(path).map_err(definite)?;
        Self::from_connection(connection)
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
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner { connection })),
        })
    }
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
        let guard = self.inner.lock().expect("sqlite");
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
        let guard = self.inner.lock().expect("sqlite");
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
        let guard = self.inner.lock().expect("sqlite");
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

    fn get_persistence_global(
        &self,
        key: PersistenceGlobalKey,
    ) -> Result<Option<JsonValue>, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        read_global(&guard.connection, key)
    }

    fn max_ts(&self) -> Result<Option<Timestamp>, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        let doc_max: Option<i64> = guard
            .connection
            .query_row("SELECT MAX(ts) FROM documents", [], |row| row.get(0))
            .map_err(definite)?;
        let doc_ts = match doc_max {
            Some(value) => Some(
                Timestamp::from_i64(value)
                    .map_err(|_| PersistError::Definite(format!("bad max ts {value}")))?,
            ),
            None => None,
        };
        let global = read_global(
            &guard.connection,
            PersistenceGlobalKey::MaxRepeatableTimestamp,
        )?;
        let global_ts = global
            .and_then(|value| value.as_u64())
            .map(|value| {
                Timestamp::from_u64(value)
                    .map_err(|_| PersistError::Definite(format!("bad global ts {value}")))
            })
            .transpose()?;
        Ok(match (doc_ts, global_ts) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        })
    }
}

impl SqlitePersistence {
    fn one_document(
        &self,
        sql: &str,
        id: &DocumentId,
        ts: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
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
        let guard = self.inner.lock().expect("sqlite");
        let docs: i64 = guard
            .connection
            .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
            .unwrap_or(1);
        let globals: i64 = guard
            .connection
            .query_row("SELECT COUNT(*) FROM persistence_globals", [], |row| {
                row.get(0)
            })
            .unwrap_or(1);
        docs == 0 && globals == 0
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
        let mut guard = self.inner.lock().expect("sqlite");
        let tx = guard.connection.transaction().map_err(definite)?;
        let written = write_in_tx(&tx, documents, indexes, conflict_strategy);
        if let Err(err) = written {
            let _ = tx.rollback();
            return Err(err);
        }
        tx.commit().map_err(uncertain)
    }

    fn write_persistence_global(
        &self,
        key: PersistenceGlobalKey,
        value: &JsonValue,
    ) -> Result<(), PersistError> {
        let guard = self.inner.lock().expect("sqlite");
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

    #[test]
    fn memory_log_roundtrip() {
        let persistence = SqlitePersistence::open_in_memory().unwrap();
        conformance::exercise(&persistence);
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
