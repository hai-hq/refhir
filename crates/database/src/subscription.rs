//! Subscriptions woken when a published write overlaps their read set.
//!
//! Convex `crates/database/src/subscription.rs`. The sync worker reruns the
//! FHIR operation after an `Invalidation`.

use std::collections::BTreeMap;

use refhir_types::{IndexId, Timestamp};
use tokio::sync::mpsc::UnboundedSender;

use crate::reads::ReadSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubToken(pub(crate) u64);

#[derive(Debug)]
pub struct Invalidation {
    pub query_id: u64,
    pub write_ts: Timestamp,
}

pub struct SubscriptionManager {
    next_id: u64,
    subs: BTreeMap<u64, Subscription>,
}

struct Subscription {
    query_id: u64,
    reads: ReadSet,
    /// Snapshot the read set was taken at. Refresh replaces it after a rerun.
    #[allow(dead_code)]
    ts: Timestamp,
    tx: UnboundedSender<Invalidation>,
}

impl SubscriptionManager {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            subs: BTreeMap::new(),
        }
    }

    pub fn insert(
        &mut self,
        query_id: u64,
        reads: ReadSet,
        ts: Timestamp,
        tx: UnboundedSender<Invalidation>,
    ) -> SubToken {
        let id = self.next_id;
        self.next_id += 1;
        self.subs.insert(
            id,
            Subscription {
                query_id,
                reads,
                ts,
                tx,
            },
        );
        SubToken(id)
    }

    pub fn contains(&self, token: SubToken) -> bool {
        self.subs.contains_key(&token.0)
    }

    pub fn remove(&mut self, token: SubToken) {
        self.subs.remove(&token.0);
    }

    pub fn refresh(&mut self, token: SubToken, reads: ReadSet, ts: Timestamp) -> bool {
        match self.subs.get_mut(&token.0) {
            Some(sub) => {
                sub.reads = reads;
                sub.ts = ts;
                true
            }
            None => false,
        }
    }

    pub fn invalidate(&mut self, keys: &[(IndexId, Vec<u8>)], write_ts: Timestamp) {
        let mut dead = Vec::new();
        for (id, sub) in &self.subs {
            if sub.reads.first_overlap(keys).is_none() {
                continue;
            }
            if sub
                .tx
                .send(Invalidation {
                    query_id: sub.query_id,
                    write_ts,
                })
                .is_err()
            {
                dead.push(*id);
            }
        }
        for id in dead {
            self.subs.remove(&id);
        }
    }
}

impl Default for SubscriptionManager {
    fn default() -> Self {
        Self::new()
    }
}
