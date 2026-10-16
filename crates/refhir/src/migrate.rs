//! `refhir migrate --status | --dry-run`: what the database holds and what startup would do to
//! it. Both only read; the server applies migrations itself when it starts.

use std::path::Path;

use std::process::ExitCode;

use crate::config::{database_value, discover_config_path, load_file, ResolvedConfig};

pub const HELP: &str = "\
refhir migrate --status | --dry-run [--config path] [--sqlite path | --postgres url]

Reports the schema version of the database the server would open (the same file, environment
and flags as `refhir`) and what starting this build would do to it. Nothing is changed, and a
SQLite file that does not exist is not created.

  --status    the versions recorded in the database, and which this build would apply
  --dry-run   the same, plus the SQL that would run

The server applies pending migrations itself at startup, in one transaction. Migrations are
not reversible: take a backup first.

Exit codes: 0 this build would start; 1 it would refuse to start (for example the database was
migrated by a newer ReFHIR); 2 the database could not be read, or the arguments were wrong.";

/// The refusal for a database named twice.
const ONE_DATABASE: &str = "pass only one of --sqlite or --postgres";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Status,
    DryRun,
}

/// The value after a flag.
fn take(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

/// Runs the subcommand: 0 if this build would start on the database, 1 if it would refuse, 2 if
/// that could not be determined.
pub fn run(args: &[String]) -> ExitCode {
    match inspect(args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("refhir: this build would refuse to start on this database");
            ExitCode::from(1)
        }
        Err(err) => {
            eprintln!("refhir: {err}");
            ExitCode::from(2)
        }
    }
}

/// Whether this build would start on the database.
fn inspect(args: &[String]) -> Result<bool, Box<dyn std::error::Error>> {
    let mut mode = None;
    let mut config_path = None;
    let mut sqlite = None;
    let mut postgres = None;
    // How many times the command line names a database, whichever flag does.
    let mut databases = 0;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(true);
            }
            "--status" | "--dry-run" if mode.is_some() => {
                return Err("pass only one of --status and --dry-run".into())
            }
            "--status" => mode = Some(Mode::Status),
            "--dry-run" => mode = Some(Mode::DryRun),
            "--config" => {
                config_path = Some(std::path::PathBuf::from(take(
                    args, &mut index, "--config",
                )?))
            }
            "--sqlite" => {
                sqlite = Some(database_value(
                    "--sqlite",
                    take(args, &mut index, "--sqlite")?,
                )?);
                databases += 1;
            }
            "--postgres" => {
                postgres = Some(database_value(
                    "--postgres",
                    take(args, &mut index, "--postgres")?,
                )?);
                databases += 1;
            }
            other => {
                return Err(format!("unknown argument {other} (see refhir migrate --help)").into())
            }
        }
        index += 1;
    }
    let Some(mode) = mode else {
        return Err("pass --status or --dry-run (see refhir migrate --help)".into());
    };
    // Checked on the flags alone: applied below, each clears the other, so the last would win. One
    // named twice is as ambiguous as both.
    if databases > 1 {
        return Err(ONE_DATABASE.into());
    }

    // The same sources as `refhir`, in the same order: file, then environment, then flags.
    let mut cfg = ResolvedConfig::defaults();
    if let Some(path) = discover_config_path(config_path.as_deref()) {
        cfg.apply_file(&load_file(&path)?)?;
    }
    // Only the database setting is read from the environment: the rest of `apply_env` validates
    // settings (role, pool size) that have nothing to do with looking at a schema.
    if let Some(url) = std::env::var("REFHIR_POSTGRES_URL")
        .ok()
        .filter(|url| !url.is_empty())
    {
        cfg.postgres = Some(url);
        cfg.memory = false;
        cfg.sqlite = None;
    }
    if let Some(path) = sqlite {
        cfg.sqlite = Some(path);
        cfg.memory = false;
        cfg.postgres = None;
    }
    if let Some(url) = postgres {
        cfg.postgres = Some(url);
        cfg.memory = false;
        cfg.sqlite = None;
    }
    // A backstop, as in `refhir`: every source clears the other kinds when it sets one, so none
    // leaves two set today. This catches a new setter that forgets to.
    if cfg.postgres.is_some() && (cfg.sqlite.is_some() || cfg.memory) {
        return Err(ONE_DATABASE.into());
    }
    if cfg.memory {
        return Err(
            "a --memory database is new at every start: there is nothing to migrate".into(),
        );
    }

    let (target, report) = match &cfg.postgres {
        // The URL carries the password, so it is not echoed.
        Some(url) => (
            "postgres".to_string(),
            refhir_postgres::migration_report(url)?,
        ),
        None => {
            let path = cfg.sqlite.as_deref().unwrap_or("refhir.sqlite");
            (
                format!("sqlite {path}"),
                refhir_sqlite::migration_report(Path::new(path))?,
            )
        }
    };
    println!("database: {target}");
    print!("{}", report.render(mode == Mode::DryRun));
    Ok(report.can_start())
}
