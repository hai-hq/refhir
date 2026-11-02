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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
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

/// What a process that serves many tenants is told when it loses one tenant's lease.
pub(crate) type LostHook = Box<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
pub(crate) struct LeaseState {
    /// Whose lease: the whole database's (classic) or one tenant's.
    scope: Scope,
    /// 0 until a lease is taken. Writes are fenced against it once it is non-zero.
    epoch: AtomicU64,
    /// Asks the keepalive thread to drop its connection, which releases the lock.
    release: Arc<AtomicBool>,
    /// The session that holds this lease with the leases of the other tenants of the process,
    /// when it serves many. `None` holds it on a connection of its own.
    hub: Option<Arc<LeaseHub>>,
    /// Set when the lease was lost while the process goes on serving other tenants.
    lost: AtomicBool,
    /// Called once when that happens.
    on_lost: Mutex<Option<LostHook>>,
}

impl LeaseState {
    pub(crate) fn new(scope: Scope) -> Self {
        Self {
            scope,
            ..Self::default()
        }
    }

    pub(crate) fn on_hub(scope: Scope, hub: Arc<LeaseHub>) -> Self {
        Self {
            scope,
            hub: Some(hub),
            ..Self::default()
        }
    }

    /// Whether the lease was lost (only a lease held on a shared session can be: a process that
    /// holds its own exits instead).
    pub(crate) fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    pub(crate) fn set_on_lost(&self, hook: LostHook) {
        *self.on_lost.lock().unwrap_or_else(|p| p.into_inner()) = Some(hook);
    }

    fn mark_lost(&self, reason: &str) {
        if self.lost.swap(true, Ordering::SeqCst) {
            return;
        }
        tracing::error!(target: "refhir", "writer lease lost: {reason}");
        if let Some(hook) = self
            .on_lost
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            hook(reason);
        }
    }

    pub(crate) fn epoch(&self) -> Option<u64> {
        match self.epoch.load(Ordering::SeqCst) {
            0 => None,
            epoch => Some(epoch),
        }
    }

    pub(crate) fn acquire(self: &Arc<Self>, url: &str) -> Result<Lease, PersistError> {
        if let Some(epoch) = self.epoch() {
            return Ok(Lease::Held { epoch });
        }
        if let (Some(hub), Scope::Tenant(id)) = (&self.hub, self.scope) {
            let Some(epoch) = hub.acquire(id, Arc::downgrade(self))? else {
                return Ok(Lease::Busy);
            };
            self.epoch.store(epoch, Ordering::SeqCst);
            tracing::info!(target: "refhir", epoch, tenant = id, "writer lease acquired");
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
        let scope = self.scope;
        let epoch = bump_epoch(&mut *client, scope)?;
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

    /// Drops the lease connection, which releases the lock. For tests and orderly shutdown, and
    /// for a process that stops serving one of its tenants.
    pub(crate) fn release(&self) {
        if let (Some(hub), Scope::Tenant(id)) = (&self.hub, self.scope) {
            if self.epoch().is_some() {
                hub.release(id);
            }
            return;
        }
        self.release.store(true, Ordering::SeqCst);
    }
}

/// Takes the next writer epoch for `scope`, on the connection that holds its lease.
fn bump_epoch(client: &mut impl GenericClient, scope: Scope) -> Result<u64, PersistError> {
    let key = PersistenceGlobalKey::WriterEpoch.as_str();
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
    text.parse()
        .map_err(|_| PersistError::Definite(format!("bad writer epoch {text:?}")))
}

/// One session that holds the writer leases of every tenant a process serves.
///
/// A lease is an advisory lock that lives as long as the session that took it, and one session can
/// hold any number of them, so a process that serves many tenants holds them all on one connection
/// instead of one each. One thread checks every held tenant's epoch, as the keepalive of a single
/// lease does. A tenant whose epoch moved (another process took it over) is lost alone: its writes
/// are already fenced by the epoch, and the process is told so it stops serving it. A session that
/// fails loses every lease at once, which the process cannot tell from another having taken them,
/// so it exits, as a process that holds one lease does.
pub struct LeaseHub {
    url: Arc<str>,
    session: Mutex<Option<Conn>>,
    held: Mutex<HashMap<i32, Held>>,
    running: AtomicBool,
}

struct Held {
    epoch: u64,
    state: Weak<LeaseState>,
}

impl LeaseHub {
    pub(crate) fn new(url: Arc<str>) -> Arc<Self> {
        Arc::new(Self {
            url,
            session: Mutex::new(None),
            held: Mutex::new(HashMap::new()),
            running: AtomicBool::new(false),
        })
    }

    /// The tenant ids whose lease this process holds.
    pub(crate) fn held(&self) -> Vec<i32> {
        let mut ids: Vec<i32> = self
            .held
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .copied()
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Tries to take tenant `id`'s lease: its new epoch, or `None` when another process holds it.
    fn acquire(
        self: &Arc<Self>,
        id: i32,
        state: Weak<LeaseState>,
    ) -> Result<Option<u64>, PersistError> {
        let mut session = self.session.lock().unwrap_or_else(|p| p.into_inner());
        if session.as_ref().is_some_and(|conn| conn.is_closed()) {
            self.session_failed("the lease session closed");
        }
        if session.is_none() {
            let mut client = Conn::new(crate::connect_client(&self.url)?);
            client
                .batch_execute("SET statement_timeout = '5s'")
                .map_err(definite)?;
            *session = Some(client);
        }
        let client = session.as_mut().expect("connected above");
        let scope = Scope::Tenant(id);
        let outcome = (|| {
            crate::set_tenant(client, scope)?;
            // Advisory locks stack within a session: a tenant this session already holds (opened
            // again after a failure, say) is not locked a second time, or one unlock would leave
            // it held.
            let held_here = self
                .held
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains_key(&id);
            if !held_here {
                let got: bool = client
                    .query_one(
                        "SELECT pg_try_advisory_lock($1, $2)",
                        &[&LEASE_NAMESPACE, &id],
                    )
                    .map_err(definite)?
                    .get(0);
                if !got {
                    return Ok(None);
                }
            }
            match bump_epoch(&mut **client, scope) {
                Ok(epoch) => Ok(Some(epoch)),
                Err(err) => {
                    // The lock was taken for a lease that was not made: give it back, or every
                    // other process would find the tenant busy with nobody holding it.
                    if !held_here {
                        let _ = client.execute(
                            "SELECT pg_advisory_unlock($1, $2)",
                            &[&LEASE_NAMESPACE, &id],
                        );
                    }
                    Err(err)
                }
            }
        })();
        match outcome {
            Ok(Some(epoch)) => {
                self.held
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(id, Held { epoch, state });
                drop(session);
                self.ensure_thread();
                Ok(Some(epoch))
            }
            Ok(None) => Ok(None),
            Err(err) => {
                if client.is_closed() {
                    if !self.held().is_empty() {
                        self.session_failed(&err.to_string());
                    }
                    *session = None;
                }
                Err(err)
            }
        }
    }

    fn release(&self, id: i32) {
        let removed = self
            .held
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        if removed.is_none() {
            return;
        }
        let mut session = self.session.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(client) = session.as_mut() {
            if let Err(err) = client.execute(
                "SELECT pg_advisory_unlock($1, $2)",
                &[&LEASE_NAMESPACE, &id],
            ) {
                tracing::warn!(target: "refhir", tenant = id, "could not give up a writer lease: {err}");
            }
        }
    }

    fn ensure_thread(self: &Arc<Self>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        if let Err(err) = std::thread::Builder::new()
            .name("refhir-lease".into())
            .spawn(move || hub_keepalive(weak))
        {
            self.running.store(false, Ordering::SeqCst);
            tracing::error!(target: "refhir", "lease thread: {err}");
        }
    }

    /// Every lease went with the session, and another process may hold them now.
    fn session_failed(&self, reason: &str) -> ! {
        lost(&format!(
            "the session that holds the tenants' leases failed: {reason}"
        ))
    }

    /// Checks every held tenant's epoch once, in one round trip.
    fn check(&self) {
        let held: Vec<(i32, u64)> = self
            .held
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(id, held)| (*id, held.epoch))
            .collect();
        if held.is_empty() {
            return;
        }
        let key = PersistenceGlobalKey::WriterEpoch.as_str();
        let mut batch = String::new();
        for (id, _) in &held {
            batch.push_str(&format!(
                "SELECT set_config('refhir.tenant', '{id}', false);                  SELECT {id}, json_value FROM persistence_globals                  WHERE instance_id = {id} AND key = '{key}'; "
            ));
        }
        let rows = {
            let mut session = self.session.lock().unwrap_or_else(|p| p.into_inner());
            let Some(client) = session.as_mut() else {
                self.session_failed("the lease session is gone");
            };
            match client.simple_query(&batch) {
                Ok(rows) => rows,
                Err(err) => self.session_failed(&err.to_string()),
            }
        };
        let mut seen: HashMap<i32, String> = HashMap::new();
        for message in rows {
            if let postgres::SimpleQueryMessage::Row(row) = message {
                if row.len() == 2 {
                    if let (Some(id), Some(value)) =
                        (row.get(0).and_then(|id| id.parse::<i32>().ok()), row.get(1))
                    {
                        seen.insert(id, value.to_string());
                    }
                }
            }
        }
        for (id, epoch) in held {
            let reason = match seen.get(&id) {
                Some(text) if text.trim().parse::<u64>().ok() == Some(epoch) => continue,
                Some(text) => format!("epoch is now {text}, this process held {epoch}"),
                None => "the writer epoch row is gone".to_string(),
            };
            let state = self
                .held
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id)
                .and_then(|held| held.state.upgrade());
            // Its writes are fenced by the epoch already; the lock goes too, so whoever moved
            // the epoch is not kept waiting for it.
            if let Some(client) = self
                .session
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_mut()
            {
                let _ = client.execute(
                    "SELECT pg_advisory_unlock($1, $2)",
                    &[&LEASE_NAMESPACE, &id],
                );
            }
            if let Some(state) = state {
                state.mark_lost(&format!("tenant {id}: {reason}"));
            }
        }
    }
}

fn hub_keepalive(hub: Weak<LeaseHub>) {
    loop {
        std::thread::sleep(KEEPALIVE);
        let Some(hub) = hub.upgrade() else {
            return;
        };
        hub.check();
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
