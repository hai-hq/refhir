//! Write log and pending writes.
//!
//! Convex `crates/database/src/write_log.rs`:
//! `WriteLog::is_stale` checks published commits in `(from, to]`.
//! `PendingWrites::is_stale` checks commits validated but not yet published.

use std::collections::BTreeMap;
use std::ops::Bound;

use refhir_types::{IndexId, Timestamp};

use crate::reads::ReadSet;
use crate::OccConflict;

#[derive(Clone, Debug, Default)]
pub struct WriteLog {
    by_index: BTreeMap<IndexId, BTreeMap<Timestamp, Vec<Vec<u8>>>>,
    max_ts: Timestamp,
}

impl WriteLog {
    pub fn new(initial: Timestamp) -> Self {
        Self {
            by_index: BTreeMap::new(),
            max_ts: initial,
        }
    }

    pub fn append(&mut self, ts: Timestamp, writes: &[(IndexId, Vec<u8>)]) {
        for (index, key) in writes {
            self.by_index
                .entry(index.clone())
                .or_default()
                .entry(ts)
                .or_default()
                .push(key.clone());
        }
        if ts > self.max_ts {
            self.max_ts = ts;
        }
    }

    pub fn is_stale(&self, reads: &ReadSet, from: Timestamp, to: Timestamp) -> Option<OccConflict> {
        for (index, by_ts) in &self.by_index {
            for (ts, keys) in by_ts.range((Bound::Excluded(from), Bound::Included(to))) {
                for key in keys {
                    if reads.overlaps(index, key) {
                        return Some(OccConflict {
                            index_id: index.clone(),
                            key: key.clone(),
                            write_ts: *ts,
                        });
                    }
                }
            }
        }
        None
    }
}

#[derive(Clone, Debug, Default)]
pub struct PendingWrites {
    by_ts: BTreeMap<Timestamp, Vec<(IndexId, Vec<u8>)>>,
}

impl PendingWrites {
    pub fn push(&mut self, ts: Timestamp, keys: Vec<(IndexId, Vec<u8>)>) {
        self.by_ts.insert(ts, keys);
    }

    pub fn front_ts(&self) -> Option<Timestamp> {
        self.by_ts.keys().next().copied()
    }

    pub fn pop_front(&mut self) {
        self.by_ts.pop_first();
    }

    pub fn remove(&mut self, ts: Timestamp) {
        self.by_ts.remove(&ts);
    }

    pub fn max_ts(&self) -> Option<Timestamp> {
        self.by_ts.keys().next_back().copied()
    }

    pub fn is_stale(&self, reads: &ReadSet) -> Option<OccConflict> {
        for (ts, keys) in &self.by_ts {
            if let Some((index_id, key)) = reads.first_overlap(keys) {
                return Some(OccConflict {
                    index_id,
                    key,
                    write_ts: *ts,
                });
            }
        }
        None
    }
}
