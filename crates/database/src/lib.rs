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
use std::sync::{Arc, Condvar, Mutex};

use refhir_persistence::{ConflictStrategy, PersistError, Persistence, PersistenceReader};
use refhir_types::{IndexId, Timestamp};
use tokio::sync::mpsc::UnboundedSender;

const ENGINE: &str = "refhir_engine";

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
    Delete, DocumentWrite, IndexPut, Put, Revision, Scanned, Stored, Transaction, WrittenId,
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

pub struct Database {
    persistence: Arc<dyn Persistence>,
    reader: Arc<dyn PersistenceReader>,
    state: Mutex<Committer>,
    cv: Condvar,
    commit_listeners: Mutex<Vec<UnboundedSender<Timestamp>>>,
}

impl Database {
    pub fn open(persistence: Arc<dyn Persistence>) -> Result<Self, PersistError> {
        let latest = persistence.max_ts()?.unwrap_or(Timestamp::ZERO);
        let reader = persistence.reader();
        Ok(Self {
            persistence,
            reader,
            state: Mutex::new(Committer::new(latest)),
            cv: Condvar::new(),
            commit_listeners: Mutex::new(Vec::new()),
        })
    }

    pub fn latest_ts(&self) -> Timestamp {
        self.state.lock().expect("committer").snapshot.latest_ts()
    }

    pub fn reader(&self) -> Arc<dyn PersistenceReader> {
        self.reader.clone()
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
        let keys = tx.occ_keys();
        let begin_ts = tx.begin_timestamp();
        let commit_ts = {
            let mut state = self.state.lock().expect("committer");
            state.validate_commit(tx.read_set(), begin_ts, &keys)?
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
        match self
            .persistence
            .write(&documents, &indexes, ConflictStrategy::Error)
        {
            Ok(()) => {}
            Err(PersistError::Definite(message)) => {
                let mut state = self.state.lock().expect("committer");
                state.pending.remove(commit_ts);
                self.cv.notify_all();
                return Err(CommitError::Persist(message));
            }
            Err(PersistError::Uncertain(message)) => uncertain_persist(message),
        }
        tracing::debug!(
            target: ENGINE,
            commit_ts = commit_ts.as_u64(),
            documents = documents.len(),
            index_rows = indexes.len(),
            "persisted, publishing"
        );
        self.publish_commit(commit_ts, keys);
        Ok(commit_ts)
    }

    /// Convex `publish_commit`: append the write log, push the snapshot, wake subscribers.
    /// Waits until this timestamp is the oldest pending commit so publishes stay ordered.
    fn publish_commit(&self, ts: Timestamp, keys: Vec<(IndexId, Vec<u8>)>) {
        let mut state = self.state.lock().expect("committer");
        while state.pending.front_ts() != Some(ts) {
            state = self.cv.wait(state).expect("committer wait");
        }
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
        let listeners = self
            .commit_listeners
            .lock()
            .expect("commit listeners")
            .clone();
        for listener in listeners {
            let _ = listener.send(ts);
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
            if n >= self.block_from {
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
