use std::env;
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
    let mut export_dir = "export".to_string();
    let mut terminology_urls = Vec::new();
    let mut trace_engine = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                println!(
                    "refhir [--sqlite path] [--memory] [--postgres url] [--bind 127.0.0.1:8080]\n\
                     [--package dir] [--references strict|fhir] [--smart]\n\
                     [--export-dir path] [--terminology url] [--trace-engine]\n\
                     Default database is refhir.sqlite. --memory uses SQLite in RAM and\n\
                     leaves no file. With no --package, the bundled\n\
                     hl7.fhir.r4.core 4.0.1 package is installed. --package may be repeated and\n\
                     replaces that default. --references defaults to strict."
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
        },
    )
    .await?;
    Ok(())
}

fn required(args: &[String], index: usize, flag: &str) -> Result<String, String> {
    args.get(index)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}
