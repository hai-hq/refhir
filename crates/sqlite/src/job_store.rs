use refhir_persistence::{
    job_from_parts, job_ms, lease_until, plan_job_end, JobLease, JobStatus, JobStore, PersistError,
    StoredJob,
};
use rusqlite::{params, params_from_iter, types::Value as SqlValue, OptionalExtension};
use serde_json::Value as JsonValue;

use crate::{definite, SqlitePersistence};

pub(crate) const JOBS_INIT: &str = r#"
CREATE TABLE IF NOT EXISTS jobs (
    id TEXT NOT NULL PRIMARY KEY,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    request TEXT NOT NULL,
    result TEXT,
    error TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    completed_at INTEGER,
    lease_owner TEXT,
    lease_until INTEGER
);
CREATE INDEX IF NOT EXISTS jobs_by_status ON jobs (status, created_at);
"#;

/// A `jobs` table made before leases existed has no lease columns.
pub(crate) fn migrate(connection: &rusqlite::Connection) -> rusqlite::Result<()> {
    let columns: Vec<String> = connection
        .prepare("SELECT name FROM pragma_table_info('jobs')")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for (name, definition) in [("lease_owner", "TEXT"), ("lease_until", "INTEGER")] {
        if !columns.iter().any(|column| column == name) {
            connection
                .execute_batch(&format!("ALTER TABLE jobs ADD COLUMN {name} {definition}"))?;
        }
    }
    Ok(())
}

const JOB_COLUMNS: &str = "id, kind, status, request, result, error, attempts, created_at, \
     completed_at, lease_owner, lease_until";

impl JobStore for SqlitePersistence {
    fn job_create(
        &self,
        id: &str,
        kind: &str,
        request: &JsonValue,
        now_ms: u64,
        lease: Option<JobLease<'_>>,
    ) -> Result<(), PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        guard
            .connection
            .execute(
                "INSERT INTO jobs (id, kind, status, request, attempts, created_at, \
                 lease_owner, lease_until) VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7)",
                params![
                    id,
                    kind,
                    JobStatus::Accepted.as_str(),
                    request.to_string(),
                    job_ms(now_ms),
                    lease.map(|lease| lease.owner),
                    lease.map(|lease| lease_until(now_ms, lease.ms)),
                ],
            )
            .map_err(definite)?;
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
        if kinds.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; kinds.len()].join(", ");
        // One statement picks the jobs and takes them, so two callers cannot both get one.
        let sql = format!(
            "UPDATE jobs SET status = 'running', attempts = attempts + 1, \
             lease_owner = ?, lease_until = ? \
             WHERE id IN (SELECT id FROM jobs WHERE status IN ('accepted', 'running') \
                 AND kind IN ({marks}) AND (lease_owner IS NULL OR lease_until <= ?) \
                 ORDER BY created_at, id LIMIT ?) \
             RETURNING {JOB_COLUMNS}"
        );
        let mut values: Vec<SqlValue> = vec![
            SqlValue::Text(owner.to_string()),
            SqlValue::Integer(lease_until(now_ms, lease_ms)),
        ];
        values.extend(kinds.iter().map(|kind| SqlValue::Text(kind.to_string())));
        values.push(SqlValue::Integer(job_ms(now_ms)));
        values.push(SqlValue::Integer(i64::from(limit)));
        let guard = self.inner.lock().expect("sqlite");
        let mut stmt = guard.connection.prepare(&sql).map_err(definite)?;
        let rows = stmt
            .query_map(params_from_iter(values), read_job_row)
            .map_err(definite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(definite)?;
        let mut jobs = rows.into_iter().collect::<Result<Vec<_>, _>>()?;
        jobs.sort_by(|a, b| (a.created_ms, &a.id).cmp(&(b.created_ms, &b.id)));
        Ok(jobs)
    }

    fn job_start(
        &self,
        id: &str,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<StoredJob>, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        guard
            .connection
            .query_row(
                &format!(
                    "UPDATE jobs SET status = 'running', attempts = attempts + 1, lease_until = ?1 \
                     WHERE id = ?2 AND lease_owner = ?3 AND status IN ('accepted', 'running') \
                     RETURNING {JOB_COLUMNS}"
                ),
                params![lease_until(now_ms, lease_ms), id, owner],
                read_job_row,
            )
            .optional()
            .map_err(definite)?
            .transpose()
    }

    fn job_renew(
        &self,
        id: &str,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<bool, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        let changed = guard
            .connection
            .execute(
                "UPDATE jobs SET lease_until = ?1 \
                 WHERE id = ?2 AND lease_owner = ?3 AND status IN ('accepted', 'running')",
                params![lease_until(now_ms, lease_ms), id, owner],
            )
            .map_err(definite)?;
        Ok(changed == 1)
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
        let end = plan_job_end(status, result, error, now_ms)?;
        let guard = self.inner.lock().expect("sqlite");
        let changed = guard
            .connection
            .execute(
                "UPDATE jobs SET status = ?1, result = ?2, error = ?3, completed_at = ?4, \
                 lease_owner = NULL, lease_until = NULL \
                 WHERE id = ?5 AND lease_owner = ?6 AND status IN ('accepted', 'running')",
                params![
                    end.status,
                    end.result,
                    end.error,
                    end.completed_ms,
                    id,
                    owner
                ],
            )
            .map_err(definite)?;
        Ok(changed == 1)
    }

    fn job_get(&self, id: &str) -> Result<Option<StoredJob>, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        guard
            .connection
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                params![id],
                read_job_row,
            )
            .optional()
            .map_err(definite)?
            .transpose()
    }
}

fn read_job_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<StoredJob, PersistError>> {
    Ok(job_from_parts(
        row.get(0)?,
        row.get(1)?,
        &row.get::<_, String>(2)?,
        &row.get::<_, String>(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "refhir-sqlite-jobs-{name}-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn remove(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(name));
        }
    }

    #[test]
    fn sqlite_job_store_in_memory() {
        let store = SqlitePersistence::open_in_memory().unwrap();
        refhir_persistence::jobs::exercise(&store, "sqlite-memory");
    }

    #[test]
    fn sqlite_job_store_on_disk() {
        let path = temp_path("exercise");
        remove(&path);
        {
            let store = SqlitePersistence::open(&path).unwrap();
            refhir_persistence::jobs::exercise(&store, "sqlite-file");
        }
        remove(&path);
    }

    #[test]
    fn jobs_survive_reopening_the_database() {
        let path = temp_path("reopen");
        remove(&path);
        let request = json!({"type": "Patient", "id": "p1"});
        let result = json!({"file": "done.ndjson"});
        {
            let store = SqlitePersistence::open(&path).unwrap();
            store
                .job_create("queued", "export", &request, 10, None)
                .unwrap();
            store
                .job_create("running", "export", &request, 20, None)
                .unwrap();
            let got = store
                .job_claim("first-process", &["export"], 25, 5, 1)
                .unwrap();
            assert_eq!(got[0].id, "queued", "oldest first");
            store
                .job_finish(
                    "queued",
                    "first-process",
                    JobStatus::Failed,
                    None,
                    Some("x"),
                    26,
                )
                .unwrap();
            let got = store
                .job_claim("first-process", &["export"], 27, 5, 1)
                .unwrap();
            assert_eq!(got[0].id, "running");
            store
                .job_create("done", "export", &request, 30, None)
                .unwrap();
            let got = store
                .job_claim("first-process", &["export"], 30, 1000, 1)
                .unwrap();
            assert_eq!(got[0].id, "done");
            assert!(store
                .job_finish(
                    "done",
                    "first-process",
                    JobStatus::Completed,
                    Some(&result),
                    None,
                    35
                )
                .unwrap());
        }

        // A new connection to the same file, as after a restart: the finished job is as it
        // was, and the one that was running is still held by a process that is gone, so it
        // is taken over once its lease has run out and not before.
        let reopened = SqlitePersistence::open(&path).unwrap();
        let done = reopened.job_get("done").unwrap().unwrap();
        assert_eq!(
            (done.status, done.result, done.completed_ms),
            (JobStatus::Completed, Some(result), Some(35))
        );
        let held = reopened.job_get("running").unwrap().unwrap();
        assert_eq!(
            (
                held.status,
                held.attempts,
                held.lease_owner.as_deref(),
                held.lease_until_ms
            ),
            (JobStatus::Running, 1, Some("first-process"), Some(32))
        );
        assert!(reopened
            .job_claim("second-process", &["export"], 31, 5, 5)
            .unwrap()
            .is_empty());
        let taken = reopened
            .job_claim("second-process", &["export"], 32, 5, 5)
            .unwrap();
        assert_eq!((taken.len(), taken[0].attempts), (1, 2));
        assert!(!reopened
            .job_finish(
                "running",
                "first-process",
                JobStatus::Completed,
                None,
                None,
                40
            )
            .unwrap());
        assert!(reopened
            .job_finish(
                "running",
                "second-process",
                JobStatus::Completed,
                Some(&json!({})),
                None,
                41
            )
            .unwrap());
        drop(reopened);
        remove(&path);
    }

    #[test]
    fn a_table_from_before_leases_gains_the_columns() {
        let path = temp_path("migrate");
        remove(&path);
        {
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE jobs (id TEXT NOT NULL PRIMARY KEY, kind TEXT NOT NULL, \
                     status TEXT NOT NULL, request TEXT NOT NULL, result TEXT, error TEXT, \
                     attempts INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, \
                     completed_at INTEGER); \
                     INSERT INTO jobs (id, kind, status, request, attempts, created_at) \
                     VALUES ('old', 'export', 'running', '{}', 1, 5);",
                )
                .unwrap();
        }
        let store = SqlitePersistence::open(&path).unwrap();
        // The row the old table held is unheld, so it is taken at once.
        let got = store.job_claim("new", &["export"], 100, 10, 5).unwrap();
        assert_eq!(
            (got.len(), got[0].id.as_str(), got[0].attempts),
            (1, "old", 2)
        );
        drop(store);
        remove(&path);
    }

    #[test]
    fn sqlite_job_claims_do_not_overlap() {
        let store: std::sync::Arc<dyn JobStore> =
            std::sync::Arc::new(SqlitePersistence::open_in_memory().unwrap());
        refhir_persistence::jobs::exercise_competing_claimers(&|| store.clone(), "sqlite");
    }
}
