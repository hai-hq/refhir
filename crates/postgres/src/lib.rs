//! Postgres `Persistence` implementation.
//!
//! Convex: `crates/postgres` implements the same `Persistence` trait as SQLite.
//! Tables match `DOCUMENTS_INIT` / `INDEXES_INIT` / `PERSISTENCE_GLOBALS_INIT`.
//! SQLite stays the default for local runs.

use std::sync::Arc;

use postgres::fallible_iterator::FallibleIterator;
use postgres::types::ToSql;
use postgres::{Client, NoTls, Row, Transaction};
use refhir_persistence::{
    document_from_parts, index_target, ConflictStrategy, DocumentLogEntry, IndexScanEntry,
    IndexScanRow, LatestDocument, Lease, LiveEntries, PersistError, Persistence,
    PersistenceGlobalKey, PersistenceIndexEntry, PersistenceReader, Pool, PoolStats,
    TimestampRange,
};
use refhir_types::{DocumentId, IndexId, Interval, Order, TabletId, Timestamp};

mod lease;
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
    done INTEGER NOT NULL,
    claimed_epoch BIGINT
);
-- Databases created before backfills were claimed by a writer epoch.
ALTER TABLE index_backfills ADD COLUMN IF NOT EXISTS claimed_epoch BIGINT;
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
    pub(crate) client: Conn,
}

/// A connection that is closed on a thread of its own.
///
/// Dropping a `postgres::Client` blocks on the connection's runtime, which Tokio refuses on a
/// thread that belongs to another runtime. A pooled connection can be dropped from one (a
/// broken one on its way out, or the pool at shutdown), so the drop is handed to a plain
/// thread. It dereferences to `Client`, so callers use it as one.
pub(crate) struct Conn(Option<Client>);

impl Conn {
    fn new(client: Client) -> Self {
        Self(Some(client))
    }
}

impl std::ops::Deref for Conn {
    type Target = Client;
    fn deref(&self) -> &Client {
        self.0.as_ref().expect("connection")
    }
}

impl std::ops::DerefMut for Conn {
    fn deref_mut(&mut self) -> &mut Client {
        self.0.as_mut().expect("connection")
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some(client) = self.0.take() {
            // Not joined: closing waits on the network, and nothing depends on it finishing.
            let _ = std::thread::Builder::new()
                .name("refhir-pg-close".into())
                .spawn(move || drop(client));
        }
    }
}

/// How `connect_with` opens the database.
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    /// Most connections the pool opens. One behaves like the old single connection.
    pub pool_size: usize,
    /// Create tables and indexes. A process that only reads sets this to false and waits for
    /// the writer to have created them, so it never runs DDL.
    pub create_schema: bool,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            pool_size: 1,
            create_schema: true,
        }
    }
}

#[derive(Clone)]
pub struct PostgresPersistence {
    inner: Arc<Pool<Inner>>,
    /// A connection that only listens for the writer's "published" notice. Dedicated, so a
    /// reader waiting on it never holds one of the pool's connections.
    listener: Arc<std::sync::Mutex<Option<Conn>>>,
    /// Held across the multi-statement conformance writes. A single connection used to make
    /// them run one at a time; with a pool they would interleave, and two installs could
    /// both pass a check before either inserts.
    gate: Arc<std::sync::Mutex<()>>,
    lease: Arc<lease::LeaseState>,
    url: Arc<str>,
}

/// Channel the writer notifies after recording a published timestamp.
const PUBLISH_CHANNEL: &str = "refhir_published";

/// Tables a reader waits for before it starts.
const REQUIRED_TABLES: &[&str] = &[
    "documents",
    "indexes",
    "persistence_globals",
    "packages",
    "smart_oauth",
];

impl PostgresPersistence {
    pub fn connect(url: &str) -> Result<Self, PersistError> {
        Self::connect_with(url, &ConnectOptions::default())
    }

    pub fn connect_with(url: &str, options: &ConnectOptions) -> Result<Self, PersistError> {
        let mut client = Conn::new(connect_client(url)?);
        if options.create_schema {
            create_schema(&mut client)?;
        } else {
            wait_for_schema(&mut client)?;
        }
        let owned: Arc<str> = Arc::from(url);
        let factory_url = owned.clone();
        let pool = Pool::new(
            options.pool_size,
            vec![Inner { client }],
            move || {
                connect_client(&factory_url).map(|client| Inner {
                    client: Conn::new(client),
                })
            },
            |inner: &Inner| !inner.client.is_closed(),
        );
        Ok(Self {
            inner: Arc::new(pool),
            listener: Arc::new(std::sync::Mutex::new(None)),
            gate: Arc::new(std::sync::Mutex::new(())),
            lease: Arc::new(lease::LeaseState::default()),
            url: owned,
        })
    }
}

/// Every table and index the schema creates. When all of them exist `create_schema` does
/// nothing: even `CREATE ... IF NOT EXISTS` locks the table first, which would stall a
/// running primary's writes whenever a standby or a restart connects.
const SCHEMA_OBJECTS: &[&str] = &[
    "documents",
    "documents_by_table_and_id",
    "indexes",
    "indexes_by_ts",
    "persistence_globals",
    "packages",
    "structure_definitions",
    "search_parameters",
    "search_parameters_by_base_code",
    "index_backfills",
    "terminology",
    "value_set_expansion",
    "backfill_parameter",
    "smart_oauth",
    "smart_oauth_by_exp",
];

/// Advisory lock around building `indexes_by_ts`, which runs outside a transaction.
const INDEX_BUILD_LOCK: i64 = 7428311;

/// Whether `index_backfills` has the column a claim is recorded in. Tables made by an earlier
/// version lack it, and a process that finds it missing must run the schema step.
fn has_claim_column(client: &mut Client) -> Result<bool, PersistError> {
    Ok(client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_name = 'index_backfills' AND column_name = 'claimed_epoch')",
            &[],
        )
        .map_err(definite)?
        .get(0))
}

fn schema_complete(client: &mut Client) -> Result<bool, PersistError> {
    for name in SCHEMA_OBJECTS {
        let exists: bool = client
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[name])
            .map_err(definite)?
            .get(0);
        if !exists {
            return Ok(false);
        }
    }
    if !has_claim_column(client)? {
        return Ok(false);
    }
    // An index that a failed concurrent build left behind exists but is not usable.
    let invalid: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
             WHERE c.relname = 'indexes_by_ts' AND NOT i.indisvalid)",
            &[],
        )
        .map_err(definite)?
        .get(0);
    Ok(!invalid)
}

fn create_schema(client: &mut Client) -> Result<(), PersistError> {
    if schema_complete(client)? {
        return Ok(());
    }
    // Concurrent `CREATE TABLE IF NOT EXISTS` can still collide on a fresh database, so
    // schema setup runs under a transaction-scoped advisory lock.
    let mut tx = client.transaction().map_err(definite)?;
    tx.batch_execute("SELECT pg_advisory_xact_lock(7428310)")
        .map_err(definite)?;
    tx.batch_execute(SCHEMA).map_err(definite)?;
    tx.batch_execute(auth_store::AUTH_INIT).map_err(definite)?;
    tx.commit().map_err(definite)?;
    // A reader in another process finds what a commit wrote by scanning index rows by ts, and
    // `max_ts` looks at the newest one. Built concurrently so an existing database keeps
    // accepting writes while it is added.
    //
    // The lock is taken by polling, never by waiting: `CREATE INDEX CONCURRENTLY` waits for
    // every other transaction to finish, so a process blocked inside `pg_advisory_lock` (which
    // is itself a transaction) while another holds the lock and builds is a deadlock.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        let got: bool = client
            .query_one("SELECT pg_try_advisory_lock($1)", &[&INDEX_BUILD_LOCK])
            .map_err(definite)?
            .get(0);
        if got {
            break;
        }
        // Whoever holds it is building the index; once it is there, there is nothing to do.
        if schema_complete(client)? {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(definite(
                "timed out waiting for another process to build indexes_by_ts",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let built = (|| {
        let invalid: bool = client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
                 WHERE c.relname = 'indexes_by_ts' AND NOT i.indisvalid)",
                &[],
            )
            .map_err(definite)?
            .get(0);
        if invalid {
            client
                .batch_execute("DROP INDEX CONCURRENTLY IF EXISTS indexes_by_ts")
                .map_err(definite)?;
        }
        client
            .batch_execute("CREATE INDEX CONCURRENTLY IF NOT EXISTS indexes_by_ts ON indexes (ts)")
            .map_err(definite)
    })();
    let released = client
        .batch_execute(&format!("SELECT pg_advisory_unlock({INDEX_BUILD_LOCK})"))
        .map_err(definite);
    built.and(released)
}

impl PostgresPersistence {
    /// Gives up the writer lease: the dedicated connection is closed, which frees the advisory
    /// lock, and the lease thread stops without treating that as a loss. For an orderly shutdown,
    /// and for a test that is about to drop the database. Writes made after this are still fenced
    /// against the epoch this process held.
    pub fn release_writer_lease(&self) {
        self.lease.release();
    }

    /// See the `gate` field.
    pub(crate) fn gate(&self) -> std::sync::MutexGuard<'_, ()> {
        self.gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Polls until the writer has created the schema. A reader that starts first (a replica
/// brought up alongside the primary) waits here instead of failing.
fn wait_for_schema(client: &mut Client) -> Result<(), PersistError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let mut missing = Vec::new();
        for table in REQUIRED_TABLES {
            let exists: bool = client
                .query_one("SELECT to_regclass($1) IS NOT NULL", &[table])
                .map_err(definite)?
                .get(0);
            if !exists {
                missing.push(*table);
            }
        }
        if !has_claim_column(client)? {
            missing.push("index_backfills.claimed_epoch");
        }
        if missing.is_empty() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(PersistError::Definite(format!(
                "the database has no {} yet; start the primary first",
                missing.join(", ")
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
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
        let mut guard = self.inner.lock()?;
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
        let mut guard = self.inner.lock()?;
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
        let mut guard = self.inner.lock()?;
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
        // ts needs no sort, no join and no grouping. Tombstones are rows too: the latest version
        // of each key is picked in `LiveEntries`.
        //
        // With a limit on an ascending scan the rows are read in chunks of a few more than the
        // limit, so a search for a page of matches does not read the whole range. A chunk can end
        // in the middle of a key's versions, so the next one starts again from that key, and
        // grows if one key alone fills a chunk.
        let ts = i64_ts(read_timestamp)?;
        let ord = order_sql(order);
        let upper = if interval.end.is_some() {
            " AND key < $4"
        } else {
            ""
        };
        let mut chunk = limit
            .filter(|_| order == Order::Asc)
            .map(|limit| limit.saturating_mul(2).saturating_add(16));
        let mut start = interval.start.clone();
        let mut live = LiveEntries::new(order, limit);
        loop {
            let cap = chunk
                .map(|rows| format!(" LIMIT {rows}"))
                .unwrap_or_default();
            let sql = format!(
                "SELECT key, deleted, table_id, document_id FROM indexes \
                 WHERE index_id = $1 AND ts <= $2 AND key >= $3{upper} \
                 ORDER BY key {ord}, ts {ord}{cap}"
            );
            let rows = {
                let mut guard = self.inner.lock()?;
                if let Some(end) = &interval.end {
                    guard
                        .client
                        .query(&sql, &[&index_id.as_bytes(), &ts, &start, end])
                } else {
                    guard
                        .client
                        .query(&sql, &[&index_id.as_bytes(), &ts, &start])
                }
                .map_err(definite)?
            };
            let read = rows.len();
            for row in rows {
                let key: Vec<u8> = row.get(0);
                let deleted: i32 = row.get(1);
                let id = if deleted != 0 {
                    None
                } else {
                    let table_id: Option<Vec<u8>> = row.get(2);
                    let document_id: Option<Vec<u8>> = row.get(3);
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
                };
                if live.push(key, id) {
                    return Ok(live.finish());
                }
            }
            let Some(rows_per_chunk) = chunk else {
                return Ok(live.finish());
            };
            if read < rows_per_chunk {
                return Ok(live.finish());
            }
            let Some(resume) = live.end_chunk() else {
                return Ok(live.finish());
            };
            if resume == start {
                chunk = Some(rows_per_chunk.saturating_mul(2));
            }
            start = resume;
        }
    }

    fn get_persistence_global(
        &self,
        key: PersistenceGlobalKey,
    ) -> Result<Option<JsonValue>, PersistError> {
        let mut guard = self.inner.lock()?;
        read_global(&mut guard.client, key)
    }

    fn load_index_writes(
        &self,
        range: TimestampRange,
    ) -> Result<Vec<PersistenceIndexEntry>, PersistError> {
        let mut guard = self.inner.lock()?;
        let rows = guard
            .client
            .query(
                "SELECT index_id, ts, key, deleted, table_id, document_id FROM indexes \
                 WHERE ts >= $1 AND ts < $2 ORDER BY ts, index_id, key",
                &[
                    &i64_ts(range.start_inclusive)?,
                    &i64_ts(range.end_exclusive)?,
                ],
            )
            .map_err(definite)?;
        rows.iter().map(index_entry_from_row).collect()
    }

    fn max_ts(&self) -> Result<Option<Timestamp>, PersistError> {
        let mut guard = self.inner.lock()?;
        // One statement, so the document log and the global are read from the same state. The
        // global holds the newest index-only commit; a value that is not a number is ignored.
        // GREATEST skips NULLs, so either side may be absent.
        let newest: Option<i64> = guard
            .client
            .query_one(
                "SELECT GREATEST(
                     (SELECT MAX(ts) FROM documents),
                     (SELECT json_value::bigint FROM persistence_globals
                      WHERE key = $1 AND json_value ~ '^[0-9]+$')
                 )",
                &[&PersistenceGlobalKey::MaxRepeatableTimestamp.as_str()],
            )
            .map_err(definite)?
            .get(0);
        newest
            .map(|value| {
                Timestamp::from_i64(value)
                    .map_err(|_| PersistError::Definite(format!("bad max ts {value}")))
            })
            .transpose()
    }
}

impl PostgresPersistence {
    fn one(
        &self,
        sql: &str,
        id: &DocumentId,
        ts: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        let mut guard = self.inner.lock()?;
        let row = guard
            .client
            .query_opt(sql, &[&id.tablet.as_bytes(), &id.id, &i64_ts(ts)?])
            .map_err(definite)?;
        row.as_ref().map(entry_from_row).transpose()
    }
}

impl Persistence for PostgresPersistence {
    fn is_fresh(&self) -> bool {
        let Ok(mut guard) = self.inner.lock() else {
            return false;
        };
        // One statement, so both counts come from the same state.
        guard
            .client
            .query_one(
                "SELECT (SELECT COUNT(*) FROM documents) = 0 \
                 AND (SELECT COUNT(*) FROM persistence_globals) = 0",
                &[],
            )
            .map(|row| row.get(0))
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
        let mut tx = guard.client.transaction().map_err(definite)?;
        if let Some(epoch) = self.lease.epoch() {
            if let Err(err) = lease::check_epoch(&mut tx, epoch) {
                let _ = tx.rollback();
                return Err(err);
            }
        }
        let written = write_in_tx(&mut tx, documents, indexes, conflict_strategy);
        if let Err(err) = written {
            let _ = tx.rollback();
            return Err(err);
        }
        let committed = tx.commit().map_err(uncertain);
        if committed.is_err() {
            guard.discard();
        }
        committed
    }

    fn try_acquire_writer_lease(&self) -> Result<Lease, PersistError> {
        self.lease.acquire(&self.url)
    }

    fn advance_published_ts(&self, ts: Timestamp) -> Result<(), PersistError> {
        let mut guard = self.inner.lock()?;
        // Not durable on its own commit: the watermark is rebuilt from the log on a primary
        // restart, so a crash that loses the last few values only delays replicas. That keeps
        // it from costing a WAL flush per commit.
        let wrote = guard.client.batch_execute(&format!(
            "BEGIN; SET LOCAL synchronous_commit = off; \
                 INSERT INTO persistence_globals (key, json_value) VALUES ('{key}', '{ts}') \
                 ON CONFLICT (key) DO UPDATE SET json_value = GREATEST( \
                     persistence_globals.json_value::bigint, EXCLUDED.json_value::bigint)::text; \
                 NOTIFY {channel}; \
                 COMMIT;",
            channel = PUBLISH_CHANNEL,
            key = PersistenceGlobalKey::PublishedTimestamp.as_str(),
            ts = ts.as_u64(),
        ));
        if let Err(err) = wrote {
            // A failure inside the explicit transaction leaves the connection aborted.
            guard.discard();
            return Err(definite(err));
        }
        Ok(())
    }

    fn pool_stats(&self) -> Option<PoolStats> {
        Some(self.inner.stats())
    }

    fn publishes_notifications(&self) -> bool {
        true
    }

    fn wait_for_publish(&self, timeout: std::time::Duration) -> bool {
        let mut listener = self
            .listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if listener.is_none() {
            let connected = connect_client(&self.url)
                .map(Conn::new)
                .and_then(|mut conn| {
                    conn.batch_execute(&format!("LISTEN {PUBLISH_CHANNEL}"))
                        .map_err(definite)?;
                    Ok(conn)
                });
            match connected {
                Ok(conn) => *listener = Some(conn),
                Err(err) => {
                    tracing::warn!(target: "refhir", "cannot listen for publishes: {err}");
                    std::thread::sleep(timeout);
                    return false;
                }
            }
        }
        // Taken into an owned value first, so the connection is not borrowed through the match.
        let outcome = {
            let conn = listener.as_mut().expect("connected above");
            let first = conn.notifications().timeout_iter(timeout).next();
            if matches!(first, Ok(Some(_))) {
                // Several publishes may have been announced; one refresh covers them all.
                let mut pending = conn.notifications();
                let mut drain = pending.iter();
                while let Ok(Some(_)) = drain.next() {}
            }
            first
        };
        match outcome {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(err) => {
                tracing::warn!(target: "refhir", "publish listener failed, reconnecting: {err}");
                *listener = None;
                std::thread::sleep(timeout.min(std::time::Duration::from_secs(1)));
                false
            }
        }
    }

    fn write_persistence_global(
        &self,
        key: PersistenceGlobalKey,
        value: &JsonValue,
    ) -> Result<(), PersistError> {
        let mut guard = self.inner.lock()?;
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
    // A commit with documents is found by `MAX(documents.ts)`. Only an index-only commit (a
    // backfill) needs the global, and the upsert keeps the larger value in one statement so
    // two concurrent persists cannot lower it. Commits with documents never touch this row,
    // so concurrent persists do not queue on it.
    if documents.is_empty() {
        if let Some(ts) = commit_timestamp(documents, indexes) {
            tx.execute(
                "INSERT INTO persistence_globals (key, json_value) VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE SET json_value = GREATEST(
                     persistence_globals.json_value::bigint, EXCLUDED.json_value::bigint)::text",
                &[
                    &PersistenceGlobalKey::MaxRepeatableTimestamp.as_str() as &(dyn ToSql + Sync),
                    &ts.as_u64().to_string(),
                ],
            )
            .map_err(definite)?;
        }
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

fn parse_global(row: Option<Row>) -> Result<Option<JsonValue>, PersistError> {
    let Some(row) = row else {
        return Ok(None);
    };
    let text: String = row.get(0);
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| PersistError::Definite(format!("bad global json: {err}")))
}

fn index_entry_from_row(row: &Row) -> Result<PersistenceIndexEntry, PersistError> {
    let index_id: Vec<u8> = row.get(0);
    let ts: i64 = row.get(1);
    let key: Vec<u8> = row.get(2);
    let deleted: i32 = row.get(3);
    let table_id: Option<Vec<u8>> = row.get(4);
    let document_id: Option<Vec<u8>> = row.get(5);
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
    Ok(PersistenceIndexEntry {
        ts: Timestamp::from_i64(ts)
            .map_err(|_| PersistError::Definite(format!("bad index ts {ts}")))?,
        index_id: IndexId::from_bytes(&index_id)
            .map_err(|_| PersistError::Definite("index_id is not utf-8".into()))?,
        key,
        value,
    })
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

/// Opens a connection on a thread of its own.
///
/// The synchronous `postgres` client starts a runtime to connect, and Tokio refuses that on a
/// thread that is part of another runtime. The server connects from startup code that runs on
/// one, and the pool opens connections on demand from request threads, so every connection is
/// made here, on a plain thread, and then handed back. Once connected the client can be used
/// from any thread.
pub(crate) fn connect_client(url: &str) -> Result<Client, PersistError> {
    let url = url.to_string();
    std::thread::Builder::new()
        .name("refhir-pg-connect".into())
        .spawn(move || Client::connect(&url, NoTls))
        .map_err(|err| definite(format!("connection thread: {err}")))?
        .join()
        .map_err(|_| definite("the connection thread panicked"))?
        .map_err(definite)
}

/// What to say about a failure. `postgres::Error` prints only "db error", which hides the
/// reason; this adds the server's message, code and detail.
pub(crate) trait ErrorText {
    fn error_text(&self) -> String;
}

impl ErrorText for postgres::Error {
    fn error_text(&self) -> String {
        if let Some(db) = self.as_db_error() {
            let mut text = format!(
                "{}: {} (SQLSTATE {})",
                db.severity(),
                db.message(),
                db.code().code()
            );
            if let Some(detail) = db.detail() {
                text.push_str(&format!(". {detail}"));
            }
            if let Some(hint) = db.hint() {
                text.push_str(&format!(". Hint: {hint}"));
            }
            return text;
        }
        let mut text = self.to_string();
        let mut source = std::error::Error::source(self);
        while let Some(cause) = source {
            text.push_str(&format!(": {cause}"));
            source = cause.source();
        }
        text
    }
}

impl ErrorText for String {
    fn error_text(&self) -> String {
        self.clone()
    }
}

impl ErrorText for &str {
    fn error_text(&self) -> String {
        (*self).to_string()
    }
}

impl ErrorText for std::io::Error {
    fn error_text(&self) -> String {
        self.to_string()
    }
}

pub(crate) fn definite(err: impl ErrorText) -> PersistError {
    PersistError::Definite(err.error_text())
}

fn uncertain(err: impl ErrorText) -> PersistError {
    PersistError::Uncertain(err.error_text())
}

mod auth_store;
mod conformance_store;

#[cfg(test)]
mod tests {
    use super::*;
    use refhir_persistence::conformance;
    use refhir_persistence::ConformanceStore as _;

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
        conformance::exercise_index_scan_ids(&persistence);
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

    /// A database of its own. The writer lease is an advisory lock, which is per database, and
    /// the other tests here wipe the shared one while these run.
    fn fresh_database(admin_url: &str, tag: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            "refhir_t_{tag}_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let mut admin = Client::connect(admin_url, NoTls).expect("admin connection");
        admin
            .batch_execute(&format!("CREATE DATABASE {name}"))
            .expect("create database");
        if admin_url.contains("://") {
            let (head, tail) = match admin_url.split_once('?') {
                Some((head, query)) => (head, format!("?{query}")),
                None => (admin_url, String::new()),
            };
            let slash = head.rfind('/').expect("database name in the url");
            format!("{}/{name}{tail}", &head[..slash])
        } else {
            // The `host=... user=...` form: drop any database and name ours.
            let kept: Vec<&str> = admin_url
                .split_whitespace()
                .filter(|part| !part.starts_with("dbname="))
                .collect();
            format!("{} dbname={name}", kept.join(" "))
        }
    }

    fn write_doc(p: &PostgresPersistence, ts: u64, name: &str) -> Result<(), PersistError> {
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

    fn acquire_within(p: &PostgresPersistence, seconds: u64) -> Lease {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        loop {
            match p.try_acquire_writer_lease().expect("lease attempt") {
                Lease::Busy if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                other => return other,
            }
        }
    }

    #[test]
    fn only_one_process_holds_the_writer_lease() {
        let Some(admin) = test_url("postgres writer lease") else {
            return;
        };
        let url = fresh_database(&admin, "lease");
        let first = PostgresPersistence::connect(&url).unwrap();
        let second = PostgresPersistence::connect(&url).unwrap();

        assert_eq!(
            first.try_acquire_writer_lease().unwrap(),
            Lease::Held { epoch: 1 }
        );
        // Asking again is the same lease, not a new epoch.
        assert_eq!(
            first.try_acquire_writer_lease().unwrap(),
            Lease::Held { epoch: 1 }
        );
        assert_eq!(second.try_acquire_writer_lease().unwrap(), Lease::Busy);

        // When the holder's session ends, the next process takes over at a higher epoch.
        first.lease.release();
        assert_eq!(acquire_within(&second, 10), Lease::Held { epoch: 2 });
    }

    #[test]
    fn a_writer_that_lost_the_lease_is_fenced() {
        let Some(admin) = test_url("postgres fencing") else {
            return;
        };
        let url = fresh_database(&admin, "fence");
        let old = PostgresPersistence::connect(&url).unwrap();
        let new = PostgresPersistence::connect(&url).unwrap();
        old.try_acquire_writer_lease().unwrap();
        write_doc(&old, 1, "before").expect("the holder writes");

        // The old primary is paused: its session goes away, a successor takes over, and then
        // it wakes up still believing it is the writer.
        old.lease.release();
        assert_eq!(acquire_within(&new, 10), Lease::Held { epoch: 2 });
        let stale = write_doc(&old, 2, "stale").unwrap_err();
        assert!(matches!(stale, PersistError::Fenced(_)), "{stale}");
        write_doc(&new, 2, "after").expect("the new holder writes");

        // The tables the committer does not write are fenced too: a replaced primary cannot move
        // a backfill's cursor, mark it complete, or announce a catalog change.
        let fenced = |result: Result<(), PersistError>| {
            assert!(matches!(result, Err(PersistError::Fenced(_))), "{result:?}");
        };
        fenced(old.update_backfill(1, Some("p"), false));
        fenced(old.complete_backfill(1, 1, "Patient"));
        fenced(old.mark_base_ready(1, "Patient"));
        fenced(old.insert_backfill(1, 1, "Patient", &[]).map(drop));
        fenced(old.bump_catalog_version().map(drop));
        let version = new
            .bump_catalog_version()
            .expect("the new holder announces changes");
        assert_eq!(new.catalog_version().unwrap(), version);

        // The stale write left nothing behind.
        let rows = new
            .load_documents(
                TimestampRange {
                    start_inclusive: Timestamp::from_u64(1).unwrap(),
                    end_exclusive: Timestamp::from_u64(10).unwrap(),
                },
                Order::Asc,
            )
            .unwrap();
        let names: Vec<_> = rows.iter().map(|row| row.id.id.clone()).collect();
        assert_eq!(names, vec![b"before".to_vec(), b"after".to_vec()]);
    }

    /// Measured, not asserted: the plan's epoch check reads the row `FOR UPDATE`, which makes every
    /// concurrent persist wait for the one before it to commit. The check uses `FOR SHARE`
    /// instead. Run with `cargo test -p refhir-postgres epoch_lock_modes -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn epoch_lock_modes_compared() {
        use std::sync::atomic::Ordering;
        let Some(admin) = test_url("epoch lock benchmark") else {
            return;
        };
        let threads = 8u64;
        let per_thread = 150u64;
        let mut results: Vec<(&str, Vec<f64>)> =
            vec![("FOR SHARE", vec![]), ("FOR UPDATE", vec![])];
        for round in 0..3 {
            for (mode, samples) in results.iter_mut() {
                lease::EXCLUSIVE_EPOCH_CHECK.store(*mode == "FOR UPDATE", Ordering::SeqCst);
                let url = fresh_database(&admin, "epochbench");
                let options = ConnectOptions {
                    pool_size: 8,
                    create_schema: true,
                };
                let p = Arc::new(PostgresPersistence::connect_with(&url, &options).unwrap());
                p.try_acquire_writer_lease().unwrap();
                let started = std::time::Instant::now();
                let workers: Vec<_> = (0..threads)
                    .map(|worker| {
                        let p = p.clone();
                        std::thread::spawn(move || {
                            for n in 0..per_thread {
                                write_doc(&p, worker * 1000 + n + 1, &format!("w{worker}-{n}"))
                                    .unwrap();
                            }
                        })
                    })
                    .collect();
                for worker in workers {
                    worker.join().unwrap();
                }
                let secs = started.elapsed().as_secs_f64();
                samples.push((threads * per_thread) as f64 / secs);
                println!(
                    "round {round} {mode:<10} {:>8.0} persists/s",
                    samples.last().unwrap()
                );
            }
        }
        lease::EXCLUSIVE_EPOCH_CHECK.store(false, Ordering::SeqCst);
        let median = |samples: &Vec<f64>| {
            let mut sorted = samples.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            sorted[sorted.len() / 2]
        };
        let (share, update) = (median(&results[0].1), median(&results[1].1));
        println!(
            "epoch check, {threads} concurrent writers, median of 3: FOR SHARE {share:.0}/s, \
             FOR UPDATE {update:.0}/s ({:.2}x)",
            share / update
        );
    }

    #[test]
    fn concurrent_persists_do_not_queue_on_the_lease() {
        let Some(admin) = test_url("postgres concurrent writes") else {
            return;
        };
        let url = fresh_database(&admin, "concurrent");
        let options = ConnectOptions {
            pool_size: 8,
            create_schema: true,
        };
        let p = Arc::new(PostgresPersistence::connect_with(&url, &options).unwrap());
        p.try_acquire_writer_lease().unwrap();
        let handles: Vec<_> = (0..8u64)
            .map(|worker| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for n in 0..20u64 {
                        write_doc(&p, worker * 100 + n + 1, &format!("w{worker}-{n}")).unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(p.max_ts().unwrap(), Some(Timestamp::from_u64(720).unwrap()));
        let stats = p.pool_stats().unwrap();
        assert!(stats.peak_in_use > 1, "writes never overlapped: {stats:?}");
        assert!(stats.open <= 8);
    }

    #[test]
    fn a_reader_connection_never_runs_ddl_and_waits_for_the_schema() {
        let Some(admin) = test_url("postgres reader connect") else {
            return;
        };
        let url = fresh_database(&admin, "reader");
        let reader_url = url.clone();
        let reader = std::thread::spawn(move || {
            let options = ConnectOptions {
                pool_size: 2,
                create_schema: false,
            };
            PostgresPersistence::connect_with(&reader_url, &options).map(|_| ())
        });
        // The reader starts first and finds nothing. It keeps waiting until a writer's connect
        // creates the tables.
        std::thread::sleep(std::time::Duration::from_millis(1200));
        assert!(!reader.is_finished(), "a reader must wait for the schema");
        PostgresPersistence::connect(&url).unwrap();
        reader
            .join()
            .unwrap()
            .expect("reader connects once the schema exists");
    }

    #[test]
    fn processes_starting_together_on_an_empty_database_all_create_the_schema() {
        let Some(admin) = test_url("postgres concurrent start") else {
            return;
        };
        // A primary, a standby and replicas brought up at once all find an empty database and
        // all try to create the tables and the timestamp index.
        let url = fresh_database(&admin, "together");
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let url = url.clone();
                std::thread::spawn(move || PostgresPersistence::connect(&url).map(|_| ()))
            })
            .collect();
        for handle in handles {
            handle
                .join()
                .unwrap()
                .expect("every process gets past schema setup");
        }
        // And the index is there, valid, exactly once.
        let mut client = Client::connect(&url, NoTls).unwrap();
        let valid: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
                 WHERE c.relname = 'indexes_by_ts' AND i.indisvalid",
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(valid, 1);
    }

    #[test]
    fn connecting_to_a_complete_schema_takes_no_table_locks() {
        let Some(admin) = test_url("postgres schema locks") else {
            return;
        };
        let url = fresh_database(&admin, "locks");
        let writer = PostgresPersistence::connect(&url).unwrap();
        // Hold a write open on `documents`, as a primary mid-commit would.
        let mut busy = Client::connect(&url, NoTls).unwrap();
        busy.batch_execute(
            "BEGIN; INSERT INTO documents (id, ts, table_id, json_value, deleted) \
             VALUES ('x', 1, 'x', '{}', 0)",
        )
        .unwrap();
        // A second process starting up must not wait for it: CREATE INDEX would.
        let (done, finished) = std::sync::mpsc::channel();
        let other_url = url.clone();
        std::thread::spawn(move || {
            let _ = done.send(PostgresPersistence::connect(&other_url).map(|_| ()));
        });
        finished
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("connect blocked behind an open write")
            .unwrap();
        busy.batch_execute("ROLLBACK").unwrap();
        drop(writer);
    }

    #[test]
    fn a_backfill_is_claimed_by_the_epoch_that_runs_it() {
        let Some(admin) = test_url("postgres backfill claims") else {
            return;
        };
        let url = fresh_database(&admin, "claims");
        let first = PostgresPersistence::connect(&url).unwrap();
        assert_eq!(
            first.try_acquire_writer_lease().unwrap(),
            Lease::Held { epoch: 1 }
        );
        let job = first.insert_backfill(1, 5, "Patient", &[]).unwrap();
        let claim = |p: &PostgresPersistence| {
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
        let second = PostgresPersistence::connect(&url).unwrap();
        assert_eq!(acquire_within(&second, 10), Lease::Held { epoch: 2 });
        assert!(second.claim_backfill(job).unwrap());
        assert_eq!(claim(&second), Some(Some(2)));

        // A process with an older (or no) epoch is refused a job a newer one holds.
        let stale = PostgresPersistence::connect(&url).unwrap();
        let refused = stale.claim_backfill(job).unwrap_err();
        assert!(matches!(refused, PersistError::Fenced(_)), "{refused}");
        assert_eq!(claim(&second), Some(Some(2)), "and the claim is unchanged");

        // A finished job, and one that does not exist, have nothing to claim.
        second.update_backfill(job, Some("p9"), true).unwrap();
        assert!(!second.claim_backfill(job).unwrap());
        assert!(!second.claim_backfill(job + 100).unwrap());
    }

    #[test]
    fn a_database_from_before_claims_gets_the_column() {
        let Some(admin) = test_url("postgres claim column migration") else {
            return;
        };
        let url = fresh_database(&admin, "migrate");
        // The table as an earlier version made it, with a job already in it.
        {
            let mut client = Client::connect(&url, NoTls).unwrap();
            client
                .batch_execute(
                    "CREATE TABLE index_backfills (
                        id BIGSERIAL PRIMARY KEY,
                        package_id BIGINT NOT NULL,
                        watermark_ts BIGINT NOT NULL,
                        tablet TEXT NOT NULL,
                        cursor_id TEXT,
                        done INTEGER NOT NULL
                    );
                    INSERT INTO index_backfills (package_id, watermark_ts, tablet, cursor_id, done)
                    VALUES (1, 7, 'Patient', 'p3', 0);",
                )
                .unwrap();
        }
        // A reader that starts first must not run DDL, and waits until the column is there.
        let reader_url = url.clone();
        let reader = std::thread::spawn(move || {
            let options = ConnectOptions {
                pool_size: 1,
                create_schema: false,
            };
            PostgresPersistence::connect_with(&reader_url, &options).map(|_| ())
        });
        std::thread::sleep(std::time::Duration::from_millis(1200));
        assert!(
            !reader.is_finished(),
            "the table exists but the column does not"
        );
        let p = PostgresPersistence::connect(&url).unwrap();
        reader
            .join()
            .unwrap()
            .expect("the reader connects once the column exists");
        let jobs = p.load_backfills().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].cursor_id.as_deref(), Some("p3"));
        assert_eq!(jobs[0].claimed_epoch, None);
        assert!(p.claim_backfill(jobs[0].id).unwrap());
        // Connecting again finds the schema complete and does nothing.
        PostgresPersistence::connect(&url).unwrap();
    }

    #[test]
    fn the_two_catalog_counters_count_separately() {
        let Some(admin) = test_url("postgres catalog counters") else {
            return;
        };
        let url = fresh_database(&admin, "counters");
        let p = PostgresPersistence::connect(&url).unwrap();
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
    fn postgres_parameter_clashes_when_configured() {
        let Some(url) = test_url("postgres parameter clashes") else {
            return;
        };
        let persistence = PostgresPersistence::connect(&url).expect("connect");
        let tag = format!("pg{}", std::process::id());
        refhir_persistence::conformance::exercise_parameter_clashes(&persistence, &tag);
    }
}
