mod config;
mod migrate;

use std::env;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, RwLock};

use config::{
    apply_env, check_release_matches_database, database_value, discover_config_path, load_file,
    release_from_plan, FhirRelease, PackagePlan, ResolvedConfig, Role,
};
use refhir_database::Database;
use refhir_fhir::{Catalog, CatalogClock};
use refhir_persistence::{
    AuthStore, ConformanceStore, GovernanceStore, JobStore, Lease, Persistence,
};
use refhir_postgres::{ConnectOptions, PostgresPersistence};
use refhir_sqlite::{SqliteOptions, SqlitePersistence};
use std::time::Duration;
use tokio::net::TcpListener;

/// Database connections to open when `--db-pool` is not given.
const DEFAULT_DB_POOL: usize = 8;
/// The refusal for a database named twice.
const ONE_DATABASE: &str = "pass only one of --sqlite, --memory, or --postgres";
/// How often a standby asks for the writer lease.
const STANDBY_POLL: Duration = Duration::from_secs(1);
/// How often a replica moves its snapshot up to the writer's watermark.
const REPLICA_REFRESH: Duration = Duration::from_millis(25);
/// How often a replica looks anyway when its backend announces publishes, in case it missed one.
const REPLICA_NOTIFIED_REFRESH: Duration = Duration::from_millis(500);
/// How often a replica checks whether the catalog changed.
const CATALOG_POLL: Duration = Duration::from_millis(250);

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("migrate") {
        return migrate::run(&args[1..]);
    }
    match serve(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("refhir: {err}");
            ExitCode::from(1)
        }
    }
}

/// One backend serves every table the server keeps: the log, the conformance tables, the
/// SMART tables, the background jobs, and the usage, request log and audit tables.
type Stores = (
    Arc<dyn Persistence>,
    Arc<dyn ConformanceStore>,
    Arc<dyn AuthStore>,
    Arc<dyn JobStore>,
    Arc<dyn GovernanceStore>,
);

/// Startup runs on this thread, outside the runtime: the synchronous Postgres client refuses
/// to block on a thread that belongs to one. The runtime only serves requests and runs the
/// small bootstrap server; `enter` lets startup spawn onto it.
fn serve(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _enter = runtime.enter();
    let mut cfg = ResolvedConfig::defaults();
    let mut config_path: Option<std::path::PathBuf> = None;
    let mut cli_packages = Vec::new();
    let mut cli_sqlite: Option<Option<String>> = None;
    let mut cli_memory = false;
    let mut cli_postgres: Option<Option<String>> = None;
    // How many times the command line names a database, whichever flag does.
    let mut cli_databases = 0;
    let mut cli_bind: Option<String> = None;
    let mut cli_references: Option<String> = None;
    let mut cli_auth = false;
    let mut cli_public_url: Option<Option<String>> = None;
    let mut cli_smart_patient: Option<Option<String>> = None;
    let mut cli_smart_encounter: Option<Option<String>> = None;
    let mut cli_smart_fhir_user: Option<Option<String>> = None;
    let mut cli_smart_dcr_token: Option<Option<String>> = None;
    let mut cli_smart_confidential_secret: Option<Option<String>> = None;
    let mut cli_demo_login = false;
    let mut cli_oidc_issuer: Option<String> = None;
    let mut cli_oidc_client_id: Option<String> = None;
    let mut cli_export_dir: Option<String> = None;
    let mut cli_terminology_urls: Option<Vec<String>> = None;
    let mut cli_trace_engine = false;
    let mut cli_sdc_archiver = false;
    let mut cli_update_as_create = false;
    let mut cli_role: Option<Role> = None;
    let mut cli_primary_url: Option<String> = None;
    let mut cli_db_pool: Option<usize> = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            "--version" | "-V" => {
                println!("refhir {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--config" => {
                index += 1;
                config_path = Some(std::path::PathBuf::from(required(
                    &args, index, "--config",
                )?));
            }
            "--sqlite" => {
                index += 1;
                cli_sqlite = Some(Some(database_value(
                    "--sqlite",
                    required(&args, index, "--sqlite")?,
                )?));
                cli_databases += 1;
            }
            "--memory" => {
                cli_memory = true;
                cli_databases += 1;
            }
            "--postgres" => {
                index += 1;
                cli_postgres = Some(Some(database_value(
                    "--postgres",
                    required(&args, index, "--postgres")?,
                )?));
                cli_databases += 1;
            }
            "--package" => {
                index += 1;
                cli_packages.push(required(&args, index, "--package")?);
            }
            "--bind" => {
                index += 1;
                cli_bind = Some(required(&args, index, "--bind")?);
            }
            "--references" => {
                index += 1;
                cli_references = Some(required(&args, index, "--references")?);
            }
            "--smart" => cli_auth = true,
            "--public-url" => {
                index += 1;
                cli_public_url = Some(Some(required(&args, index, "--public-url")?));
            }
            "--smart-patient" => {
                index += 1;
                cli_smart_patient = Some(Some(required(&args, index, "--smart-patient")?));
            }
            "--smart-encounter" => {
                index += 1;
                cli_smart_encounter = Some(Some(required(&args, index, "--smart-encounter")?));
            }
            "--smart-fhir-user" => {
                index += 1;
                cli_smart_fhir_user = Some(Some(required(&args, index, "--smart-fhir-user")?));
            }
            "--smart-dcr-token" => {
                index += 1;
                cli_smart_dcr_token = Some(Some(required(&args, index, "--smart-dcr-token")?));
            }
            "--smart-confidential-secret" => {
                index += 1;
                cli_smart_confidential_secret =
                    Some(Some(required(&args, index, "--smart-confidential-secret")?));
            }
            "--smart-demo-login" => cli_demo_login = true,
            "--oidc-issuer" => {
                index += 1;
                cli_oidc_issuer = Some(required(&args, index, "--oidc-issuer")?);
            }
            "--oidc-client-id" => {
                index += 1;
                cli_oidc_client_id = Some(required(&args, index, "--oidc-client-id")?);
            }
            "--sdc-archiver" => cli_sdc_archiver = true,
            "--update-as-create" => cli_update_as_create = true,
            "--export-dir" => {
                index += 1;
                cli_export_dir = Some(required(&args, index, "--export-dir")?);
            }
            "--terminology" => {
                index += 1;
                let url = required(&args, index, "--terminology")?;
                cli_terminology_urls.get_or_insert_with(Vec::new).push(url);
            }
            "--trace-engine" => cli_trace_engine = true,
            "--role" => {
                index += 1;
                cli_role = Some(Role::parse(&required(&args, index, "--role")?)?);
            }
            "--standby" => cli_role = Some(Role::Standby),
            "--primary-url" => {
                index += 1;
                cli_primary_url = Some(required(&args, index, "--primary-url")?);
            }
            "--db-pool" => {
                index += 1;
                let size = required(&args, index, "--db-pool")?;
                cli_db_pool = Some(
                    size.parse::<usize>()
                        .ok()
                        .filter(|size| *size > 0)
                        .ok_or_else(|| {
                            format!("--db-pool must be a positive number, not `{size}`")
                        })?,
                );
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
        index += 1;
    }

    // Checked here, on the flags alone: once they are applied below each one clears the other two,
    // so the last would win and the check after them could never see two. One named twice is as
    // ambiguous as two kinds.
    if cli_databases > 1 {
        return Err(ONE_DATABASE.into());
    }

    let file_path = discover_config_path(config_path.as_deref());
    let mut unknown_sections = Vec::new();
    if let Some(path) = &file_path {
        let file = load_file(path)?;
        cfg.apply_file(&file)?;
        unknown_sections = file.unknown.keys().cloned().collect();
    }
    apply_env(&mut cfg)?;

    if let Some(v) = cli_sqlite {
        cfg.sqlite = v;
        cfg.memory = false;
        cfg.postgres = None;
    }
    if cli_memory {
        cfg.memory = true;
        cfg.sqlite = None;
        cfg.postgres = None;
    }
    if let Some(v) = cli_postgres {
        cfg.postgres = v;
        cfg.memory = false;
        cfg.sqlite = None;
    }
    if !cli_packages.is_empty() {
        cfg.packages = match cfg.packages {
            PackagePlan::Bundled(release, mut igs) => {
                igs.extend(cli_packages);
                PackagePlan::Bundled(release, igs)
            }
        };
    }
    if let Some(v) = cli_bind {
        cfg.bind = v;
    }
    if let Some(v) = cli_references {
        cfg.references = v;
    }
    if cli_auth {
        cfg.auth = true;
    }
    if let Some(v) = cli_public_url {
        cfg.public_url = v;
    }
    if let Some(v) = cli_smart_patient {
        cfg.smart_patient = v;
    }
    if let Some(v) = cli_smart_encounter {
        cfg.smart_encounter = v;
    }
    if let Some(v) = cli_smart_fhir_user {
        cfg.smart_fhir_user = v;
    }
    if let Some(v) = cli_smart_dcr_token {
        cfg.smart_dcr_token = v;
    }
    if let Some(v) = cli_smart_confidential_secret {
        cfg.smart_confidential_secret = v;
    }
    if cli_demo_login {
        cfg.smart_demo_login = true;
    }
    if cli_oidc_issuer.is_some() || cli_oidc_client_id.is_some() {
        let oidc = cfg
            .oidc
            .get_or_insert_with(|| refhir_server::OidcConfig::new("", ""));
        if let Some(v) = cli_oidc_issuer {
            oidc.issuer = v;
        }
        if let Some(v) = cli_oidc_client_id {
            oidc.client_id = v;
        }
    }
    if let Some(v) = cli_export_dir {
        cfg.export_dir = v;
    }
    if let Some(urls) = cli_terminology_urls {
        cfg.terminology_urls = urls;
    }
    if cli_trace_engine {
        cfg.trace_engine = true;
    }
    if cli_sdc_archiver {
        cfg.sdc_archiver = true;
    }
    if cli_update_as_create {
        cfg.update_as_create = true;
    }
    if let Some(role) = cli_role {
        cfg.role = role;
    }
    if cli_primary_url.is_some() {
        cfg.primary_url = cli_primary_url;
    }
    if cli_db_pool.is_some() {
        cfg.db_pool = cli_db_pool;
    }

    // A limit that cannot work is refused now, and not discovered by the first caller to hit it.
    cfg.governance.validate(cfg.auth)?;
    // A backstop. Every source clears the other two kinds when it sets one, so none leaves two set
    // today: a settings file that names two quietly resolves to one (Postgres over SQLite, and
    // `kind = "memory"` over a path). This catches a new setter that forgets to clear them.
    if cfg.postgres.is_some() && (cfg.sqlite.is_some() || cfg.memory) {
        return Err(ONE_DATABASE.into());
    }
    if cfg.sqlite.is_some() && cfg.memory {
        return Err(ONE_DATABASE.into());
    }

    let default_filter = if cfg.trace_engine {
        "info,refhir_server=info,refhir_engine=debug"
    } else {
        "info,refhir_server=info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter)),
        )
        .init();
    if let Some(path) = &file_path {
        tracing::info!("loaded config {}", path.display());
    }
    for section in unknown_sections {
        tracing::warn!(
            "config section [{section}] is not one this server reads and is ignored; \
             check its spelling"
        );
    }
    if cfg.governance.proxy.trusted_hops > 0 && cfg.governance.proxy.trusted_peers.is_empty() {
        tracing::warn!(
            "[server] trusted_proxy_hops is set without trusted_proxy_peers, so X-Forwarded-For \
             is believed on every connection: a client that reaches this server directly can \
             choose, or frame, an address. List the networks your proxies connect from"
        );
    }
    if cfg.trace_engine {
        tracing::info!("reactive engine tracing enabled (target refhir_engine=debug)");
    }

    if cfg.auth {
        // Settings that can be checked up front are, before any package install.
        let loopback = cfg.bind.parse::<SocketAddr>()?.ip().is_loopback();
        check_login(&cfg, loopback)?;
        if cfg.public_url.is_none() && !loopback {
            return Err("--smart on a non-loopback bind requires --public-url".into());
        }
    }

    check_role(&cfg)?;
    ensure_package_cache_env(&cfg);
    tracing::info!("{}", cfg.describe_governance());
    if cfg.governance.audit.enabled && cfg.governance.audit.chain_key.is_none() {
        tracing::warn!(
            "the audit trail is hash-chained but the chain has no key (REFHIR_AUDIT_KEY): it \
             catches mistakes, and proves nothing against someone who can write the database. \
             Set a key (`openssl rand -hex 32`) kept outside the database"
        );
    }
    if cfg.memory && cfg.governance.audit.enabled {
        tracing::warn!(
            "audit is on but the database is in memory, so the audit trail ends with this process"
        );
    }

    let role = cfg.role;
    let addr: SocketAddr = cfg.bind.parse()?;
    // Every role starts answering at once, on the port the real server will use: `/livez` says
    // the process is up, and `/readyz` and `/healthz` say 503 until it can take traffic. A
    // primary would otherwise refuse connections while it installs packages and backfills, which
    // a probe cannot tell from a crash.
    let std_listener = std::net::TcpListener::bind(addr)?;
    std_listener.set_nonblocking(true)?;
    let probe = std_listener.try_clone()?;
    let detail = match role {
        Role::Primary => "opening the database, installing packages and backfilling search indexes",
        Role::Standby => "waiting for the writer lease",
        Role::Replica => "waiting for the primary's schema and catalog",
    };
    let bootstrap = refhir_server::serve_bootstrap(probe, role.name(), detail)?;
    let listener = TcpListener::from_std(std_listener)?;
    tracing::info!("{} listening on http://{addr}, not ready yet", role.name());

    let pool_size = cfg.db_pool.unwrap_or(DEFAULT_DB_POOL);
    let (persistence, store, auth_store, job_store, governance_store): Stores =
        if let Some(url) = &cfg.postgres {
            let options = ConnectOptions {
                pool_size,
                // A replica never runs DDL: it waits for the primary to have created the tables.
                create_schema: role != Role::Replica,
            };
            let persistence = Arc::new(PostgresPersistence::connect_with(url, &options)?);
            (
                persistence.clone(),
                persistence.clone(),
                persistence.clone(),
                persistence.clone(),
                persistence,
            )
        } else if cfg.memory {
            let persistence = Arc::new(SqlitePersistence::open_in_memory()?);
            (
                persistence.clone(),
                persistence.clone(),
                persistence.clone(),
                persistence.clone(),
                persistence,
            )
        } else {
            let path = cfg
                .sqlite
                .clone()
                .unwrap_or_else(|| "refhir.sqlite".to_string());
            let options = SqliteOptions {
                read_connections: cfg
                    .db_pool
                    .unwrap_or(SqliteOptions::default().read_connections),
            };
            let persistence = Arc::new(SqlitePersistence::open_with(Path::new(&path), &options)?);
            (
                persistence.clone(),
                persistence.clone(),
                persistence.clone(),
                persistence.clone(),
                persistence,
            )
        };

    // A standby loads the catalog now, while it waits, and keeps it current. Loading the full
    // R4 catalog takes seconds, and doing that only after the lease is won would leave the
    // service without a writer for all of them.
    let warm = if role == Role::Standby {
        // The latest revision, not the stable version a replica follows: after a takeover this
        // process may have to finish a backfill, so it needs the pending parameters too.
        match Catalog::open_consistent(store.as_ref(), CatalogClock::Latest) {
            Ok((loaded, version)) => {
                let catalog = Arc::new(RwLock::new(loaded));
                let watcher = refhir_fhir::watch_catalog(
                    store.clone(),
                    &catalog,
                    version,
                    CATALOG_POLL,
                    CatalogClock::Latest,
                );
                Some((catalog, watcher))
            }
            Err(err) => {
                tracing::warn!("standby could not load the catalog while waiting: {err}");
                None
            }
        }
    } else {
        None
    };

    let (db, catalog, smart_tables) = if role == Role::Replica {
        start_replica(&cfg, persistence, store.clone(), auth_store)?
    } else {
        // Becoming the writer comes first: `Database::open` reads the newest timestamp, which
        // is only final once no other process can still commit.
        loop {
            match persistence.try_acquire_writer_lease()? {
                Lease::Held { epoch } => {
                    tracing::info!("{} holds the writer lease (epoch {epoch})", role.name());
                    break;
                }
                Lease::Busy if role == Role::Standby => {
                    std::thread::sleep(STANDBY_POLL);
                }
                Lease::Busy => {
                    return Err(
                        "another process holds the writer lease. Start this one with \
                                --role standby to wait for it, or stop the other primary"
                            .into(),
                    );
                }
            }
        }
        let started = std::time::Instant::now();
        let db = Arc::new(Database::open(persistence)?);
        let opened = started.elapsed();
        let catalog = match warm {
            // The previous primary may have changed the catalog after the last poll: catch up,
            // which costs nothing when it did not.
            Some((catalog, watcher)) => {
                let version = watcher.stop();
                refhir_fhir::reload_catalog(
                    store.as_ref(),
                    &catalog,
                    version,
                    CatalogClock::Latest,
                )?;
                catalog
            }
            None => Arc::new(RwLock::new(Catalog::open(store.as_ref())?)),
        };
        let loaded = started.elapsed();
        check_release_matches_database(release_from_plan(&cfg.packages), store.as_ref())?;
        install_packages(store.as_ref(), &db, &catalog, &cfg.packages)?;
        let installed = started.elapsed();

        refhir_fhir::run_backfills(store.as_ref(), &db, &catalog)?;
        let backfilled = started.elapsed();
        refhir_fhir::repair_conformance(store.as_ref(), &db, &catalog)?;
        let repaired = started.elapsed();
        tracing::info!(
            "startup after the lease: {repaired:.1?} (database {opened:.1?}, catalog {:.1?}, \
             packages {:.1?}, backfills {:.1?}, repair {:.1?})",
            loaded.saturating_sub(opened),
            installed.saturating_sub(loaded),
            backfilled.saturating_sub(installed),
            repaired.saturating_sub(backfilled)
        );
        (db, catalog, auth_store)
    };
    let unexpandable = refhir_fhir::unexpandable_value_sets(&catalog);
    if !unexpandable.is_empty() {
        tracing::info!(
            target: "refhir",
            count = unexpandable.len(),
            "value sets cannot be expanded with the installed terminology; POST ValueSet/$expandability lists them with the reason for each"
        );
    }
    let policy = refhir_fhir::ReferencePolicy::parse(&cfg.references)
        .ok_or_else(|| "--references must be strict or fhir".to_string())?;
    {
        let mut current = catalog.write().expect("catalog");
        current.set_reference_policy(policy);
        current.set_update_create(cfg.update_as_create);
    }

    let mut smart_dcr_token = cfg.smart_dcr_token;
    let mut smart_confidential_secret = cfg.smart_confidential_secret;
    let mut smart = None;
    if cfg.auth {
        fill_smart_credentials(&mut smart_dcr_token, &mut smart_confidential_secret)?;
        // OAuth clients, codes, tokens and the signing key live in the database.
        let tables = if role == Role::Replica {
            // The primary owns the built-in clients and the signing key; wait for the key.
            let tables = refhir_server::SmartStore::open_reader(smart_tables);
            wait_for_signing_key(&tables);
            tables
        } else {
            let secret = smart_confidential_secret.clone().unwrap_or_default();
            let tables = refhir_server::SmartStore::try_open(smart_tables, secret)?;
            tables.init_signing_key()?;
            tables
        };
        smart = Some(Arc::new(tables));
    }
    runtime.block_on(bootstrap.stop());
    let local = listener.local_addr()?;
    tracing::info!("listening on http://{local}");
    runtime.block_on(refhir_server::serve(
        db,
        catalog,
        store,
        listener,
        refhir_server::ServerConfig {
            auth: cfg.auth,
            export_dir: cfg.export_dir,
            terminology_urls: cfg.terminology_urls,
            sdc_archiver: cfg.sdc_archiver,
            public_url: cfg.public_url,
            smart_patient: cfg.smart_patient,
            smart_encounter: cfg.smart_encounter,
            smart_fhir_user: cfg.smart_fhir_user,
            smart_dcr_token,
            smart_confidential_secret,
            oidc: cfg.oidc,
            smart_demo_login: cfg.smart_demo_login,
            smart,
            primary_url: cfg.primary_url,
            max_staleness: Duration::from_secs(5),
            ready_timeout: Duration::from_secs(2),
            ready_cache: Duration::from_secs(1),
            min_ts_wait: Duration::from_millis(250),
            sync_token_recheck: Duration::from_secs(30),
            default_page_size: cfg.default_page_size,
            // Export jobs are rows in the same database, so they outlive a restart.
            jobs: Some(refhir_server::SharedJobStore::new(job_store)),
            // So are usage, the request log and the audit trail.
            governance: cfg.governance,
            governance_store: Some(refhir_server::SharedGovernanceStore::new(governance_store)),
        },
    ))?;
    Ok(())
}

/// Settings that make no sense for a role, or that would make several processes disagree.
fn check_role(cfg: &ResolvedConfig) -> Result<(), Box<dyn std::error::Error>> {
    let role = cfg.role;
    if role == Role::Primary {
        return Ok(());
    }
    let name = role.name();
    if cfg.memory {
        return Err(
            format!("--memory is private to one process, so it cannot run as a {name}").into(),
        );
    }
    if role == Role::Replica && cfg.postgres.is_none() {
        return Err(
            "a replica reads the database the primary writes from another process, which needs \
             --postgres (SQLite can only be shared on one machine, by a standby)"
                .into(),
        );
    }
    if cfg.auth {
        // Each process would otherwise invent its own, and a process that restarts would
        // replace the confidential secret every other process is using.
        let pinned = |value: &Option<String>| value.as_deref().is_some_and(|text| !text.is_empty());
        if !pinned(&cfg.smart_dcr_token) || !pinned(&cfg.smart_confidential_secret) {
            return Err(format!(
                "a {name} needs --smart-dcr-token and --smart-confidential-secret (or \
                 REFHIR_SMART_DCR_TOKEN and REFHIR_SMART_CONFIDENTIAL_SECRET) set to the same \
                 values on every process, so they agree on the built-in client and the \
                 registration token"
            )
            .into());
        }
    }
    if role == Role::Replica && cfg.primary_url.is_none() {
        tracing::warn!(
            "no --primary-url: this replica cannot forward writes, and answers them with 503"
        );
    }
    Ok(())
}

type ReplicaParts = (Arc<Database>, Arc<RwLock<Catalog>>, Arc<dyn AuthStore>);

/// A replica has no committer. It waits until the primary has published a timestamp and
/// installed its packages, then follows the primary's watermark and catalog version.
fn start_replica(
    cfg: &ResolvedConfig,
    persistence: Arc<dyn Persistence>,
    store: Arc<dyn ConformanceStore>,
    auth_store: Arc<dyn AuthStore>,
) -> Result<ReplicaParts, Box<dyn std::error::Error>> {
    let persistence_notifies = persistence.publishes_notifications();
    let mut attempts = 0u64;
    let db = loop {
        let waiting_for = match Database::open_read_only(persistence.clone()) {
            Ok(db) => {
                if store.load_packages()?.is_empty() {
                    "the primary to install its packages".to_string()
                } else {
                    break Arc::new(db);
                }
            }
            Err(err) => err.to_string(),
        };
        if attempts % 10 == 0 {
            tracing::info!("replica waiting for {waiting_for}");
        }
        attempts += 1;
        std::thread::sleep(Duration::from_secs(1));
    };
    check_release_matches_database(release_from_plan(&cfg.packages), store.as_ref())?;
    // A replica follows the stable version: it never loads a parameter that is still pending.
    let (loaded, version) = Catalog::open_consistent(store.as_ref(), CatalogClock::Stable)?;
    let catalog = Arc::new(RwLock::new(loaded));
    // With a notification channel the refresher wakes the moment the writer publishes, so the
    // timer is only a safety net for a missed notice.
    db.start_refresher(if persistence_notifies {
        REPLICA_NOTIFIED_REFRESH
    } else {
        REPLICA_REFRESH
    });
    refhir_fhir::watch_catalog(store, &catalog, version, CATALOG_POLL, CatalogClock::Stable);
    Ok((db, catalog, auth_store))
}

/// The primary creates the signing key. A replica that starts first waits for it.
fn wait_for_signing_key(tables: &refhir_server::SmartStore) {
    let mut attempts = 0u64;
    loop {
        match tables.init_signing_key() {
            Ok(()) => return,
            Err(err) => {
                if attempts % 10 == 0 {
                    tracing::info!("replica waiting for the signing key: {err}");
                }
                attempts += 1;
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// SMART needs a way to sign users in: an OpenID Connect provider, or explicit demo login.
fn check_login(cfg: &ResolvedConfig, loopback: bool) -> Result<(), Box<dyn std::error::Error>> {
    match (&cfg.oidc, cfg.smart_demo_login) {
        (Some(_), true) => Err("choose one login: [smart.oidc] or --smart-demo-login, not both".into()),
        (Some(oidc), false) => {
            oidc.validate()?;
            if cfg.public_url.is_none() && oidc.redirect_uri.is_none() && !loopback {
                return Err("[smart.oidc] on a non-loopback bind requires --public-url or oidc.redirect_uri".into());
            }
            Ok(())
        }
        (None, true) => {
            if loopback {
                tracing::warn!("SMART demo login is on: any username is accepted with no credential check");
            } else if env_flag("REFHIR_ALLOW_REMOTE_DEMO_LOGIN") {
                tracing::error!(
                    "SMART demo login is on and the server is reachable beyond loopback (REFHIR_ALLOW_REMOTE_DEMO_LOGIN): anyone who can connect can sign in as any user and read patient data"
                );
            } else {
                return Err("SMART demo login accepts any username, so it is only allowed on a loopback bind. Configure [smart.oidc], or set REFHIR_ALLOW_REMOTE_DEMO_LOGIN=1 for a throwaway test server".into());
            }
            Ok(())
        }
        (None, false) => Err(
            "SMART is enabled but no login is configured: set [smart.oidc] (issuer, client_id, client_secret) \
             for real sign-in, or pass --smart-demo-login for testing only"
                .into(),
        ),
    }
}

fn ensure_package_cache_env(cfg: &ResolvedConfig) {
    if env::var("REFHIR_PACKAGE_CACHE")
        .ok()
        .is_some_and(|value| !value.is_empty())
    {
        return;
    }
    let cache = if cfg.memory || cfg.postgres.is_some() {
        Path::new(".refhir").join("packages")
    } else {
        let file = cfg
            .sqlite
            .clone()
            .unwrap_or_else(|| "refhir.sqlite".to_string());
        Path::new(&file)
            .parent()
            .map(|parent| parent.join("package-cache"))
            .unwrap_or_else(|| Path::new("package-cache").to_path_buf())
    };
    env::set_var("REFHIR_PACKAGE_CACHE", cache);
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .ok()
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

fn skip_release_defaults() -> bool {
    env_flag("REFHIR_SKIP_DEFAULT_PACKAGES")
}

fn install_release_defaults(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &Arc<RwLock<Catalog>>,
    release: FhirRelease,
) -> Result<(), Box<dyn std::error::Error>> {
    if skip_release_defaults() {
        tracing::debug!(target: "refhir", "skipping release default packages (REFHIR_SKIP_DEFAULT_PACKAGES)");
        return Ok(());
    }
    match release {
        FhirRelease::R4 => {
            let specs = refhir_fhir::default_r4_registry_specs();
            // A database that already has this release's packages is not a fresh service:
            // no registry or cache access at all.
            if refhir_fhir::packages_installed(store, &specs)? {
                tracing::info!(target: "refhir", "FHIR R4 release packages already installed");
                return Ok(());
            }
            for spec in specs {
                if let Err(err) = refhir_fhir::install_package(store, db, catalog, spec) {
                    // Only a failed download is fixed by changing the source; any other error
                    // (a conflict, an invalid package, the database) is reported as it is.
                    if !is_download_error(&err) {
                        return Err(err.into());
                    }
                    // There is no bundled copy to fall back on: say where it looked.
                    let source = refhir_fhir::NpmRegistry::from_env()
                        .map(|registry| registry.base_url)
                        .unwrap_or_else(|_| "the configured package registry".into());
                    return Err(format!(
                        "cannot install the default FHIR packages ({spec}) from {source}: {err}. \
                         Point [conformance].package_registry (or REFHIR_PACKAGE_REGISTRY) at a \
                         reachable registry, an HTTPS mirror, or s3://bucket/prefix"
                    )
                    .into());
                }
            }
        }
    }
    Ok(())
}

/// A package that could not be fetched: not found, refused, or unreachable.
fn is_download_error(err: &refhir_fhir::FhirError) -> bool {
    use refhir_fhir::FhirError;
    match err {
        FhirError::NotFound(message) => message.starts_with("package not found at "),
        FhirError::Storage(message) | FhirError::Invalid(message) => {
            message.starts_with("download ")
        }
        _ => false,
    }
}

fn install_packages(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &Arc<RwLock<Catalog>>,
    plan: &PackagePlan,
) -> Result<(), Box<dyn std::error::Error>> {
    let PackagePlan::Bundled(release, extra) = plan;
    install_release_defaults(store, db, catalog, *release)?;
    for spec in extra {
        refhir_fhir::install_package(store, db, catalog, spec)?;
    }
    Ok(())
}

fn print_help() {
    println!(
        "refhir [--config path] [--sqlite path | --memory | --postgres url] [--bind 127.0.0.1:8080]\n\
         [--package dir|name@version] [--references strict|fhir] [--smart]\n\
         [--public-url url] [--smart-patient id] [--smart-encounter id]\n\
         [--smart-fhir-user Resource/id] [--smart-dcr-token token] [--update-as-create]\n\
         [--smart-confidential-secret secret]\n\
         [--oidc-issuer url --oidc-client-id id] [--smart-demo-login]\n\
         [--export-dir path] [--terminology url] [--trace-engine] [--sdc-archiver]\n\
         [--role primary|standby|replica] [--standby] [--primary-url url] [--db-pool n]\n\
         refhir migrate --status | --dry-run   (schema versions; see refhir migrate --help)\n\
         refhir --version | -V                 (print the version and exit)\n\
         \n\
         Config file (optional): refhir.toml in the working directory, /etc/refhir/refhir.toml,\n\
         or REFHIR_CONFIG. CLI flags override the file. See refhir.toml.example.\n\
         \n\
         Default database is refhir.sqlite. --memory uses SQLite in RAM and\n\
         leaves no file. [fhir].version selects release defaults (r4: hl7.fhir.r4.core@4.0.1\n\
         and hl7.fhir.uv.sdc@4.0.0) downloaded from [conformance].package_registry (default\n\
         packages.fhir.org; a mirror URL or s3://bucket/prefix also works), cached and skipped\n\
         when already in the database. The server ships no packages and does not start if\n\
         that source is unreachable on a first start. --package may be repeated to add IGs (directory, .tgz,\n\
         or name@version). --references defaults to strict.\n\
         --smart enables SMART App Launch 2.2. --public-url is required when\n\
         --smart is used with a non-loopback --bind. Generated DCR and confidential\n\
         secrets print once to stderr in a terminal and are never written to logs.\n\
         --sdc-archiver stores QuestionnaireResponse without allowing a later read.\n\
         \n\
         Rate limits, quotas, the audit trail and the request log are set in the config file\n\
         ([rate_limit], [quota], [audit], [request_log], [metering]). Usage, the request log and\n\
         the audit trail are on by default; nothing is limited until a limit is configured.\n\
         GET /_audit, /_usage and /_requests read them back (system/AuditEvent.read with --smart).\n\
         \n\
         Several processes on one database: exactly one primary writes and holds a lease on the\n\
         database. --standby (--role standby) waits for that lease and takes over when the\n\
         primary is gone. --role replica reads what the primary publishes and sends writes, and\n\
         reads that need a newer snapshot, to --primary-url. /healthz is 200 when a process can\n\
         take traffic and 503 when it cannot.\n\
         \n\
         Probes, open under --smart: GET /livez is 200 while the process is up (use it for\n\
         liveness; /healthz is 503 on a standby). GET /readyz is 200 when startup is over and the\n\
         database answers, and 503 {{\"status\":\"starting\"|\"unavailable\",\"reason\":...}} otherwise.\n\
         \n\
         --db-pool sets the most Postgres connections (default 8), or, on SQLite, the read\n\
         connections next to the one writer (default 4)."
    );
}

fn fill_smart_credentials(
    dcr: &mut Option<String>,
    confidential: &mut Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let generated_dcr = dcr.as_deref().is_none_or(str::is_empty);
    let generated_secret = confidential.as_deref().is_none_or(str::is_empty);
    if generated_dcr {
        *dcr = Some(uuid::Uuid::new_v4().to_string());
    }
    if generated_secret {
        *confidential = Some(uuid::Uuid::new_v4().to_string());
    }
    if !generated_dcr && !generated_secret {
        return Ok(());
    }
    // `--role primary` is also the single-process default, so this is a warning and not a refusal.
    // A standby refuses to start without pinned values, because the one that takes over writes its
    // own over the stored ones: generated here, they stop working at the first failover.
    tracing::warn!(
        "the SMART DCR token or confidential client secret was generated at startup and changes on          every restart. Before running a standby or replica, set REFHIR_SMART_DCR_TOKEN and          REFHIR_SMART_CONFIDENTIAL_SECRET to the same values on every process, this one included"
    );
    if !std::io::stderr().is_terminal() {
        return Ok(());
    }
    if generated_dcr {
        eprintln!(
            "SMART DCR initial access token (Authorization: Bearer on POST /oauth/register): {}",
            dcr.as_deref().unwrap_or("")
        );
    }
    if generated_secret {
        eprintln!(
            "SMART confidential client_id=confidential secret={}",
            confidential.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

fn required(args: &[String], index: usize, flag: &str) -> Result<String, String> {
    args.get(index)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use refhir_fhir::FhirError;

    fn config(role: Role) -> ResolvedConfig {
        let mut cfg = ResolvedConfig::defaults();
        cfg.role = role;
        cfg.postgres = Some("postgres://example/db".into());
        cfg
    }

    #[test]
    fn roles_parse_and_unknown_ones_are_refused() {
        assert_eq!(Role::parse("primary").unwrap(), Role::Primary);
        assert_eq!(Role::parse(" Standby ").unwrap(), Role::Standby);
        assert_eq!(Role::parse("REPLICA").unwrap(), Role::Replica);
        let err = Role::parse("leader").unwrap_err();
        assert!(err.contains("primary, standby or replica"), "{err}");
    }

    #[test]
    fn a_primary_is_never_refused_by_the_role_checks() {
        let mut cfg = config(Role::Primary);
        cfg.memory = true;
        cfg.postgres = None;
        cfg.auth = true;
        assert!(check_role(&cfg).is_ok());
    }

    #[test]
    fn roles_that_share_a_database_need_one_that_can_be_shared() {
        let mut memory = config(Role::Standby);
        memory.postgres = None;
        memory.memory = true;
        let err = check_role(&memory).unwrap_err().to_string();
        assert!(err.contains("private to one process"), "{err}");

        // A standby can share a SQLite file on one machine; a replica needs Postgres.
        let mut sqlite = config(Role::Standby);
        sqlite.postgres = None;
        sqlite.sqlite = Some("shared.sqlite".into());
        assert!(check_role(&sqlite).is_ok());
        let mut replica = config(Role::Replica);
        replica.postgres = None;
        replica.sqlite = Some("shared.sqlite".into());
        let err = check_role(&replica).unwrap_err().to_string();
        assert!(err.contains("--postgres"), "{err}");
    }

    #[test]
    fn processes_that_serve_smart_must_agree_on_its_secrets() {
        for role in [Role::Standby, Role::Replica] {
            let mut cfg = config(role);
            cfg.auth = true;
            let err = check_role(&cfg).unwrap_err().to_string();
            assert!(err.contains("--smart-dcr-token"), "{err}");
            cfg.smart_dcr_token = Some("token".into());
            assert!(
                check_role(&cfg).is_err(),
                "the confidential secret is still unset"
            );
            cfg.smart_confidential_secret = Some("secret".into());
            assert!(check_role(&cfg).is_ok());
        }
    }

    #[test]
    fn only_download_failures_get_the_registry_advice() {
        let download = [
            FhirError::NotFound("package not found at http://x/a/1".into()),
            FhirError::Storage("download http://x/a/1: Connection refused".into()),
            FhirError::Invalid("download http://x/a/1: HTTP 403 (access denied)".into()),
        ];
        assert!(download.iter().all(is_download_error));
        let other = [
            FhirError::Invalid("search parameter Patient.x is defined twice".into()),
            FhirError::Invalid("version conflict: a needs b@2".into()),
            FhirError::Storage("database is locked".into()),
            FhirError::NotFound("resource Patient/1".into()),
        ];
        assert!(!other.iter().any(is_download_error));
    }
}
