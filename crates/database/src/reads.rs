//! Read set. Convex `crates/database/src/reads.rs` `ReadSet`.
//!
//! Each read records index intervals. A later write conflicts when one of its
//! index keys falls inside an interval.

use std::collections::BTreeMap;

use haifhir_types::{IndexId, Interval};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadSet {
    indexed: BTreeMap<IndexId, Vec<Interval>>,
}

impl ReadSet {
    pub fn record(&mut self, index: IndexId, interval: Interval) {
        self.indexed.entry(index).or_default().push(interval);
    }

    pub fn overlaps(&self, index: &IndexId, key: &[u8]) -> bool {
        match self.indexed.get(index) {
            Some(intervals) => intervals.iter().any(|interval| interval.contains(key)),
            None => false,
        }
    }

    /// First write key that lands in this read set.
    /// Convex `ReadSet::writes_overlap_by_index`, without text indexes.
    pub fn first_overlap(&self, writes: &[(IndexId, Vec<u8>)]) -> Option<(IndexId, Vec<u8>)> {
        for (index, key) in writes {
            if self.overlaps(index, key) {
                return Some((index.clone(), key.clone()));
            }
        }
        None
    }
}
