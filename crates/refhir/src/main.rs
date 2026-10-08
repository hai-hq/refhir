use std::env;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, RwLock};

use refhir_database::Database;
use refhir_fhir::Catalog;
use refhir_persistence::{ConformanceStore, Persistence};
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

#[tokio::main]
async fn serve(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let mut sqlite: Option<String> = None;
    let mut memory = false;
    let mut postgres: Option<String> = None;
    let mut packages = Vec::new();
    let mut bind = "127.0.0.1:8080".to_string();
    let mut references = "strict".to_string();
    let mut auth = false;
    let mut public_url = None;
    let mut smart_patient = None;
    let mut smart_encounter = None;
    let mut smart_fhir_user = None;
    let mut smart_dcr_token = None;
    let mut smart_confidential_secret = None;
    let mut export_dir = "export".to_string();
    let mut terminology_urls = Vec::new();
    let mut trace_engine = false;
    let mut sdc_archiver = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                println!(
                    "refhir [--sqlite path] [--memory] [--postgres url] [--bind 127.0.0.1:8080]\n\
                     [--package dir] [--references strict|fhir] [--smart]\n\
                     [--public-url url] [--smart-patient id] [--smart-encounter id]\n\
                     [--smart-fhir-user Resource/id] [--smart-dcr-token token]\n\
                     [--smart-confidential-secret secret]\n\
                     [--export-dir path] [--terminology url] [--trace-engine] [--sdc-archiver]\n\
                     Default database is refhir.sqlite. --memory uses SQLite in RAM and\n\
                     leaves no file. With no --package, the bundled\n\
                     hl7.fhir.r4.core 4.0.1 package and the SDC 4.0.0 slice are installed.\n\
                     --package may be repeated and replaces that default. --references defaults to strict.\n\
                     --smart enables SMART App Launch 2.2. --public-url is required when\n\
                     --smart is used with a non-loopback --bind. Generated DCR and confidential\n\
                     secrets print once to stderr in a terminal and are never written to logs.\n\
                     --sdc-archiver stores QuestionnaireResponse without allowing a later read."
                );
                return Ok(());
            }
            "--sqlite" => {
                index += 1;
                sqlite = Some(required(&args, index, "--sqlite")?);
            }
            "--memory" => memory = true,
            "--postgres" => {
                index += 1;
                postgres = Some(required(&args, index, "--postgres")?);
            }
            "--package" => {
                index += 1;
                packages.push(required(&args, index, "--package")?);
            }
            "--bind" => {
                index += 1;
                bind = required(&args, index, "--bind")?;
            }
            "--references" => {
                index += 1;
                references = required(&args, index, "--references")?;
            }
            "--smart" => auth = true,
            "--public-url" => {
                index += 1;
                public_url = Some(required(&args, index, "--public-url")?);
            }
            "--smart-patient" => {
                index += 1;
                smart_patient = Some(required(&args, index, "--smart-patient")?);
            }
            "--smart-encounter" => {
                index += 1;
                smart_encounter = Some(required(&args, index, "--smart-encounter")?);
            }
            "--smart-fhir-user" => {
                index += 1;
                smart_fhir_user = Some(required(&args, index, "--smart-fhir-user")?);
            }
            "--smart-dcr-token" => {
                index += 1;
                smart_dcr_token = Some(required(&args, index, "--smart-dcr-token")?);
            }
            "--smart-confidential-secret" => {
                index += 1;
                smart_confidential_secret =
                    Some(required(&args, index, "--smart-confidential-secret")?);
            }
            "--sdc-archiver" => sdc_archiver = true,
            "--export-dir" => {
                index += 1;
                export_dir = required(&args, index, "--export-dir")?;
            }
            "--terminology" => {
                index += 1;
                terminology_urls.push(required(&args, index, "--terminology")?);
            }
            "--trace-engine" => trace_engine = true,
            other => return Err(format!("unknown argument {other}").into()),
        }
        index += 1;
    }
    if postgres.is_some() && (sqlite.is_some() || memory) {
        return Err("pass only one of --sqlite, --memory, or --postgres".into());
    }
    if sqlite.is_some() && memory {
        return Err("pass only one of --sqlite, --memory, or --postgres".into());
    }

    let default_filter = if trace_engine {
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
    if trace_engine {
        tracing::info!("reactive engine tracing enabled (target refhir_engine=debug)");
    }
    let (db, store): (Arc<dyn Persistence>, Arc<dyn ConformanceStore>) = if let Some(url) = postgres
    {
        let persistence = Arc::new(PostgresPersistence::connect(&url)?);
        (persistence.clone(), persistence)
    } else if memory {
        let persistence = Arc::new(SqlitePersistence::open_in_memory()?);
        (persistence.clone(), persistence)
    } else {
        let path = sqlite.unwrap_or_else(|| "refhir.sqlite".to_string());
        let persistence = Arc::new(SqlitePersistence::open(Path::new(&path))?);
        (persistence.clone(), persistence)
    };
    let db = Arc::new(Database::open(db)?);
    let catalog = Arc::new(RwLock::new(Catalog::open(store.as_ref())?));
    if packages.is_empty() {
        refhir_fhir::install_package_dir(store.as_ref(), &db, &catalog, refhir_fhir::r4_dir())?;
        refhir_fhir::install_package_dir(store.as_ref(), &db, &catalog, refhir_fhir::sdc_dir())?;
    } else {
        for path in &packages {
            refhir_fhir::install_package_dir(store.as_ref(), &db, &catalog, Path::new(path))?;
        }
    }
    refhir_fhir::run_backfills(store.as_ref(), &db, &catalog)?;
    refhir_fhir::repair_conformance(store.as_ref(), &db, &catalog)?;
    let policy = refhir_fhir::ReferencePolicy::parse(&references)
        .ok_or_else(|| format!("--references must be strict or fhir"))?;
    catalog
        .write()
        .expect("catalog")
        .set_reference_policy(policy);
    let addr: SocketAddr = bind.parse()?;
    if auth && public_url.is_none() && !addr.ip().is_loopback() {
        return Err("--smart on a non-loopback bind requires --public-url".into());
    }
    if auth {
        fill_smart_credentials(&mut smart_dcr_token, &mut smart_confidential_secret)?;
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
            auth,
            export_dir,
            terminology_urls,
            sdc_archiver,
            public_url,
            smart_patient,
            smart_encounter,
            smart_fhir_user,
            smart_dcr_token,
            smart_confidential_secret,
            smart: None,
        },
    )
    .await?;
    Ok(())
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
