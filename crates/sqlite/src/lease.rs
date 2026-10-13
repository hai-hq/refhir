//! The writer lease for a database file: one process at a time may write.
//!
//! An exclusive `flock` on `<db>.lock` keeps a second writer out, and the kernel drops it when
//! the holder dies, so there is nothing to keep alive. The epoch in `persistence_globals`
//! backs it up the same way it does for Postgres: every write checks it inside its own
//! transaction. An in-memory database has nothing to share, so it takes no lock and fences
//! nothing.
//!
//! `flock` needs a local filesystem. On a network mount it may not exclude anyone, so run
//! several processes against one database on Postgres instead.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use refhir_persistence::{Lease, PersistError, PersistenceGlobalKey, Pool};
use rusqlite::{params, OptionalExtension, Transaction};

use crate::{definite, Inner};

#[derive(Default)]
pub(crate) struct LeaseState {
    /// 0 until a lease is taken. Writes are fenced against it once it is non-zero.
    epoch: AtomicU64,
    /// Holding the file keeps the lock; dropping it releases the lock.
    file: Mutex<Option<File>>,
}

impl LeaseState {
    pub(crate) fn epoch(&self) -> Option<u64> {
        match self.epoch.load(Ordering::SeqCst) {
            0 => None,
            epoch => Some(epoch),
        }
    }

    pub(crate) fn acquire(
        &self,
        lock_path: Option<&Path>,
        writer: &Pool<Inner>,
    ) -> Result<Lease, PersistError> {
        if let Some(epoch) = self.epoch() {
            return Ok(Lease::Held { epoch });
        }
        let Some(lock_path) = lock_path else {
            // In memory: nobody else can open this database.
            return Ok(Lease::Held { epoch: 0 });
        };
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)
            .map_err(|err| definite(format!("open {}: {err}", lock_path.display())))?;
        if !try_lock_exclusive(&file)? {
            return Ok(Lease::Busy);
        }
        let key = PersistenceGlobalKey::WriterEpoch.as_str();
        let guard = writer.lock()?;
        let epoch: i64 = guard
            .connection
            .query_row(
                "INSERT INTO persistence_globals (key, json_value) VALUES (?1, '1')
                 ON CONFLICT (key) DO UPDATE
                 SET json_value = CAST(CAST(json_value AS INTEGER) + 1 AS TEXT)
                 RETURNING CAST(json_value AS INTEGER)",
                params![key],
                |row| row.get(0),
            )
            .map_err(definite)?;
        drop(guard);
        let epoch = epoch as u64;
        *self.file.lock().expect("lease file") = Some(file);
        self.epoch.store(epoch, Ordering::SeqCst);
        tracing::info!(target: "refhir", epoch, "writer lease acquired");
        Ok(Lease::Held { epoch })
    }

    /// Drops the lock file handle, releasing the lock. For tests and orderly shutdown.
    #[allow(dead_code)]
    pub(crate) fn release(&self) {
        *self.file.lock().expect("lease file") = None;
    }
}

#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> Result<bool, PersistError> {
    use std::os::fd::AsRawFd;
    // SAFETY: `flock` on a descriptor this function borrows for the call.
    let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if status == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(definite(format!("flock: {err}")))
    }
}

/// Without `flock` there is nothing to take, so a process starts as before and the lease only
/// bumps the epoch. Two writers on one file are then not kept apart: use Postgres for that.
#[cfg(not(unix))]
fn try_lock_exclusive(_file: &File) -> Result<bool, PersistError> {
    tracing::warn!(
        target: "refhir",
        "no file lock on this platform: two processes can open this SQLite file as writers"
    );
    Ok(true)
}

/// Inside a write transaction: the epoch row must still be ours.
pub(crate) fn check_epoch(tx: &Transaction<'_>, epoch: u64) -> Result<(), PersistError> {
    let key = PersistenceGlobalKey::WriterEpoch.as_str();
    let text: Option<String> = tx
        .query_row(
            "SELECT json_value FROM persistence_globals WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(definite)?;
    match text.and_then(|text| text.trim().parse::<u64>().ok()) {
        Some(current) if current == epoch => Ok(()),
        Some(current) => Err(PersistError::Fenced(format!(
            "writer epoch is {current}, this process held {epoch}"
        ))),
        None => Err(PersistError::Fenced(format!(
            "writer epoch row is gone, this process held {epoch}"
        ))),
    }
}
