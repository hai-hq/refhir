use postgres::Row;
use refhir_persistence::{
    job_from_parts, job_ms, lease_until, plan_job_end, JobLease, JobStatus, JobStore, PersistError,
    StoredJob,
};
use serde_json::Value as JsonValue;

use crate::{definite, PostgresPersistence};

const JOB_COLUMNS: &str = "id, kind, status, request, result, error, attempts, created_at, \
     completed_at, lease_owner, lease_until";

impl JobStore for PostgresPersistence {
    fn job_create(
        &self,
        id: &str,
        kind: &str,
        request: &JsonValue,
        now_ms: u64,
        lease: Option<JobLease<'_>>,
    ) -> Result<(), PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let owner = lease.map(|lease| lease.owner);
            let until = lease.map(|lease| lease_until(now_ms, lease.ms));
            guard
                .client
                .execute(
                    &format!(
                        "INSERT INTO jobs ({}id, kind, status, request, attempts, created_at, \
                         lease_owner, lease_until) VALUES ({}$1, $2, $3, $4, 0, $5, $6, $7)",
                        self.scope.cols(),
                        self.scope.vals()
                    ),
                    &[
                        &id,
                        &kind,
                        &JobStatus::Accepted.as_str(),
                        &request.to_string(),
                        &job_ms(now_ms),
                        &owner,
                        &until,
                    ],
                )
                .map_err(definite)?;
            Ok(())
        })
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
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let kinds: Vec<String> = kinds.iter().map(|kind| kind.to_string()).collect();
            // The inner select locks the rows it picks and skips any another server has locked
            // in the middle of its own claim, so concurrent claimers each get different jobs
            // without waiting for one another. The lease it writes is what keeps the job
            // afterwards, with no lock held while the work runs.
            let rows = guard
                .client
                .query(
                    // The pick is a CTE that must run once. As `WHERE id IN (SELECT ... LIMIT n FOR
                    // UPDATE SKIP LOCKED)` the planner may run it again for each row of the update
                    // (it does, for the statement as the driver sends it, with the limit as a
                    // parameter): the rows the update has locked are skipped, so each run picks
                    // others, and more than `n` jobs are claimed. A CTE that locks rows is never
                    // inlined into the statement that uses it, on any Postgres version, so it runs
                    // once. (No `MATERIALIZED`, which would need Postgres 12.)
                    //
                    // The tenant is in the pick and in the update: a claim that reached another
                    // tenant's jobs would run its export against this tenant's data.
                    &format!(
                        "WITH picked AS (SELECT id AS picked_id FROM jobs \
                             WHERE {scoped}status IN ('accepted', 'running') AND kind = ANY($3) \
                               AND (lease_owner IS NULL OR lease_until <= $4) \
                             ORDER BY created_at, id LIMIT $5 FOR UPDATE SKIP LOCKED) \
                         UPDATE jobs SET status = 'running', attempts = attempts + 1, \
                         lease_owner = $1, lease_until = $2 \
                         FROM picked WHERE {scoped}jobs.id = picked.picked_id \
                         RETURNING {JOB_COLUMNS}",
                        scoped = self.scope.and()
                    ),
                    &[
                        &owner,
                        &lease_until(now_ms, lease_ms),
                        &kinds,
                        &job_ms(now_ms),
                        &i64::from(limit),
                    ],
                )
                .map_err(definite)?;
            let mut jobs = rows
                .iter()
                .map(read_job_row)
                .collect::<Result<Vec<_>, _>>()?;
            jobs.sort_by(|a, b| (a.created_ms, &a.id).cmp(&(b.created_ms, &b.id)));
            Ok(jobs)
        })
    }

    fn job_start(
        &self,
        id: &str,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<StoredJob>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let row = guard
                .client
                .query_opt(
                    &format!(
                        "UPDATE jobs SET status = 'running', attempts = attempts + 1, \
                         lease_until = $1 \
                         WHERE {}id = $2 AND lease_owner = $3 AND status IN ('accepted', 'running') \
                         RETURNING {JOB_COLUMNS}",
                        self.scope.and()
                    ),
                    &[&lease_until(now_ms, lease_ms), &id, &owner],
                )
                .map_err(definite)?;
            row.as_ref().map(read_job_row).transpose()
        })
    }

    fn job_renew(
        &self,
        id: &str,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<bool, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let changed = guard
                .client
                .execute(
                    &format!(
                        "UPDATE jobs SET lease_until = $1 \
                         WHERE {}id = $2 AND lease_owner = $3 AND status IN ('accepted', 'running')",
                        self.scope.and()
                    ),
                    &[&lease_until(now_ms, lease_ms), &id, &owner],
                )
                .map_err(definite)?;
            Ok(changed == 1)
        })
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
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let changed = guard
                .client
                .execute(
                    &format!(
                        "UPDATE jobs SET status = $1, result = $2, error = $3, completed_at = $4, \
                         lease_owner = NULL, lease_until = NULL \
                         WHERE {}id = $5 AND lease_owner = $6 AND status IN ('accepted', 'running')",
                        self.scope.and()
                    ),
                    &[
                        &end.status,
                        &end.result,
                        &end.error,
                        &end.completed_ms,
                        &id,
                        &owner,
                    ],
                )
                .map_err(definite)?;
            Ok(changed == 1)
        })
    }

    fn clock_ms(&self) -> Result<Option<u64>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let ms: i64 = guard
                .client
                .query_one(
                    "SELECT (extract(epoch FROM clock_timestamp()) * 1000)::bigint",
                    &[],
                )
                .map_err(definite)?
                .get(0);
            Ok(u64::try_from(ms).ok())
        })
    }

    fn job_get(&self, id: &str) -> Result<Option<StoredJob>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let row = guard
                .client
                .query_opt(
                    &format!(
                        "SELECT {JOB_COLUMNS} FROM jobs WHERE {}id = $1",
                        self.scope.and()
                    ),
                    &[&id],
                )
                .map_err(definite)?;
            row.as_ref().map(read_job_row).transpose()
        })
    }
}

fn read_job_row(row: &Row) -> Result<StoredJob, PersistError> {
    job_from_parts(
        row.get(0),
        row.get(1),
        &row.get::<_, String>(2),
        &row.get::<_, String>(3),
        row.get(4),
        row.get(5),
        i64::from(row.get::<_, i32>(6)),
        row.get(7),
        row.get(8),
        row.get(9),
        row.get(10),
    )
}
