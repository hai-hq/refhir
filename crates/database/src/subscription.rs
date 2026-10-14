//! Subscriptions woken when a published write overlaps their read set.
//!
//! The sync worker reruns the FHIR operation after an `Invalidation`.

use std::collections::BTreeMap;

use refhir_types::{IndexId, Timestamp};
use tokio::sync::mpsc::UnboundedSender;

use crate::reads::ReadSet;

const ENGINE: &str = "refhir_engine";

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
        let reads_summary = reads.summary();
        self.subs.insert(
            id,
            Subscription {
                query_id,
                reads,
                ts,
                tx,
            },
        );
        tracing::debug!(
            target: ENGINE,
            query_id,
            sub_token = id,
            snapshot_ts = ts.as_u64(),
            reads = reads_summary,
            live_subscriptions = self.subs.len(),
            "subscription registered"
        );
        SubToken(id)
    }

    pub fn contains(&self, token: SubToken) -> bool {
        self.subs.contains_key(&token.0)
    }

    pub fn remove(&mut self, token: SubToken) {
        if self.subs.remove(&token.0).is_some() {
            tracing::debug!(
                target: ENGINE,
                sub_token = token.0,
                live_subscriptions = self.subs.len(),
                "subscription removed"
            );
        }
    }

    pub fn refresh(&mut self, token: SubToken, reads: ReadSet, ts: Timestamp) -> bool {
        match self.subs.get_mut(&token.0) {
            Some(sub) => {
                let reads_summary = reads.summary();
                sub.reads = reads;
                sub.ts = ts;
                tracing::debug!(
                    target: ENGINE,
                    sub_token = token.0,
                    query_id = sub.query_id,
                    snapshot_ts = ts.as_u64(),
                    reads = reads_summary,
                    "subscription read set refreshed"
                );
                true
            }
            None => false,
        }
    }

    pub fn invalidate(&mut self, keys: &[(IndexId, Vec<u8>)], write_ts: Timestamp) {
        let mut dead = Vec::new();
        let mut notified = Vec::new();
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
            } else {
                notified.push(sub.query_id);
            }
        }
        if !notified.is_empty() {
            tracing::debug!(
                target: ENGINE,
                write_ts = write_ts.as_u64(),
                write_keys = keys.len(),
                query_ids = ?notified,
                "subscriptions invalidated, reruns scheduled"
            );
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
