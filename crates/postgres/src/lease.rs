//! The writer lease: one process at a time may write.
//!
//! The lease is a session-level advisory lock on a dedicated connection that is never part of
//! the pool, plus an epoch in `persistence_globals`. The lock keeps a second writer out. The
//! epoch covers the case the lock cannot: a paused primary whose session was dropped by the
//! server wakes up after a successor took over, still believing it is the writer. Every write
//! reads the epoch inside its own transaction and fails with `Fenced` if it is not the
//! holder's.
//!
//! Do not reuse the key of the schema-setup lock in `connect_with`. That one is
//! transaction-scoped and short; this one lives as long as the process.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use postgres::{Client, Transaction};
use refhir_persistence::{Lease, PersistError, PersistenceGlobalKey};

use crate::definite;

/// `pg_try_advisory_lock` key for the writer lease.
const LEASE_KEY: i64 = 0x7265_6668_6972_0001;

/// How often the lease connection checks that it still holds the lease.
const KEEPALIVE: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(crate) struct LeaseState {
    /// 0 until a lease is taken. Writes are fenced against it once it is non-zero.
    epoch: AtomicU64,
    /// Asks the keepalive thread to drop its connection, which releases the lock.
    release: Arc<AtomicBool>,
}

impl LeaseState {
    pub(crate) fn epoch(&self) -> Option<u64> {
        match self.epoch.load(Ordering::SeqCst) {
            0 => None,
            epoch => Some(epoch),
        }
    }

    pub(crate) fn acquire(&self, url: &str) -> Result<Lease, PersistError> {
        if let Some(epoch) = self.epoch() {
            return Ok(Lease::Held { epoch });
        }
        let mut client = crate::connect_client(url)?;
        let got: bool = client
            .query_one("SELECT pg_try_advisory_lock($1)", &[&LEASE_KEY])
            .map_err(definite)?
            .get(0);
        if !got {
            return Ok(Lease::Busy);
        }
        // A keepalive that hangs on a dead network must fail instead of blocking forever.
        client
            .batch_execute("SET statement_timeout = '5s'")
            .map_err(definite)?;
        let key = PersistenceGlobalKey::WriterEpoch.as_str();
        let text: String = client
            .query_one(
                "INSERT INTO persistence_globals (key, json_value) VALUES ($1, '1')
                 ON CONFLICT (key) DO UPDATE
                 SET json_value = (persistence_globals.json_value::bigint + 1)::text
                 RETURNING json_value",
                &[&key],
            )
            .map_err(definite)?
            .get(0);
        let epoch: u64 = text
            .parse()
            .map_err(|_| PersistError::Definite(format!("bad writer epoch {text:?}")))?;
        self.release.store(false, Ordering::SeqCst);
        self.epoch.store(epoch, Ordering::SeqCst);
        let release = self.release.clone();
        std::thread::Builder::new()
            .name("refhir-lease".into())
            .spawn(move || keepalive(client, epoch, release))
            .map_err(|err| PersistError::Definite(format!("lease thread: {err}")))?;
        tracing::info!(target: "refhir", epoch, "writer lease acquired");
        Ok(Lease::Held { epoch })
    }

    /// Drops the lease connection, which releases the lock. For tests and orderly shutdown.
    #[allow(dead_code)]
    pub(crate) fn release(&self) {
        self.release.store(true, Ordering::SeqCst);
    }
}

fn keepalive(mut client: Client, epoch: u64, release: Arc<AtomicBool>) {
    let key = PersistenceGlobalKey::WriterEpoch.as_str();
    loop {
        // Sleep in slices so a release request is honoured quickly.
        let mut slept = Duration::ZERO;
        while slept < KEEPALIVE {
            if release.load(Ordering::SeqCst) {
                drop(client);
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
            slept += Duration::from_millis(100);
        }
        let current = client.query_opt(
            "SELECT json_value FROM persistence_globals WHERE key = $1",
            &[&key],
        );
        match current {
            Ok(Some(row)) => {
                let text: String = row.get(0);
                if text.trim().parse::<u64>().ok() != Some(epoch) {
                    lost(&format!("epoch is now {text}, this process held {epoch}"));
                }
            }
            Ok(None) => lost("the writer epoch row is gone"),
            Err(err) => lost(&format!("lease connection failed: {err}")),
        }
    }
}

/// The lock may already belong to someone else, so this process must stop writing. Exiting is
/// the same answer `uncertain_persist` gives.
fn lost(reason: &str) -> ! {
    eprintln!("refhir: writer lease lost, exiting: {reason}");
    std::process::exit(70);
}

/// Test-only: switch the epoch check to `FOR UPDATE` to measure what the shared lock buys.
#[cfg(test)]
pub(crate) static EXCLUSIVE_EPOCH_CHECK: AtomicBool = AtomicBool::new(false);

fn epoch_lock_clause() -> &'static str {
    #[cfg(test)]
    if EXCLUSIVE_EPOCH_CHECK.load(Ordering::SeqCst) {
        return "FOR UPDATE";
    }
    "FOR SHARE"
}

/// Inside a write transaction: the epoch row must still be ours. `FOR SHARE` lets any number
/// of this process's concurrent persists read it together, while a successor's bump (an
/// update) waits for them and then makes every later check fail.
pub(crate) fn check_epoch(tx: &mut Transaction<'_>, epoch: u64) -> Result<(), PersistError> {
    let key = PersistenceGlobalKey::WriterEpoch.as_str();
    let row = tx
        .query_opt(
            &format!(
                "SELECT json_value FROM persistence_globals WHERE key = $1 {}",
                epoch_lock_clause()
            ),
            &[&key],
        )
        .map_err(definite)?;
    let current = row
        .map(|row| row.get::<_, String>(0))
        .and_then(|text| text.trim().parse::<u64>().ok());
    match current {
        Some(current) if current == epoch => Ok(()),
        Some(current) => Err(PersistError::Fenced(format!(
            "writer epoch is {current}, this process held {epoch}"
        ))),
        None => Err(PersistError::Fenced(format!(
            "writer epoch row is gone, this process held {epoch}"
        ))),
    }
}
