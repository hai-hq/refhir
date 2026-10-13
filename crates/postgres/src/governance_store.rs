use postgres::types::ToSql;
use postgres::Row;
use refhir_persistence::{
    audit_digest, sql_int, sql_uint, AuditFilter, AuditRow, ChainKey, ChainedRow, GovernanceStore,
    Param, PersistError, Pruned, RequestLogFilter, RequestLogRow, UsageCounters, UsageFilter,
    UsageKey, UsageRow, Where, CHAIN_GENESIS,
};

/// Held for the length of a transaction that extends the audit chain, so that servers appending
/// at the same time take turns and make one chain. (The schema is set up under 7428310, and
/// the timestamp index is built under 7428311.)
const CHAIN_LOCK: i64 = 7428312;

use crate::{definite, PostgresPersistence};

// Every number is BIGINT so that it reads back as an i64.
pub(crate) const GOVERNANCE_INIT: &str = r#"
CREATE TABLE IF NOT EXISTS usage_daily (
    period_day BIGINT NOT NULL,
    principal TEXT NOT NULL,
    request_class TEXT NOT NULL,
    requests BIGINT NOT NULL DEFAULT 0,
    client_errors BIGINT NOT NULL DEFAULT 0,
    server_errors BIGINT NOT NULL DEFAULT 0,
    throttled BIGINT NOT NULL DEFAULT 0,
    bytes_in BIGINT NOT NULL DEFAULT 0,
    bytes_out BIGINT NOT NULL DEFAULT 0,
    duration_ms BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (period_day, principal, request_class)
);
CREATE INDEX IF NOT EXISTS usage_daily_by_principal ON usage_daily (principal, period_day);
CREATE TABLE IF NOT EXISTS request_log (
    id BIGSERIAL PRIMARY KEY,
    ts BIGINT NOT NULL,
    request_id TEXT NOT NULL,
    principal TEXT NOT NULL,
    remote_ip TEXT,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    params TEXT NOT NULL,
    request_class TEXT NOT NULL,
    status BIGINT NOT NULL,
    duration_ms BIGINT NOT NULL,
    bytes_in BIGINT NOT NULL,
    bytes_out BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS request_log_by_ts ON request_log (ts);
CREATE INDEX IF NOT EXISTS request_log_by_principal ON request_log (principal, id);
CREATE INDEX IF NOT EXISTS request_log_by_request ON request_log (request_id);
CREATE TABLE IF NOT EXISTS governance_members (
    owner TEXT NOT NULL PRIMARY KEY,
    seen_ms BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS audit_log (
    id BIGSERIAL PRIMARY KEY,
    ts BIGINT NOT NULL,
    request_id TEXT NOT NULL,
    source TEXT NOT NULL,
    action TEXT NOT NULL,
    subtype TEXT NOT NULL,
    outcome BIGINT NOT NULL,
    status BIGINT NOT NULL,
    principal TEXT NOT NULL,
    client_id TEXT,
    user_id TEXT,
    fhir_user TEXT,
    remote_ip TEXT,
    resource_type TEXT,
    resource_id TEXT,
    patient TEXT,
    query TEXT,
    detail TEXT,
    prev_hash BYTEA,
    hash BYTEA
);
ALTER TABLE audit_log ADD COLUMN IF NOT EXISTS prev_hash BYTEA;
ALTER TABLE audit_log ADD COLUMN IF NOT EXISTS hash BYTEA;
CREATE INDEX IF NOT EXISTS audit_log_by_ts ON audit_log (ts);
CREATE INDEX IF NOT EXISTS audit_log_by_resource ON audit_log (resource_type, resource_id, id);
CREATE INDEX IF NOT EXISTS audit_log_by_patient ON audit_log (patient, id);
CREATE INDEX IF NOT EXISTS audit_log_by_principal ON audit_log (principal, id);
CREATE INDEX IF NOT EXISTS audit_log_by_request ON audit_log (request_id);
"#;

const AUDIT_COLUMNS: &str = "id, ts, request_id, source, action, subtype, outcome, status, \
     principal, client_id, user_id, fhir_user, remote_ip, resource_type, resource_id, patient, \
     query, detail";

const REQUEST_COLUMNS: &str = "id, ts, request_id, principal, remote_ip, method, path, params, \
     request_class, status, duration_ms, bytes_in, bytes_out";

const USAGE_COLUMNS: &str = "period_day, principal, request_class, requests, client_errors, \
     server_errors, throttled, bytes_in, bytes_out, duration_ms";

/// The values of a filter, boxed so that they can be handed to the client as one slice.
fn bind(clause: &Where) -> Vec<Box<dyn ToSql + Sync>> {
    clause
        .params
        .iter()
        .map(|param| match param {
            Param::Text(text) => Box::new(text.clone()) as Box<dyn ToSql + Sync>,
            Param::Int(value) => Box::new(*value),
        })
        .collect()
}

fn refs(values: &[Box<dyn ToSql + Sync>]) -> Vec<&(dyn ToSql + Sync)> {
    values.iter().map(|value| value.as_ref()).collect()
}

fn usage_from_row(row: &Row) -> UsageRow {
    UsageRow {
        key: UsageKey {
            day: u32::try_from(row.get::<_, i64>(0)).unwrap_or(0),
            principal: row.get(1),
            class: row.get(2),
        },
        counters: UsageCounters {
            requests: sql_uint(row.get(3)),
            client_errors: sql_uint(row.get(4)),
            server_errors: sql_uint(row.get(5)),
            throttled: sql_uint(row.get(6)),
            bytes_in: sql_uint(row.get(7)),
            bytes_out: sql_uint(row.get(8)),
            duration_ms: sql_uint(row.get(9)),
        },
    }
}

fn request_from_row(row: &Row) -> RequestLogRow {
    RequestLogRow {
        id: sql_uint(row.get(0)),
        ts_ms: sql_uint(row.get(1)),
        request_id: row.get(2),
        principal: row.get(3),
        remote_ip: row.get(4),
        method: row.get(5),
        path: row.get(6),
        params: row.get(7),
        class: row.get(8),
        status: u16::try_from(row.get::<_, i64>(9)).unwrap_or(0),
        duration_ms: sql_uint(row.get(10)),
        bytes_in: sql_uint(row.get(11)),
        bytes_out: sql_uint(row.get(12)),
    }
}

fn audit_from_row(row: &Row) -> AuditRow {
    AuditRow {
        id: sql_uint(row.get(0)),
        ts_ms: sql_uint(row.get(1)),
        request_id: row.get(2),
        source: row.get(3),
        action: row.get(4),
        subtype: row.get(5),
        outcome: u8::try_from(row.get::<_, i64>(6)).unwrap_or(0),
        status: u16::try_from(row.get::<_, i64>(7)).unwrap_or(0),
        principal: row.get(8),
        client_id: row.get(9),
        user: row.get(10),
        fhir_user: row.get(11),
        remote_ip: row.get(12),
        resource_type: row.get(13),
        resource_id: row.get(14),
        patient: row.get(15),
        query: row.get(16),
        detail: row.get(17),
    }
}

impl GovernanceStore for PostgresPersistence {
    fn usage_add(&self, rows: &[UsageRow]) -> Result<(), PersistError> {
        if rows.is_empty() {
            return Ok(());
        }
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            let stmt = tx
                .prepare(
                    "INSERT INTO usage_daily (period_day, principal, request_class, requests, \
                     client_errors, server_errors, throttled, bytes_in, bytes_out, duration_ms) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
                     ON CONFLICT (period_day, principal, request_class) DO UPDATE SET \
                     requests = usage_daily.requests + EXCLUDED.requests, \
                     client_errors = usage_daily.client_errors + EXCLUDED.client_errors, \
                     server_errors = usage_daily.server_errors + EXCLUDED.server_errors, \
                     throttled = usage_daily.throttled + EXCLUDED.throttled, \
                     bytes_in = usage_daily.bytes_in + EXCLUDED.bytes_in, \
                     bytes_out = usage_daily.bytes_out + EXCLUDED.bytes_out, \
                     duration_ms = usage_daily.duration_ms + EXCLUDED.duration_ms",
                )
                .map_err(definite)?;
            for row in rows {
                tx.execute(
                    &stmt,
                    &[
                        &i64::from(row.key.day),
                        &row.key.principal,
                        &row.key.class,
                        &sql_int(row.counters.requests),
                        &sql_int(row.counters.client_errors),
                        &sql_int(row.counters.server_errors),
                        &sql_int(row.counters.throttled),
                        &sql_int(row.counters.bytes_in),
                        &sql_int(row.counters.bytes_out),
                        &sql_int(row.counters.duration_ms),
                    ],
                )
                .map_err(definite)?;
            }
            tx.commit().map_err(definite)
        })
    }

    fn usage_query(&self, filter: &UsageFilter) -> Result<Vec<UsageRow>, PersistError> {
        crate::off_runtime(|| {
            let clause = filter.to_where();
            let sql = format!(
                "SELECT {USAGE_COLUMNS} FROM usage_daily{} \
                 ORDER BY period_day DESC, principal, request_class LIMIT {}",
                clause.sql(true, 1),
                filter.limit.clamp(1, 10_000)
            );
            let values = bind(&clause);
            let mut guard = self.inner.lock()?;
            let rows = guard.client.query(&sql, &refs(&values)).map_err(definite)?;
            Ok(rows.iter().map(usage_from_row).collect())
        })
    }

    fn request_log_append(&self, rows: &[RequestLogRow]) -> Result<(), PersistError> {
        if rows.is_empty() {
            return Ok(());
        }
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            let stmt = tx
                .prepare(
                    "INSERT INTO request_log (ts, request_id, principal, remote_ip, method, \
                     path, params, request_class, status, duration_ms, bytes_in, bytes_out) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                )
                .map_err(definite)?;
            for row in rows {
                tx.execute(
                    &stmt,
                    &[
                        &sql_int(row.ts_ms),
                        &row.request_id,
                        &row.principal,
                        &row.remote_ip,
                        &row.method,
                        &row.path,
                        &row.params,
                        &row.class,
                        &i64::from(row.status),
                        &sql_int(row.duration_ms),
                        &sql_int(row.bytes_in),
                        &sql_int(row.bytes_out),
                    ],
                )
                .map_err(definite)?;
            }
            tx.commit().map_err(definite)
        })
    }

    fn request_log_query(
        &self,
        filter: &RequestLogFilter,
    ) -> Result<Vec<RequestLogRow>, PersistError> {
        crate::off_runtime(|| {
            let clause = filter.to_where();
            let sql = format!(
                "SELECT {REQUEST_COLUMNS} FROM request_log{} ORDER BY id DESC LIMIT {}",
                clause.sql(true, 1),
                filter.limit.clamp(1, 10_000)
            );
            let values = bind(&clause);
            let mut guard = self.inner.lock()?;
            let rows = guard.client.query(&sql, &refs(&values)).map_err(definite)?;
            Ok(rows.iter().map(request_from_row).collect())
        })
    }

    fn audit_append(&self, rows: &[AuditRow], key: Option<&ChainKey>) -> Result<(), PersistError> {
        if rows.is_empty() {
            return Ok(());
        }
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            // Every server that extends the chain takes this first, so the hash read below is
            // still the last one when the rows that follow it are inserted.
            tx.execute("SELECT pg_advisory_xact_lock($1)", &[&CHAIN_LOCK])
                .map_err(definite)?;
            let mut previous: Vec<u8> = tx
                .query_opt("SELECT hash FROM audit_log ORDER BY id DESC LIMIT 1", &[])
                .map_err(definite)?
                .and_then(|row| row.get::<_, Option<Vec<u8>>>(0))
                .unwrap_or_else(|| CHAIN_GENESIS.to_vec());
            let stmt = tx
                .prepare(
                    "INSERT INTO audit_log (ts, request_id, source, action, subtype, outcome, \
                     status, principal, client_id, user_id, fhir_user, remote_ip, resource_type, \
                     resource_id, patient, query, detail, prev_hash, hash) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
                     $16, $17, $18, $19)",
                )
                .map_err(definite)?;
            for row in rows {
                let hash = audit_digest(key, &previous, row).to_vec();
                tx.execute(
                    &stmt,
                    &[
                        &sql_int(row.ts_ms),
                        &row.request_id,
                        &row.source,
                        &row.action,
                        &row.subtype,
                        &i64::from(row.outcome),
                        &i64::from(row.status),
                        &row.principal,
                        &row.client_id,
                        &row.user,
                        &row.fhir_user,
                        &row.remote_ip,
                        &row.resource_type,
                        &row.resource_id,
                        &row.patient,
                        &row.query,
                        &row.detail,
                        &previous,
                        &hash,
                    ],
                )
                .map_err(definite)?;
                previous = hash;
            }
            tx.commit().map_err(definite)
        })
    }

    fn audit_query(&self, filter: &AuditFilter) -> Result<Vec<AuditRow>, PersistError> {
        crate::off_runtime(|| {
            let clause = filter.to_where();
            let sql = format!(
                "SELECT {AUDIT_COLUMNS} FROM audit_log{} ORDER BY id DESC LIMIT {}",
                clause.sql(true, 1),
                filter.limit.clamp(1, 10_000)
            );
            let values = bind(&clause);
            let mut guard = self.inner.lock()?;
            let rows = guard.client.query(&sql, &refs(&values)).map_err(definite)?;
            Ok(rows.iter().map(audit_from_row).collect())
        })
    }

    fn audit_scan(&self, after_id: u64, limit: u32) -> Result<Vec<ChainedRow>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let rows = guard
                .client
                .query(
                    &format!(
                        "SELECT {AUDIT_COLUMNS}, prev_hash, hash FROM audit_log WHERE id > $1 \
                         ORDER BY id ASC LIMIT $2"
                    ),
                    &[&sql_int(after_id), &i64::from(limit)],
                )
                .map_err(definite)?;
            Ok(rows
                .iter()
                .map(|row| ChainedRow {
                    row: audit_from_row(row),
                    prev_hash: row.get(18),
                    hash: row.get(19),
                })
                .collect())
        })
    }

    fn audit_head(&self) -> Result<Option<(u64, Vec<u8>)>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let row = guard
                .client
                .query_opt(
                    "SELECT id, hash FROM audit_log WHERE hash IS NOT NULL ORDER BY id DESC LIMIT 1",
                    &[],
                )
                .map_err(definite)?;
            Ok(row.map(|row| (sql_uint(row.get(0)), row.get::<_, Vec<u8>>(1))))
        })
    }

    fn members_touch(&self, owner: &str, _now_ms: u64, ttl_ms: u64) -> Result<u32, PersistError> {
        // The database's clock, read inside the statements: the servers' own may disagree, and
        // this is the one thing they all have in common.
        const NOW: &str = "(extract(epoch FROM clock_timestamp()) * 1000)::bigint";
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            tx.execute(
                &format!(
                    "INSERT INTO governance_members (owner, seen_ms) VALUES ($1, {NOW}) \
                     ON CONFLICT (owner) DO UPDATE SET seen_ms = EXCLUDED.seen_ms"
                ),
                &[&owner],
            )
            .map_err(definite)?;
            tx.execute(
                &format!("DELETE FROM governance_members WHERE seen_ms < {NOW} - $1"),
                &[&sql_int(ttl_ms)],
            )
            .map_err(definite)?;
            let count: i64 = tx
                .query_one("SELECT count(*) FROM governance_members", &[])
                .map_err(definite)?
                .get(0);
            tx.commit().map_err(definite)?;
            Ok(u32::try_from(count).unwrap_or(u32::MAX))
        })
    }

    fn members_leave(&self, owner: &str) -> Result<(), PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            guard
                .client
                .execute("DELETE FROM governance_members WHERE owner = $1", &[&owner])
                .map_err(definite)?;
            Ok(())
        })
    }

    fn prune(
        &self,
        request_log_before_ms: Option<u64>,
        audit_before_ms: Option<u64>,
    ) -> Result<Pruned, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            let mut pruned = Pruned::default();
            if let Some(before) = request_log_before_ms {
                pruned.request_log = tx
                    .execute("DELETE FROM request_log WHERE ts < $1", &[&sql_int(before)])
                    .map_err(definite)?;
            }
            if let Some(before) = audit_before_ms {
                // From the start of the trail only, so that no gap is left in the chain: see
                // `GovernanceStore::prune`. Under the chain lock, so that nothing is appended
                // between the deletion's reads and its writes.
                tx.execute("SELECT pg_advisory_xact_lock($1)", &[&CHAIN_LOCK])
                    .map_err(definite)?;
                pruned.audit = tx
                    .execute(
                        "DELETE FROM audit_log WHERE ts < $1 AND id < \
                         COALESCE((SELECT min(id) FROM audit_log WHERE ts >= $1), \
                                  9223372036854775807)",
                        &[&sql_int(before)],
                    )
                    .map_err(definite)?;
            }
            tx.commit().map_err(definite)?;
            Ok(pruned)
        })
    }
}
