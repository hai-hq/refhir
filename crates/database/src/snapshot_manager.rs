//! Published snapshot timestamp.
//!
//! Convex `crates/database/src/snapshot_manager.rs`: `latest_ts` and `push`.
//! Document bytes stay in the log. The manager only advances the repeatable
//! timestamp readers are allowed to use.

use refhir_types::Timestamp;

#[derive(Clone, Debug)]
pub struct SnapshotManager {
    latest_ts: Timestamp,
}

impl SnapshotManager {
    pub fn new(latest_ts: Timestamp) -> Self {
        Self { latest_ts }
    }

    pub fn latest_ts(&self) -> Timestamp {
        self.latest_ts
    }

    pub fn push(&mut self, ts: Timestamp) {
        assert!(ts > self.latest_ts, "snapshot timestamps must increase");
        self.latest_ts = ts;
    }
}
