use std::env;
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use haifhir_database::Database;
use haifhir_persistence::Persistence;
use haifhir_postgres::PostgresPersistence;
use haifhir_sqlite::SqlitePersistence;
use tokio::net::TcpListener;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match serve(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("haifhir: {err}");
            ExitCode::from(1)
        }
    }
}

#[tokio::main]
async fn serve(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let mut sqlite: Option<String> = None;
    let mut postgres: Option<String> = None;
    let mut bind = "127.0.0.1:8080".to_string();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                println!(
                    "haifhir [--sqlite path] [--postgres url] [--bind 127.0.0.1:8080]\n\
                     Default database is haifhir.sqlite."
                );
                return Ok(());
            }
            "--sqlite" => {
                index += 1;
                sqlite = Some(required(&args, index, "--sqlite")?);
            }
            "--postgres" => {
                index += 1;
                postgres = Some(required(&args, index, "--postgres")?);
            }
            "--bind" => {
                index += 1;
                bind = required(&args, index, "--bind")?;
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
        index += 1;
    }
    if postgres.is_some() && sqlite.is_some() {
        return Err("pass either --sqlite or --postgres".into());
    }

    tracing_subscriber::fmt::init();
    let persistence: Arc<dyn Persistence> = if let Some(url) = postgres {
        Arc::new(PostgresPersistence::connect(&url)?)
    } else {
        let path = sqlite.unwrap_or_else(|| "haifhir.sqlite".to_string());
        Arc::new(SqlitePersistence::open(Path::new(&path))?)
    };
    let db = Arc::new(Database::open(persistence)?);
    let addr: SocketAddr = bind.parse()?;
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    tracing::info!("listening on http://{local}");
    haifhir_server::serve(db, listener).await?;
    Ok(())
}

fn required(args: &[String], index: usize, flag: &str) -> Result<String, String> {
    args.get(index)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}
