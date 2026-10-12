//! A small blocking connection pool.
//!
//! Both SQL backends used to keep one connection behind one `Mutex`. A `Pool` with a maximum
//! of one connection behaves the same way (callers queue), so the baseline is unchanged; a
//! larger maximum lets reads and persists run on separate connections. Connections are made
//! lazily, returned on drop, and discarded when `healthy` says they are broken.
//!
//! No caller may hold a connection while asking for a second one. The old single `Mutex`
//! would have deadlocked on that too, so nothing does it today.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

use crate::PersistError;

/// Counters a pool keeps. Exposed through `/healthz?verbose=1` and used for the load baseline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Connections the pool may open at most.
    pub max: u64,
    /// Connections open right now (idle plus checked out).
    pub open: u64,
    /// Connections checked out right now.
    pub in_use: u64,
    /// Highest `in_use` seen.
    pub peak_in_use: u64,
    /// Total checkouts.
    pub acquires: u64,
    /// Checkouts that had to wait for a free connection.
    pub waits: u64,
    /// Total nanoseconds spent waiting for a connection (or for a new one to open).
    pub wait_ns: u64,
}

struct State<T> {
    idle: Vec<T>,
    /// Connections that exist: idle, checked out, or being opened.
    total: usize,
    in_use: usize,
}

type Factory<T> = Box<dyn Fn() -> Result<T, PersistError> + Send + Sync>;
type Health<T> = Box<dyn Fn(&T) -> bool + Send + Sync>;

pub struct Pool<T> {
    state: Mutex<State<T>>,
    available: Condvar,
    max: usize,
    make: Factory<T>,
    healthy: Health<T>,
    peak: AtomicU64,
    acquires: AtomicU64,
    waits: AtomicU64,
    wait_ns: AtomicU64,
}

impl<T> Pool<T> {
    /// `initial` connections are already open. More are made on demand, up to `max`.
    pub fn new(
        max: usize,
        initial: Vec<T>,
        make: impl Fn() -> Result<T, PersistError> + Send + Sync + 'static,
        healthy: impl Fn(&T) -> bool + Send + Sync + 'static,
    ) -> Self {
        let max = max.max(1).max(initial.len());
        let total = initial.len();
        Self {
            state: Mutex::new(State {
                idle: initial,
                total,
                in_use: 0,
            }),
            available: Condvar::new(),
            max,
            make: Box::new(make),
            healthy: Box::new(healthy),
            peak: AtomicU64::new(0),
            acquires: AtomicU64::new(0),
            waits: AtomicU64::new(0),
            wait_ns: AtomicU64::new(0),
        }
    }

    /// Takes a connection, waiting if every one is in use.
    pub fn get(&self) -> Result<PoolGuard<'_, T>, PersistError> {
        let started = Instant::now();
        let mut waited = false;
        let mut state = self.state.lock().expect("pool");
        loop {
            if let Some(conn) = state.idle.pop() {
                state.in_use += 1;
                self.note_checkout(state.in_use, waited, started);
                return Ok(PoolGuard {
                    pool: self,
                    conn: Some(conn),
                    broken: false,
                });
            }
            if state.total < self.max {
                state.total += 1;
                drop(state);
                // Opening a connection can take a while, so the pool lock is not held.
                match (self.make)() {
                    Ok(conn) => {
                        let mut state = self.state.lock().expect("pool");
                        state.in_use += 1;
                        self.note_checkout(state.in_use, waited, started);
                        return Ok(PoolGuard {
                            pool: self,
                            conn: Some(conn),
                            broken: false,
                        });
                    }
                    Err(err) => {
                        let mut state = self.state.lock().expect("pool");
                        state.total -= 1;
                        self.available.notify_one();
                        return Err(err);
                    }
                }
            }
            waited = true;
            state = self.available.wait(state).expect("pool wait");
        }
    }

    /// `Mutex::lock` shape, so call sites read the same as before the pool: the error type is
    /// the pool's, and `expect` on it is for callers that cannot return one.
    pub fn lock(&self) -> Result<PoolGuard<'_, T>, PersistError> {
        self.get()
    }

    fn note_checkout(&self, in_use: usize, waited: bool, started: Instant) {
        self.acquires.fetch_add(1, Ordering::Relaxed);
        self.peak.fetch_max(in_use as u64, Ordering::Relaxed);
        if waited {
            self.waits.fetch_add(1, Ordering::Relaxed);
            self.wait_ns
                .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }

    pub fn stats(&self) -> PoolStats {
        let state = self.state.lock().expect("pool");
        PoolStats {
            max: self.max as u64,
            open: state.total as u64,
            in_use: state.in_use as u64,
            peak_in_use: self.peak.load(Ordering::Relaxed),
            acquires: self.acquires.load(Ordering::Relaxed),
            waits: self.waits.load(Ordering::Relaxed),
            wait_ns: self.wait_ns.load(Ordering::Relaxed),
        }
    }

    fn give_back(&self, conn: T, broken: bool) {
        let keep = !broken && (self.healthy)(&conn);
        let mut state = self.state.lock().expect("pool");
        state.in_use -= 1;
        if keep {
            state.idle.push(conn);
        } else {
            state.total -= 1;
        }
        self.available.notify_one();
    }
}

/// A checked-out connection. Dereferences to the connection and returns it on drop.
pub struct PoolGuard<'a, T> {
    pool: &'a Pool<T>,
    conn: Option<T>,
    broken: bool,
}

impl<T> PoolGuard<'_, T> {
    /// Drop this connection instead of returning it, after an error that may have left it
    /// in an unknown state.
    pub fn discard(&mut self) {
        self.broken = true;
    }
}

impl<T> Deref for PoolGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.conn.as_ref().expect("connection")
    }
}

impl<T> DerefMut for PoolGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.conn.as_mut().expect("connection")
    }
}

impl<T> Drop for PoolGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.give_back(conn, self.broken);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use std::time::Duration;

    fn counter_pool(max: usize) -> (Arc<Pool<usize>>, Arc<AtomicUsize>) {
        let made = Arc::new(AtomicUsize::new(0));
        let seen = made.clone();
        let pool = Pool::new(
            max,
            Vec::new(),
            move || Ok(seen.fetch_add(1, Ordering::SeqCst)),
            |_| true,
        );
        (Arc::new(pool), made)
    }

    #[test]
    fn opens_lazily_and_reuses() {
        let (pool, made) = counter_pool(4);
        {
            let first = pool.get().unwrap();
            assert_eq!(*first, 0);
        }
        let again = pool.get().unwrap();
        assert_eq!(
            *again, 0,
            "an idle connection is reused before opening another"
        );
        let other = pool.get().unwrap();
        assert_eq!(*other, 1);
        assert_eq!(made.load(Ordering::SeqCst), 2);
        assert_eq!(pool.stats().in_use, 2);
    }

    #[test]
    fn a_pool_of_one_makes_callers_queue() {
        let (pool, _) = counter_pool(1);
        let held = pool.get().unwrap();
        let waiter = {
            let pool = pool.clone();
            std::thread::spawn(move || {
                let guard = pool.get().unwrap();
                *guard
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(!waiter.is_finished());
        drop(held);
        assert_eq!(waiter.join().unwrap(), 0);
        let stats = pool.stats();
        assert_eq!(stats.waits, 1);
        assert!(stats.wait_ns > 0);
        assert_eq!(stats.peak_in_use, 1);
    }

    #[test]
    fn broken_connections_are_dropped_and_replaced() {
        let (pool, made) = counter_pool(1);
        {
            let mut guard = pool.get().unwrap();
            guard.discard();
        }
        assert_eq!(pool.stats().open, 0);
        let replacement = pool.get().unwrap();
        assert_eq!(*replacement, 1);
        assert_eq!(made.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_failed_open_frees_its_slot() {
        let pool: Pool<u8> = Pool::new(
            1,
            Vec::new(),
            || Err(PersistError::Definite("down".into())),
            |_| true,
        );
        assert!(pool.get().is_err());
        assert_eq!(pool.stats().open, 0);
        assert!(pool.get().is_err(), "the slot was released, not leaked");
    }
}
