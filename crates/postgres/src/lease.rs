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

use postgres::{GenericClient, Transaction};
use refhir_persistence::{Lease, PersistError, PersistenceGlobalKey};

use crate::scope::Scope;
use crate::{definite, Conn};

/// `pg_try_advisory_lock` key for the writer lease of a database in the classic layout.
const LEASE_KEY: i64 = 0x7265_6668_6972_0001;

/// First half of the two-part `pg_try_advisory_lock(namespace, tenant)` key a tenant's lease uses.
/// Postgres keeps the two-integer and the single-bigint advisory lock spaces apart, so no tenant id
/// can collide with `LEASE_KEY` or the migration and audit locks, and the registry hands out each
/// id once, so no two tenants collide with each other.
pub(crate) const LEASE_NAMESPACE: i32 = 0x7265_6668;

/// How often the lease connection checks that it still holds the lease.
const KEEPALIVE: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(crate) struct LeaseState {
    /// Whose lease: the whole database's (classic) or one tenant's.
    scope: Scope,
    /// 0 until a lease is taken. Writes are fenced against it once it is non-zero.
    epoch: AtomicU64,
    /// Asks the keepalive thread to drop its connection, which releases the lock.
    release: Arc<AtomicBool>,
}

impl LeaseState {
    pub(crate) fn new(scope: Scope) -> Self {
        Self {
            scope,
            ..Self::default()
        }
    }

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
        // A `Conn`, so that giving it up (when another process holds the lease) closes it on a
        // thread of its own: dropping a bare `Client` blocks on a runtime, which a thread that
        // belongs to another runtime may not do.
        let mut client = Conn::new(crate::connect_client_for(url, self.scope)?);
        let got: bool = match self.scope {
            Scope::Classic => client.query_one("SELECT pg_try_advisory_lock($1)", &[&LEASE_KEY]),
            Scope::Tenant(id) => client.query_one(
                "SELECT pg_try_advisory_lock($1, $2)",
                &[&LEASE_NAMESPACE, &id],
            ),
        }
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
        let scope = self.scope;
        let text: String = client
            .query_one(
                &format!(
                    "INSERT INTO persistence_globals ({}key, json_value) VALUES ({}$1, '1')
                     ON CONFLICT {} DO UPDATE
                     SET json_value = (persistence_globals.json_value::bigint + 1)::text
                     RETURNING json_value",
                    scope.cols(),
                    scope.vals(),
                    scope.conflict("key")
                ),
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
            .spawn(move || keepalive(client, scope, epoch, release))
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

fn keepalive(mut client: Conn, scope: Scope, epoch: u64, release: Arc<AtomicBool>) {
    let key = PersistenceGlobalKey::WriterEpoch.as_str();
    let read_epoch = format!(
        "SELECT json_value FROM persistence_globals WHERE {}key = $1",
        scope.and()
    );
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
        let current = client.query_opt(&read_epoch, &[&key]);
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
pub(crate) fn check_epoch(
    tx: &mut Transaction<'_>,
    scope: Scope,
    epoch: u64,
) -> Result<(), PersistError> {
    let key = PersistenceGlobalKey::WriterEpoch.as_str();
    let row = tx
        .query_opt(
            &format!(
                "SELECT json_value FROM persistence_globals WHERE {}key = $1 {}",
                scope.and(),
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

/// Whether some process holds tenant `id`'s writer lease. It reads the lock table and takes
/// nothing, so asking cannot make a primary that is starting at that moment find the lease busy
/// (a `try_lock` and release would, for the instant it held it).
pub(crate) fn is_held(client: &mut impl GenericClient, id: i32) -> Result<bool, PersistError> {
    // A two-part advisory lock shows as `classid`, `objid` and `objsubid = 2`.
    let held: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND granted \
             AND objsubid = 2 AND classid = $1::int4::oid AND objid = $2::int4::oid \
             AND database = (SELECT oid FROM pg_database WHERE datname = current_database()))",
            &[&LEASE_NAMESPACE, &id],
        )
        .map_err(definite)?
        .get(0);
    Ok(held)
}
