//! Snapshot, transaction, and one committer.
//!
//! Convex `crates/database/src/committer.rs`:
//! `next_commit_ts`, `commit_has_conflict`, `write_to_persistence`, `publish_commit`.
//! Read-only transactions skip persistence (`Committer::go` when `is_readonly`).
//! An uncertain persist exits the process. Convex:
//! "Write failed. Unsure if transaction committed to disk."

mod reads;
mod snapshot_manager;
mod subscription;
mod transaction;
mod write_log;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use refhir_persistence::{
    ConflictStrategy, PersistError, Persistence, PersistenceGlobalKey, PersistenceReader,
    PoolStats, TimestampRange,
};
use refhir_types::{IndexId, Timestamp};
use tokio::sync::mpsc::UnboundedSender;

const ENGINE: &str = "refhir_engine";

/// The writer's published watermark, if it has recorded one.
fn read_published(persistence: &dyn Persistence) -> Result<Option<Timestamp>, PersistError> {
    let value = persistence.get_persistence_global(PersistenceGlobalKey::PublishedTimestamp)?;
    let Some(value) = value else {
        return Ok(None);
    };
    let number = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .ok_or_else(|| PersistError::Definite(format!("bad published timestamp {value}")))?;
    Timestamp::from_u64(number)
        .map(Some)
        .map_err(|_| PersistError::Definite(format!("bad published timestamp {number}")))
}

fn write_keys_summary(keys: &[(IndexId, Vec<u8>)], limit: usize) -> String {
    keys.iter()
        .take(limit)
        .map(|(index, key)| format!("{}:{}", index.as_str(), String::from_utf8_lossy(key)))
        .collect::<Vec<_>>()
        .join(", ")
}

pub use reads::ReadSet;
pub use subscription::{Invalidation, SubToken};
pub use transaction::{
    Delete, DocumentWrite, IndexPut, Put, Revision, Scanned, ScannedEntry, ScannedRow, ScannedText,
    Stored, Transaction, WrittenId,
};

/// OCC conflict. Convex `ConflictingRead` reduced to the index key and write ts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OccConflict {
    pub index_id: IndexId,
    pub key: Vec<u8>,
    pub write_ts: Timestamp,
}

#[derive(Debug)]
pub enum CommitError {
    Occ(OccConflict),
    Persist(String),
    Invalid(String),
    /// This database is a reader in a multi-process deployment. The write belongs on the
    /// primary.
    ReadOnly,
}

impl CommitError {
    pub fn is_occ(&self) -> bool {
        matches!(self, CommitError::Occ(_))
    }
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommitError::Occ(conflict) => write!(
                f,
                "occ conflict on {} at {}",
                conflict.index_id, conflict.write_ts
            ),
            CommitError::Persist(message) => write!(f, "{message}"),
            CommitError::Invalid(message) => write!(f, "{message}"),
            CommitError::ReadOnly => {
                write!(
                    f,
                    "this server is a read-only replica; send writes to the primary"
                )
            }
        }
    }
}

impl std::error::Error for CommitError {}

#[derive(Debug)]
pub enum SubscribeError {
    Stale,
    Gone,
}

impl std::fmt::Display for SubscribeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubscribeError::Stale => write!(f, "subscription read set is stale"),
            SubscribeError::Gone => write!(f, "subscription is closed"),
        }
    }
}

impl std::error::Error for SubscribeError {}

thread_local! {
    static UNCERTAIN_HOOK: RefCell<Option<fn(String)>> = RefCell::new(None);
}

/// Test harness stand-in for `process::exit` after an uncertain persist.
#[cfg(test)]
pub(crate) fn set_uncertain_hook(hook: Option<fn(String)>) {
    UNCERTAIN_HOOK.with(|slot| *slot.borrow_mut() = hook);
}

fn uncertain_persist(message: String) -> ! {
    let hook = UNCERTAIN_HOOK.with(|slot| *slot.borrow());
    if let Some(hook) = hook {
        hook(message.clone());
    }
    eprintln!("refhir: uncertain persistence write, exiting: {message}");
    std::process::exit(70);
}

/// A newer writer holds the lease, so this process is a stale primary. It must not write
/// again, and it cannot tell what the new primary has done since, so it exits.
fn fenced_persist(message: String) -> ! {
    let hook = UNCERTAIN_HOOK.with(|slot| *slot.borrow());
    if let Some(hook) = hook {
        hook(message.clone());
    }
    eprintln!("refhir: fenced by a newer writer, exiting: {message}");
    std::process::exit(71);
}

struct Committer {
    snapshot: snapshot_manager::SnapshotManager,
    log: write_log::WriteLog,
    pending: write_log::PendingWrites,
    last_assigned_ts: Timestamp,
    subscriptions: subscription::SubscriptionManager,
    occ_faults: usize,
}

impl Committer {
    fn new(latest: Timestamp) -> Self {
        Self {
            snapshot: snapshot_manager::SnapshotManager::new(latest),
            log: write_log::WriteLog::new(latest),
            pending: write_log::PendingWrites::default(),
            last_assigned_ts: latest,
            subscriptions: subscription::SubscriptionManager::new(),
            occ_faults: 0,
        }
    }

    /// Convex `Committer::next_commit_ts`.
    fn next_commit_ts(&mut self) -> Result<Timestamp, CommitError> {
        let latest = self.snapshot.latest_ts();
        let pending = self.pending.max_ts().unwrap_or(latest);
        let base = latest.max(pending).max(self.last_assigned_ts);
        let next = base
            .succ()
            .map_err(|_| CommitError::Invalid("commit timestamp overflow".into()))?;
        self.last_assigned_ts = next;
        Ok(next)
    }

    /// Convex `validate_commit` / `commit_has_conflict`.
    fn validate_commit(
        &mut self,
        reads: &ReadSet,
        begin_ts: Timestamp,
        keys: &[(IndexId, Vec<u8>)],
    ) -> Result<Timestamp, CommitError> {
        if self.occ_faults > 0 {
            self.occ_faults -= 1;
            tracing::debug!(target: ENGINE, begin_ts = begin_ts.as_u64(), "occ fault injected (test)");
            return Err(CommitError::Occ(OccConflict {
                index_id: IndexId::new("occ-fault"),
                key: Vec::new(),
                write_ts: self.snapshot.latest_ts(),
            }));
        }
        let commit_ts = self.next_commit_ts()?;
        if let Some(conflict) = self.log.is_stale(reads, begin_ts, commit_ts) {
            tracing::debug!(
                target: ENGINE,
                begin_ts = begin_ts.as_u64(),
                commit_ts = commit_ts.as_u64(),
                index = conflict.index_id.as_str(),
                write_ts = conflict.write_ts.as_u64(),
                reads = reads.summary(),
                "occ conflict (published write log)"
            );
            return Err(CommitError::Occ(conflict));
        }
        if let Some(conflict) = self.pending.is_stale(reads) {
            tracing::debug!(
                target: ENGINE,
                begin_ts = begin_ts.as_u64(),
                commit_ts = commit_ts.as_u64(),
                index = conflict.index_id.as_str(),
                write_ts = conflict.write_ts.as_u64(),
                reads = reads.summary(),
                "occ conflict (pending commit)"
            );
            return Err(CommitError::Occ(conflict));
        }
        self.pending.push(commit_ts, keys.to_vec());
        Ok(commit_ts)
    }

    fn reads_are_stale(&self, reads: &ReadSet, ts: Timestamp) -> bool {
        if ts > self.snapshot.latest_ts() {
            return true;
        }
        self.log
            .is_stale(reads, ts, self.snapshot.latest_ts())
            .is_some()
            || self.pending.is_stale(reads).is_some()
    }
}

/// Where time goes in a commit and in the connection pool. Cumulative since start.
#[derive(Clone, Copy, Debug, Default)]
pub struct DatabaseStats {
    pub commits: u64,
    pub occ_conflicts: u64,
    /// Waiting for the committer lock, then validating and assigning a timestamp.
    pub validate_ns: u64,
    /// The persistence write: waiting for a connection plus the SQL transaction.
    pub persist_ns: u64,
    /// Waiting for earlier commits to publish before this one may.
    pub publish_wait_ns: u64,
    /// Writing the published-up-to watermark.
    pub watermark_ns: u64,
    /// Refreshes of a read-only database that moved its snapshot forward.
    pub refreshes: u64,
    pub pool: Option<PoolStats>,
}

#[derive(Default)]
struct Counters {
    commits: AtomicU64,
    occ_conflicts: AtomicU64,
    validate_ns: AtomicU64,
    persist_ns: AtomicU64,
    publish_wait_ns: AtomicU64,
    watermark_ns: AtomicU64,
    refreshes: AtomicU64,
}

fn add_ns(counter: &AtomicU64, started: Instant) {
    counter.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Owns the committer: assigns timestamps, persists, publishes.
    Writer,
    /// Follows the published watermark of a writer in another process.
    Reader,
}

/// Removes a validated commit from the pending list if its thread leaves before publishing.
/// Without it a panic between validation and publish would leave a pending entry that no
/// one ever removes, and every later publish would wait for it forever.
struct PendingGuard<'a> {
    db: &'a Database,
    ts: Timestamp,
    armed: bool,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // The lock may be poisoned by the very panic that brought us here.
        let mut state = match self.db.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.pending.remove(self.ts);
        self.db.cv.notify_all();
    }
}

pub struct Database {
    persistence: Arc<dyn Persistence>,
    reader: Arc<dyn PersistenceReader>,
    state: Mutex<Committer>,
    cv: Condvar,
    commit_listeners: Mutex<Vec<UnboundedSender<Timestamp>>>,
    mode: Mode,
    counters: Counters,
    /// Highest timestamp that wants to be recorded as published.
    published_target: AtomicU64,
    /// Highest timestamp actually recorded. The lock makes concurrent publishers share one
    /// write: whoever gets it records the newest target, the rest find it already covered.
    published_written: Mutex<u64>,
    /// Serializes `refresh` so two threads never apply the same commits twice.
    refresh_lock: Mutex<()>,
    /// Woken when the snapshot moves, for `wait_for_ts`.
    advanced: (Mutex<()>, Condvar),
    /// When a reader last confirmed it was in step with the writer's watermark.
    refreshed_at: Mutex<Option<Instant>>,
}

impl Database {
    /// Opens the writer: the one process that assigns timestamps. Records the current
    /// timestamp as published, since everything already in the log is.
    pub fn open(persistence: Arc<dyn Persistence>) -> Result<Self, PersistError> {
        let latest = persistence.max_ts()?.unwrap_or(Timestamp::ZERO);
        persistence.advance_published_ts(latest)?;
        let mut db = Self::new(persistence, latest, Mode::Writer);
        *db.published_written.get_mut().expect("published") = latest.as_u64();
        db.published_target = AtomicU64::new(latest.as_u64());
        Ok(db)
    }

    /// Opens a reader for a database another process writes. It has no committer: its
    /// snapshot is the writer's published watermark, moved forward by `refresh`, and a
    /// transaction with writes fails with `CommitError::ReadOnly`.
    pub fn open_read_only(persistence: Arc<dyn Persistence>) -> Result<Self, PersistError> {
        let published = read_published(persistence.as_ref())?.ok_or_else(|| {
            PersistError::Definite(
                "the writer has not published a timestamp yet; start the primary first".into(),
            )
        })?;
        Ok(Self::new(persistence, published, Mode::Reader))
    }

    fn new(persistence: Arc<dyn Persistence>, latest: Timestamp, mode: Mode) -> Self {
        let reader = persistence.reader();
        Self {
            persistence,
            reader,
            state: Mutex::new(Committer::new(latest)),
            cv: Condvar::new(),
            commit_listeners: Mutex::new(Vec::new()),
            mode,
            counters: Counters::default(),
            published_target: AtomicU64::new(0),
            published_written: Mutex::new(0),
            refresh_lock: Mutex::new(()),
            advanced: (Mutex::new(()), Condvar::new()),
            refreshed_at: Mutex::new((mode == Mode::Reader).then(Instant::now)),
        }
    }

    /// Reader side: how long since the last successful `refresh`, whether or not it moved
    /// the snapshot. `None` for a writer.
    pub fn refreshed_ago(&self) -> Option<Duration> {
        self.refreshed_at
            .lock()
            .expect("refreshed")
            .map(|at| at.elapsed())
    }

    /// Reader side: how many timestamps the writer's watermark is ahead of this snapshot.
    pub fn published_lag(&self) -> Result<Option<u64>, PersistError> {
        let Some(published) = read_published(self.persistence.as_ref())? else {
            return Ok(None);
        };
        Ok(Some(
            published.as_u64().saturating_sub(self.latest_ts().as_u64()),
        ))
    }

    pub fn is_read_only(&self) -> bool {
        self.mode == Mode::Reader
    }

    pub fn latest_ts(&self) -> Timestamp {
        self.state.lock().expect("committer").snapshot.latest_ts()
    }

    pub fn reader(&self) -> Arc<dyn PersistenceReader> {
        self.reader.clone()
    }

    pub fn stats(&self) -> DatabaseStats {
        let c = &self.counters;
        DatabaseStats {
            commits: c.commits.load(Ordering::Relaxed),
            occ_conflicts: c.occ_conflicts.load(Ordering::Relaxed),
            validate_ns: c.validate_ns.load(Ordering::Relaxed),
            persist_ns: c.persist_ns.load(Ordering::Relaxed),
            publish_wait_ns: c.publish_wait_ns.load(Ordering::Relaxed),
            watermark_ns: c.watermark_ns.load(Ordering::Relaxed),
            refreshes: c.refreshes.load(Ordering::Relaxed),
            pool: self.persistence.pool_stats(),
        }
    }

    pub fn begin(&self) -> Transaction {
        let state = self.state.lock().expect("committer");
        let snapshot_ts = state.snapshot.latest_ts();
        tracing::trace!(
            target: ENGINE,
            snapshot_ts = snapshot_ts.as_u64(),
            "transaction opened at snapshot"
        );
        Transaction::new(snapshot_ts, self.reader.clone())
    }

    pub fn commit(&self, tx: Transaction) -> Result<Timestamp, CommitError> {
        self.commit_with(tx, |_json, _ts| {})
    }

    /// Assigns a timestamp, checks OCC, persists, then publishes.
    /// `stamp` rewrites buffered JSON once `commit_ts` is known and before the log write.
    pub fn commit_with(
        &self,
        mut tx: Transaction,
        mut stamp: impl FnMut(&mut String, Timestamp),
    ) -> Result<Timestamp, CommitError> {
        if !tx.has_writes() {
            tracing::debug!(
                target: ENGINE,
                snapshot_ts = tx.begin_timestamp().as_u64(),
                reads = tx.read_set().summary(),
                "read-only transaction finished (no publish)"
            );
            return Ok(tx.begin_timestamp());
        }
        if self.mode == Mode::Reader {
            return Err(CommitError::ReadOnly);
        }
        let keys = tx.occ_keys();
        let begin_ts = tx.begin_timestamp();
        let validating = Instant::now();
        let commit_ts = {
            let mut state = self.state.lock().expect("committer");
            match state.validate_commit(tx.read_set(), begin_ts, &keys) {
                Ok(ts) => ts,
                Err(err) => {
                    if err.is_occ() {
                        self.counters.occ_conflicts.fetch_add(1, Ordering::Relaxed);
                    }
                    return Err(err);
                }
            }
        };
        add_ns(&self.counters.validate_ns, validating);
        let mut pending = PendingGuard {
            db: self,
            ts: commit_ts,
            armed: true,
        };
        tracing::debug!(
            target: ENGINE,
            begin_ts = begin_ts.as_u64(),
            commit_ts = commit_ts.as_u64(),
            write_keys = keys.len(),
            keys = write_keys_summary(&keys, 12),
            reads = tx.read_set().summary(),
            "commit validated, persisting"
        );
        tx.stamp_new_json(commit_ts, &mut stamp);
        let (documents, indexes) = tx.log_entries(commit_ts);
        let persisting = Instant::now();
        let written = self
            .persistence
            .write(&documents, &indexes, ConflictStrategy::Error);
        add_ns(&self.counters.persist_ns, persisting);
        match written {
            Ok(()) => {}
            // The guard removes the pending entry and wakes the publishers.
            Err(PersistError::Definite(message)) => return Err(CommitError::Persist(message)),
            Err(PersistError::Uncertain(message)) => uncertain_persist(message),
            Err(PersistError::Fenced(message)) => fenced_persist(message),
        }
        tracing::debug!(
            target: ENGINE,
            commit_ts = commit_ts.as_u64(),
            documents = documents.len(),
            index_rows = indexes.len(),
            "persisted, publishing"
        );
        // `publish_commit` takes over the pending entry.
        pending.armed = false;
        self.publish_commit(commit_ts, keys);
        self.counters.commits.fetch_add(1, Ordering::Relaxed);
        Ok(commit_ts)
    }

    /// Convex `publish_commit`: append the write log, push the snapshot, wake subscribers.
    /// Waits until this timestamp is the oldest pending commit so publishes stay ordered.
    fn publish_commit(&self, ts: Timestamp, keys: Vec<(IndexId, Vec<u8>)>) {
        let waiting = Instant::now();
        let mut state = self.state.lock().expect("committer");
        while state.pending.front_ts() != Some(ts) {
            state = self.cv.wait(state).expect("committer wait");
        }
        add_ns(&self.counters.publish_wait_ns, waiting);
        state.pending.pop_front();
        state.log.append(ts, &keys);
        state.snapshot.push(ts);
        state.subscriptions.invalidate(&keys, ts);
        tracing::debug!(
            target: ENGINE,
            commit_ts = ts.as_u64(),
            latest_ts = state.snapshot.latest_ts().as_u64(),
            write_keys = keys.len(),
            "commit published, snapshot advanced"
        );
        self.cv.notify_all();
        drop(state);
        // Recorded after the in-order pop, so the watermark never covers a timestamp whose
        // predecessor is still pending. Everything at or below it is already persisted.
        self.record_published(ts);
        self.notify_advanced();
        let listeners = self
            .commit_listeners
            .lock()
            .expect("commit listeners")
            .clone();
        for listener in listeners {
            let _ = listener.send(ts);
        }
    }

    /// Writes the published watermark. Concurrent publishers share one write.
    fn record_published(&self, ts: Timestamp) {
        let started = Instant::now();
        self.published_target
            .fetch_max(ts.as_u64(), Ordering::SeqCst);
        let mut written = self.published_written.lock().expect("published");
        if *written >= ts.as_u64() {
            return;
        }
        let target = self.published_target.load(Ordering::SeqCst);
        let result = Timestamp::from_u64(target)
            .map_err(|_| PersistError::Definite(format!("bad timestamp {target}")))
            .and_then(|target| self.persistence.advance_published_ts(target));
        match result {
            Ok(()) => *written = target,
            // A reader falls behind until the next commit. The commit itself is durable.
            Err(err) => tracing::warn!(
                target: ENGINE,
                "could not record the published watermark {target}: {err}"
            ),
        }
        add_ns(&self.counters.watermark_ns, started);
    }

    fn notify_advanced(&self) {
        let _guard = self.advanced.0.lock().expect("advanced");
        self.advanced.1.notify_all();
    }

    /// Reader side: move the snapshot up to the writer's published watermark, applying what
    /// each new commit wrote to the write log and to live-query subscriptions, in order.
    /// Returns the snapshot timestamp afterwards.
    pub fn refresh(&self) -> Result<Timestamp, PersistError> {
        let ts = self.refresh_inner()?;
        *self.refreshed_at.lock().expect("refreshed") = Some(Instant::now());
        Ok(ts)
    }

    fn refresh_inner(&self) -> Result<Timestamp, PersistError> {
        let _one_at_a_time = self.refresh_lock.lock().expect("refresh");
        let Some(target) = read_published(self.persistence.as_ref())? else {
            return Ok(self.latest_ts());
        };
        let from = self.latest_ts();
        if target <= from {
            return Ok(from);
        }
        let start = from
            .succ()
            .map_err(|_| PersistError::Definite("timestamp overflow".into()))?;
        let end = target
            .succ()
            .map_err(|_| PersistError::Definite("timestamp overflow".into()))?;
        let rows = self.reader.load_index_writes(TimestampRange {
            start_inclusive: start,
            end_exclusive: end,
        })?;
        let mut by_ts: BTreeMap<Timestamp, Vec<(IndexId, Vec<u8>)>> = BTreeMap::new();
        for row in rows {
            by_ts
                .entry(row.ts)
                .or_default()
                .push((row.index_id, row.key));
        }
        let mut moved = false;
        for (ts, keys) in by_ts {
            moved |= self.apply_external_commit(ts, &keys);
        }
        // A timestamp with no index rows still moves the snapshot.
        moved |= self.apply_external_commit(target, &[]);
        if moved {
            self.counters.refreshes.fetch_add(1, Ordering::Relaxed);
        }
        Ok(self.latest_ts())
    }

    /// Reader side: apply one commit another process made. Appends what it wrote to the write
    /// log, moves the snapshot to `ts`, invalidates the live queries that read those keys, and
    /// wakes `on_commit` listeners and `wait_for_ts` callers, in the same way publishing a local
    /// commit does. `ts` must be newer than the snapshot, and commits must be applied in order;
    /// an older one is ignored. Returns whether the snapshot moved. A writer never takes commits
    /// from elsewhere, so on one this does nothing.
    pub fn apply_external_commit(&self, ts: Timestamp, keys: &[(IndexId, Vec<u8>)]) -> bool {
        if self.mode != Mode::Reader {
            return false;
        }
        {
            let mut state = self.state.lock().expect("committer");
            if ts <= state.snapshot.latest_ts() {
                return false;
            }
            state.log.append(ts, keys);
            state.snapshot.push(ts);
            state.subscriptions.invalidate(keys, ts);
        }
        self.notify_advanced();
        let listeners = self
            .commit_listeners
            .lock()
            .expect("commit listeners")
            .clone();
        for listener in &listeners {
            let _ = listener.send(ts);
        }
        true
    }

    /// Reader side: keep `refresh`ing in the background until the database is dropped.
    pub fn start_refresher(self: &Arc<Self>, interval: Duration) {
        let weak: Weak<Database> = Arc::downgrade(self);
        // Held by the thread itself, so waiting does not keep the database alive.
        let persistence = self.persistence.clone();
        let spawned = std::thread::Builder::new()
            .name("refhir-refresh".into())
            .spawn(move || loop {
                // Wakes as soon as the writer announces a publish where the backend can say so,
                // and otherwise after `interval`.
                persistence.wait_for_publish(interval);
                let Some(db) = weak.upgrade() else {
                    return;
                };
                if let Err(err) = db.refresh() {
                    tracing::warn!(target: ENGINE, "refresh failed: {err}");
                }
            });
        if let Err(err) = spawned {
            tracing::error!(target: ENGINE, "could not start the refresh thread: {err}");
        }
    }

    /// Waits until the snapshot has reached `ts`. False if `timeout` passes first.
    pub fn wait_for_ts(&self, ts: Timestamp, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut guard = self.advanced.0.lock().expect("advanced");
        loop {
            if self.latest_ts() >= ts {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            // A short slice, so a refresh that lands between the check and the wait is
            // noticed without relying on the wakeup.
            let slice = (deadline - now).min(Duration::from_millis(20));
            guard = self
                .advanced
                .1
                .wait_timeout(guard, slice)
                .expect("advanced wait")
                .0;
        }
    }

    /// Wakes `listener` after a commit is durable and live queries have been invalidated.
    pub fn on_commit(&self, listener: UnboundedSender<Timestamp>) {
        self.commit_listeners
            .lock()
            .expect("commit listeners")
            .push(listener);
    }

    pub fn subscribe(
        &self,
        reads: ReadSet,
        ts: Timestamp,
        query_id: u64,
        tx: UnboundedSender<Invalidation>,
    ) -> Result<SubToken, SubscribeError> {
        let mut state = self.state.lock().expect("committer");
        if state.reads_are_stale(&reads, ts) {
            tracing::debug!(
                target: ENGINE,
                query_id,
                snapshot_ts = ts.as_u64(),
                latest_ts = state.snapshot.latest_ts().as_u64(),
                "subscribe rejected: read set stale"
            );
            return Err(SubscribeError::Stale);
        }
        Ok(state.subscriptions.insert(query_id, reads, ts, tx))
    }

    pub fn refresh_subscription(
        &self,
        token: SubToken,
        reads: ReadSet,
        ts: Timestamp,
    ) -> Result<(), SubscribeError> {
        let mut state = self.state.lock().expect("committer");
        if !state.subscriptions.contains(token) {
            return Err(SubscribeError::Gone);
        }
        if state.reads_are_stale(&reads, ts) {
            tracing::debug!(
                target: ENGINE,
                sub_token = token.0,
                snapshot_ts = ts.as_u64(),
                latest_ts = state.snapshot.latest_ts().as_u64(),
                "subscription refresh rejected: read set stale"
            );
            return Err(SubscribeError::Stale);
        }
        if state.subscriptions.refresh(token, reads, ts) {
            Ok(())
        } else {
            Err(SubscribeError::Gone)
        }
    }

    pub fn unsubscribe(&self, token: SubToken) {
        self.state
            .lock()
            .expect("committer")
            .subscriptions
            .remove(token);
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn testing_fail_next_commits(&self, n: usize) {
        self.state.lock().expect("committer").occ_faults = n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use refhir_persistence::{
        ConflictStrategy, DocumentLogEntry, IndexScanRow, PersistError, Persistence,
        PersistenceGlobalKey, PersistenceIndexEntry, PersistenceReader, TimestampRange,
    };
    use refhir_sqlite::SqlitePersistence;
    use refhir_types::{exact_key, DocumentId, IndexId, Interval, Order, TabletId};
    use serde_json::json;

    fn memory_db() -> Database {
        Database::open(Arc::new(SqlitePersistence::open_in_memory().unwrap())).unwrap()
    }

    fn tablet() -> TabletId {
        TabletId::new("t")
    }

    fn by_id() -> IndexId {
        IndexId::new("t/by_id")
    }

    fn put_doc(tx: &mut Transaction, id: &str, body: &str, prev: Option<Timestamp>) {
        let key = (by_id(), exact_key(id));
        tx.stage_put(Put {
            tablet: tablet(),
            id: id.into(),
            json: json!({"id": id, "body": body}),
            new_keys: vec![key.clone()],
            old_keys: if prev.is_some() { vec![key] } else { vec![] },
            prev_ts: prev,
        })
        .unwrap();
    }

    #[test]
    fn readonly_commit_does_not_publish() {
        let db = memory_db();
        let mut tx = db.begin();
        assert!(tx.get(&tablet(), "a", &by_id()).unwrap().is_none());
        let begin = tx.begin_timestamp();
        assert_eq!(db.commit(tx).unwrap(), begin);
        assert_eq!(db.latest_ts(), Timestamp::ZERO);
    }

    #[test]
    fn commit_is_visible_only_at_the_new_timestamp() {
        let db = memory_db();
        let mut tx = db.begin();
        put_doc(&mut tx, "a", "one", None);
        let ts = db.commit(tx).unwrap();
        assert_eq!(ts.as_u64(), 1);
        assert_eq!(db.latest_ts(), ts);
        let rows_before = db
            .reader()
            .index_scan(&by_id(), Timestamp::ZERO, &Interval::all(), Order::Asc)
            .unwrap();
        assert!(rows_before.is_empty());
        let rows_after = db
            .reader()
            .index_scan(&by_id(), ts, &Interval::all(), Order::Asc)
            .unwrap();
        assert_eq!(rows_after.len(), 1);
        let mut snapshot = db.begin();
        let stored = snapshot.get(&tablet(), "a", &by_id()).unwrap().unwrap();
        assert_eq!(stored.ts, Some(ts));
        assert_eq!(stored.json["body"], "one");
    }

    #[test]
    fn overlapping_read_sets_abort_the_second_writer() {
        let db = memory_db();
        let mut seed = db.begin();
        put_doc(&mut seed, "a", "v1", None);
        db.commit(seed).unwrap();

        let mut first = db.begin();
        let current = first.get(&tablet(), "a", &by_id()).unwrap().unwrap();
        put_doc(&mut first, "a", "from-first", current.ts);

        let mut second = db.begin();
        let current = second.get(&tablet(), "a", &by_id()).unwrap().unwrap();
        put_doc(&mut second, "a", "from-second", current.ts);

        db.commit(first).unwrap();
        let err = db.commit(second).unwrap_err();
        assert!(err.is_occ(), "{err}");
    }

    #[test]
    fn disjoint_writes_both_commit() {
        let db = memory_db();
        let mut first = db.begin();
        put_doc(&mut first, "a", "a", None);
        let mut second = db.begin();
        put_doc(&mut second, "b", "b", None);
        db.commit(first).unwrap();
        db.commit(second).unwrap();
        let mut tx = db.begin();
        assert!(tx.get(&tablet(), "a", &by_id()).unwrap().is_some());
        assert!(tx.get(&tablet(), "b", &by_id()).unwrap().is_some());
    }

    #[test]
    fn pending_write_conflicts_before_publish() {
        let entered = Arc::new(Gate::new());
        let release = Arc::new(Gate::new());
        let _release_on_drop = SignalOnDrop(release.clone());
        let persistence = Arc::new(GatedPersistence {
            inner: SqlitePersistence::open_in_memory().unwrap(),
            writes: AtomicUsize::new(0),
            block_from: 1,
            block_until: usize::MAX,
            entered: entered.clone(),
            release,
        });
        let db = Arc::new(Database::open(persistence).unwrap());
        let mut seed = db.begin();
        put_doc(&mut seed, "a", "v1", None);
        db.commit(seed).unwrap();

        let mut blocked = db.begin();
        let current = blocked.get(&tablet(), "a", &by_id()).unwrap().unwrap();
        put_doc(&mut blocked, "a", "blocked", current.ts);
        let db_thread = db.clone();
        let handle = std::thread::spawn(move || db_thread.commit(blocked));
        assert!(
            entered.wait_timeout(Duration::from_secs(5)),
            "commit did not reach persistence"
        );

        let mut other = db.begin();
        let current = other.get(&tablet(), "a", &by_id()).unwrap().unwrap();
        put_doc(&mut other, "a", "other", current.ts);
        let err = db.commit(other).unwrap_err();
        assert!(err.is_occ(), "{err}");
        assert_eq!(db.latest_ts().as_u64(), 1);

        _release_on_drop.0.signal();
        let committed = handle.join().expect("thread").expect("commit");
        assert_eq!(committed.as_u64(), 2);
        assert_eq!(db.latest_ts(), committed);
    }

    #[test]
    fn uncertain_persist_does_not_publish() {
        let persistence = Arc::new(UncertainPersistence {
            inner: SqlitePersistence::open_in_memory().unwrap(),
        });
        let db = Database::open(persistence.clone()).unwrap();
        let mut tx = db.begin();
        put_doc(&mut tx, "a", "nope", None);
        set_uncertain_hook(Some(|message| panic!("uncertain:{message}")));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| db.commit(tx)));
        set_uncertain_hook(None);
        assert!(result.is_err());
        assert_eq!(db.latest_ts(), Timestamp::ZERO);
        assert!(persistence
            .latest_document(
                &DocumentId::new(tablet(), b"a".as_slice()),
                Timestamp::from_u64(10).unwrap(),
            )
            .unwrap()
            .is_none());
    }

    #[test]
    fn subscription_fires_when_write_overlaps() {
        let db = memory_db();
        let mut seed = db.begin();
        put_doc(&mut seed, "a", "v1", None);
        db.commit(seed).unwrap();

        let mut tx = db.begin();
        tx.get(&tablet(), "a", &by_id()).unwrap().unwrap();
        let reads = tx.read_set().clone();
        let ts = tx.begin_timestamp();
        db.commit(tx).unwrap();

        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        db.subscribe(reads, ts, 7, sender).unwrap();

        let mut quiet = db.begin();
        put_doc(&mut quiet, "b", "other", None);
        db.commit(quiet).unwrap();
        assert!(receiver.try_recv().is_err());

        let mut update = db.begin();
        let current = update.get(&tablet(), "a", &by_id()).unwrap().unwrap();
        put_doc(&mut update, "a", "v2", current.ts);
        db.commit(update).unwrap();
        let event = receiver.try_recv().expect("invalidation");
        assert_eq!(event.query_id, 7);
    }

    fn published(db: &Database) -> Option<u64> {
        db.reader()
            .get_persistence_global(PersistenceGlobalKey::PublishedTimestamp)
            .unwrap()
            .and_then(|value| value.as_u64())
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "refhir-db-{tag}-{}-{}.sqlite",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ))
    }

    fn remove_db(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm", ".lock"] {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            let _ = std::fs::remove_file(name);
        }
    }

    #[test]
    fn the_watermark_never_covers_a_timestamp_with_a_pending_predecessor() {
        let entered = Arc::new(Gate::new());
        let release = Arc::new(Gate::new());
        let _release_on_drop = SignalOnDrop(release.clone());
        // Write 0 is the seed. Write 1 stalls in persistence while write 2 goes through.
        let persistence = Arc::new(GatedPersistence {
            inner: SqlitePersistence::open_in_memory().unwrap(),
            writes: AtomicUsize::new(0),
            block_from: 1,
            block_until: 2,
            entered: entered.clone(),
            release,
        });
        let db = Arc::new(Database::open(persistence).unwrap());
        assert_eq!(
            published(&db),
            Some(0),
            "a new database starts published at 0"
        );
        let mut seed = db.begin();
        put_doc(&mut seed, "seed", "s", None);
        let seeded = db.commit(seed).unwrap();
        assert_eq!(published(&db), Some(seeded.as_u64()));

        // Takes the next timestamp, then stalls.
        let mut slow = db.begin();
        put_doc(&mut slow, "slow", "s", None);
        let slow_db = db.clone();
        let slow_handle = std::thread::spawn(move || slow_db.commit(slow));
        assert!(entered.wait_timeout(Duration::from_secs(5)));

        // A later commit persists, but cannot publish ahead of the one still pending.
        let mut quick = db.begin();
        put_doc(&mut quick, "quick", "q", None);
        let quick_db = db.clone();
        let quick_handle = std::thread::spawn(move || quick_db.commit(quick));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            published(&db),
            Some(seeded.as_u64()),
            "the watermark must stay behind the pending commit"
        );
        assert_eq!(db.latest_ts(), seeded);

        _release_on_drop.0.signal();
        let slow_ts = slow_handle.join().unwrap().unwrap();
        let quick_ts = quick_handle.join().unwrap().unwrap();
        assert!(slow_ts < quick_ts);
        assert_eq!(published(&db), Some(quick_ts.as_u64()));
        assert_eq!(db.latest_ts(), quick_ts);
    }

    #[test]
    fn concurrent_commits_leave_the_watermark_at_the_newest_timestamp() {
        let path = temp_path("concurrent");
        let db =
            Arc::new(Database::open(Arc::new(SqlitePersistence::open(&path).unwrap())).unwrap());
        let handles: Vec<_> = (0..8)
            .map(|worker| {
                let db = db.clone();
                std::thread::spawn(move || {
                    for n in 0..10 {
                        let mut tx = db.begin();
                        put_doc(&mut tx, &format!("w{worker}-{n}"), "x", None);
                        db.commit(tx).unwrap();
                        // Never ahead of what readers may use.
                        assert!(published(&db).unwrap() <= db.latest_ts().as_u64());
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(db.latest_ts().as_u64(), 80);
        assert_eq!(published(&db), Some(80));
        assert_eq!(db.stats().commits, 80);
        remove_db(&path);
    }

    #[test]
    fn a_reader_follows_the_writer_after_a_refresh() {
        let path = temp_path("follow");
        let writer = Database::open(Arc::new(SqlitePersistence::open(&path).unwrap())).unwrap();
        let reader =
            Database::open_read_only(Arc::new(SqlitePersistence::open(&path).unwrap())).unwrap();
        assert!(reader.is_read_only() && !writer.is_read_only());

        let mut tx = writer.begin();
        put_doc(&mut tx, "a", "one", None);
        let ts = writer.commit(tx).unwrap();

        // Nothing moves until the reader refreshes.
        assert_eq!(reader.latest_ts(), Timestamp::ZERO);
        assert!(reader
            .begin()
            .get(&tablet(), "a", &by_id())
            .unwrap()
            .is_none());
        assert_eq!(reader.refresh().unwrap(), ts);
        let stored = reader
            .begin()
            .get(&tablet(), "a", &by_id())
            .unwrap()
            .unwrap();
        assert_eq!(stored.json["body"], "one");
        assert_eq!(reader.published_lag().unwrap(), Some(0));
        assert!(reader.refreshed_ago().is_some());

        // A write on the reader is refused; a read-only transaction is fine.
        let mut tx = reader.begin();
        put_doc(&mut tx, "b", "two", None);
        assert!(matches!(reader.commit(tx), Err(CommitError::ReadOnly)));
        assert!(reader.commit(reader.begin()).is_ok());
        remove_db(&path);
    }

    #[test]
    fn a_reader_needs_a_published_timestamp() {
        let path = temp_path("unpublished");
        // Created and written, but never opened by a writer.
        drop(SqlitePersistence::open(&path).unwrap());
        let err = Database::open_read_only(Arc::new(SqlitePersistence::open(&path).unwrap()))
            .err()
            .expect("no watermark yet");
        assert!(err.to_string().contains("start the primary"), "{err}");
        remove_db(&path);
    }

    #[test]
    fn a_reader_invalidates_live_queries_when_the_writer_commits() {
        let path = temp_path("invalidate");
        let writer = Database::open(Arc::new(SqlitePersistence::open(&path).unwrap())).unwrap();
        let reader =
            Database::open_read_only(Arc::new(SqlitePersistence::open(&path).unwrap())).unwrap();

        let mut seed = writer.begin();
        put_doc(&mut seed, "a", "v1", None);
        writer.commit(seed).unwrap();
        reader.refresh().unwrap();

        let mut query = reader.begin();
        query.get(&tablet(), "a", &by_id()).unwrap().unwrap();
        let reads = query.read_set().clone();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        reader
            .subscribe(reads, query.begin_timestamp(), 11, sender)
            .unwrap();

        // An unrelated commit leaves the query alone.
        let mut quiet = writer.begin();
        put_doc(&mut quiet, "b", "other", None);
        writer.commit(quiet).unwrap();
        reader.refresh().unwrap();
        assert!(receiver.try_recv().is_err());

        // A commit to what the query read invalidates it, with that commit's timestamp.
        let mut update = writer.begin();
        let current = update.get(&tablet(), "a", &by_id()).unwrap().unwrap();
        put_doc(&mut update, "a", "v2", current.ts);
        let ts = writer.commit(update).unwrap();
        reader.refresh().unwrap();
        let event = receiver.try_recv().expect("invalidation");
        assert_eq!((event.query_id, event.write_ts), (11, ts));
        remove_db(&path);
    }

    #[test]
    fn without_a_notification_channel_a_reader_waits_for_its_timer() {
        // The control for the Postgres notification test: SQLite cannot announce a publish, so a
        // reader with a long timer does not catch up in the time a notification would take.
        let path = temp_path("no-notify");
        let writer = Database::open(Arc::new(SqlitePersistence::open(&path).unwrap())).unwrap();
        let store = Arc::new(SqlitePersistence::open(&path).unwrap());
        assert!(!store.publishes_notifications());
        let reader = Arc::new(Database::open_read_only(store).unwrap());
        reader.start_refresher(Duration::from_secs(30));
        let mut tx = writer.begin();
        put_doc(&mut tx, "a", "one", None);
        let ts = writer.commit(tx).unwrap();
        assert!(
            !reader.wait_for_ts(ts, Duration::from_millis(600)),
            "nothing woke the reader, so it must still be waiting for its timer"
        );
        // Refreshing by hand shows the data was there all along.
        reader.refresh().unwrap();
        assert!(reader.latest_ts() >= ts);
        remove_db(&path);
    }

    #[test]
    fn an_external_commit_can_be_applied_directly_and_only_in_order() {
        let path = temp_path("external");
        let writer = Database::open(Arc::new(SqlitePersistence::open(&path).unwrap())).unwrap();
        let reader =
            Database::open_read_only(Arc::new(SqlitePersistence::open(&path).unwrap())).unwrap();
        let key = (by_id(), exact_key("a"));
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut seed = reader.begin();
        seed.get(&tablet(), "a", &by_id()).unwrap();
        reader
            .subscribe(seed.read_set().clone(), seed.begin_timestamp(), 5, sender)
            .unwrap();
        let (commits, mut seen) = tokio::sync::mpsc::unbounded_channel();
        reader.on_commit(commits);

        let ts = Timestamp::from_u64(1).unwrap();
        assert!(reader.apply_external_commit(ts, &[key.clone()]));
        assert_eq!(reader.latest_ts(), ts);
        assert_eq!(receiver.try_recv().expect("invalidation").write_ts, ts);
        assert_eq!(seen.try_recv().expect("commit listener"), ts);
        // An older or repeated commit changes nothing.
        assert!(!reader.apply_external_commit(ts, &[key.clone()]));
        assert!(receiver.try_recv().is_err());
        // A writer takes no commits from elsewhere.
        assert!(!writer.apply_external_commit(Timestamp::from_u64(9).unwrap(), &[key]));
        assert_eq!(writer.latest_ts(), Timestamp::ZERO);
        remove_db(&path);
    }

    #[test]
    fn a_reader_applies_index_only_commits() {
        let path = temp_path("index-only");
        let writer = Database::open(Arc::new(SqlitePersistence::open(&path).unwrap())).unwrap();
        let reader =
            Database::open_read_only(Arc::new(SqlitePersistence::open(&path).unwrap())).unwrap();
        let mut seed = writer.begin();
        put_doc(&mut seed, "a", "v1", None);
        writer.commit(seed).unwrap();
        reader.refresh().unwrap();

        let mut query = reader.begin();
        query.get(&tablet(), "a", &by_id()).unwrap().unwrap();
        let reads = query.read_set().clone();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        reader
            .subscribe(reads, query.begin_timestamp(), 3, sender)
            .unwrap();

        // A commit that only rewrites the index entry for `a`, with no document row.
        let mut tx = writer.begin();
        tx.stage_index(IndexPut {
            tablet: tablet(),
            id: "a".into(),
            keys: vec![(by_id(), exact_key("a"))],
            old_keys: vec![(by_id(), exact_key("a"))],
        })
        .unwrap();
        let ts = writer.commit(tx).unwrap();
        reader.refresh().unwrap();
        assert_eq!(reader.latest_ts(), ts);
        let event = receiver
            .try_recv()
            .expect("invalidation from an index-only commit");
        assert_eq!(event.write_ts, ts);
        remove_db(&path);
    }

    #[test]
    fn wait_for_ts_returns_once_the_refresher_catches_up() {
        let path = temp_path("wait");
        let writer = Database::open(Arc::new(SqlitePersistence::open(&path).unwrap())).unwrap();
        let reader = Arc::new(
            Database::open_read_only(Arc::new(SqlitePersistence::open(&path).unwrap())).unwrap(),
        );
        reader.start_refresher(Duration::from_millis(10));
        let mut tx = writer.begin();
        put_doc(&mut tx, "a", "one", None);
        let ts = writer.commit(tx).unwrap();
        assert!(reader.wait_for_ts(ts, Duration::from_secs(5)));
        assert!(reader.latest_ts() >= ts);
        // A timestamp nobody has published yet times out.
        let future = Timestamp::from_u64(ts.as_u64() + 100).unwrap();
        assert!(!reader.wait_for_ts(future, Duration::from_millis(100)));
        remove_db(&path);
    }

    #[test]
    fn a_panic_after_validation_does_not_wedge_later_commits() {
        let db = Arc::new(memory_db());
        let mut doomed = db.begin();
        put_doc(&mut doomed, "a", "boom", None);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = db.commit_with(doomed, |_json, _ts| panic!("stamp failed"));
        }));
        assert!(panicked.is_err());

        // The abandoned timestamp left a pending entry. If it were still there, this commit
        // would wait for it forever.
        let (done, finished) = std::sync::mpsc::channel();
        let worker = db.clone();
        std::thread::spawn(move || {
            let mut tx = worker.begin();
            put_doc(&mut tx, "b", "fine", None);
            let _ = done.send(worker.commit(tx));
        });
        let ts = finished
            .recv_timeout(Duration::from_secs(5))
            .expect("commit is stuck behind an abandoned pending entry")
            .unwrap();
        assert_eq!(db.latest_ts(), ts);
    }

    #[test]
    fn stats_count_commits_and_report_the_pool() {
        let db = memory_db();
        let mut tx = db.begin();
        put_doc(&mut tx, "a", "one", None);
        db.commit(tx).unwrap();
        let stats = db.stats();
        assert_eq!(stats.commits, 1);
        assert_eq!(stats.occ_conflicts, 0);
        assert!(stats.pool.is_some());
    }

    #[test]
    fn elapsed_time_is_added_to_a_counter() {
        // A start time in the past, so the answer does not depend on the clock's resolution
        // or on how fast the work being timed happens to be.
        let counter = AtomicU64::new(0);
        add_ns(&counter, Instant::now() - Duration::from_millis(5));
        add_ns(&counter, Instant::now() - Duration::from_millis(5));
        assert!(counter.load(Ordering::Relaxed) >= 10_000_000);
    }

    struct SignalOnDrop(Arc<Gate>);

    impl Drop for SignalOnDrop {
        fn drop(&mut self) {
            self.0.signal();
        }
    }

    struct Gate {
        ready: Mutex<bool>,
        cv: Condvar,
    }

    impl Gate {
        fn new() -> Self {
            Self {
                ready: Mutex::new(false),
                cv: Condvar::new(),
            }
        }

        fn signal(&self) {
            let mut ready = self.ready.lock().expect("gate");
            *ready = true;
            self.cv.notify_all();
        }

        fn wait_timeout(&self, dur: Duration) -> bool {
            let mut ready = self.ready.lock().expect("gate");
            let start = std::time::Instant::now();
            while !*ready {
                let left = dur.saturating_sub(start.elapsed());
                if left.is_zero() {
                    return false;
                }
                let (guard, timeout) = self.cv.wait_timeout(ready, left).expect("gate");
                ready = guard;
                if timeout.timed_out() && !*ready {
                    return false;
                }
            }
            true
        }
    }

    struct GatedPersistence {
        inner: SqlitePersistence,
        writes: AtomicUsize,
        block_from: usize,
        /// Writes numbered `block_from..block_until` wait for `release`.
        block_until: usize,
        entered: Arc<Gate>,
        release: Arc<Gate>,
    }

    impl PersistenceReader for GatedPersistence {
        fn load_documents(
            &self,
            range: TimestampRange,
            order: Order,
        ) -> Result<Vec<DocumentLogEntry>, PersistError> {
            self.inner.load_documents(range, order)
        }
        fn latest_document(
            &self,
            id: &DocumentId,
            snapshot: Timestamp,
        ) -> Result<Option<DocumentLogEntry>, PersistError> {
            self.inner.latest_document(id, snapshot)
        }
        fn previous_revision(
            &self,
            id: &DocumentId,
            at: Timestamp,
        ) -> Result<Option<DocumentLogEntry>, PersistError> {
            self.inner.previous_revision(id, at)
        }
        fn exact_revision(
            &self,
            id: &DocumentId,
            ts: Timestamp,
        ) -> Result<Option<DocumentLogEntry>, PersistError> {
            self.inner.exact_revision(id, ts)
        }
        fn document_history(
            &self,
            id: &DocumentId,
            snapshot: Timestamp,
        ) -> Result<Vec<DocumentLogEntry>, PersistError> {
            self.inner.document_history(id, snapshot)
        }
        fn index_scan(
            &self,
            index_id: &IndexId,
            read_timestamp: Timestamp,
            interval: &Interval,
            order: Order,
        ) -> Result<Vec<IndexScanRow>, PersistError> {
            self.inner
                .index_scan(index_id, read_timestamp, interval, order)
        }
        fn get_persistence_global(
            &self,
            key: PersistenceGlobalKey,
        ) -> Result<Option<serde_json::Value>, PersistError> {
            self.inner.get_persistence_global(key)
        }
        fn max_ts(&self) -> Result<Option<Timestamp>, PersistError> {
            self.inner.max_ts()
        }
        fn load_index_writes(
            &self,
            range: TimestampRange,
        ) -> Result<Vec<PersistenceIndexEntry>, PersistError> {
            self.inner.load_index_writes(range)
        }
    }

    impl Persistence for GatedPersistence {
        fn is_fresh(&self) -> bool {
            self.inner.is_fresh()
        }
        fn reader(&self) -> Arc<dyn PersistenceReader> {
            self.inner.reader()
        }
        fn write(
            &self,
            documents: &[DocumentLogEntry],
            indexes: &[PersistenceIndexEntry],
            conflict_strategy: ConflictStrategy,
        ) -> Result<(), PersistError> {
            let n = self.writes.fetch_add(1, Ordering::SeqCst);
            if n >= self.block_from && n < self.block_until {
                self.entered.signal();
                if !self.release.wait_timeout(Duration::from_secs(5)) {
                    return Err(PersistError::Definite("gate timeout".into()));
                }
            }
            self.inner.write(documents, indexes, conflict_strategy)
        }
        fn write_persistence_global(
            &self,
            key: PersistenceGlobalKey,
            value: &serde_json::Value,
        ) -> Result<(), PersistError> {
            self.inner.write_persistence_global(key, value)
        }
        fn advance_published_ts(&self, ts: Timestamp) -> Result<(), PersistError> {
            self.inner.advance_published_ts(ts)
        }
    }

    struct UncertainPersistence {
        inner: SqlitePersistence,
    }

    impl PersistenceReader for UncertainPersistence {
        fn load_documents(
            &self,
            range: TimestampRange,
            order: Order,
        ) -> Result<Vec<DocumentLogEntry>, PersistError> {
            self.inner.load_documents(range, order)
        }
        fn latest_document(
            &self,
            id: &DocumentId,
            snapshot: Timestamp,
        ) -> Result<Option<DocumentLogEntry>, PersistError> {
            self.inner.latest_document(id, snapshot)
        }
        fn previous_revision(
            &self,
            id: &DocumentId,
            at: Timestamp,
        ) -> Result<Option<DocumentLogEntry>, PersistError> {
            self.inner.previous_revision(id, at)
        }
        fn exact_revision(
            &self,
            id: &DocumentId,
            ts: Timestamp,
        ) -> Result<Option<DocumentLogEntry>, PersistError> {
            self.inner.exact_revision(id, ts)
        }
        fn document_history(
            &self,
            id: &DocumentId,
            snapshot: Timestamp,
        ) -> Result<Vec<DocumentLogEntry>, PersistError> {
            self.inner.document_history(id, snapshot)
        }
        fn index_scan(
            &self,
            index_id: &IndexId,
            read_timestamp: Timestamp,
            interval: &Interval,
            order: Order,
        ) -> Result<Vec<IndexScanRow>, PersistError> {
            self.inner
                .index_scan(index_id, read_timestamp, interval, order)
        }
        fn get_persistence_global(
            &self,
            key: PersistenceGlobalKey,
        ) -> Result<Option<serde_json::Value>, PersistError> {
            self.inner.get_persistence_global(key)
        }
        fn max_ts(&self) -> Result<Option<Timestamp>, PersistError> {
            self.inner.max_ts()
        }
        fn load_index_writes(
            &self,
            range: TimestampRange,
        ) -> Result<Vec<PersistenceIndexEntry>, PersistError> {
            self.inner.load_index_writes(range)
        }
    }

    impl Persistence for UncertainPersistence {
        fn is_fresh(&self) -> bool {
            self.inner.is_fresh()
        }
        fn reader(&self) -> Arc<dyn PersistenceReader> {
            self.inner.reader()
        }
        fn write(
            &self,
            _documents: &[DocumentLogEntry],
            _indexes: &[PersistenceIndexEntry],
            _conflict_strategy: ConflictStrategy,
        ) -> Result<(), PersistError> {
            Err(PersistError::Uncertain("disk".into()))
        }
        fn write_persistence_global(
            &self,
            key: PersistenceGlobalKey,
            value: &serde_json::Value,
        ) -> Result<(), PersistError> {
            self.inner.write_persistence_global(key, value)
        }
    }
}
