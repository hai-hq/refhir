//! Connections that the tenants served by one process share.
//!
//! A process that serves one tenant (or a classic database) keeps a pool of its own, a connection
//! that holds its writer lease, one that announces its publishes and, on a replica, one that
//! listens for them. A process that serves many tenants would need all of that again for each of
//! them, and a database's `max_connections` would be spent long before its CPU was. So such a
//! process opens [`SharedConnections`] once:
//!
//! - **one pool** that every tenant checks connections out of. A connection says which tenant it
//!   serves (`refhir.tenant`, which row-level security reads) when it is checked out for a tenant
//!   other than the one it served last, so a tenant never reads with another tenant's setting.
//!   Each tenant may hold at most its *share* of the pool at once, so one busy tenant cannot take
//!   every connection and leave the others waiting;
//! - **one session** that holds every tenant's writer lease (an advisory lock each, see
//!   [`crate::lease::LeaseHub`]);
//! - **one connection** that announces every tenant's publishes, and **one** that listens for
//!   them (see [`crate::listen::ListenHub`]).
//!
//! So a process needs the pool's size plus three connections, however many tenants it serves.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use refhir_persistence::{PersistError, Pool, PoolGuard, PoolStats};

use crate::scope::Scope;
use crate::tenant::{self, Layout};
use crate::{connect_client, definite, migrate, off_runtime, set_tenant, Conn, Inner};

/// The connections a [`PostgresPersistence`](crate::PostgresPersistence) checks out: its own
/// pool, or its tenant's share of one that several tenants use.
pub(crate) struct Connections {
    pool: Arc<Pool<Inner>>,
    scope: Scope,
    share: Option<Share>,
}

impl Connections {
    /// A pool of this persistence's own: its connections already serve `scope`.
    pub(crate) fn own(pool: Pool<Inner>, scope: Scope) -> Self {
        Self {
            pool: Arc::new(pool),
            scope,
            share: None,
        }
    }

    /// Up to `share` connections of a pool other tenants use too.
    pub(crate) fn shared(pool: Arc<Pool<Inner>>, scope: Scope, share: usize) -> Self {
        Self {
            pool,
            scope,
            share: Some(Share::new(share)),
        }
    }

    /// `Mutex::lock` shape, as the pool's own: waits for a connection, which then serves this
    /// persistence's tenant.
    pub(crate) fn lock(&self) -> Result<Checkout<'_>, PersistError> {
        let permit = self.share.as_ref().map(Share::acquire);
        let mut guard = self.pool.get()?;
        if let Scope::Tenant(id) = self.scope {
            if guard.tenant != Some(id) {
                // Forgotten first: a connection whose setting is in doubt must not be kept.
                guard.tenant = None;
                if let Err(err) = set_tenant(&mut guard.client, self.scope) {
                    guard.discard();
                    return Err(err);
                }
                guard.tenant = Some(id);
            }
        }
        Ok(Checkout {
            guard,
            _permit: permit,
        })
    }

    /// The pool's counters, or for a tenant of a shared pool, its own part of them: `max` is its
    /// share, and the rest counts only its checkouts. `open` is the whole pool's.
    pub(crate) fn stats(&self) -> PoolStats {
        let pool = self.pool.stats();
        match &self.share {
            None => pool,
            Some(share) => share.stats(pool.open),
        }
    }
}

/// A checked-out connection. It dereferences to the pool's guard, and through that to the
/// connection, so callers use it as one.
pub(crate) struct Checkout<'a> {
    guard: PoolGuard<'a, Inner>,
    // Dropped after the connection is back in the pool.
    _permit: Option<Permit<'a>>,
}

impl<'a> std::ops::Deref for Checkout<'a> {
    type Target = PoolGuard<'a, Inner>;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl std::ops::DerefMut for Checkout<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

/// How many connections of a shared pool one tenant may hold at once.
struct Share {
    limit: usize,
    state: Mutex<ShareState>,
    freed: Condvar,
    acquires: AtomicU64,
    waits: AtomicU64,
    wait_ns: AtomicU64,
}

#[derive(Default)]
struct ShareState {
    in_use: usize,
    peak: usize,
}

struct Permit<'a>(&'a Share);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        state.in_use -= 1;
        self.0.freed.notify_one();
    }
}

impl Share {
    fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            state: Mutex::new(ShareState::default()),
            freed: Condvar::new(),
            acquires: AtomicU64::new(0),
            waits: AtomicU64::new(0),
            wait_ns: AtomicU64::new(0),
        }
    }

    fn acquire(&self) -> Permit<'_> {
        let started = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut waited = false;
        while state.in_use >= self.limit {
            waited = true;
            state = self.freed.wait(state).unwrap_or_else(|p| p.into_inner());
        }
        state.in_use += 1;
        state.peak = state.peak.max(state.in_use);
        self.acquires.fetch_add(1, Ordering::Relaxed);
        if waited {
            self.waits.fetch_add(1, Ordering::Relaxed);
            self.wait_ns
                .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        Permit(self)
    }

    fn stats(&self, open: u64) -> PoolStats {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        PoolStats {
            max: self.limit as u64,
            open,
            in_use: state.in_use as u64,
            peak_in_use: state.peak as u64,
            acquires: self.acquires.load(Ordering::Relaxed),
            waits: self.waits.load(Ordering::Relaxed),
            wait_ns: self.wait_ns.load(Ordering::Relaxed),
        }
    }
}

/// How [`SharedConnections::connect`] opens the database.
#[derive(Clone, Debug)]
pub struct SharedOptions {
    /// Most connections the shared pool opens, for every tenant together.
    pub pool_size: usize,
    /// Apply pending schema migrations, and give an empty database the shared layout. A process
    /// that only reads sets this to false, and waits for a writer to have done both.
    pub create_schema: bool,
}

impl Default for SharedOptions {
    fn default() -> Self {
        Self {
            pool_size: 8,
            create_schema: true,
        }
    }
}

/// The connections every tenant a process serves shares. See the module documentation.
pub struct SharedConnections {
    pub(crate) url: Arc<str>,
    pub(crate) pool: Arc<Pool<Inner>>,
    pub(crate) lease: Arc<crate::lease::LeaseHub>,
    pub(crate) announce: Arc<crate::announce::SharedLine>,
    pub(crate) listen: Arc<crate::listen::ListenHub>,
    pub(crate) row_security: bool,
}

impl std::fmt::Debug for SharedConnections {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The URL carries the password.
        f.write_str("SharedConnections")
    }
}

impl SharedConnections {
    /// Opens the shared pool on the database at `url`, which must hold tenants or be empty. An
    /// empty one is given the shared layout (with no tenants yet) unless `create_schema` is off,
    /// in which case this waits for a writer to have done it.
    pub fn connect(url: &str, options: &SharedOptions) -> Result<Arc<Self>, PersistError> {
        off_runtime(|| {
            let mut client = Conn::new(connect_client(url)?);
            let mut attempts = 0u64;
            loop {
                match tenant::detect_layout(&mut *client)? {
                    Layout::Classic => {
                        return Err(definite(
                            "this database was created without tenants, and a database keeps its \
                             layout: serve many tenants from an empty database, or one made by \
                             `refhir tenant create`",
                        ))
                    }
                    Layout::Shared => break,
                    Layout::Empty if options.create_schema => break,
                    Layout::Empty => {
                        if attempts % 10 == 0 {
                            tracing::info!(
                                target: "refhir",
                                "waiting for a primary to give the empty database the shared layout"
                            );
                        }
                        attempts += 1;
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            }
            if options.create_schema {
                migrate::run(&mut client, migrate::SHARED_MIGRATIONS)?;
            } else {
                migrate::wait_until_current(&mut client, migrate::SHARED_MIGRATIONS)?;
            }
            let row_security = crate::check_row_security(&mut client, Scope::Tenant(0));
            let owned: Arc<str> = Arc::from(url);
            let factory_url = owned.clone();
            let pool = Pool::new(
                options.pool_size,
                vec![Inner::new(client)],
                move || connect_client(&factory_url).map(|client| Inner::new(Conn::new(client))),
                |inner: &Inner| !inner.client.is_closed(),
            );
            Ok(Arc::new(Self {
                lease: crate::lease::LeaseHub::new(owned.clone()),
                announce: crate::announce::SharedLine::new(owned.clone()),
                listen: crate::listen::ListenHub::new(owned.clone()),
                url: owned,
                pool: Arc::new(pool),
                row_security,
            }))
        })
    }

    /// The shared pool's counters, for every tenant together.
    pub fn stats(&self) -> PoolStats {
        self.pool.stats()
    }

    /// Whether the database's row-level security binds the role this process connects as.
    pub fn row_security_applies(&self) -> bool {
        self.row_security
    }

    /// Wakes `wake` whenever the tenant registry changes (a tenant is created, deleted, restored
    /// or purged by any process), until the returned handle is dropped.
    pub fn watch_registry(&self) -> crate::listen::Subscription {
        self.listen
            .subscribe(crate::tenant::REGISTRY_CHANNEL.to_string())
    }

    /// Runs `f` on a connection of the shared pool that serves no tenant, for the registry.
    pub(crate) fn with_registry<T>(
        &self,
        f: impl FnOnce(&mut postgres::Client) -> Result<T, PersistError> + Send,
    ) -> Result<T, PersistError>
    where
        T: Send,
    {
        off_runtime(|| {
            let mut guard = self.pool.get()?;
            if guard.tenant.is_some() {
                // The registry has no row-level security, but a connection that is put back
                // must say truthfully which tenant it serves.
                guard.tenant = None;
                if let Err(err) = guard
                    .client
                    .batch_execute("SELECT set_config('refhir.tenant', '', false)")
                {
                    guard.discard();
                    return Err(definite(err));
                }
            }
            let outcome = f(&mut guard.client);
            if outcome.is_err() && guard.client.is_closed() {
                guard.discard();
            }
            outcome
        })
    }
}
