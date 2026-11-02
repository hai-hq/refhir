//! `refhir --all-tenants`: one process serves every tenant of a shared Postgres database.
//!
//! Each tenant gets the server a process started with `--tenant` would have (its own database
//! handle, catalog, SMART tables, health card key, jobs and audit trail), built in this process and
//! put behind a [`TenantHost`] that picks the tenant of each request. The tenants share one pool of
//! connections and one connection each for the leases, the announcements and the listening (see
//! `refhir_postgres::SharedConnections`), so the process needs a handful of connections, not a
//! handful per tenant.
//!
//! The registry is the truth: the process reads it at start, and again whenever any process
//! changes it (it is announced) or every few seconds in case an announcement was missed. A tenant
//! that appears is opened, one that is deleted stops being served, and on the primary one whose
//! grace period is over has its rows removed. The admin API ([`TenantAdmin`]) changes the registry
//! and then does the same at once, so a tenant it creates is served when the call returns.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::http::StatusCode;
use refhir_database::Database;
use refhir_fhir::{Catalog, CatalogClock};
use refhir_persistence::{AuthStore, ConformanceStore, Lease, Persistence};
use refhir_postgres::{Created, PostgresPersistence, SharedConnections, SharedOptions, Tenant};
use refhir_server::tenancy::{
    tenant_secret, AdminError, CreateTenant, DeleteTenant, HostConfig, Routing, TenantAdmin,
    TenantHost, TenantLimits, TenantSettings, TenantState,
};
use refhir_server::TenantApp;
use serde_json::{json, Value};
use tokio::net::TcpListener;

use crate::config::{release_from_plan, PackagePlan, ResolvedConfig, Role};

/// Connections the shared pool opens at most when `--db-pool` is not given. Every tenant draws on
/// it, so it is larger than a process that serves one needs.
const DEFAULT_HOST_POOL: usize = 16;
/// How often the registry is read even when no change was announced.
const REGISTRY_POLL: Duration = Duration::from_secs(5);
/// How often deleted tenants past their grace period are looked for.
const PURGE_EVERY: Duration = Duration::from_secs(60);
/// How often a tenant whose lease another process holds asks for it again.
const LEASE_RETRY: Duration = Duration::from_secs(1);
/// Tenants whose catalog is loaded and packages installed at the same time. Each takes seconds of
/// CPU and a good deal of memory while it does, so a process with many tenants opens them a few at
/// a time (a tenant waiting for its lease or its primary does not count: it holds nothing yet).
const MAX_CONCURRENT_OPENS: usize = 4;
/// The wait before a tenant that failed to open is tried again, doubling up to the longest.
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_LONGEST: Duration = Duration::from_secs(300);

/// A standby or replica that serves every tenant must be given the two SMART secrets the tenants'
/// own are made from, since a process that makes them up would disagree with the others.
fn check_secrets_pinned(
    role: Role,
    dcr: &Option<String>,
    confidential: &Option<String>,
) -> Result<(), String> {
    let pinned = |value: &Option<String>| value.as_deref().is_some_and(|text| !text.is_empty());
    if role != Role::Primary && !(pinned(dcr) && pinned(confidential)) {
        return Err(format!(
            "a {} that serves every tenant needs --smart-dcr-token and --smart-confidential-secret \
             (or REFHIR_SMART_DCR_TOKEN and REFHIR_SMART_CONFIDENTIAL_SECRET) set to the same \
             values on every process, since tenants' SMART secrets are made from them",
            role.name()
        ));
    }
    Ok(())
}

/// How long to wait before the `attempts`th retry of a tenant that failed to open: the first wait,
/// doubling, up to the longest.
fn retry_delay(attempts: u32) -> Duration {
    RETRY_FIRST
        .saturating_mul(1u32 << attempts.saturating_sub(1).min(10))
        .min(RETRY_LONGEST)
}

/// A counting semaphore for [`MAX_CONCURRENT_OPENS`].
struct Permits {
    free: Mutex<usize>,
    freed: std::sync::Condvar,
}

struct Permit<'a>(&'a Permits);

impl Permits {
    fn new(count: usize) -> Self {
        Self {
            free: Mutex::new(count),
            freed: std::sync::Condvar::new(),
        }
    }

    fn acquire(&self) -> Permit<'_> {
        let mut free = self.free.lock().unwrap_or_else(|p| p.into_inner());
        while *free == 0 {
            free = self.freed.wait(free).unwrap_or_else(|p| p.into_inner());
        }
        *free -= 1;
        Permit(self)
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap_or_else(|p| p.into_inner()) += 1;
        self.0.freed.notify_one();
    }
}

/// Gives a tenant's writer lease back unless the tenant opened: a lease kept by a tenant that
/// failed to open would keep every other process out of it.
struct ReleaseLease(Option<PostgresPersistence>);

impl Drop for ReleaseLease {
    fn drop(&mut self) {
        if let Some(persistence) = self.0.take() {
            persistence.release_writer_lease();
        }
    }
}

/// What a tenant this process serves is made of. Dropping it lets everything go once its
/// requests are answered.
struct Opened {
    persistence: PostgresPersistence,
    db: Arc<Database>,
    catalog: Arc<RwLock<Catalog>>,
    app: Arc<TenantApp>,
    /// A replica's catalog follower, stopped with the tenant.
    watcher: Mutex<Option<refhir_fhir::CatalogWatcher>>,
}

/// One tenant's place in the process: being opened, open, or neither.
#[derive(Default)]
struct Entry {
    /// Held by whoever opens or stops the tenant, so two never do it at once.
    busy: Arc<Mutex<()>>,
    opened: Option<Arc<Opened>>,
    /// Set when the tenant should no longer be opened: an opener waiting for its lease gives up.
    cancelled: Arc<AtomicBool>,
}

pub(crate) struct Manager {
    /// Itself, for the work it hands to threads of their own.
    me: std::sync::Weak<Manager>,
    cfg: ResolvedConfig,
    url: String,
    shared: Arc<SharedConnections>,
    host: Arc<TenantHost>,
    routing: Routing,
    runtime: tokio::runtime::Handle,
    entries: Mutex<HashMap<String, Entry>>,
    /// Tenants that failed to open: how many times in a row, and when to try again.
    failed: Mutex<HashMap<String, (u32, std::time::Instant)>>,
    opens: Permits,
    /// Connections of the pool one tenant may hold, when its limits name none.
    default_share: usize,
    /// The secrets each tenant's are made from (see `tenant_secret`).
    dcr_secret: String,
    confidential_secret: String,
    /// Whether the process can sign SMART users in (an OIDC provider, or demo login).
    login: bool,
}

/// Runs the process: serves `listener` (and the admin listener, if any) until told to stop.
pub(crate) fn run(
    mut cfg: ResolvedConfig,
    runtime: &tokio::runtime::Runtime,
    listener: TcpListener,
    bootstrap: refhir_server::Bootstrap,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = cfg
        .postgres
        .clone()
        .ok_or("--all-tenants needs --postgres: tenants share one Postgres database")?;
    if cfg.health_cards_key_file.is_some() {
        return Err(
            "[health_cards] key_file is one key, and each tenant signs its cards with its own: \
             leave it out, and create tenants with health_cards on"
                .into(),
        );
    }
    let role = cfg.role;
    let local = listener.local_addr()?;
    let routing = routing(&cfg, local)?;
    let pool_size = cfg.db_pool.unwrap_or(DEFAULT_HOST_POOL);
    let default_share = cfg
        .tenants
        .connections_per_tenant
        .unwrap_or((pool_size / 2).max(1))
        .min(pool_size);
    let login = cfg.oidc.is_some() || cfg.smart_demo_login;
    if cfg.auth || login {
        let loopback = local.ip().is_loopback();
        crate::check_login(&cfg, loopback)?;
    }
    // Any tenant can have SMART, whatever the process was started with, and each tenant's secrets
    // come from these two. Processes that disagree on them, or a standby that takes over with ones
    // it made up, would break every confidential client of every SMART tenant.
    check_secrets_pinned(role, &cfg.smart_dcr_token, &cfg.smart_confidential_secret)?;
    crate::fill_smart_credentials(&mut cfg.smart_dcr_token, &mut cfg.smart_confidential_secret)?;
    let shared = SharedConnections::connect(
        &url,
        &SharedOptions {
            pool_size,
            create_schema: role != Role::Replica,
        },
    )?;
    if !shared.row_security_applies() {
        tracing::warn!(
            "the database role is a superuser or bypasses row-level security, so the database does \
             not keep tenants apart: only this process's own statements do. Connect as an ordinary \
             role that owns the database (see docs/clustering.md)"
        );
    }
    let host = TenantHost::new(HostConfig {
        routing: routing.clone(),
        admin_token: cfg.tenants.admin_token.clone(),
        role: role.name().to_string(),
        primary_url: cfg.primary_url.clone(),
        default_limits: cfg.tenants.limits.clone(),
    });
    let manager = Arc::new_cyclic(|me| Manager {
        me: me.clone(),
        dcr_secret: cfg.smart_dcr_token.clone().unwrap_or_default(),
        confidential_secret: cfg.smart_confidential_secret.clone().unwrap_or_default(),
        login,
        url,
        shared: shared.clone(),
        host: host.clone(),
        routing,
        runtime: runtime.handle().clone(),
        entries: Mutex::new(HashMap::new()),
        failed: Mutex::new(HashMap::new()),
        opens: Permits::new(MAX_CONCURRENT_OPENS),
        default_share,
        cfg,
    });
    host.set_admin(manager.clone());
    {
        let manager = Arc::downgrade(&manager);
        host.set_pool_stats(move |name| {
            manager
                .upgrade()?
                .opened(name)
                .and_then(|opened| opened.persistence.pool_stats())
        });
    }

    // The process answers from here on: tenants say they are starting until they are open.
    runtime.block_on(bootstrap.stop());
    let admin_listener = match &manager.cfg.tenants.admin_bind {
        Some(bind) => {
            let addr: SocketAddr = bind
                .parse()
                .map_err(|err| format!("[tenants] admin_bind `{bind}`: {err}"))?;
            Some(runtime.block_on(TcpListener::bind(addr))?)
        }
        None => None,
    };
    tracing::info!(
        "serving every tenant of the database as {} on http://{local}",
        role.name()
    );
    if let Some(admin) = &admin_listener {
        tracing::info!("admin API on http://{}", admin.local_addr()?);
    }

    manager.sync_registry();
    host.set_ready();
    manager.clone().start_background();

    let router = host.router();
    let admin_router = host.admin_router();
    runtime.block_on(async move {
        let main = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(refhir_server::shutdown_signal());
        match admin_listener {
            Some(admin) => {
                let admin = axum::serve(admin, admin_router)
                    .with_graceful_shutdown(refhir_server::shutdown_signal());
                tokio::select! {
                    result = std::future::IntoFuture::into_future(main) => result?,
                    result = std::future::IntoFuture::into_future(admin) => result?,
                }
            }
            None => main.await?,
        }
        Ok::<(), std::io::Error>(())
    })?;
    tracing::info!("stopping; writing what each tenant buffered");
    let names: Vec<String> = manager
        .entries
        .lock()
        .expect("entries")
        .keys()
        .cloned()
        .collect();
    for name in names {
        manager.stop(
            &name,
            TenantState::Unavailable("the process is stopping".into()),
        );
    }
    Ok(())
}

/// How requests find their tenant, and each tenant's address.
fn routing(cfg: &ResolvedConfig, local: SocketAddr) -> Result<Routing, String> {
    let public_url = cfg
        .tenants
        .public_url
        .clone()
        .or_else(|| cfg.public_url.clone())
        .or_else(|| {
            // On a loopback bind the address a request arrives at is the one to name; anywhere
            // else it is a guess, and SMART and health cards would name it to the world.
            local.ip().is_loopback().then(|| format!("http://{local}"))
        });
    let routing = Routing {
        path_prefix: cfg.tenants.path_prefix.clone(),
        domain: cfg.tenants.domain.clone(),
        public_url,
    };
    routing.validate()?;
    if routing.public_url.is_none() {
        return Err(
            "--all-tenants on a non-loopback bind needs each tenant's address: set [tenants] \
             public_url (`https://fhir.example.com/t/{tenant}`) or --public-url"
                .into(),
        );
    }
    Ok(routing)
}

impl Manager {
    fn start_background(self: Arc<Self>) {
        let manager = self.clone();
        std::thread::Builder::new()
            .name("refhir-registry".into())
            .spawn(move || manager.watch_registry())
            .expect("registry thread");
        if self.cfg.role != Role::Replica {
            let manager = self.clone();
            std::thread::Builder::new()
                .name("refhir-purge".into())
                .spawn(move || loop {
                    std::thread::sleep(PURGE_EVERY);
                    manager.purge_due();
                })
                .expect("purge thread");
            let manager = self;
            std::thread::Builder::new()
                .name("refhir-storage".into())
                .spawn(move || loop {
                    manager.measure_storage();
                    std::thread::sleep(manager.cfg.tenants.storage_check_interval);
                })
                .expect("storage thread");
        }
    }

    /// Reads the registry again whenever it is announced as changed, and every few seconds.
    fn watch_registry(self: Arc<Self>) {
        let changes = self.shared.watch_registry();
        let mut last = String::new();
        loop {
            changes.wait(REGISTRY_POLL);
            match self.shared.registry_signature() {
                // Unchanged, unless a tenant that failed to open is due another try.
                Ok(signature) if signature == last && !self.retry_due() => continue,
                Ok(signature) => last = signature,
                Err(err) => {
                    tracing::warn!("cannot read the tenant registry: {err}");
                    continue;
                }
            }
            self.sync_registry();
        }
    }

    /// Notes that tenant `name` failed to open once more, and when it may be tried again.
    fn note_failure(&self, name: &str) {
        let mut failed = self.failed.lock().expect("failed");
        let attempts = failed.get(name).map_or(0, |(attempts, _)| *attempts) + 1;
        failed.insert(
            name.to_string(),
            (attempts, std::time::Instant::now() + retry_delay(attempts)),
        );
    }

    fn forget_failures(&self, name: &str) {
        self.failed.lock().expect("failed").remove(name);
    }

    /// Whether tenant `name` may be tried: it has not failed, or its wait is over.
    fn may_try(&self, name: &str) -> bool {
        self.failed
            .lock()
            .expect("failed")
            .get(name)
            .is_none_or(|(_, again)| *again <= std::time::Instant::now())
    }

    /// Whether some tenant that failed to open is due another try.
    fn retry_due(&self) -> bool {
        let now = std::time::Instant::now();
        self.failed
            .lock()
            .expect("failed")
            .values()
            .any(|(_, again)| *again <= now)
    }

    /// Opens what the registry has and this process does not serve, and stops what it no longer
    /// has.
    fn sync_registry(self: &Arc<Self>) {
        let tenants = match self.shared.list_tenants() {
            Ok(tenants) => tenants,
            Err(err) => {
                tracing::warn!("cannot read the tenant registry: {err}");
                return;
            }
        };
        let listed: HashMap<String, Tenant> = tenants
            .into_iter()
            .map(|tenant| (tenant.name.clone(), tenant))
            .collect();
        let known: Vec<String> = self
            .entries
            .lock()
            .expect("entries")
            .keys()
            .cloned()
            .collect();
        for name in known {
            match listed.get(&name) {
                Some(tenant) if !tenant.is_deleted() => {}
                Some(_) => self.stop(&name, TenantState::Deleted),
                None => {
                    self.stop(&name, TenantState::Deleted);
                    self.host.remove(&name);
                    self.entries.lock().expect("entries").remove(&name);
                }
            }
        }
        for (name, tenant) in listed {
            if tenant.is_deleted() {
                if self.host.state(&name).is_none() {
                    self.host
                        .set(&name, TenantState::Deleted, TenantLimits::default());
                }
                continue;
            }
            let wanted = {
                let mut entries = self.entries.lock().expect("entries");
                let entry = entries.entry(name.clone()).or_default();
                let fresh =
                    entry.opened.is_none() && self.may_try(&name) && entry.busy.try_lock().is_ok();
                if fresh {
                    entry.cancelled.store(false, Ordering::SeqCst);
                }
                fresh
            };
            if wanted {
                let manager = self.clone();
                std::thread::Builder::new()
                    .name(format!("refhir-open-{name}"))
                    .spawn(move || {
                        if let Err(err) = manager.ensure_open(&name, &tenant) {
                            tracing::error!("tenant `{name}` cannot be served: {err}");
                        }
                    })
                    .expect("opener thread");
            }
        }
    }

    fn settings_of(tenant: &Tenant) -> TenantSettings {
        serde_json::from_value(tenant.settings.clone()).unwrap_or_else(|err| {
            tracing::warn!(
                "tenant `{}` has settings this release does not read ({err}); serving it with none",
                tenant.name
            );
            TenantSettings::default()
        })
    }

    fn limits_of(&self, settings: &TenantSettings) -> TenantLimits {
        settings.limits.or(&self.cfg.tenants.limits)
    }

    /// Opens tenant `name` unless it is open, and waits until it is. Blocks.
    fn ensure_open(self: &Arc<Self>, name: &str, tenant: &Tenant) -> Result<(), String> {
        let (busy, cancelled) = {
            let mut entries = self.entries.lock().expect("entries");
            let entry = entries.entry(name.to_string()).or_default();
            (entry.busy.clone(), entry.cancelled.clone())
        };
        let _busy = busy.lock().unwrap_or_else(|p| p.into_inner());
        if self.opened(name).is_some() {
            return Ok(());
        }
        let settings = Self::settings_of(tenant);
        let limits = self.limits_of(&settings);
        self.host.set(name, TenantState::Starting, limits.clone());
        match self.open(name, &settings, &limits, &cancelled) {
            Ok(Some(opened)) => {
                let opened = Arc::new(opened);
                self.host
                    .set(name, TenantState::Ready(opened.app.clone()), limits);
                if let Some(entry) = self.entries.lock().expect("entries").get_mut(name) {
                    entry.opened = Some(opened);
                }
                self.forget_failures(name);
                tracing::info!("serving tenant {name}");
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(err) => {
                self.host
                    .set_state(name, TenantState::Unavailable(err.clone()));
                self.note_failure(name);
                Err(err)
            }
        }
    }

    fn opened(&self, name: &str) -> Option<Arc<Opened>> {
        self.entries
            .lock()
            .expect("entries")
            .get(name)
            .and_then(|entry| entry.opened.clone())
    }

    /// Builds tenant `name`'s server for this process's role. `None` when it was cancelled while
    /// it waited (it was deleted, or the process is stopping).
    fn open(
        self: &Arc<Self>,
        name: &str,
        settings: &TenantSettings,
        limits: &TenantLimits,
        cancelled: &AtomicBool,
    ) -> Result<Option<Opened>, String> {
        let share = limits.connections.unwrap_or(self.default_share);
        let persistence = PostgresPersistence::open_tenant(&self.shared, name, share)
            .map_err(|err| err.to_string())?;
        let store: Arc<dyn ConformanceStore> = Arc::new(persistence.clone());
        let auth_store: Arc<dyn AuthStore> = Arc::new(persistence.clone());
        let as_persistence: Arc<dyn Persistence> = Arc::new(persistence.clone());
        let replica = self.cfg.role == Role::Replica;
        let mut watcher = None;
        let mut lease_guard = ReleaseLease(None);
        let (db, catalog) = if replica {
            let mut attempts = 0u64;
            let db = loop {
                if cancelled.load(Ordering::SeqCst) {
                    return Ok(None);
                }
                match Database::open_read_only(as_persistence.clone()) {
                    Ok(db)
                        if store
                            .load_packages()
                            .is_ok_and(|packages| !packages.is_empty()) =>
                    {
                        break Arc::new(db)
                    }
                    _ => {}
                }
                if attempts % 10 == 0 {
                    tracing::info!("tenant `{name}` waits for its primary");
                }
                attempts += 1;
                std::thread::sleep(Duration::from_secs(1));
            };
            let _permit = self.opens.acquire();
            let (loaded, version) = Catalog::open_consistent(store.as_ref(), CatalogClock::Stable)
                .map_err(|err| err.to_string())?;
            let catalog = Arc::new(RwLock::new(loaded));
            db.start_refresher(crate::REPLICA_NOTIFIED_REFRESH);
            watcher = Some(refhir_fhir::watch_catalog(
                store.clone(),
                &catalog,
                version,
                crate::CATALOG_POLL,
                CatalogClock::Stable,
            ));
            (db, catalog)
        } else {
            {
                let host = self.host.clone();
                let manager = Arc::downgrade(self);
                let tenant = name.to_string();
                persistence.on_writer_lease_lost(move |reason| {
                    tracing::error!("tenant `{tenant}` lost its writer lease: {reason}");
                    if let Some(manager) = manager.upgrade() {
                        let tenant = tenant.clone();
                        // Not on the lease thread, which checks the other tenants.
                        std::thread::spawn(move || {
                            manager.stop(
                                &tenant,
                                TenantState::Unavailable("another process took it over".into()),
                            );
                            manager.reopen_later(&tenant);
                        });
                    } else {
                        host.set_state(
                            &tenant,
                            TenantState::Unavailable("another process took it over".into()),
                        );
                    }
                });
            }
            let mut said = false;
            loop {
                if cancelled.load(Ordering::SeqCst) {
                    return Ok(None);
                }
                match persistence
                    .try_acquire_writer_lease()
                    .map_err(|err| err.to_string())?
                {
                    Lease::Held { .. } => break,
                    Lease::Busy => {
                        if !said {
                            tracing::info!(
                                "tenant `{name}`: another process holds its writer lease; waiting \
                                 for it"
                            );
                            self.host.set_state(
                                name,
                                TenantState::Unavailable(
                                    "another process holds its writer lease".into(),
                                ),
                            );
                            said = true;
                        }
                        std::thread::sleep(LEASE_RETRY);
                    }
                }
            }
            // From here a failure gives the lease back, and the permit bounds the heavy part.
            lease_guard = ReleaseLease(Some(persistence.clone()));
            let _permit = self.opens.acquire();
            if self.cfg.role == Role::Standby {
                // Holding a tenant's lease makes this process its primary, and so the one that
                // changes tenants.
                self.host.set_role("primary");
            }
            let db = Arc::new(Database::open(as_persistence).map_err(|err| err.to_string())?);
            let catalog = Arc::new(RwLock::new(
                Catalog::open(store.as_ref()).map_err(|err| err.to_string())?,
            ));
            catalog.write().expect("catalog").set_rebuild_in_place(true);
            let prepared = (|| -> Result<(), Box<dyn std::error::Error>> {
                crate::config::check_release_matches_database(
                    release_from_plan(&self.cfg.packages),
                    store.as_ref(),
                )?;
                refhir_fhir::collect_superseded(store.as_ref(), &db, &catalog);
                let PackagePlan::Bundled(release, mut extra) = self.cfg.packages.clone();
                extra.extend(settings.packages.iter().cloned());
                crate::install_packages(
                    store.as_ref(),
                    &db,
                    &catalog,
                    &PackagePlan::Bundled(release, extra),
                )?;
                refhir_fhir::run_backfills(store.as_ref(), &db, &catalog)?;
                refhir_fhir::repair_conformance(store.as_ref(), &db, &catalog)?;
                Ok(())
            })();
            catalog
                .write()
                .expect("catalog")
                .set_rebuild_in_place(false);
            if let Err(err) = prepared {
                persistence.release_writer_lease();
                return Err(err.to_string());
            }
            (db, catalog)
        };
        let policy = refhir_fhir::ReferencePolicy::parse(&self.cfg.references)
            .ok_or_else(|| "--references must be strict or fhir".to_string())?;
        {
            let mut current = catalog.write().expect("catalog");
            current.set_reference_policy(policy);
            current.set_update_create(settings.update_as_create || self.cfg.update_as_create);
        }
        let dcr = tenant_secret(&self.dcr_secret, name, "smart-dcr-token");
        let confidential = tenant_secret(&self.confidential_secret, name, "smart-confidential");
        let smart = if settings.smart {
            let tables = if replica {
                let tables = refhir_server::SmartStore::open_reader(auth_store.clone());
                wait_for(name, cancelled, "its SMART signing key", || {
                    tables.init_signing_key()
                })?;
                tables
            } else {
                let tables =
                    refhir_server::SmartStore::try_open(auth_store.clone(), confidential.clone())
                        .map_err(|err| err.to_string())?;
                tables.init_signing_key()?;
                tables
            };
            Some(Arc::new(tables))
        } else {
            None
        };
        let health_cards = if settings.health_cards {
            let key = if replica {
                wait_for(name, cancelled, "its health card key", || {
                    refhir_server::IssuerKey::for_tenant(auth_store.as_ref(), false).map(|_| ())
                })?;
                refhir_server::IssuerKey::for_tenant(auth_store.as_ref(), false)?
            } else {
                refhir_server::IssuerKey::for_tenant(auth_store.as_ref(), true)?
            };
            Some(Arc::new(key))
        } else {
            None
        };
        let export_dir = PathBuf::from(&self.cfg.export_dir).join(name);
        let config = refhir_server::ServerConfig {
            auth: settings.smart,
            export_dir: export_dir.to_string_lossy().into_owned(),
            terminology_urls: self.cfg.terminology_urls.clone(),
            sdc_archiver: self.cfg.sdc_archiver,
            public_url: self.routing.public_url(name),
            smart_patient: self.cfg.smart_patient.clone(),
            smart_encounter: self.cfg.smart_encounter.clone(),
            smart_fhir_user: self.cfg.smart_fhir_user.clone(),
            smart_dcr_token: Some(dcr),
            smart_confidential_secret: Some(confidential),
            oidc: self.cfg.oidc.clone(),
            smart_demo_login: self.cfg.smart_demo_login,
            health_cards,
            smart,
            primary_url: self.cfg.primary_url.clone(),
            tenant: Some(name.to_string()),
            max_staleness: Duration::from_secs(5),
            ready_timeout: Duration::from_secs(2),
            ready_cache: Duration::from_secs(1),
            min_ts_wait: Duration::from_millis(250),
            sync_token_recheck: Duration::from_secs(30),
            default_page_size: self.cfg.default_page_size,
            jobs: Some(refhir_server::SharedJobStore::new(Arc::new(
                persistence.clone(),
            ))),
            governance: self.cfg.governance.clone(),
            governance_store: Some(refhir_server::SharedGovernanceStore::new(Arc::new(
                persistence.clone(),
            ))),
        };
        // Opened: the lease is the tenant's now.
        lease_guard.0 = None;
        // The server starts tasks on the runtime.
        let _enter = self.runtime.enter();
        let app = refhir_server::tenant_app(db.clone(), catalog.clone(), store, config);
        Ok(Some(Opened {
            persistence,
            db,
            catalog,
            app: Arc::new(app),
            watcher: Mutex::new(watcher),
        }))
    }

    /// Stops serving tenant `name`, which then answers as `state` says: ends its sockets and
    /// background work, writes what it buffered, and gives up its writer lease.
    fn stop(&self, name: &str, state: TenantState) {
        let (busy, cancelled) = {
            let entries = self.entries.lock().expect("entries");
            match entries.get(name) {
                Some(entry) => (entry.busy.clone(), entry.cancelled.clone()),
                None => {
                    drop(entries);
                    self.host.set_state(name, state);
                    return;
                }
            }
        };
        // An opener waiting for the lease gives up, and then lets go of `busy`.
        cancelled.store(true, Ordering::SeqCst);
        self.forget_failures(name);
        let _busy = busy.lock().unwrap_or_else(|p| p.into_inner());
        // No new requests from here on.
        self.host.set_state(name, state);
        let opened = self
            .entries
            .lock()
            .expect("entries")
            .get_mut(name)
            .and_then(|entry| entry.opened.take());
        if let Some(opened) = opened {
            self.runtime.block_on(opened.app.shut_down());
            if let Some(watcher) = opened.watcher.lock().expect("watcher").take() {
                watcher.stop();
            }
            opened.persistence.release_writer_lease();
            tracing::info!("stopped serving tenant {name}");
        }
    }

    /// A tenant that lost its lease is opened again, which waits for the lease as a standby does.
    fn reopen_later(self: &Arc<Self>, name: &str) {
        let tenant = match self.shared.get_tenant(name) {
            Ok(Some(tenant)) if !tenant.is_deleted() => tenant,
            _ => return,
        };
        if let Some(entry) = self.entries.lock().expect("entries").get_mut(name) {
            entry.cancelled.store(false, Ordering::SeqCst);
        }
        if let Err(err) = self.ensure_open(name, &tenant) {
            tracing::error!("tenant `{name}` cannot be served: {err}");
        }
    }

    /// Removes the rows of the deleted tenants whose grace period is over.
    fn purge_due(&self) {
        let due = match self.shared.tenants_due_for_purge() {
            Ok(due) => due,
            Err(err) => {
                tracing::warn!("cannot look for deleted tenants: {err}");
                return;
            }
        };
        for name in due {
            self.purge(&name);
        }
    }

    fn purge(&self, name: &str) -> Option<u64> {
        self.stop(name, TenantState::Deleted);
        match self.shared.purge_tenant(name) {
            Ok(rows) => {
                tracing::info!("removed deleted tenant {name}: {rows} rows");
                self.host.remove(name);
                self.entries.lock().expect("entries").remove(name);
                Some(rows)
            }
            Err(err) => {
                tracing::warn!("deleted tenant `{name}` not removed yet: {err}");
                None
            }
        }
    }

    fn measure_storage(&self) {
        let opened: Vec<(String, Arc<Opened>)> = self
            .entries
            .lock()
            .expect("entries")
            .iter()
            .filter_map(|(name, entry)| entry.opened.clone().map(|opened| (name.clone(), opened)))
            .collect();
        for (name, opened) in opened {
            match opened.persistence.storage_usage() {
                Ok(usage) => self.host.set_storage(&name, usage.bytes()),
                Err(err) => tracing::warn!("cannot measure what tenant `{name}` stores: {err}"),
            }
        }
    }

    /// The JSON the admin API answers with for `tenant`.
    fn describe(&self, tenant: &Tenant) -> Value {
        let mut body = json!({
            "name": tenant.name,
            "id": tenant.id,
            "created_at": tenant.created_at,
            "status": if tenant.is_deleted() { "deleted" } else { "active" },
            "settings": tenant.settings,
            "primary_running": tenant.primary_running,
            "public_url": self.routing.public_url(&tenant.name),
        });
        if let Some(at) = &tenant.deleted_at {
            body["deleted_at"] = json!(at);
            body["purge_after"] = json!(tenant.purge_after);
        }
        if let Some(opened) = self.opened(&tenant.name) {
            let store: &dyn ConformanceStore = &opened.persistence;
            if let Ok(packages) = store.load_packages() {
                let mut packages: Vec<Value> = packages
                    .into_iter()
                    .map(|package| json!({"name": package.name, "version": package.version}))
                    .collect();
                packages.sort_by_key(|package| package.to_string());
                body["packages"] = json!(packages);
            }
            body["latest_ts"] = json!(opened.db.latest_ts().to_string());
        }
        body
    }

    /// The SMART credentials of tenant `name`, which only the admin API's create hands out.
    fn credentials(&self, name: &str) -> Value {
        json!({
            "registration_token": tenant_secret(&self.dcr_secret, name, "smart-dcr-token"),
            "confidential_client_id": "confidential",
            "confidential_client_secret": tenant_secret(&self.confidential_secret, name, "smart-confidential"),
        })
    }

    fn admin_error(err: refhir_persistence::PersistError) -> AdminError {
        let text = err.to_string();
        if text.contains("is not a tenant name") {
            AdminError::invalid(text)
        } else {
            AdminError::internal(text)
        }
    }

    /// Writes every resource of tenant `name` out before it is deleted; the file written.
    fn export_before_delete(&self, name: &str, opened: &Opened) -> Result<PathBuf, String> {
        let dir = PathBuf::from(&self.cfg.export_dir).join(name);
        std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        let path = dir.join(format!("deleted-{stamp}.ndjson"));
        refhir_server::export_everything(&opened.db, &opened.catalog, &path)?;
        Ok(path)
    }

    /// Tells the configured hook that tenant `name` was deleted. Its answer is reported, never
    /// a reason to undo the deletion.
    fn call_hook(&self, body: &Value) -> Option<Value> {
        let hook = self.cfg.tenants.delete_hook.as_deref()?;
        let sent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(10))
            .build()
            .post(hook)
            .set("content-type", "application/json")
            .send_string(&body.to_string());
        Some(match sent {
            Ok(response) => json!({"status": response.status()}),
            Err(ureq::Error::Status(status, _)) => {
                tracing::warn!("the tenant delete hook answered {status}");
                json!({"status": status})
            }
            Err(err) => {
                tracing::warn!("the tenant delete hook could not be reached: {err}");
                json!({"error": err.to_string()})
            }
        })
    }
}

/// Waits for something the tenant's primary makes, unless the tenant is cancelled meanwhile.
fn wait_for<T>(
    name: &str,
    cancelled: &AtomicBool,
    what: &str,
    mut attempt: impl FnMut() -> Result<T, String>,
) -> Result<T, String> {
    let mut attempts = 0u64;
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(err) => {
                if cancelled.load(Ordering::SeqCst) {
                    return Err(format!("stopped while waiting for {what}"));
                }
                if attempts % 10 == 0 {
                    tracing::info!("tenant `{name}` waits for {what}: {err}");
                }
                attempts += 1;
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

impl TenantAdmin for Manager {
    fn create(&self, request: CreateTenant) -> Result<(Value, bool), AdminError> {
        refhir_postgres::validate_tenant_name(&request.name)
            .map_err(|err| AdminError::invalid(err.to_string()))?;
        let settings = TenantSettings {
            packages: request.packages,
            update_as_create: request.update_as_create,
            smart: request.smart.unwrap_or(self.cfg.auth),
            health_cards: request.health_cards,
            limits: request.limits,
        };
        if settings.smart && !self.login {
            return Err(AdminError::invalid(
                "SMART needs the process to sign users in: configure [smart.oidc] (or demo login \
                 for testing) and start it again",
            ));
        }
        if settings.packages.iter().any(|spec| spec.trim().is_empty()) {
            return Err(AdminError::invalid(
                "a package is name@version, a directory or a .tgz",
            ));
        }
        let wanted = serde_json::to_value(&settings).expect("settings json");
        let name = request.name;
        let (tenant, created) = match self
            .shared
            .create_tenant(&name, &wanted)
            .map_err(Self::admin_error)?
        {
            Created::New(tenant) => (tenant, true),
            Created::Existing(tenant) => {
                if tenant.is_deleted() {
                    return Err(AdminError::new(
                        StatusCode::CONFLICT,
                        format!(
                            "tenant `{name}` is deleted and waits to be removed: restore it, or \
                             create it again after {}",
                            tenant.purge_after.clone().unwrap_or_default()
                        ),
                    ));
                }
                if Self::settings_of(&tenant) != settings {
                    return Err(AdminError::new(
                        StatusCode::CONFLICT,
                        format!(
                            "tenant `{name}` exists with other settings: {}",
                            tenant.settings
                        ),
                    ));
                }
                (tenant, false)
            }
        };
        let this = self.arc();
        if let Err(err) = this.ensure_open(&name, &tenant) {
            if created {
                // A tenant that never opened (a package that cannot be installed, say) is taken
                // back, so the same request can be sent again once it is fixed.
                this.stop(&name, TenantState::Deleted);
                let _ = refhir_postgres::delete_tenant(&self.url, &name);
                this.host.remove(&name);
                this.entries.lock().expect("entries").remove(&name);
                return Err(AdminError::invalid(format!(
                    "tenant `{name}` could not be opened, and was not created: {err}"
                )));
            }
            return Err(AdminError::new(StatusCode::SERVICE_UNAVAILABLE, err));
        }
        let tenant = self
            .shared
            .get_tenant(&name)
            .map_err(Self::admin_error)?
            .unwrap_or(tenant);
        let mut body = self.describe(&tenant);
        if settings.smart {
            body["smart"] = self.credentials(&name);
        }
        Ok((body, created))
    }

    fn list(&self) -> Result<Vec<Value>, AdminError> {
        let tenants = self.shared.list_tenants().map_err(Self::admin_error)?;
        Ok(tenants.iter().map(|tenant| self.describe(tenant)).collect())
    }

    fn get(&self, name: &str) -> Result<Value, AdminError> {
        let tenant = self
            .shared
            .get_tenant(name)
            .map_err(Self::admin_error)?
            .ok_or_else(|| AdminError::not_found(name))?;
        let mut body = self.describe(&tenant);
        if let Some(opened) = self.opened(name) {
            if let Ok(usage) = opened.persistence.storage_usage() {
                self.host.set_storage(name, usage.bytes());
                body["size"] = json!({
                    "documents": usage.documents,
                    "document_bytes": usage.document_bytes,
                    "index_rows": usage.index_rows,
                    "index_bytes": usage.index_bytes,
                    "bytes": usage.bytes(),
                });
            }
        }
        Ok(body)
    }

    fn delete(&self, name: &str, request: DeleteTenant) -> Result<Value, AdminError> {
        let tenant = self
            .shared
            .get_tenant(name)
            .map_err(Self::admin_error)?
            .ok_or_else(|| AdminError::not_found(name))?;
        if tenant.is_deleted() {
            return Ok(self.describe(&tenant));
        }
        let grace = request.grace.unwrap_or(self.cfg.tenants.delete_grace);
        let export = request.export.unwrap_or(self.cfg.tenants.export_on_delete);
        let this = self.arc();
        // No new requests from here on; those being answered finish against the open tenant.
        let previous = self.host.set_state(name, TenantState::Deleted);
        // What is being answered finishes before the tenant is written out, so what it wrote is in
        // the export (a request that arrives from now on is refused).
        if let Some(counters) = self.host.counters(name) {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while counters.in_flight() > 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let mut exported = None;
        if export {
            match self.opened(name) {
                Some(opened) => match self.export_before_delete(name, &opened) {
                    Ok(path) => exported = Some(path),
                    Err(err) => {
                        if let Some(previous) = previous {
                            self.host.set_state(name, previous);
                        }
                        return Err(AdminError::internal(format!(
                            "tenant `{name}` was not deleted: writing its resources out failed: \
                             {err}"
                        )));
                    }
                },
                None => {
                    if let Some(previous) = previous {
                        self.host.set_state(name, previous);
                    }
                    return Err(AdminError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!(
                            "tenant `{name}` is not open here, so its resources cannot be written \
                             out: try again when it is, or delete it with export=false"
                        ),
                    ));
                }
            }
        }
        let deleted = self
            .shared
            .delete_tenant(name, grace.as_secs())
            .map_err(Self::admin_error)?
            .ok_or_else(|| AdminError::not_found(name))?;
        this.stop(name, TenantState::Deleted);
        let mut body = self.describe(&deleted);
        if let Some(path) = &exported {
            body["export"] = json!({"file": path.to_string_lossy()});
        }
        let hook = self.call_hook(&json!({
            "event": "tenant.deleted",
            "tenant": name,
            "deleted_at": deleted.deleted_at,
            "purge_after": deleted.purge_after,
            "export": exported.as_ref().map(|path| path.to_string_lossy().into_owned()),
        }));
        if let Some(hook) = hook {
            body["hook"] = hook;
        }
        if grace.is_zero() {
            if let Some(rows) = this.purge(name) {
                body["status"] = json!("removed");
                body["removed_rows"] = json!(rows);
            }
        }
        Ok(body)
    }

    fn restore(&self, name: &str) -> Result<Value, AdminError> {
        let tenant = self
            .shared
            .restore_tenant(name)
            .map_err(Self::admin_error)?
            .ok_or_else(|| AdminError::not_found(name))?;
        let this = self.arc();
        if let Some(entry) = self.entries.lock().expect("entries").get_mut(name) {
            entry.cancelled.store(false, Ordering::SeqCst);
        }
        this.ensure_open(name, &tenant)
            .map_err(|err| AdminError::new(StatusCode::SERVICE_UNAVAILABLE, err))?;
        Ok(self.describe(&tenant))
    }
}

impl Manager {
    /// `self` as the `Arc` it lives in (the admin API holds it as one).
    fn arc(&self) -> Arc<Self> {
        self.me.upgrade().expect("the manager outlives its calls")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn at_most_the_permitted_number_of_tenants_open_at_once() {
        let permits = Arc::new(Permits::new(2));
        let (running, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let (permits, running, peak) = (permits.clone(), running.clone(), peak.clone());
                std::thread::spawn(move || {
                    let _permit = permits.acquire();
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(30));
                    running.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "two ran together, never three"
        );
        // Every permit came back: all of them can be taken again at once.
        let (first, second) = (permits.acquire(), permits.acquire());
        drop((first, second));
    }

    #[test]
    fn a_tenant_that_failed_to_open_is_tried_again_after_a_growing_wait() {
        let waits: Vec<u64> = (1..=9).map(|n| retry_delay(n).as_secs()).collect();
        assert_eq!(waits, [5, 10, 20, 40, 80, 160, 300, 300, 300]);
        // The wait never overflows however many times it failed.
        assert_eq!(retry_delay(u32::MAX), RETRY_LONGEST);
        assert_eq!(retry_delay(0), RETRY_FIRST);
    }

    #[test]
    fn a_standby_or_replica_that_serves_every_tenant_needs_both_secrets_pinned() {
        let some = |text: &str| Some(text.to_string());
        for role in [Role::Standby, Role::Replica] {
            assert!(check_secrets_pinned(role, &None, &None).is_err());
            assert!(check_secrets_pinned(role, &some("a"), &None).is_err());
            assert!(check_secrets_pinned(role, &None, &some("b")).is_err());
            assert!(check_secrets_pinned(role, &some(""), &some("b")).is_err());
            assert!(check_secrets_pinned(role, &some("a"), &some("b")).is_ok());
        }
        let said = check_secrets_pinned(Role::Replica, &None, &None).unwrap_err();
        assert!(
            said.contains("--smart-dcr-token") && said.contains("replica"),
            "{said}"
        );
        // A primary may be the only process, and then makes (and warns about) its own.
        assert!(check_secrets_pinned(Role::Primary, &None, &None).is_ok());
    }
}
