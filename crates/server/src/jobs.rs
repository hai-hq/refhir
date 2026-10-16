//! Work that runs after `publish_commit`: Subscription delivery, bulk export, remote terminology.
//!
//! It runs as a job after the commit, not as another transaction engine.
//!
//! Bulk export is a durable job. Its row lives in the database (the `jobs` table behind
//! [`JobStore`]), not in this process, so a client can ask for the outcome after a restart, and
//! a job the restart cut off is run again.
//!
//! Subscription delivery is not durable. A rest-hook notification that failed waits in memory for
//! its retries, and a restart drops it. What delivery learns about a Subscription is written to
//! it as `status` and `error`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use refhir_database::Database;
use refhir_fhir::{Catalog, ConformanceStore, FhirError, RunError};
use refhir_persistence::{
    JobLease, JobStatus, JobStore, MemoryJobStore, PersistError, StoredJob, TimestampRange,
};
use refhir_types::{Order, Timestamp};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use crate::http::{fhir_json_public, AppState, ServerConfig};

/// `kind` of the rows written for `$export`.
const EXPORT_KIND: &str = "export";

/// A job that has been started this many times without finishing is not started again. Each
/// start is recorded before the work begins, so a job that keeps killing the process stops after
/// this many restarts instead of crash-looping the server.
const MAX_ATTEMPTS: u32 = 3;

/// How long a server holds a job without renewing it. A server that dies mid-job is out of the
/// way after this long; a live one renews three times in that time.
const LEASE: Duration = Duration::from_secs(30);

/// The most jobs the sweep leaves running at once. Each export holds the whole log in memory, so
/// a server that finds a long queue works through it a few at a time.
const MAX_RUNNING: usize = 4;

/// How many commits one read of the log covers during an export. The store's connection is held
/// for the length of a read, and the lease heartbeat needs it too, so no read may be long.
const EXPORT_SLICE: u64 = 5_000;

/// The database-backed job table, as passed in `ServerConfig::jobs`.
#[derive(Clone)]
pub struct SharedJobStore(Arc<dyn JobStore>);

impl SharedJobStore {
    pub fn new(store: Arc<dyn JobStore>) -> Self {
        Self(store)
    }
}

impl std::fmt::Debug for SharedJobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedJobStore")
    }
}

/// Runs one kind of job. It runs on a blocking thread, and it runs again from the start if the
/// process stops before it finishes, so it must be safe to repeat.
pub trait JobHandler: Send + Sync {
    /// The job's `result` on success, or the text for its `error`.
    fn run(&self, job: &StoredJob) -> Result<Value, String>;
}

impl<F> JobHandler for F
where
    F: Fn(&StoredJob) -> Result<Value, String> + Send + Sync,
{
    fn run(&self, job: &StoredJob) -> Result<Value, String> {
        self(job)
    }
}

/// Accepts jobs, records them in the [`JobStore`], and runs the ones it holds through the
/// handler registered for their `kind`.
///
/// Any number of servers can share one job table. A job is worked on by the server that holds
/// its lease: this one takes jobs nobody holds, renews the lease while the work goes on, and
/// lets go of it when the work ends. A job a stopped server was holding is taken by whichever
/// server notices its lease has run out, which the periodic sweep makes sure happens without
/// waiting for a restart.
pub struct JobRunner {
    store: Arc<dyn JobStore>,
    handlers: BTreeMap<String, Arc<dyn JobHandler>>,
    /// Who holds a job, unique to this process: a restarted server is a new owner, so it does
    /// not mistake the job its previous self was running for its own.
    owner: String,
    lease: Duration,
    /// Jobs this process is running. A job in here is not started a second time, whatever the
    /// table says about its lease.
    running: Mutex<BTreeSet<String>>,
}

impl JobRunner {
    pub fn new(store: Arc<dyn JobStore>) -> Self {
        Self::with_lease(store, LEASE)
    }

    /// A runner whose leases last `lease`. Shorter than the default only makes sense in tests.
    pub fn with_lease(store: Arc<dyn JobStore>, lease: Duration) -> Self {
        Self {
            store,
            handlers: BTreeMap::new(),
            owner: format!("{}-{}", std::process::id(), Uuid::new_v4()),
            lease,
            running: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Handle jobs of `kind`. A later call for the same kind replaces the earlier handler.
    pub fn register(&mut self, kind: &str, handler: Arc<dyn JobHandler>) {
        self.handlers.insert(kind.to_string(), handler);
    }

    pub(crate) fn store(&self) -> &Arc<dyn JobStore> {
        &self.store
    }

    fn lease_ms(&self) -> u64 {
        u64::try_from(self.lease.as_millis()).unwrap_or(u64::MAX)
    }

    /// The time leases are measured on: the store's clock when it has one all servers share,
    /// since a lease written by one server is judged by another. Blocks.
    fn now(&self) -> u64 {
        self.store.clock_ms().ok().flatten().unwrap_or_else(now_ms)
    }

    /// Record a new job, held by this server from the start, and run it in the background. The
    /// job is in the store before this returns, so the caller can hand out its id.
    pub async fn submit(
        self: &Arc<Self>,
        kind: &str,
        request: Value,
    ) -> Result<String, PersistError> {
        if !self.handlers.contains_key(kind) {
            return Err(PersistError::Definite(format!(
                "no handler is registered for job kind {kind}"
            )));
        }
        let id = Uuid::new_v4().to_string();
        let (runner, row_id, row_kind) = (self.clone(), id.clone(), kind.to_string());
        let started = tokio::task::spawn_blocking(move || {
            let lease = JobLease {
                owner: &runner.owner,
                ms: runner.lease_ms(),
            };
            runner
                .store
                .job_create(&row_id, &row_kind, &request, runner.now(), Some(lease))?;
            runner
                .store
                .job_start(&row_id, &runner.owner, runner.now(), runner.lease_ms())
        })
        .await
        .map_err(|_| PersistError::Definite("job store worker stopped".into()))??;
        if let Some(job) = started {
            self.running.lock().expect("running").insert(job.id.clone());
            let runner = self.clone();
            tokio::task::spawn_blocking(move || runner.execute(&job));
        }
        Ok(id)
    }

    /// Start sweeping: at once, and then every third of a lease, take the jobs of the kinds
    /// this server handles that nobody holds, and run them. This is what runs again a job the
    /// last process was cut off from, and a job another server stopped working on. Start-up
    /// does not wait for any of it.
    pub fn recover(self: &Arc<Self>) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let every = (self.lease / 3).max(Duration::from_millis(10));
        // The sweep does not keep the runner alive: once the server that owns it is dropped, the
        // sweep ends instead of going on claiming jobs for a server that is gone.
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(runner) = weak.upgrade() else {
                    break;
                };
                let claimer = runner.clone();
                let claimed = tokio::task::spawn_blocking(move || claimer.sweep()).await;
                let Ok(claimed) = claimed else { continue };
                for job in claimed {
                    tracing::info!(
                        job = %job.id,
                        kind = %job.kind,
                        attempt = job.attempts,
                        "running a job nobody was holding"
                    );
                    let runner = runner.clone();
                    tokio::task::spawn_blocking(move || runner.execute(&job));
                }
            }
        });
    }

    /// Take the jobs nobody holds, as many as this server has room to run. Blocks.
    fn sweep(&self) -> Vec<StoredJob> {
        let busy = self.running.lock().expect("running").len();
        let room = MAX_RUNNING.saturating_sub(busy);
        if room == 0 {
            return Vec::new();
        }
        let kinds: Vec<&str> = self.handlers.keys().map(String::as_str).collect();
        match self.store.job_claim(
            &self.owner,
            &kinds,
            self.now(),
            self.lease_ms(),
            room as u32,
        ) {
            Ok(jobs) => {
                // A job this server is already running can come back here when a slow read made
                // its lease lapse. Its heartbeat keeps the lease it just took, so it is left to
                // finish rather than started again beside itself.
                let mut running = self.running.lock().expect("running");
                jobs.into_iter()
                    .filter(|job| running.insert(job.id.clone()))
                    .collect()
            }
            Err(err) => {
                tracing::error!(error = %err, "could not claim jobs");
                Vec::new()
            }
        }
    }

    /// Run `job`, which this server holds, to the end and record how it went. Blocks.
    fn execute(&self, job: &StoredJob) {
        let _running = Running {
            runner: self,
            id: &job.id,
        };
        if job.attempts > MAX_ATTEMPTS {
            let message = format!("gave up after {} interrupted attempts", job.attempts - 1);
            self.finish(&job.id, JobStatus::Failed, None, Some(&message));
            return;
        }
        let Some(handler) = self.handlers.get(&job.kind) else {
            let message = format!("no handler for job kind {}", job.kind);
            self.finish(&job.id, JobStatus::Failed, None, Some(&message));
            return;
        };
        // The lease is renewed on its own thread for as long as the work goes on.
        let (outcome, still_held) = std::thread::scope(|scope| {
            let (stop, stopped) = std::sync::mpsc::channel::<()>();
            let beat = scope.spawn(|| self.hold(&job.id, stopped));
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| handler.run(job)))
                .unwrap_or_else(|_| Err("job handler panicked".to_string()));
            drop(stop);
            (outcome, beat.join().unwrap_or(true))
        });
        if !still_held {
            tracing::warn!(
                job = %job.id,
                "lost the lease on a job while running it; another server holds it now"
            );
        }
        match outcome {
            Ok(result) => self.finish(&job.id, JobStatus::Completed, Some(&result), None),
            Err(message) => self.finish(&job.id, JobStatus::Failed, None, Some(&message)),
        }
    }

    /// Renew the lease on `id` every third of a lease until `stopped` closes. Returns whether
    /// it was still held when it stopped.
    fn hold(&self, id: &str, stopped: std::sync::mpsc::Receiver<()>) -> bool {
        loop {
            match stopped.recv_timeout(self.lease / 3) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    match self
                        .store
                        .job_renew(id, &self.owner, self.now(), self.lease_ms())
                    {
                        Ok(true) => {}
                        Ok(false) => return false,
                        // A failed renewal is tried again; the lease has time to spare.
                        Err(err) => {
                            tracing::warn!(job = %id, error = %err, "could not renew a job lease")
                        }
                    }
                }
                _ => return true,
            }
        }
    }

    fn finish(&self, id: &str, status: JobStatus, result: Option<&Value>, error: Option<&str>) {
        match self
            .store
            .job_finish(id, &self.owner, status, result, error, self.now())
        {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                job = %id,
                "not recording the outcome of a job this server no longer holds"
            ),
            // The lease runs out, and the job is run again.
            Err(err) => tracing::error!(job = %id, error = %err, "could not record job outcome"),
        }
    }
}

/// Takes a job off the running list when its execution ends, however it ends.
struct Running<'a> {
    runner: &'a JobRunner,
    id: &'a str,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.runner.running.lock().expect("running").remove(self.id);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Build the runner for a server, register what it can run, and start sweeping for jobs.
pub(crate) fn start(
    db: &Arc<Database>,
    catalog: &Arc<std::sync::RwLock<Catalog>>,
    config: &ServerConfig,
) -> Arc<JobRunner> {
    let store = match &config.jobs {
        Some(shared) => shared.0.clone(),
        None => {
            tracing::warn!(
                "no durable job store configured: jobs live in memory and are lost on restart"
            );
            Arc::new(MemoryJobStore::default())
        }
    };
    let mut runner = JobRunner::new(store);
    runner.register(
        EXPORT_KIND,
        Arc::new(ExportHandler {
            db: db.clone(),
            catalog: catalog.clone(),
            dir: PathBuf::from(&config.export_dir),
        }),
    );
    let runner = Arc::new(runner);
    // Only the primary runs jobs, and sweeps for the ones another primary was cut off from. A
    // replica keeps no files of its own and hands every job request to the primary.
    if !db.is_read_only() {
        runner.recover();
    }
    runner
}

fn kick() -> &'static Mutex<Option<UnboundedSender<Timestamp>>> {
    static KICK: std::sync::OnceLock<Mutex<Option<UnboundedSender<Timestamp>>>> =
        std::sync::OnceLock::new();
    KICK.get_or_init(|| Mutex::new(None))
}

pub fn attach(
    db: Arc<Database>,
    catalog: Arc<std::sync::RwLock<Catalog>>,
    store: Arc<dyn ConformanceStore>,
    config: &ServerConfig,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    db.on_commit(tx.clone());
    *kick().lock().expect("kick") = Some(tx);
    let urls = config.terminology_urls.clone();
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let relay = Arc::new(Relay {
        db,
        catalog,
        store,
        pending: Mutex::new(Vec::new()),
    });
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                Some(ts) = rx.recv() => {
                    let relay = relay.clone();
                    let urls = urls.clone();
                    tokio::task::spawn_blocking(move || {
                        drain_hooks(&relay);
                        deliver_subscriptions(&relay, ts);
                        if !urls.is_empty() {
                            fetch_terminology(relay.store.as_ref(), &relay.db, &relay.catalog, &urls);
                        }
                    })
                    .await
                    .ok();
                }
                _ = ticker.tick() => {
                    let relay = relay.clone();
                    tokio::task::spawn_blocking(move || drain_hooks(&relay)).await.ok();
                }
            }
        }
    });
}

pub async fn accept_export(
    app: &AppState,
    type_name: Option<String>,
    id: Option<String>,
    body: &[u8],
) -> Response {
    let _ = body;
    // Export jobs are run, and their files kept, by the primary: a replica sends all of it there.
    if app.db.is_read_only() {
        return crate::cluster::read_only_response(crate::cluster::READ_ONLY_MESSAGE);
    }
    // The job is in the database before the 202 goes out, so the id always resolves later.
    let request = json!({"type": type_name, "id": id});
    let job_id = match app.jobs.submit(EXPORT_KIND, request).await {
        Ok(job_id) => job_id,
        Err(err) => {
            tracing::error!(error = %err, "could not record export job");
            return fhir_json_public(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"resourceType": "OperationOutcome", "issue": [{"severity": "error", "code": "exception", "diagnostics": "export could not be recorded"}]}),
            );
        }
    };
    let mut response = fhir_json_public(
        StatusCode::ACCEPTED,
        json!({
            "resourceType": "OperationOutcome",
            "issue": [{"severity": "information", "code": "informational", "diagnostics": "export accepted"}]
        }),
    );
    if let Ok(location) = format!("/export/{job_id}").parse() {
        response
            .headers_mut()
            .insert(header::CONTENT_LOCATION, location);
    }
    response
}

pub async fn status(app: &AppState, job: &str) -> Response {
    // Export jobs are run, and their files kept, by the primary: a replica sends all of it there.
    if app.db.is_read_only() {
        return crate::cluster::read_only_response(crate::cluster::READ_ONLY_MESSAGE);
    }
    match export_job(app, job).await {
        Ok(job) => (StatusCode::OK, Json(status_body(&job))).into_response(),
        Err(response) => response,
    }
}

pub async fn file(app: &AppState, job: &str) -> Response {
    // Export jobs are run, and their files kept, by the primary: a replica sends all of it there.
    if app.db.is_read_only() {
        return crate::cluster::read_only_response(crate::cluster::READ_ONLY_MESSAGE);
    }
    let Ok(job) = export_job(app, job).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Only a completed job has a finished file, and its result names it. The directory is the
    // configured one, so this works after a restart.
    let Some(name) = job
        .result
        .as_ref()
        .filter(|_| job.status == JobStatus::Completed)
        .and_then(|result| result.get("file"))
        .and_then(Value::as_str)
        .and_then(|name| Path::new(name).file_name())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = PathBuf::from(&app.config.export_dir).join(name);
    match tokio::task::spawn_blocking(move || fs::read(path)).await {
        Ok(Ok(bytes)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/fhir+ndjson")],
            bytes,
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The export job with this id, or the response to send when there is none.
async fn export_job(app: &AppState, id: &str) -> Result<StoredJob, Response> {
    let runner = app.jobs.clone();
    let id = id.to_string();
    let found = tokio::task::spawn_blocking(move || runner.store().job_get(&id))
        .await
        .map_err(|_| PersistError::Definite("job store worker stopped".into()))
        .and_then(|found| found);
    match found {
        Ok(Some(job)) if job.kind == EXPORT_KIND => Ok(job),
        Ok(_) => Err(fhir_json_public(
            StatusCode::NOT_FOUND,
            json!({"resourceType": "OperationOutcome", "issue": [{"severity": "error", "code": "not-found", "diagnostics": "export job was not found"}]}),
        )),
        Err(err) => {
            tracing::error!(error = %err, "could not read export job");
            Err(fhir_json_public(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"resourceType": "OperationOutcome", "issue": [{"severity": "error", "code": "exception", "diagnostics": "export job could not be read"}]}),
            ))
        }
    }
}

/// `status` is `accepted`, `running`, `completed` or `error`. The table calls a failed job
/// `failed`; the API keeps the word it has always used for it.
fn status_body(job: &StoredJob) -> Value {
    let status = match job.status {
        JobStatus::Failed => "error",
        other => other.as_str(),
    };
    json!({
        "id": job.id,
        "status": status,
        "error": job.error,
        "kind": job.kind,
        "createdAt": rfc3339(job.created_ms),
        "completedAt": job.completed_ms.map(rfc3339),
    })
}

/// Unix milliseconds as an RFC 3339 UTC instant.
fn rfc3339(ms: u64) -> String {
    let secs = ms / 1000;
    let (year, month, day) = civil_from_days(secs / 86_400);
    let rest = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        ms % 1000
    )
}

/// Days since 1970-01-01 as a civil date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// Writes `{job id}.ndjson` under the export directory. It rewrites the file from scratch, so
/// running it again after an interruption is safe.
struct ExportHandler {
    db: Arc<Database>,
    catalog: Arc<std::sync::RwLock<Catalog>>,
    dir: PathBuf,
}

impl JobHandler for ExportHandler {
    fn run(&self, job: &StoredJob) -> Result<Value, String> {
        let type_name = job.request.get("type").and_then(Value::as_str);
        let id = job.request.get("id").and_then(Value::as_str);
        fs::create_dir_all(&self.dir).map_err(|err| err.to_string())?;
        let file = format!("{}.ndjson", job.id);
        // Each attempt writes a file of its own and moves it into place when it is whole, so an
        // attempt that lost its lease cannot interleave with the one that took over, and a
        // reader never sees a half-written export.
        let part = self.dir.join(format!("{}.{}.part", job.id, job.attempts));
        let written = write_export(&self.db, &self.catalog, type_name, id, &part)
            .and_then(|()| fs::rename(&part, self.dir.join(&file)).map_err(|err| err.to_string()));
        if written.is_err() {
            let _ = fs::remove_file(&part);
        }
        written?;
        Ok(json!({"file": file}))
    }
}

fn write_export(
    db: &Database,
    catalog: &std::sync::RwLock<Catalog>,
    type_name: Option<&str>,
    id: Option<&str>,
    path: &PathBuf,
) -> Result<(), String> {
    let snapshot = db.latest_ts();
    let end = snapshot
        .succ()
        .map_err(|_| "timestamp overflow".to_string())?;
    let catalog = catalog.read().expect("catalog");
    let mut seen = BTreeMap::new();
    // The log is read a slice of commits at a time rather than in one query: the store holds its
    // connection for the length of a read, and the lease heartbeat needs it between slices.
    let mut start = Timestamp::ZERO;
    while start < end {
        let stop = Timestamp::from_u64(start.as_u64().saturating_add(EXPORT_SLICE))
            .map_or(end, |next| next.min(end));
        let rows = db
            .reader()
            .load_documents(
                TimestampRange {
                    start_inclusive: start,
                    end_exclusive: stop,
                },
                Order::Asc,
            )
            .map_err(|err| err.to_string())?;
        for row in rows {
            let key = (row.id.tablet.as_str().to_string(), row.id.clone());
            if row.value.is_none() {
                seen.remove(&key);
                continue;
            }
            seen.insert(key, row.value.clone());
        }
        start = stop;
    }
    let members = group_members(&seen, type_name, id);
    let mut file = fs::File::create(path).map_err(|err| err.to_string())?;
    for ((tablet, doc_id), value) in &seen {
        let Some(text) = value.as_deref() else {
            continue;
        };
        if !export_keeps(
            &catalog,
            tablet,
            doc_id.id_str().unwrap_or(""),
            text,
            type_name,
            id,
            &members,
        ) {
            continue;
        }
        writeln!(file, "{text}").map_err(|err| err.to_string())?;
    }
    // The job is recorded as completed next; the file must not lag behind that.
    file.sync_all().map_err(|err| err.to_string())?;
    Ok(())
}

fn group_members(
    seen: &BTreeMap<(String, refhir_types::DocumentId), Option<String>>,
    type_name: Option<&str>,
    id: Option<&str>,
) -> Vec<String> {
    if type_name != Some("Group") {
        return Vec::new();
    }
    let Some(id) = id else {
        return Vec::new();
    };
    let Some(text) = seen.iter().find_map(|((tablet, doc_id), value)| {
        if tablet == "Group" && doc_id.id_str() == Some(id) {
            value.as_deref()
        } else {
            None
        }
    }) else {
        return Vec::new();
    };
    let Ok(group) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let mut patients = Vec::new();
    let Some(members) = group.get("member").and_then(|value| value.as_array()) else {
        return patients;
    };
    for member in members {
        let Some(reference) = member
            .pointer("/entity/reference")
            .and_then(|value| value.as_str())
        else {
            continue;
        };
        if let Some(patient) = reference
            .rsplit_once('/')
            .filter(|(type_name, _)| *type_name == "Patient" || type_name.ends_with("/Patient"))
            .map(|(_, id)| id.to_string())
        {
            patients.push(patient);
        }
    }
    patients
}

fn export_keeps(
    catalog: &Catalog,
    tablet: &str,
    id: &str,
    text: &str,
    type_name: Option<&str>,
    focus: Option<&str>,
    group_patients: &[String],
) -> bool {
    if catalog.sdc_archiver() && tablet == "QuestionnaireResponse" {
        return false;
    }
    let Some(type_name) = type_name else {
        return true;
    };
    if type_name == "Group" {
        if focus.is_none() {
            return tablet == "Group";
        }
        if tablet == "Group" && Some(id) == focus {
            return true;
        }
        return group_patients
            .iter()
            .any(|patient| patient_export(catalog, tablet, id, text, patient));
    }
    if type_name == "Patient" {
        let Some(patient) = focus else {
            return tablet == "Patient" || in_patient_compartment(catalog, tablet);
        };
        return patient_export(catalog, tablet, id, text, patient);
    }
    tablet == type_name
}

fn patient_export(catalog: &Catalog, tablet: &str, id: &str, text: &str, patient: &str) -> bool {
    if tablet == "Patient" {
        return id == patient;
    }
    if !in_patient_compartment(catalog, tablet) {
        return false;
    }
    let Ok(resource) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    let Some(compartment) = catalog.compartment("Patient") else {
        return false;
    };
    let Some(params) = compartment.params_for(tablet) else {
        return false;
    };
    params
        .iter()
        .any(|name| field_references(resource.get(name), patient))
}

fn in_patient_compartment(catalog: &Catalog, tablet: &str) -> bool {
    catalog
        .compartment("Patient")
        .is_some_and(|compartment| compartment.params_for(tablet).is_some())
}

fn field_references(value: Option<&Value>, patient: &str) -> bool {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .any(|item| field_references(Some(item), patient)),
        Some(Value::Object(map)) => map
            .get("reference")
            .and_then(|item| item.as_str())
            .is_some_and(|reference| {
                reference == format!("Patient/{patient}")
                    || reference.ends_with(&format!("/Patient/{patient}"))
            }),
        _ => false,
    }
}

fn deliver_subscriptions(relay: &Relay, ts: Timestamp) {
    let db = &*relay.db;
    let catalog = relay.catalog.read().expect("catalog");
    let Ok(_model) = catalog.get("Subscription") else {
        return;
    };
    let mut tx = db.begin();
    let Ok(bundle) = refhir_fhir::dispatch(&mut tx, &catalog, "Subscription/search", &json!({}))
    else {
        return;
    };
    let entries = bundle
        .get("entry")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    drop(tx);
    let end = ts.succ().unwrap_or(Timestamp::MAX_PERSISTED);
    let Ok(rows) = db.reader().load_documents(
        TimestampRange {
            start_inclusive: ts,
            end_exclusive: end,
        },
        Order::Asc,
    ) else {
        return;
    };
    // The rest-hook notifications to send. They go out once the catalog lock is let go: a dead
    // endpoint takes seconds to time out, and a writer waiting for the lock would hold every
    // reader up behind it.
    let mut deliveries = Vec::<(Arc<Target>, String)>::new();
    for entry in entries {
        let Some(subscription) = entry.get("resource") else {
            continue;
        };
        if subscription.get("status").and_then(|value| value.as_str()) == Some("off") {
            continue;
        }
        let criteria = subscription
            .get("criteria")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let channel = subscription
            .pointer("/channel/type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let endpoint = subscription
            .pointer("/channel/endpoint")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let Some((type_name, query)) = criteria.split_once('?').or(Some((criteria, ""))) else {
            continue;
        };
        // Made at the first match, so a commit that matches nothing costs nothing here.
        let mut hook: Option<Arc<Target>> = None;
        for row in &rows {
            if row.id.tablet.as_str() != type_name {
                continue;
            }
            let Some(text) = row.value.as_deref() else {
                continue;
            };
            let Ok(resource) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            if !criteria_matches(
                &catalog,
                db,
                type_name,
                query,
                row.id.id_str().unwrap_or(""),
            ) {
                continue;
            }
            let body = notification_body(type_name, resource, text);
            if channel == "rest-hook" {
                let target = hook
                    .get_or_insert_with(|| Arc::new(Target::of(subscription, endpoint)))
                    .clone();
                deliveries.push((target, body));
            } else if channel == "websocket" {
                let id = subscription
                    .get("id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                notify_socket(id, &body);
            }
        }
    }
    drop(catalog);
    // How each Subscription's delivery ended, where that is already known: recorded once all
    // are sent, and once for each.
    let mut outcomes = BTreeMap::<String, (Arc<Target>, Outcome)>::new();
    for (target, body) in deliveries {
        if let Some(outcome) = post_hook(relay, &target, &body) {
            outcomes
                .entry(target.id.clone())
                .or_insert((target, outcome));
        }
    }
    for (target, outcome) in outcomes.into_values() {
        relay.record(&target, outcome);
    }
}

/// The text a notification carries for `resource`, which is the stored text unless the resource
/// is a Subscription. Its `channel.header` entries are credentials for its own receiver, so they
/// are left out of what another Subscription's receiver is sent (a Subscription whose criteria
/// are `Subscription` is notified of every other one).
fn notification_body(type_name: &str, mut resource: Value, text: &str) -> String {
    if type_name != "Subscription" {
        return text.to_string();
    }
    if let Some(channel) = resource.get_mut("channel").and_then(Value::as_object_mut) {
        channel.remove("header");
    }
    resource.to_string()
}

fn criteria_matches(
    catalog: &Catalog,
    db: &Database,
    type_name: &str,
    query: &str,
    id: &str,
) -> bool {
    if query.is_empty() {
        return true;
    }
    if catalog.get(type_name).is_err() {
        return false;
    }
    let mut args = serde_json::Map::new();
    for (key, value) in jobs_query(query) {
        args.insert(key, Value::String(value));
    }
    let mut tx = db.begin();
    let Ok(bundle) = refhir_fhir::dispatch(
        &mut tx,
        catalog,
        &format!("{type_name}/search"),
        &Value::Object(args),
    ) else {
        return false;
    };
    bundle
        .get("entry")
        .and_then(|value| value.as_array())
        .is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry
                    .pointer("/resource/id")
                    .and_then(|value| value.as_str())
                    == Some(id)
            })
        })
}

fn socket_senders() -> &'static Mutex<BTreeMap<String, UnboundedSender<String>>> {
    static SOCKETS: std::sync::OnceLock<Mutex<BTreeMap<String, UnboundedSender<String>>>> =
        std::sync::OnceLock::new();
    SOCKETS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub fn register_socket(id: String) -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    socket_senders().lock().expect("sockets").insert(id, tx);
    rx
}

fn notify_socket(id: &str, payload: &str) {
    if let Some(sender) = socket_senders().lock().expect("sockets").get(id) {
        let _ = sender.send(payload.to_string());
    }
}

pub(crate) fn jobs_query(query: &str) -> BTreeMap<String, String> {
    let mut params = BTreeMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        refhir_fhir::add_param(&mut params, key.to_string(), value.to_string());
    }
    params
}

fn fetch_terminology(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &std::sync::RwLock<Catalog>,
    urls: &[String],
) {
    for url in urls {
        let Ok(body) = http_get(url) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&body) else {
            continue;
        };
        let Some(resource_url) = value.get("url").and_then(|item| item.as_str()) else {
            continue;
        };
        let Some(resource_type) = value.get("resourceType").and_then(|item| item.as_str()) else {
            continue;
        };
        let Ok(package_id) = store.ensure_local_package("4.0.1") else {
            continue;
        };
        let row = refhir_persistence::NewTerminology {
            resource_type: resource_type.to_string(),
            url: resource_url.to_string(),
            json: value.to_string(),
        };
        if store.upsert_terminology(package_id, &row).is_err() {
            continue;
        }
        if refhir_fhir::repair_conformance(store, db, catalog).is_err() {
            continue;
        }
    }
}

/// Sends before a rest-hook notification is given up on: this many in all, the first
/// [`HOOK_QUICK_ATTEMPTS`] at once and each of the rest when the retry queue next runs after the
/// notification is due.
const HOOK_ATTEMPTS: u8 = 8;

/// Attempts made straight away, a moment apart, before a failed notification is left to the queue.
const HOOK_QUICK_ATTEMPTS: u8 = 3;

/// The least time between two of the later attempts. The retry queue runs after every commit as
/// well as every two seconds, and without this a busy server would spend all of them in a moment
/// on an endpoint that has been down for a second.
const HOOK_RETRY_GAP: Duration = Duration::from_millis(1500);

/// The longest `Subscription.error` the server writes.
const ERROR_CHARS: usize = 500;

/// Headers a `channel.header` entry cannot set. The request's framing and connection handling are
/// the server's, and `Cookie` and `Proxy-*` carry an identity that is not the Subscription's to
/// assert.
const CONTROLLED_HEADERS: [&str; 10] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "expect",
    "cookie",
];

fn controlled_header(name: &str) -> bool {
    CONTROLLED_HEADERS
        .iter()
        .any(|controlled| name.eq_ignore_ascii_case(controlled))
        || name.to_ascii_lowercase().starts_with("proxy-")
}

/// What the delivery loop needs to send notifications and to record how they went.
struct Relay {
    db: Arc<Database>,
    catalog: Arc<std::sync::RwLock<Catalog>>,
    store: Arc<dyn ConformanceStore>,
    /// Notifications that failed and wait for another attempt. They are held in memory only: a
    /// restart loses them.
    pending: Mutex<Vec<PendingHook>>,
}

/// The Subscription a rest-hook notification is for, as it was when the notification was sent.
struct Target {
    id: String,
    status: Option<String>,
    endpoint: String,
    /// `channel.header` as (name, value), or why an entry cannot be sent. These are credentials:
    /// they are never logged, and never put in an error message.
    headers: Result<Vec<(String, String)>, String>,
}

impl Target {
    fn of(subscription: &Value, endpoint: &str) -> Target {
        Target {
            id: subscription
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            status: subscription
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
            endpoint: endpoint.to_string(),
            headers: channel_headers(subscription),
        }
    }
}

/// How a notification ended.
enum Outcome {
    Delivered,
    Failed(String),
}

/// A failure that is recorded as it is: stamped with the time.
fn failure(message: &str) -> Outcome {
    Outcome::Failed(format!("{}: {message}", rfc3339(now_ms())))
}

struct PendingHook {
    target: Arc<Target>,
    body: String,
    attempts: u8,
    /// Not sent again before this.
    due: Instant,
}

/// R4 `channel.header`: each entry is `Name: value`, and a name appears once.
fn channel_headers(subscription: &Value) -> Result<Vec<(String, String)>, String> {
    let Some(entries) = subscription
        .pointer("/channel/header")
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };
    let mut headers = Vec::<(String, String)>::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let number = index + 1;
        let invalid =
            || format!("channel.header entry {number} is not a valid \"Name: value\" HTTP header");
        let (name, value) = entry
            .as_str()
            .and_then(|line| line.split_once(':'))
            .ok_or_else(invalid)?;
        let (name, value) = (name.trim(), value.trim());
        if header::HeaderName::from_bytes(name.as_bytes()).is_err()
            || header::HeaderValue::from_str(value).is_err()
        {
            return Err(invalid());
        }
        if controlled_header(name) {
            return Err(format!(
                "channel.header entry {number} sets a header the server controls"
            ));
        }
        // The HTTP client treats names as case-sensitive, so a repeat would be sent twice or
        // would replace the first, depending on how it is spelt.
        if headers
            .iter()
            .any(|(seen, _)| seen.eq_ignore_ascii_case(name))
        {
            return Err(format!(
                "channel.header entry {number} repeats the name of an earlier entry"
            ));
        }
        headers.push((name.to_string(), value.to_string()));
    }
    Ok(headers)
}

impl Relay {
    /// The Subscription as it is stored now, or `None` if there is none.
    fn subscription(&self, id: &str) -> Option<Value> {
        refhir_fhir::commit_operation(
            self.store.as_ref(),
            &self.db,
            &self.catalog,
            "Subscription/read",
            &json!({"id": id}),
        )
        .ok()
        .map(|run| run.value)
    }

    /// `target` as its Subscription says it is now, for a notification that is being retried. It
    /// is `None` when the notification should not go out any more: the Subscription is gone, is
    /// `off`, or sends somewhere else than it did. A header that was rotated since is the one
    /// sent.
    fn refreshed(&self, target: &Target) -> Option<Arc<Target>> {
        let subscription = self.subscription(&target.id)?;
        let endpoint = subscription
            .pointer("/channel/endpoint")
            .and_then(Value::as_str)
            .unwrap_or("");
        let live = subscription.get("status").and_then(Value::as_str) != Some("off")
            && subscription
                .pointer("/channel/type")
                .and_then(Value::as_str)
                == Some("rest-hook")
            && endpoint == target.endpoint;
        live.then(|| Arc::new(Target::of(&subscription, endpoint)))
    }

    /// Write how deliveries are going into the Subscription, as R4 intends: `status` becomes
    /// `error` and `error` says why when a notification fails for good, and `status` returns to
    /// `active` when one gets through after that.
    ///
    /// What is stored now decides, not what it was when the notification was made. `off` is the
    /// client's to set and is never changed, and neither is a Subscription whose endpoint has
    /// moved. A write follows only a result that disagrees with the stored status, so an endpoint
    /// that stays down costs one write, not one for each notification, and a Subscription whose
    /// criteria match Subscriptions cannot keep waking itself. The patch tests the status and
    /// endpoint it was decided on, so an edit made in the moment between reading and writing wins
    /// and nothing is written.
    fn record(&self, target: &Target, outcome: Outcome) {
        // Nothing to undo for a delivery to a Subscription that was not in error when it was sent.
        if matches!(outcome, Outcome::Delivered) && target.status.as_deref() != Some("error") {
            return;
        }
        let Some(subscription) = self.subscription(&target.id) else {
            return;
        };
        let status = subscription.get("status").and_then(Value::as_str);
        let endpoint = subscription
            .pointer("/channel/endpoint")
            .and_then(Value::as_str)
            .unwrap_or("");
        if status == Some("off") || endpoint != target.endpoint {
            return;
        }
        let in_error = status == Some("error");
        let (new_status, change) = match outcome {
            Outcome::Failed(message) if !in_error => {
                let message: String = message.chars().take(ERROR_CHARS).collect();
                (
                    "error",
                    vec![
                        json!({"op": "add", "path": "/status", "value": "error"}),
                        json!({"op": "add", "path": "/error", "value": message}),
                    ],
                )
            }
            Outcome::Delivered if in_error => (
                "active",
                vec![
                    json!({"op": "add", "path": "/status", "value": "active"}),
                    json!({"op": "remove", "path": "/error"}),
                ],
            ),
            _ => return,
        };
        let mut patch = Vec::new();
        if let Some(status) = status {
            patch.push(json!({"op": "test", "path": "/status", "value": status}));
        }
        if !endpoint.is_empty() {
            patch.push(json!({"op": "test", "path": "/channel/endpoint", "value": endpoint}));
        }
        patch.extend(change);
        let args = json!({"id": target.id, "patch": patch});
        match refhir_fhir::commit_operation(
            self.store.as_ref(),
            &self.db,
            &self.catalog,
            "Subscription/patch",
            &args,
        ) {
            Ok(_) => {
                tracing::info!(subscription = %target.id, status = new_status, "subscription status changed")
            }
            // Edited, or gone, in the moment since it was read.
            Err(RunError::Fhir(FhirError::Conflict(_) | FhirError::NotFound(_))) => {
                tracing::debug!(subscription = %target.id, "subscription changed; status left as it is")
            }
            Err(err) => {
                tracing::warn!(subscription = %target.id, error = %err, "could not update subscription status")
            }
        }
    }
}

/// Send a notification, trying a few times a moment apart. If every try fails it is queued for
/// the retry queue and nothing is known yet; otherwise this says how it ended.
fn post_hook(relay: &Relay, target: &Arc<Target>, body: &str) -> Option<Outcome> {
    if target.endpoint.is_empty() {
        return Some(failure("the rest-hook channel has no endpoint"));
    }
    let headers = match &target.headers {
        Ok(headers) => headers,
        // The same entries would fail again.
        Err(message) => return Some(failure(message)),
    };
    for attempt in 1..=HOOK_QUICK_ATTEMPTS {
        if http_post(&target.endpoint, headers, body).is_ok() {
            return Some(Outcome::Delivered);
        }
        if attempt < HOOK_QUICK_ATTEMPTS {
            std::thread::sleep(Duration::from_millis(40 * u64::from(attempt)));
        }
    }
    relay.pending.lock().expect("hooks").push(PendingHook {
        target: target.clone(),
        body: body.to_string(),
        attempts: HOOK_QUICK_ATTEMPTS,
        due: Instant::now() + HOOK_RETRY_GAP,
    });
    None
}

fn drain_hooks(relay: &Relay) {
    let now = Instant::now();
    let batch = std::mem::take(&mut *relay.pending.lock().expect("hooks"));
    for mut hook in batch {
        if hook.due > now {
            relay.pending.lock().expect("hooks").push(hook);
            continue;
        }
        // The Subscription may have changed since this was queued; if it has gone, is `off` or
        // sends elsewhere, the notification ends here.
        let Some(target) = relay.refreshed(&hook.target) else {
            continue;
        };
        hook.target = target;
        let headers = match &hook.target.headers {
            Ok(headers) => headers,
            Err(message) => {
                relay.record(&hook.target, failure(message));
                continue;
            }
        };
        hook.attempts = hook.attempts.saturating_add(1);
        match http_post(&hook.target.endpoint, headers, &hook.body) {
            Ok(()) => relay.record(&hook.target, Outcome::Delivered),
            Err(error) if hook.attempts >= HOOK_ATTEMPTS => {
                tracing::warn!(subscription = %hook.target.id, attempts = hook.attempts, "rest-hook notification given up on");
                relay.record(
                    &hook.target,
                    failure(&format!(
                        "notification not delivered after {} attempts: {error}",
                        hook.attempts
                    )),
                );
            }
            Err(_) => {
                hook.due = Instant::now() + HOOK_RETRY_GAP;
                relay.pending.lock().expect("hooks").push(hook);
            }
        }
    }
}

const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

fn agent(redirects: u32) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(HTTP_TIMEOUT)
        .redirects(redirects)
        .build()
}

fn http_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| agent(5))
}

/// The agent for notifications. It does not follow redirects: a notification that is redirected
/// has not been delivered, a 301, 302 or 303 would turn the POST into a GET the receiver never
/// sees as one, and the headers (which can hold a credential) must not follow it to another host.
fn hook_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| agent(0))
}

fn http_post(url: &str, headers: &[(String, String)], body: &str) -> Result<(), String> {
    let mut request = hook_agent().post(url);
    // The client compares header names case-sensitively, so a `content-type` entry would not
    // replace this one and both would be sent.
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
    {
        request = request.set("Content-Type", "application/fhir+json");
    }
    for (name, value) in headers {
        request = request.set(name, value);
    }
    let response = request.send_string(body).map_err(|err| {
        // The text of this one repeats the header line, and the line is a credential.
        if err.kind() == ureq::ErrorKind::BadHeader {
            "a channel.header entry is not a valid HTTP header".to_string()
        } else {
            err.to_string()
        }
    })?;
    if (200..300).contains(&response.status()) {
        Ok(())
    } else {
        Err(format!("hook status {}", response.status()))
    }
}

fn http_get(url: &str) -> Result<String, String> {
    http_agent()
        .get(url)
        .call()
        .map_err(|err| err.to_string())?
        .into_string()
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn https_endpoints_are_attempted() {
        let err = http_post("https://127.0.0.1:1/hook", &[], "{}").unwrap_err();
        assert!(!err.contains("only http"), "{err}");
    }

    fn headers_of(entries: Value) -> Result<Vec<(String, String)>, String> {
        channel_headers(&json!({"channel": {"type": "rest-hook", "header": entries}}))
    }

    #[test]
    fn channel_headers_are_name_colon_value() {
        assert_eq!(channel_headers(&json!({"channel": {}})), Ok(Vec::new()));
        assert_eq!(
            headers_of(json!(["Authorization: Bearer a:b", "X-Tenant:acme"])),
            Ok(vec![
                ("Authorization".to_string(), "Bearer a:b".to_string()),
                ("X-Tenant".to_string(), "acme".to_string()),
            ])
        );
        for bad in [
            "no colon",
            ": value",
            "Bad Name: value",
            "X-A: line\r\nX-B: smuggled",
        ] {
            let err = headers_of(json!(["X-Ok: 1", bad])).unwrap_err();
            assert!(err.contains("entry 2"), "{bad}: {err}");
            // The entry is a credential, so what is wrong with it is said without repeating it.
            assert!(!err.contains("smuggled"), "{bad}: {err}");
        }
        // The request's framing, its connection handling and a cookie or proxy identity are not
        // the Subscription's to set.
        for controlled in [
            "Host: elsewhere",
            "content-length: 0",
            "Transfer-Encoding: chunked",
            "Keep-Alive: timeout=5",
            "Upgrade: websocket",
            "TE: trailers",
            "Trailer: Expires",
            "Expect: 100-continue",
            "Cookie: session=1",
            "Proxy-Authorization: Basic abc",
        ] {
            assert!(
                headers_of(json!([controlled]))
                    .unwrap_err()
                    .contains("controls"),
                "{controlled}"
            );
        }
        // A name is given once, whatever its case: the client would send both or keep one
        // depending on how it is spelt.
        for repeated in ["x-tenant: other", "X-TENANT: other", "X-Tenant: acme"] {
            let err = headers_of(json!(["X-Tenant: acme", repeated])).unwrap_err();
            assert!(err.contains("entry 2") && err.contains("repeats"), "{err}");
        }
    }

    #[test]
    fn a_redirected_notification_is_not_delivered() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            // Answers every connection, so a client that followed the redirect would find one.
            while let Ok((mut stream, _)) = listener.accept() {
                stream
                    .set_read_timeout(Some(Duration::from_millis(200)))
                    .ok();
                // The headers and the body can arrive apart. Reading until the client has nothing
                // more to say keeps the close from resetting the connection under its answer.
                let mut buffer = [0_u8; 2048];
                while matches!(std::io::Read::read(&mut stream, &mut buffer), Ok(n) if n > 0) {}
                let _ = stream.write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                let _ = stream.flush();
            }
        });
        let headers = [("X-Api-Key".to_string(), "secret".to_string())];
        let err = http_post(&url, &headers, "{}").unwrap_err();
        assert!(err.contains("302"), "{err}");
    }

    #[test]
    fn instants_are_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(rfc3339(4_102_444_799_999), "2099-12-31T23:59:59.999Z");
    }

    /// Poll the store until the job has finished.
    async fn settled(store: &dyn JobStore, id: &str) -> StoredJob {
        for _ in 0..400 {
            let job = store.job_get(id).unwrap().expect("job exists");
            if job.status.finished() {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job {id} never finished");
    }

    const SHORT: Duration = Duration::from_millis(150);

    /// What a server that died mid-job leaves: running, held by an owner whose lease has run out.
    fn left_by_a_dead_server(store: &dyn JobStore, id: &str, kind: &str, starts: u32) {
        let lease = JobLease {
            owner: "dead-server",
            ms: 1,
        };
        store
            .job_create(id, kind, &json!({}), 1, Some(lease))
            .unwrap();
        for _ in 0..starts {
            store.job_start(id, "dead-server", 1, 1).unwrap();
        }
    }

    #[tokio::test]
    async fn a_job_goes_to_the_handler_for_its_kind() {
        let store = Arc::new(MemoryJobStore::default());
        let mut runner = JobRunner::new(store.clone());
        runner.register(
            "echo",
            Arc::new(|job: &StoredJob| Ok(json!({"saw": job.request.clone()}))),
        );
        runner.register(
            "refuse",
            Arc::new(|_: &StoredJob| Err("no thanks".to_string())),
        );
        runner.register(
            "explode",
            Arc::new(|_: &StoredJob| -> Result<Value, String> { panic!("a bug in a handler") }),
        );
        let runner = Arc::new(runner);

        let echo = runner.submit("echo", json!({"n": 1})).await.unwrap();
        // Persisted before submit returns, whatever the handler goes on to do.
        assert!(store.job_get(&echo).unwrap().is_some());
        let done = settled(store.as_ref(), &echo).await;
        assert_eq!(done.status, JobStatus::Completed);
        assert_eq!(done.result, Some(json!({"saw": {"n": 1}})));
        assert_eq!(done.attempts, 1);
        assert_eq!(
            done.lease_owner, None,
            "the lease is let go when the work ends"
        );

        let refused = runner.submit("refuse", json!({})).await.unwrap();
        let done = settled(store.as_ref(), &refused).await;
        assert_eq!(done.status, JobStatus::Failed);
        assert_eq!(done.error.as_deref(), Some("no thanks"));

        let exploded = runner.submit("explode", json!({})).await.unwrap();
        let done = settled(store.as_ref(), &exploded).await;
        assert_eq!(done.status, JobStatus::Failed);
        assert_eq!(done.error.as_deref(), Some("job handler panicked"));

        // A kind this server cannot run is refused up front, not left to fail later.
        let err = runner
            .submit("nobody-handles-this", json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nobody-handles-this"), "{err}");
    }

    #[tokio::test]
    async fn a_restart_runs_what_the_dead_server_left_and_nothing_else() {
        let store = Arc::new(MemoryJobStore::default());
        let runs = Arc::new(AtomicUsize::new(0));
        let mut runner = JobRunner::with_lease(store.clone(), SHORT);
        let counted = runs.clone();
        runner.register(
            "count",
            Arc::new(move |job: &StoredJob| {
                counted.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
                Ok(json!({"attempts_seen": job.attempts}))
            }),
        );
        let runner = Arc::new(runner);

        left_by_a_dead_server(store.as_ref(), "cut-off", "count", 1);
        store
            .job_create("queued", "count", &json!({}), 3, None)
            .unwrap();
        let holder = JobLease {
            owner: "finished-by",
            ms: 1000,
        };
        store
            .job_create("done", "count", &json!({}), 4, Some(holder))
            .unwrap();
        store
            .job_finish(
                "done",
                "finished-by",
                JobStatus::Completed,
                Some(&json!("kept")),
                None,
                6,
            )
            .unwrap();

        // A job being run by this process is not picked up a second time by the sweep.
        let live = runner.submit("count", json!({})).await.unwrap();
        runner.recover();
        for id in ["cut-off", "queued", live.as_str()] {
            let job = settled(store.as_ref(), id).await;
            assert_eq!(job.status, JobStatus::Completed, "{id}");
        }
        assert_eq!(
            runs.load(Ordering::SeqCst),
            3,
            "cut-off, the waiting one and the live one"
        );
        assert_eq!(
            store.job_get("cut-off").unwrap().unwrap().result,
            Some(json!({"attempts_seen": 2})),
            "the handler sees that this is the second attempt"
        );
        assert_eq!(
            store.job_get("done").unwrap().unwrap().result,
            Some(json!("kept"))
        );
    }

    #[tokio::test]
    async fn two_servers_sharing_a_table_run_a_job_once() {
        let store = Arc::new(MemoryJobStore::default());
        let runs = Arc::new(AtomicUsize::new(0));
        let server = |store: &Arc<MemoryJobStore>| {
            let mut runner = JobRunner::with_lease(store.clone(), SHORT);
            let counted = runs.clone();
            runner.register(
                "count",
                Arc::new(move |_: &StoredJob| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(400));
                    Ok(json!({}))
                }),
            );
            Arc::new(runner)
        };
        let (a, b) = (server(&store), server(&store));
        assert_ne!(a.owner(), b.owner());
        for n in 0..6 {
            store
                .job_create(&format!("job-{n}"), "count", &json!({}), n, None)
                .unwrap();
        }
        a.recover();
        b.recover();
        for n in 0..6 {
            settled(store.as_ref(), &format!("job-{n}")).await;
        }
        assert_eq!(
            runs.load(Ordering::SeqCst),
            6,
            "each job ran once, between the two servers"
        );
        for n in 0..6 {
            assert_eq!(
                store
                    .job_get(&format!("job-{n}"))
                    .unwrap()
                    .unwrap()
                    .attempts,
                1
            );
        }
    }

    #[tokio::test]
    async fn a_long_job_keeps_its_lease_and_is_not_taken_by_another_server() {
        let store = Arc::new(MemoryJobStore::default());
        let runs = Arc::new(AtomicUsize::new(0));
        let server = |store: &Arc<MemoryJobStore>| {
            let mut runner = JobRunner::with_lease(store.clone(), SHORT);
            let counted = runs.clone();
            runner.register(
                "slow",
                Arc::new(move |_: &StoredJob| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    // Five times the lease: only renewing keeps it.
                    std::thread::sleep(SHORT * 5);
                    Ok(json!({}))
                }),
            );
            Arc::new(runner)
        };
        let (a, b) = (server(&store), server(&store));
        b.recover();
        let id = a.submit("slow", json!({})).await.unwrap();
        let done = settled(store.as_ref(), &id).await;
        assert_eq!(done.status, JobStatus::Completed);
        assert_eq!(
            done.attempts, 1,
            "never taken from the server that was working on it"
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_job_held_by_a_dead_server_is_taken_only_after_its_lease_runs_out() {
        let store = Arc::new(MemoryJobStore::default());
        let runs = Arc::new(AtomicUsize::new(0));
        let mut runner = JobRunner::with_lease(store.clone(), SHORT);
        let counted = runs.clone();
        runner.register(
            "count",
            Arc::new(move |_: &StoredJob| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(json!({}))
            }),
        );
        let runner = Arc::new(runner);
        // Held by a server that has not renewed, for another 400 ms.
        let lease = JobLease {
            owner: "dead-server",
            ms: 400,
        };
        store
            .job_create("held", "count", &json!({}), now_ms(), Some(lease))
            .unwrap();
        store
            .job_start("held", "dead-server", now_ms(), 400)
            .unwrap();
        runner.recover();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 0, "still held");
        let done = settled(store.as_ref(), "held").await;
        assert_eq!((done.status, done.attempts), (JobStatus::Completed, 2));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_server_that_lost_its_lease_does_not_overwrite_the_new_holders_result() {
        let store = Arc::new(MemoryJobStore::default());
        let mut runner = JobRunner::with_lease(store.clone(), SHORT);
        runner.register(
            "slow",
            Arc::new(|_: &StoredJob| {
                std::thread::sleep(SHORT * 4);
                Ok(json!({"from": "the server that lost it"}))
            }),
        );
        let runner = Arc::new(runner);
        let id = runner.submit("slow", json!({})).await.unwrap();
        // Another server decides this one is gone, as it would if the renewals stopped getting
        // through, and takes the job.
        let far_future = now_ms() + 60_000;
        let taken = store
            .job_claim("thief", &["slow"], far_future, 60_000, 1)
            .unwrap();
        assert_eq!(taken.len(), 1, "an unrenewed lease can be taken");
        // The first server finishes its work and tries to record it.
        tokio::time::sleep(SHORT * 6).await;
        let row = store.job_get(&id).unwrap().unwrap();
        assert_eq!(
            (row.status, row.result, row.lease_owner.as_deref()),
            (JobStatus::Running, None, Some("thief")),
            "the loser's result was not recorded"
        );
        assert!(store
            .job_finish(
                &id,
                "thief",
                JobStatus::Completed,
                Some(&json!({"from": "thief"})),
                None,
                far_future
            )
            .unwrap());
        assert_eq!(
            store.job_get(&id).unwrap().unwrap().result,
            Some(json!({"from": "thief"}))
        );
    }

    #[tokio::test]
    async fn a_job_that_keeps_being_cut_off_is_given_up_on() {
        let store = Arc::new(MemoryJobStore::default());
        let mut runner = JobRunner::with_lease(store.clone(), SHORT);
        runner.register("count", Arc::new(|_: &StoredJob| Ok(json!({}))));
        let runner = Arc::new(runner);
        left_by_a_dead_server(store.as_ref(), "stuck", "count", MAX_ATTEMPTS);
        runner.recover();
        let done = settled(store.as_ref(), "stuck").await;
        assert_eq!(done.status, JobStatus::Failed);
        assert!(done.error.unwrap().contains("gave up"));
    }

    #[tokio::test]
    async fn a_kind_this_server_does_not_handle_is_left_for_one_that_does() {
        let store = Arc::new(MemoryJobStore::default());
        let mut old = JobRunner::with_lease(store.clone(), SHORT);
        old.register("export", Arc::new(|_: &StoredJob| Ok(json!({}))));
        let old = Arc::new(old);
        store
            .job_create("new-kind", "from-the-future", &json!({}), 1, None)
            .unwrap();
        old.recover();
        tokio::time::sleep(SHORT).await;
        let row = store.job_get("new-kind").unwrap().unwrap();
        assert_eq!(
            (row.status, row.attempts, row.lease_owner),
            (JobStatus::Accepted, 0, None),
            "an older server does not fail a job it has never heard of"
        );
        let mut newer = JobRunner::with_lease(store.clone(), SHORT);
        newer.register(
            "from-the-future",
            Arc::new(|_: &StoredJob| Ok(json!({"ok": true}))),
        );
        let newer = Arc::new(newer);
        newer.recover();
        let done = settled(store.as_ref(), "new-kind").await;
        assert_eq!(done.status, JobStatus::Completed);
    }

    #[tokio::test]
    async fn a_dropped_runner_stops_sweeping() {
        let store = Arc::new(MemoryJobStore::default());
        let mut runner = JobRunner::with_lease(store.clone(), SHORT);
        runner.register("export", Arc::new(|_: &StoredJob| Ok(json!({}))));
        let runner = Arc::new(runner);
        runner.recover();
        drop(runner);
        tokio::time::sleep(SHORT).await;
        store
            .job_create("after-the-drop", "export", &json!({}), 1, None)
            .unwrap();
        tokio::time::sleep(SHORT * 2).await;
        let row = store.job_get("after-the-drop").unwrap().unwrap();
        assert_eq!(
            (row.status, row.attempts),
            (JobStatus::Accepted, 0),
            "nobody is left to take it"
        );
    }
}
