//! Postgres `Persistence` implementation.
//!
//! Convex: `crates/postgres` implements the same `Persistence` trait as SQLite.
//! Tables match `DOCUMENTS_INIT` / `INDEXES_INIT` / `PERSISTENCE_GLOBALS_INIT`.
//! SQLite stays the default for local runs.

use std::sync::{Arc, Mutex};

use postgres::types::ToSql;
use postgres::{Client, NoTls, Row, Transaction};
use refhir_persistence::{
    document_from_parts, ConflictStrategy, DocumentLogEntry, IndexScanRow, LatestDocument,
    PersistError, Persistence, PersistenceGlobalKey, PersistenceIndexEntry, PersistenceReader,
    TimestampRange,
};
use refhir_types::{DocumentId, IndexId, Interval, Order, Timestamp};
use serde_json::Value as JsonValue;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS documents (
    id BYTEA NOT NULL,
    ts BIGINT NOT NULL,
    table_id BYTEA NOT NULL,
    json_value TEXT NULL,
    deleted INTEGER NOT NULL,
    prev_ts BIGINT,
    PRIMARY KEY (ts, table_id, id)
);
CREATE INDEX IF NOT EXISTS documents_by_table_and_id ON documents (table_id, id, ts);

CREATE TABLE IF NOT EXISTS indexes (
    index_id BYTEA NOT NULL,
    ts BIGINT NOT NULL,
    key BYTEA NOT NULL,
    deleted INTEGER NOT NULL,
    table_id BYTEA NULL,
    document_id BYTEA NULL,
    PRIMARY KEY (index_id, key, ts)
);

CREATE TABLE IF NOT EXISTS persistence_globals (
    key TEXT NOT NULL,
    json_value TEXT NOT NULL,
    PRIMARY KEY (key)
);

CREATE TABLE IF NOT EXISTS packages (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    fhir_version TEXT NOT NULL,
    UNIQUE (name, version)
);
CREATE TABLE IF NOT EXISTS structure_definitions (
    package_id BIGINT NOT NULL,
    url TEXT NOT NULL,
    type_name TEXT NOT NULL,
    kind TEXT NOT NULL,
    is_abstract INTEGER NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, url)
);
CREATE TABLE IF NOT EXISTS search_parameters (
    package_id BIGINT NOT NULL,
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
    id BIGSERIAL PRIMARY KEY,
    package_id BIGINT NOT NULL,
    watermark_ts BIGINT NOT NULL,
    tablet TEXT NOT NULL,
    cursor_id TEXT,
    done INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS terminology (
    package_id BIGINT NOT NULL,
    resource_type TEXT NOT NULL,
    url TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, url)
);
CREATE TABLE IF NOT EXISTS value_set_expansion (
    package_id BIGINT NOT NULL,
    url TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, url)
);
CREATE TABLE IF NOT EXISTS backfill_parameter (
    backfill_id BIGINT NOT NULL,
    url TEXT NOT NULL,
    PRIMARY KEY (backfill_id, url)
);
"#;

pub(crate) struct Inner {
    pub(crate) client: Client,
}

#[derive(Clone)]
pub struct PostgresPersistence {
    inner: Arc<Mutex<Inner>>,
}

impl PostgresPersistence {
    pub fn connect(url: &str) -> Result<Self, PersistError> {
        let mut client = Client::connect(url, NoTls).map_err(definite)?;
        // Concurrent `CREATE TABLE IF NOT EXISTS` can still collide on a fresh database, so
        // schema setup runs under a transaction-scoped advisory lock.
        let mut tx = client.transaction().map_err(definite)?;
        tx.batch_execute("SELECT pg_advisory_xact_lock(7428310)")
            .map_err(definite)?;
        tx.batch_execute(SCHEMA).map_err(definite)?;
        tx.batch_execute(auth_store::AUTH_INIT).map_err(definite)?;
        tx.commit().map_err(definite)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner { client })),
        })
    }
}

impl PersistenceReader for PostgresPersistence {
    fn load_documents(
        &self,
        range: TimestampRange,
        order: Order,
    ) -> Result<Vec<DocumentLogEntry>, PersistError> {
        let ord = order_sql(order);
        let sql = format!(
            "SELECT id, ts, table_id, json_value, deleted, prev_ts FROM documents \
             WHERE ts >= $1 AND ts < $2 ORDER BY ts {ord}, table_id {ord}, id {ord}"
        );
        let mut guard = self.inner.lock().expect("postgres");
        let rows = guard
            .client
            .query(
                &sql,
                &[
                    &i64_ts(range.start_inclusive)?,
                    &i64_ts(range.end_exclusive)?,
                ],
            )
            .map_err(definite)?;
        rows.iter().map(entry_from_row).collect()
    }

    fn latest_document(
        &self,
        id: &DocumentId,
        snapshot: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        self.one(
            "SELECT id, ts, table_id, json_value, deleted, prev_ts FROM documents \
             WHERE table_id = $1 AND id = $2 AND ts <= $3 ORDER BY ts DESC LIMIT 1",
            id,
            snapshot,
        )
    }

    fn previous_revision(
        &self,
        id: &DocumentId,
        at: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        self.one(
            "SELECT id, ts, table_id, json_value, deleted, prev_ts FROM documents \
             WHERE table_id = $1 AND id = $2 AND ts < $3 ORDER BY ts DESC LIMIT 1",
            id,
            at,
        )
    }

    fn exact_revision(
        &self,
        id: &DocumentId,
        ts: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        self.one(
            "SELECT id, ts, table_id, json_value, deleted, prev_ts FROM documents \
             WHERE table_id = $1 AND id = $2 AND ts = $3",
            id,
            ts,
        )
    }

    fn document_history(
        &self,
        id: &DocumentId,
        snapshot: Timestamp,
    ) -> Result<Vec<DocumentLogEntry>, PersistError> {
        let mut guard = self.inner.lock().expect("postgres");
        let rows = guard
            .client
            .query(
                "SELECT id, ts, table_id, json_value, deleted, prev_ts FROM documents \
                 WHERE table_id = $1 AND id = $2 AND ts <= $3 ORDER BY ts DESC",
                &[&id.tablet.as_bytes(), &id.id, &i64_ts(snapshot)?],
            )
            .map_err(definite)?;
        rows.iter().map(entry_from_row).collect()
    }

    fn index_scan(
        &self,
        index_id: &IndexId,
        read_timestamp: Timestamp,
        interval: &Interval,
        order: Order,
    ) -> Result<Vec<IndexScanRow>, PersistError> {
        let upper = if interval.end.is_some() {
            " AND key < $4"
        } else {
            ""
        };
        let ord = order_sql(order);
        let sql = format!(
            r#"
SELECT B.key, C.ts, B.document_id, C.table_id, C.json_value, C.prev_ts
FROM (
    SELECT index_id, key, MAX(ts) as max_ts
    FROM indexes
    WHERE index_id = $1 AND ts <= $2 AND key >= $3{upper}
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
    WHERE D.table_id = B.table_id AND D.id = B.document_id AND D.ts <= $2
 )
ORDER BY B.key {ord}
"#
        );
        let mut guard = self.inner.lock().expect("postgres");
        let ts = i64_ts(read_timestamp)?;
        let rows = if let Some(end) = &interval.end {
            guard
                .client
                .query(&sql, &[&index_id.as_bytes(), &ts, &interval.start, end])
        } else {
            guard
                .client
                .query(&sql, &[&index_id.as_bytes(), &ts, &interval.start])
        }
        .map_err(definite)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let key: Vec<u8> = row.get(0);
            let row_ts: i64 = row.get(1);
            let document_id: Vec<u8> = row.get(2);
            let table_id: Vec<u8> = row.get(3);
            let json_value: Option<String> = row.get(4);
            let prev_ts: Option<i64> = row.get(5);
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
        let mut guard = self.inner.lock().expect("postgres");
        read_global(&mut guard.client, key)
    }

    fn max_ts(&self) -> Result<Option<Timestamp>, PersistError> {
        let mut guard = self.inner.lock().expect("postgres");
        let doc_max: Option<i64> = guard
            .client
            .query_one("SELECT MAX(ts) FROM documents", &[])
            .map_err(definite)?
            .get(0);
        let doc_ts = match doc_max {
            Some(value) => Some(
                Timestamp::from_i64(value)
                    .map_err(|_| PersistError::Definite(format!("bad max ts {value}")))?,
            ),
            None => None,
        };
        let global = read_global(
            &mut guard.client,
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

impl PostgresPersistence {
    fn one(
        &self,
        sql: &str,
        id: &DocumentId,
        ts: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        let mut guard = self.inner.lock().expect("postgres");
        let row = guard
            .client
            .query_opt(sql, &[&id.tablet.as_bytes(), &id.id, &i64_ts(ts)?])
            .map_err(definite)?;
        row.as_ref().map(entry_from_row).transpose()
    }
}

impl Persistence for PostgresPersistence {
    fn is_fresh(&self) -> bool {
        let mut guard = self.inner.lock().expect("postgres");
        let docs: i64 = guard
            .client
            .query_one("SELECT COUNT(*) FROM documents", &[])
            .map(|row| row.get(0))
            .unwrap_or(1);
        let globals: i64 = guard
            .client
            .query_one("SELECT COUNT(*) FROM persistence_globals", &[])
            .map(|row| row.get(0))
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
        let mut guard = self.inner.lock().expect("postgres");
        let mut tx = guard.client.transaction().map_err(definite)?;
        let written = write_in_tx(&mut tx, documents, indexes, conflict_strategy);
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
        let mut guard = self.inner.lock().expect("postgres");
        guard
            .client
            .execute(
                "INSERT INTO persistence_globals (key, json_value) VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE SET json_value = EXCLUDED.json_value",
                &[&key.as_str(), &value.to_string()],
            )
            .map_err(definite)?;
        Ok(())
    }
}

fn write_in_tx(
    tx: &mut Transaction<'_>,
    documents: &[DocumentLogEntry],
    indexes: &[PersistenceIndexEntry],
    conflict_strategy: ConflictStrategy,
) -> Result<(), PersistError> {
    let document_sql = match conflict_strategy {
        ConflictStrategy::Error => {
            "INSERT INTO documents (id, ts, table_id, json_value, deleted, prev_ts) \
             VALUES ($1, $2, $3, $4, $5, $6)"
        }
        ConflictStrategy::Overwrite => {
            "INSERT INTO documents (id, ts, table_id, json_value, deleted, prev_ts) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (ts, table_id, id) DO UPDATE SET \
             json_value = EXCLUDED.json_value, deleted = EXCLUDED.deleted, prev_ts = EXCLUDED.prev_ts"
        }
    };
    let index_sql = match conflict_strategy {
        ConflictStrategy::Error => {
            "INSERT INTO indexes (index_id, ts, key, deleted, table_id, document_id) \
             VALUES ($1, $2, $3, $4, $5, $6)"
        }
        ConflictStrategy::Overwrite => {
            "INSERT INTO indexes (index_id, ts, key, deleted, table_id, document_id) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (index_id, key, ts) DO UPDATE SET \
             deleted = EXCLUDED.deleted, table_id = EXCLUDED.table_id, document_id = EXCLUDED.document_id"
        }
    };
    for document in documents {
        let deleted: i32 = if document.value.is_none() { 1 } else { 0 };
        let prev = document.prev_ts.map(|ts| ts.to_i64().ok()).flatten();
        tx.execute(
            document_sql,
            &[
                &document.id.id as &(dyn ToSql + Sync),
                &i64_ts(document.ts)?,
                &document.id.tablet.as_bytes(),
                &document.value,
                &deleted,
                &prev,
            ],
        )
        .map_err(definite)?;
    }
    for index in indexes {
        let deleted: i32 = if index.value.is_none() { 1 } else { 0 };
        let table_id = index.value.as_ref().map(|id| id.tablet.as_bytes().to_vec());
        let document_id = index.value.as_ref().map(|id| id.id.clone());
        tx.execute(
            index_sql,
            &[
                &index.index_id.as_bytes() as &(dyn ToSql + Sync),
                &i64_ts(index.ts)?,
                &index.key,
                &deleted,
                &table_id,
                &document_id,
            ],
        )
        .map_err(definite)?;
    }
    if let Some(ts) = commit_timestamp(documents, indexes) {
        let current = read_global_tx(tx, PersistenceGlobalKey::MaxRepeatableTimestamp)?;
        let current_ts = current
            .and_then(|value| value.as_u64())
            .and_then(|value| Timestamp::from_u64(value).ok());
        let stored = match current_ts {
            Some(existing) if existing > ts => existing,
            _ => ts,
        };
        tx.execute(
            "INSERT INTO persistence_globals (key, json_value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET json_value = EXCLUDED.json_value",
            &[
                &PersistenceGlobalKey::MaxRepeatableTimestamp.as_str() as &(dyn ToSql + Sync),
                &serde_json::json!(stored.as_u64()).to_string(),
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
    client: &mut Client,
    key: PersistenceGlobalKey,
) -> Result<Option<JsonValue>, PersistError> {
    let row = client
        .query_opt(
            "SELECT json_value FROM persistence_globals WHERE key = $1",
            &[&key.as_str()],
        )
        .map_err(definite)?;
    parse_global(row)
}

fn read_global_tx(
    tx: &mut Transaction<'_>,
    key: PersistenceGlobalKey,
) -> Result<Option<JsonValue>, PersistError> {
    let row = tx
        .query_opt(
            "SELECT json_value FROM persistence_globals WHERE key = $1",
            &[&key.as_str()],
        )
        .map_err(definite)?;
    parse_global(row)
}

fn parse_global(row: Option<Row>) -> Result<Option<JsonValue>, PersistError> {
    let Some(row) = row else {
        return Ok(None);
    };
    let text: String = row.get(0);
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| PersistError::Definite(format!("bad global json: {err}")))
}

fn entry_from_row(row: &Row) -> Result<DocumentLogEntry, PersistError> {
    let id: Vec<u8> = row.get(0);
    let ts: i64 = row.get(1);
    let table_id: Vec<u8> = row.get(2);
    let json_value: Option<String> = row.get(3);
    let deleted: i32 = row.get(4);
    let prev_ts: Option<i64> = row.get(5);
    document_from_parts(id, ts, table_id, json_value, deleted != 0, prev_ts)
}

fn order_sql(order: Order) -> &'static str {
    match order {
        Order::Asc => "ASC",
        Order::Desc => "DESC",
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use refhir_persistence::conformance;

    /// The Postgres URL for tests. CI sets `REFHIR_REQUIRE_POSTGRES` so a missing database
    /// fails the test instead of letting it pass without running anything.
    fn test_url(what: &str) -> Option<String> {
        match std::env::var("HAIFHIR_POSTGRES_URL") {
            Ok(url) => Some(url),
            Err(_) if std::env::var("REFHIR_REQUIRE_POSTGRES").is_ok() => {
                panic!("REFHIR_REQUIRE_POSTGRES is set but HAIFHIR_POSTGRES_URL is not ({what})")
            }
            Err(_) => {
                eprintln!("HAIFHIR_POSTGRES_URL unset, skipping {what}");
                None
            }
        }
    }

    #[test]
    fn postgres_log_roundtrip_when_configured() {
        let Some(url) = test_url("postgres conformance") else {
            return;
        };
        let persistence = PostgresPersistence::connect(&url).expect("connect");
        {
            let mut guard = persistence.inner.lock().expect("postgres");
            guard
                .client
                .batch_execute(
                    "DELETE FROM indexes; DELETE FROM documents; DELETE FROM persistence_globals;",
                )
                .expect("clean");
        }
        conformance::exercise(&persistence);
    }

    #[test]
    fn postgres_auth_store_when_configured() {
        let Some(url) = test_url("postgres auth store") else {
            return;
        };
        let persistence = PostgresPersistence::connect(&url).expect("connect");
        {
            let mut guard = persistence.inner.lock().expect("postgres");
            guard
                .client
                .batch_execute("DELETE FROM smart_oauth;")
                .expect("clean");
        }
        refhir_persistence::auth::exercise(&persistence);
    }

    #[test]
    fn postgres_parameter_clashes_when_configured() {
        let Some(url) = test_url("postgres parameter clashes") else {
            return;
        };
        let persistence = PostgresPersistence::connect(&url).expect("connect");
        let tag = format!("pg{}", std::process::id());
        refhir_persistence::conformance::exercise_parameter_clashes(&persistence, &tag);
    }
}
