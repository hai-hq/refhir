mod config;

use std::env;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, RwLock};

use config::{
    apply_env, check_release_matches_database, discover_config_path, load_file, release_from_plan,
    FhirRelease, PackagePlan, ResolvedConfig,
};
use refhir_database::Database;
use refhir_fhir::Catalog;
use refhir_persistence::{AuthStore, ConformanceStore, JobStore, Persistence};
use refhir_postgres::PostgresPersistence;
use refhir_sqlite::SqlitePersistence;
use tokio::net::TcpListener;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match serve(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("refhir: {err}");
            ExitCode::from(1)
        }
    }
}

/// One backend serves every table the server keeps: the log, the conformance tables, the
/// SMART tables and the background jobs.
type Stores = (
    Arc<dyn Persistence>,
    Arc<dyn ConformanceStore>,
    Arc<dyn AuthStore>,
    Arc<dyn JobStore>,
);

#[tokio::main]
async fn serve(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let mut cfg = ResolvedConfig::defaults();
    let mut config_path: Option<std::path::PathBuf> = None;
    let mut cli_packages = Vec::new();
    let mut cli_sqlite: Option<Option<String>> = None;
    let mut cli_memory = false;
    let mut cli_postgres: Option<Option<String>> = None;
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

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                print_help();
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
                cli_sqlite = Some(Some(required(&args, index, "--sqlite")?));
            }
            "--memory" => cli_memory = true,
            "--postgres" => {
                index += 1;
                cli_postgres = Some(Some(required(&args, index, "--postgres")?));
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
            other => return Err(format!("unknown argument {other}").into()),
        }
        index += 1;
    }

    let file_path = discover_config_path(config_path.as_deref());
    if let Some(path) = &file_path {
        let file = load_file(path)?;
        cfg.apply_file(&file)?;
    }
    apply_env(&mut cfg);

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

    if cfg.postgres.is_some() && (cfg.sqlite.is_some() || cfg.memory) {
        return Err("pass only one of --sqlite, --memory, or --postgres".into());
    }
    if cfg.sqlite.is_some() && cfg.memory {
        return Err("pass only one of --sqlite, --memory, or --postgres".into());
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

    ensure_package_cache_env(&cfg);

    let (db, store, auth_store, job_store): Stores = if let Some(url) = &cfg.postgres {
        let persistence = Arc::new(PostgresPersistence::connect(url)?);
        (
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
            persistence,
        )
    } else {
        let path = cfg.sqlite.unwrap_or_else(|| "refhir.sqlite".to_string());
        let persistence = Arc::new(SqlitePersistence::open(Path::new(&path))?);
        (
            persistence.clone(),
            persistence.clone(),
            persistence.clone(),
            persistence,
        )
    };
    // Nothing listens until this is done, so a slow start is a refused connection. Each phase
    // says how long it took, so that a wait can be put down to the package download, the
    // install, or the catalog.
    let phase = std::time::Instant::now();
    let db = Arc::new(Database::open(db)?);
    let catalog = Arc::new(RwLock::new(Catalog::open(store.as_ref())?));
    tracing::info!(target: "refhir", elapsed_ms = phase.elapsed().as_millis() as u64, "database and catalog opened");
    check_release_matches_database(release_from_plan(&cfg.packages), store.as_ref())?;
    let phase = std::time::Instant::now();
    install_packages(store.as_ref(), &db, &catalog, &cfg.packages)?;
    tracing::info!(target: "refhir", elapsed_ms = phase.elapsed().as_millis() as u64, "packages installed");

    let phase = std::time::Instant::now();
    refhir_fhir::run_backfills(store.as_ref(), &db, &catalog)?;
    refhir_fhir::repair_conformance(store.as_ref(), &db, &catalog)?;
    tracing::info!(target: "refhir", elapsed_ms = phase.elapsed().as_millis() as u64, "search index backfills and conformance repair done");
    let unexpandable = refhir_fhir::unexpandable_value_sets(&catalog);
    if !unexpandable.is_empty() {
        tracing::info!(
            target: "refhir",
            count = unexpandable.len(),
            "value sets cannot be expanded with the installed terminology; POST ValueSet/$expandability lists them with the reason for each"
        );
    }
    let policy = refhir_fhir::ReferencePolicy::parse(&cfg.references)
        .ok_or_else(|| format!("--references must be strict or fhir"))?;
    {
        let mut current = catalog.write().expect("catalog");
        current.set_reference_policy(policy);
        current.set_update_create(cfg.update_as_create);
    }
    let addr: SocketAddr = cfg.bind.parse()?;

    let mut smart_dcr_token = cfg.smart_dcr_token;
    let mut smart_confidential_secret = cfg.smart_confidential_secret;
    let mut smart = None;
    if cfg.auth {
        fill_smart_credentials(&mut smart_dcr_token, &mut smart_confidential_secret)?;
        // OAuth clients, codes, tokens and the signing key live in the database.
        let secret = smart_confidential_secret.clone().unwrap_or_default();
        let tables = refhir_server::SmartStore::open(auth_store, secret);
        tables.init_signing_key()?;
        smart = Some(Arc::new(tables));
    }
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    tracing::info!("listening on http://{local}");
    refhir_server::serve(
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
            // Export jobs are rows in the same database, so they outlive a restart.
            jobs: Some(refhir_server::SharedJobStore::new(job_store)),
        },
    )
    .await?;
    Ok(())
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
        "refhir [--config path] [--sqlite path] [--memory] [--postgres url] [--bind 127.0.0.1:8080]\n\
         [--package dir|name@version] [--references strict|fhir] [--smart]\n\
         [--public-url url] [--smart-patient id] [--smart-encounter id]\n\
         [--smart-fhir-user Resource/id] [--smart-dcr-token token] [--update-as-create]\n\
         [--smart-confidential-secret secret]\n\
         [--oidc-issuer url --oidc-client-id id] [--smart-demo-login]\n\
         [--export-dir path] [--terminology url] [--trace-engine] [--sdc-archiver]\n\
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
         --sdc-archiver stores QuestionnaireResponse without allowing a later read."
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
