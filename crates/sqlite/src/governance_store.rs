use refhir_persistence::{
    audit_digest, sql_int, sql_uint, AuditFilter, AuditRow, ChainKey, ChainedRow, GovernanceStore,
    Param, PersistError, Pruned, RequestLogFilter, RequestLogRow, UsageFilter, UsageKey, UsageRow,
    Where, CHAIN_GENESIS,
};
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, OptionalExtension, Row, TransactionBehavior};

use crate::{definite, SqlitePersistence};

const AUDIT_COLUMNS: &str = "id, ts, request_id, source, action, subtype, outcome, status, \
     principal, client_id, user_id, fhir_user, remote_ip, resource_type, resource_id, patient, \
     query, detail";

const REQUEST_COLUMNS: &str = "id, ts, request_id, principal, remote_ip, method, path, params, \
     request_class, status, duration_ms, bytes_in, bytes_out";

const USAGE_COLUMNS: &str = "period_day, principal, request_class, requests, client_errors, \
     server_errors, throttled, bytes_in, bytes_out, duration_ms";

fn bind(clause: &Where) -> Vec<SqlValue> {
    clause
        .params
        .iter()
        .map(|param| match param {
            Param::Text(text) => SqlValue::Text(text.clone()),
            Param::Int(value) => SqlValue::Integer(*value),
        })
        .collect()
}

fn usage_from_row(row: &Row<'_>) -> rusqlite::Result<UsageRow> {
    Ok(UsageRow {
        key: UsageKey {
            day: u32::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
            principal: row.get(1)?,
            class: row.get(2)?,
        },
        counters: refhir_persistence::UsageCounters {
            requests: sql_uint(row.get(3)?),
            client_errors: sql_uint(row.get(4)?),
            server_errors: sql_uint(row.get(5)?),
            throttled: sql_uint(row.get(6)?),
            bytes_in: sql_uint(row.get(7)?),
            bytes_out: sql_uint(row.get(8)?),
            duration_ms: sql_uint(row.get(9)?),
        },
    })
}

fn request_from_row(row: &Row<'_>) -> rusqlite::Result<RequestLogRow> {
    Ok(RequestLogRow {
        id: sql_uint(row.get(0)?),
        ts_ms: sql_uint(row.get(1)?),
        request_id: row.get(2)?,
        principal: row.get(3)?,
        remote_ip: row.get(4)?,
        method: row.get(5)?,
        path: row.get(6)?,
        params: row.get(7)?,
        class: row.get(8)?,
        status: u16::try_from(row.get::<_, i64>(9)?).unwrap_or(0),
        duration_ms: sql_uint(row.get(10)?),
        bytes_in: sql_uint(row.get(11)?),
        bytes_out: sql_uint(row.get(12)?),
    })
}

fn audit_from_row(row: &Row<'_>) -> rusqlite::Result<AuditRow> {
    Ok(AuditRow {
        id: sql_uint(row.get(0)?),
        ts_ms: sql_uint(row.get(1)?),
        request_id: row.get(2)?,
        source: row.get(3)?,
        action: row.get(4)?,
        subtype: row.get(5)?,
        outcome: u8::try_from(row.get::<_, i64>(6)?).unwrap_or(0),
        status: u16::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
        principal: row.get(8)?,
        client_id: row.get(9)?,
        user: row.get(10)?,
        fhir_user: row.get(11)?,
        remote_ip: row.get(12)?,
        resource_type: row.get(13)?,
        resource_id: row.get(14)?,
        patient: row.get(15)?,
        query: row.get(16)?,
        detail: row.get(17)?,
    })
}

impl GovernanceStore for SqlitePersistence {
    fn usage_add(&self, rows: &[UsageRow]) -> Result<(), PersistError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut guard = self.inner.lock()?;
        let tx = guard.connection.transaction().map_err(definite)?;
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO usage_daily (period_day, principal, request_class, requests, \
                     client_errors, server_errors, throttled, bytes_in, bytes_out, duration_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                     ON CONFLICT (period_day, principal, request_class) DO UPDATE SET \
                     requests = usage_daily.requests + excluded.requests, \
                     client_errors = usage_daily.client_errors + excluded.client_errors, \
                     server_errors = usage_daily.server_errors + excluded.server_errors, \
                     throttled = usage_daily.throttled + excluded.throttled, \
                     bytes_in = usage_daily.bytes_in + excluded.bytes_in, \
                     bytes_out = usage_daily.bytes_out + excluded.bytes_out, \
                     duration_ms = usage_daily.duration_ms + excluded.duration_ms",
                )
                .map_err(definite)?;
            for row in rows {
                stmt.execute(params![
                    i64::from(row.key.day),
                    row.key.principal,
                    row.key.class,
                    sql_int(row.counters.requests),
                    sql_int(row.counters.client_errors),
                    sql_int(row.counters.server_errors),
                    sql_int(row.counters.throttled),
                    sql_int(row.counters.bytes_in),
                    sql_int(row.counters.bytes_out),
                    sql_int(row.counters.duration_ms),
                ])
                .map_err(definite)?;
            }
        }
        tx.commit().map_err(definite)
    }

    fn usage_query(&self, filter: &UsageFilter) -> Result<Vec<UsageRow>, PersistError> {
        let clause = filter.to_where();
        let sql = format!(
            "SELECT {USAGE_COLUMNS} FROM usage_daily{} \
             ORDER BY period_day DESC, principal, request_class LIMIT {}",
            clause.sql(false, 1),
            filter.limit.clamp(1, 10_000)
        );
        let guard = self.inner.lock()?;
        let mut stmt = guard.connection.prepare(&sql).map_err(definite)?;
        let rows = stmt
            .query_map(params_from_iter(bind(&clause)), usage_from_row)
            .map_err(definite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(definite)?;
        Ok(rows)
    }

    fn request_log_append(&self, rows: &[RequestLogRow]) -> Result<(), PersistError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut guard = self.inner.lock()?;
        let tx = guard.connection.transaction().map_err(definite)?;
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO request_log (ts, request_id, principal, remote_ip, method, \
                     path, params, request_class, status, duration_ms, bytes_in, bytes_out) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                )
                .map_err(definite)?;
            for row in rows {
                stmt.execute(params![
                    sql_int(row.ts_ms),
                    row.request_id,
                    row.principal,
                    row.remote_ip,
                    row.method,
                    row.path,
                    row.params,
                    row.class,
                    i64::from(row.status),
                    sql_int(row.duration_ms),
                    sql_int(row.bytes_in),
                    sql_int(row.bytes_out),
                ])
                .map_err(definite)?;
            }
        }
        tx.commit().map_err(definite)
    }

    fn request_log_query(
        &self,
        filter: &RequestLogFilter,
    ) -> Result<Vec<RequestLogRow>, PersistError> {
        let clause = filter.to_where();
        let sql = format!(
            "SELECT {REQUEST_COLUMNS} FROM request_log{} ORDER BY id DESC LIMIT {}",
            clause.sql(false, 1),
            filter.limit.clamp(1, 10_000)
        );
        let guard = self.inner.lock()?;
        let mut stmt = guard.connection.prepare(&sql).map_err(definite)?;
        let rows = stmt
            .query_map(params_from_iter(bind(&clause)), request_from_row)
            .map_err(definite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(definite)?;
        Ok(rows)
    }

    fn audit_append(&self, rows: &[AuditRow], key: Option<&ChainKey>) -> Result<(), PersistError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut guard = self.inner.lock()?;
        // Taking the write lock before reading the last hash, so that no other connection can
        // add a row between the read and the insert and make the chain fork.
        let tx = guard
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(definite)?;
        let mut previous: Vec<u8> = tx
            .query_row(
                "SELECT hash FROM audit_log ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()
            .map_err(definite)?
            .flatten()
            .unwrap_or_else(|| CHAIN_GENESIS.to_vec());
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO audit_log (ts, request_id, source, action, subtype, outcome, \
                     status, principal, client_id, user_id, fhir_user, remote_ip, resource_type, \
                     resource_id, patient, query, detail, prev_hash, hash) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
                     ?16, ?17, ?18, ?19)",
                )
                .map_err(definite)?;
            for row in rows {
                let hash = audit_digest(key, &previous, row).to_vec();
                stmt.execute(params![
                    sql_int(row.ts_ms),
                    row.request_id,
                    row.source,
                    row.action,
                    row.subtype,
                    i64::from(row.outcome),
                    i64::from(row.status),
                    row.principal,
                    row.client_id,
                    row.user,
                    row.fhir_user,
                    row.remote_ip,
                    row.resource_type,
                    row.resource_id,
                    row.patient,
                    row.query,
                    row.detail,
                    previous,
                    hash,
                ])
                .map_err(definite)?;
                previous = hash;
            }
        }
        tx.commit().map_err(definite)
    }

    fn audit_query(&self, filter: &AuditFilter) -> Result<Vec<AuditRow>, PersistError> {
        let clause = filter.to_where();
        let sql = format!(
            "SELECT {AUDIT_COLUMNS} FROM audit_log{} ORDER BY id DESC LIMIT {}",
            clause.sql(false, 1),
            filter.limit.clamp(1, 10_000)
        );
        let guard = self.inner.lock()?;
        let mut stmt = guard.connection.prepare(&sql).map_err(definite)?;
        let rows = stmt
            .query_map(params_from_iter(bind(&clause)), audit_from_row)
            .map_err(definite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(definite)?;
        Ok(rows)
    }

    fn audit_scan(&self, after_id: u64, limit: u32) -> Result<Vec<ChainedRow>, PersistError> {
        let guard = self.inner.lock()?;
        let mut stmt = guard
            .connection
            .prepare_cached(&format!(
                "SELECT {AUDIT_COLUMNS}, prev_hash, hash FROM audit_log WHERE id > ?1 \
                 ORDER BY id ASC LIMIT ?2"
            ))
            .map_err(definite)?;
        let rows = stmt
            .query_map(params![sql_int(after_id), i64::from(limit)], |row| {
                Ok(ChainedRow {
                    row: audit_from_row(row)?,
                    prev_hash: row.get(18)?,
                    hash: row.get(19)?,
                })
            })
            .map_err(definite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(definite)?;
        Ok(rows)
    }

    fn audit_head(&self) -> Result<Option<(u64, Vec<u8>)>, PersistError> {
        let guard = self.inner.lock()?;
        guard
            .connection
            .query_row(
                "SELECT id, hash FROM audit_log WHERE hash IS NOT NULL ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((sql_uint(row.get(0)?), row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(definite)
    }

    fn members_touch(&self, owner: &str, now_ms: u64, ttl_ms: u64) -> Result<u32, PersistError> {
        let mut guard = self.inner.lock()?;
        let tx = guard.connection.transaction().map_err(definite)?;
        tx.execute(
            "INSERT INTO governance_members (owner, seen_ms) VALUES (?1, ?2) \
             ON CONFLICT (owner) DO UPDATE SET seen_ms = excluded.seen_ms",
            params![owner, sql_int(now_ms)],
        )
        .map_err(definite)?;
        tx.execute(
            "DELETE FROM governance_members WHERE seen_ms < ?1",
            [sql_int(now_ms.saturating_sub(ttl_ms))],
        )
        .map_err(definite)?;
        let count: i64 = tx
            .query_row("SELECT count(*) FROM governance_members", [], |row| {
                row.get(0)
            })
            .map_err(definite)?;
        tx.commit().map_err(definite)?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    fn members_leave(&self, owner: &str) -> Result<(), PersistError> {
        let guard = self.inner.lock()?;
        guard
            .connection
            .execute("DELETE FROM governance_members WHERE owner = ?1", [owner])
            .map_err(definite)?;
        Ok(())
    }

    fn prune(
        &self,
        request_log_before_ms: Option<u64>,
        audit_before_ms: Option<u64>,
    ) -> Result<Pruned, PersistError> {
        let mut guard = self.inner.lock()?;
        let tx = guard.connection.transaction().map_err(definite)?;
        let mut pruned = Pruned::default();
        if let Some(before) = request_log_before_ms {
            pruned.request_log = tx
                .execute("DELETE FROM request_log WHERE ts < ?1", [sql_int(before)])
                .map_err(definite)? as u64;
        }
        if let Some(before) = audit_before_ms {
            // From the start of the trail only, so that no gap is left in the chain: see
            // `GovernanceStore::prune`.
            pruned.audit = tx
                .execute(
                    "DELETE FROM audit_log WHERE ts < ?1 AND id < \
                     COALESCE((SELECT min(id) FROM audit_log WHERE ts >= ?1), 9223372036854775807)",
                    [sql_int(before)],
                )
                .map_err(definite)? as u64;
        }
        tx.commit().map_err(definite)?;
        Ok(pruned)
    }
}

#[cfg(test)]
mod tests {
    use refhir_persistence::{AuditFilter, AuditRow, GovernanceStore, UsageCounters, UsageFilter};
    use refhir_persistence::{UsageKey, UsageRow};

    use crate::SqlitePersistence;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "refhir-governance-{name}-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn remove(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn sqlite_store() {
        refhir_persistence::governance_checks::exercise(
            &SqlitePersistence::open_in_memory().unwrap(),
            "sqlite",
        );
    }

    fn chain_key() -> refhir_persistence::ChainKey {
        refhir_persistence::ChainKey::new(b"a key for the sqlite tests".to_vec())
    }

    fn audit_row(n: u32) -> AuditRow {
        AuditRow {
            ts_ms: u64::from(n),
            request_id: format!("r{n}"),
            source: "http".into(),
            action: "R".into(),
            subtype: "read".into(),
            principal: "client:a".into(),
            resource_id: Some(format!("p{n}")),
            ..Default::default()
        }
    }

    fn report(store: &SqlitePersistence) -> refhir_persistence::ChainReport {
        refhir_persistence::verify_audit_chain(store, Some(&chain_key()), None).unwrap()
    }

    #[test]
    fn sqlite_chain() {
        refhir_persistence::governance_checks::exercise_chain(
            &SqlitePersistence::open_in_memory().unwrap(),
            "sqlite",
        );
    }

    #[test]
    fn writers_on_separate_connections_make_one_chain() {
        let path = temp_path("chain");
        remove(&path);
        let open = {
            let path = path.clone();
            move || -> std::sync::Arc<dyn GovernanceStore> {
                std::sync::Arc::new(SqlitePersistence::open(&path).unwrap())
            }
        };
        refhir_persistence::governance_checks::exercise_concurrent_chain(&open, "sqlite");
        remove(&path);
    }

    #[test]
    fn what_is_done_to_the_table_behind_the_servers_back_is_found() {
        let key = chain_key();
        let fresh = || {
            let store = SqlitePersistence::open_in_memory().unwrap();
            let rows: Vec<AuditRow> = (0..6).map(audit_row).collect();
            store.audit_append(&rows, Some(&key)).unwrap();
            assert!(report(&store).intact());
            store
        };
        let sql = |store: &SqlitePersistence, statement: &str| {
            store
                .inner
                .lock()
                .unwrap()
                .connection
                .execute_batch(statement)
                .unwrap();
        };
        let found = |store: &SqlitePersistence| {
            report(store)
                .breaks
                .iter()
                .map(|found| (found.id, found.problem))
                .collect::<Vec<_>>()
        };
        use refhir_persistence::ChainProblem::{Detached, Edited, Unhashed};

        let edited = fresh();
        sql(
            &edited,
            "UPDATE audit_log SET resource_id = 'someone-else' WHERE id = 3",
        );
        assert_eq!(found(&edited), vec![(3, Edited)]);
        let erased = fresh();
        sql(
            &erased,
            "UPDATE audit_log SET principal = 'client:nobody' WHERE id = 2",
        );
        assert_eq!(found(&erased), vec![(2, Edited)], "who did it, rewritten");
        let removed = fresh();
        sql(&removed, "DELETE FROM audit_log WHERE id = 3");
        assert_eq!(found(&removed), vec![(4, Detached)]);
        let inserted = fresh();
        sql(
            &inserted,
            "INSERT INTO audit_log (ts, request_id, source, action, subtype, outcome, status, principal) \
             VALUES (9, 'made-up', 'http', 'R', 'read', 0, 200, 'client:a')",
        );
        assert_eq!(found(&inserted), vec![(7, Unhashed)]);
        // A row made up with a hash of its own does not follow, and is not what the key makes.
        let forged = fresh();
        sql(
            &forged,
            "INSERT INTO audit_log (ts, request_id, source, action, subtype, outcome, status, principal, prev_hash, hash) \
             VALUES (9, 'made-up', 'http', 'R', 'read', 0, 200, 'client:a', x'00', x'01')",
        );
        assert_eq!(found(&forged), vec![(7, Detached), (7, Edited)]);
        // Hashes taken off the front of the trail so that it can be rewritten: the first row
        // that still has one does not follow from the start, and is found.
        let stripped = fresh();
        sql(
            &stripped,
            "UPDATE audit_log SET hash = NULL, prev_hash = NULL, principal = 'client:nobody' \
             WHERE id <= 3",
        );
        assert_eq!(found(&stripped), vec![(4, Detached)]);
        assert!(!report(&stripped).intact());
        // And with every hash taken off, nothing has been verified, which is not 'intact'.
        let wiped = fresh();
        sql(&wiped, "UPDATE audit_log SET hash = NULL, prev_hash = NULL");
        let wiped_report = report(&wiped);
        assert_eq!((wiped_report.checked, wiped_report.unchained), (0, 6));
        assert!(!wiped_report.intact());
        // Retention from the start leaves a chain that begins later, and no alarm.
        let aged = fresh();
        let pruned = aged.prune(None, Some(3)).unwrap();
        assert_eq!(pruned.audit, 3);
        assert!(report(&aged).intact());
        assert_eq!(report(&aged).checked, 3);
    }

    #[test]
    fn rows_with_no_hash_before_the_chain_starts_are_not_a_fault() {
        let store = SqlitePersistence::open_in_memory().unwrap();
        store
            .inner
            .lock()
            .unwrap()
            .connection
            .execute_batch(
                "INSERT INTO audit_log (ts, request_id, source, action, subtype, outcome, status, principal) \
                 VALUES (1, 'old-1', 'http', 'R', 'read', 0, 200, 'client:a'), \
                        (2, 'old-2', 'http', 'R', 'read', 0, 200, 'client:a');",
            )
            .unwrap();
        let before = report(&store);
        assert_eq!(
            (before.checked, before.unchained, before.breaks_total),
            (0, 2, 0)
        );
        assert!(!before.intact(), "nothing in it has been verified");
        store
            .audit_append(&[audit_row(10), audit_row(11)], Some(&chain_key()))
            .unwrap();
        let after = report(&store);
        assert!(after.intact(), "{after:?}");
        assert_eq!(
            (after.checked, after.unchained),
            (2, 2),
            "the old rows are not a fault"
        );
        // The first row of the chain is chained to the start, not to a row that had no hash.
        let scanned = store.audit_scan(2, 10).unwrap();
        assert_eq!(
            scanned[0].prev_hash.as_deref(),
            Some(&refhir_persistence::CHAIN_GENESIS[..])
        );
    }

    #[test]
    fn sqlite_members() {
        refhir_persistence::governance_checks::exercise_members(
            &SqlitePersistence::open_in_memory().unwrap(),
            "sqlite",
        );
    }

    #[test]
    fn rows_survive_a_restart_and_usage_keeps_adding() {
        let path = temp_path("restart");
        remove(&path);
        let key = UsageKey {
            day: 7,
            principal: "client:c".into(),
            class: "read".into(),
        };
        let one = UsageCounters {
            requests: 1,
            ..Default::default()
        };
        {
            let store = SqlitePersistence::open(&path).unwrap();
            store
                .usage_add(&[UsageRow {
                    key: key.clone(),
                    counters: one,
                }])
                .unwrap();
            store
                .audit_append(
                    &[AuditRow {
                        ts_ms: 5,
                        request_id: "r".into(),
                        source: "http".into(),
                        action: "R".into(),
                        subtype: "read".into(),
                        principal: "client:c".into(),
                        ..Default::default()
                    }],
                    None,
                )
                .unwrap();
        }
        let reopened = SqlitePersistence::open(&path).unwrap();
        // A second server, or the same one after a restart, adds to what is there.
        reopened
            .usage_add(&[UsageRow {
                key: key.clone(),
                counters: one,
            }])
            .unwrap();
        let rows = reopened
            .usage_query(&UsageFilter {
                principal: Some("client:c".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].counters.requests, 2);
        let audit = reopened
            .audit_query(&AuditFilter {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!((audit.len(), audit[0].request_id.as_str()), (1, "r"));
        drop(reopened);
        remove(&path);
    }
}
