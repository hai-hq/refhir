//! What recording the published-up-to watermark costs a commit. Phase 3 of the plan asks for the
//! cost to be measured against the baseline, not assumed.
//!
//! The same commits are made twice, once through `PostgresPersistence` as it is and once through a
//! wrapper whose `advance_published_ts` does nothing, so the only difference is the watermark
//! write (and the notice that goes with it). Run with:
//! `cargo test -p refhir --test bench_watermark -- --ignored --nocapture`

use std::sync::Arc;
use std::time::Instant;

use refhir_database::{Database, Put};
use refhir_persistence::{
    ConflictStrategy, DocumentLogEntry, IndexScanRow, Lease, PersistError, Persistence,
    PersistenceGlobalKey, PersistenceIndexEntry, PersistenceReader, PoolStats, TimestampRange,
};
use refhir_postgres::{ConnectOptions, PostgresPersistence};
use refhir_types::{exact_key, DocumentId, IndexId, Interval, Order, TabletId, Timestamp};
use serde_json::json;

mod common;
use common::fresh_database;

/// Delegates everything, and drops the watermark write.
struct WithoutWatermark(Arc<PostgresPersistence>);

impl PersistenceReader for WithoutWatermark {
    fn load_documents(
        &self,
        r: TimestampRange,
        o: Order,
    ) -> Result<Vec<DocumentLogEntry>, PersistError> {
        self.0.load_documents(r, o)
    }
    fn latest_document(
        &self,
        id: &DocumentId,
        s: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        self.0.latest_document(id, s)
    }
    fn previous_revision(
        &self,
        id: &DocumentId,
        at: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        self.0.previous_revision(id, at)
    }
    fn exact_revision(
        &self,
        id: &DocumentId,
        ts: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        self.0.exact_revision(id, ts)
    }
    fn document_history(
        &self,
        id: &DocumentId,
        s: Timestamp,
    ) -> Result<Vec<DocumentLogEntry>, PersistError> {
        self.0.document_history(id, s)
    }
    fn index_scan(
        &self,
        i: &IndexId,
        t: Timestamp,
        iv: &Interval,
        o: Order,
    ) -> Result<Vec<IndexScanRow>, PersistError> {
        self.0.index_scan(i, t, iv, o)
    }
    fn get_persistence_global(
        &self,
        k: PersistenceGlobalKey,
    ) -> Result<Option<serde_json::Value>, PersistError> {
        self.0.get_persistence_global(k)
    }
    fn max_ts(&self) -> Result<Option<Timestamp>, PersistError> {
        self.0.max_ts()
    }
    fn load_index_writes(
        &self,
        r: TimestampRange,
    ) -> Result<Vec<PersistenceIndexEntry>, PersistError> {
        self.0.load_index_writes(r)
    }
}

impl Persistence for WithoutWatermark {
    fn is_fresh(&self) -> bool {
        self.0.is_fresh()
    }
    fn reader(&self) -> Arc<dyn PersistenceReader> {
        self.0.reader()
    }
    fn write(
        &self,
        d: &[DocumentLogEntry],
        i: &[PersistenceIndexEntry],
        c: ConflictStrategy,
    ) -> Result<(), PersistError> {
        self.0.write(d, i, c)
    }
    fn write_persistence_global(
        &self,
        k: PersistenceGlobalKey,
        v: &serde_json::Value,
    ) -> Result<(), PersistError> {
        self.0.write_persistence_global(k, v)
    }
    fn try_acquire_writer_lease(&self) -> Result<Lease, PersistError> {
        self.0.try_acquire_writer_lease()
    }
    fn advance_published_ts(&self, _ts: Timestamp) -> Result<(), PersistError> {
        Ok(()) // the one thing this wrapper changes
    }
    fn pool_stats(&self) -> Option<PoolStats> {
        self.0.pool_stats()
    }
}

fn commit_one(db: &Database, id: &str) {
    let key = (IndexId::new("t/by_id"), exact_key(id));
    let mut tx = db.begin();
    tx.stage_put(Put {
        tablet: TabletId::new("t"),
        id: id.into(),
        json: json!({"id": id}),
        new_keys: vec![key],
        old_keys: vec![],
        prev_ts: None,
    })
    .unwrap();
    db.commit(tx).unwrap();
}

/// Commits per second for `threads` writers making `per_thread` commits each.
fn run(admin: &str, with_watermark: bool, threads: usize, per_thread: usize) -> (f64, u64) {
    // Declared first, so it is dropped last: after the store has let go of the lease.
    let database = fresh_database(admin, "bench");
    let options = ConnectOptions {
        pool_size: 8,
        create_schema: true,
    };
    let store = Arc::new(PostgresPersistence::connect_with(&database.url, &options).unwrap());
    store.try_acquire_writer_lease().unwrap();
    let persistence: Arc<dyn Persistence> = if with_watermark {
        store.clone()
    } else {
        Arc::new(WithoutWatermark(store.clone()))
    };
    let db = Arc::new(Database::open(persistence).unwrap());
    let started = Instant::now();
    let workers: Vec<_> = (0..threads)
        .map(|worker| {
            let db = db.clone();
            std::thread::spawn(move || {
                for n in 0..per_thread {
                    commit_one(&db, &format!("w{worker}-{n}"));
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let secs = started.elapsed().as_secs_f64();
    let watermark_ns = db.stats().watermark_ns;
    drop(db);
    // Let go of the lease first: dropping the database under a live lease would (rightly) make
    // the keepalive treat the lease as lost and exit the process.
    store.release_writer_lease();
    std::thread::sleep(std::time::Duration::from_millis(400));
    drop(store);
    ((threads * per_thread) as f64 / secs, watermark_ns)
}

#[test]
#[ignore]
fn the_watermark_costs_a_commit_this_much() {
    let Ok(admin) = std::env::var("HAIFHIR_POSTGRES_URL") else {
        eprintln!("HAIFHIR_POSTGRES_URL unset, skipping the watermark benchmark");
        return;
    };
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    println!();
    println!(
        "{:<24} {:>14} {:>14} {:>10} {:>16}",
        "workload", "no watermark", "watermark", "cost", "watermark time/commit"
    );
    for (label, threads, per_thread) in [
        ("1 writer, 300 commits", 1, 300),
        ("8 writers, 8 x 60 commits", 8, 60),
    ] {
        let (mut off, mut on, mut ns) = (vec![], vec![], vec![]);
        // Alternate, so drift in the machine's load hits both the same way.
        for _ in 0..3 {
            off.push(run(&admin, false, threads, per_thread).0);
            let (rate, watermark_ns) = run(&admin, true, threads, per_thread);
            on.push(rate);
            ns.push(watermark_ns as f64 / (threads * per_thread) as f64);
        }
        let (off, on, ns) = (median(off), median(on), median(ns));
        println!(
            "{label:<24} {off:>11.0}/s {on:>11.0}/s {:>9.1}% {:>13.0} us",
            (off - on) / off * 100.0,
            ns / 1000.0
        );
    }
}
