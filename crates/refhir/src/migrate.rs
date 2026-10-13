//! `refhir migrate --status | --dry-run`: what the database holds and what startup would do to
//! it. Both only read; the server applies migrations itself when it starts.

use std::path::Path;

use crate::config::{apply_env, discover_config_path, load_file, ResolvedConfig};

pub const HELP: &str = "\
refhir migrate --status | --dry-run [--config path] [--sqlite path | --postgres url]

Reports the schema version of the database the server would open (the same file, environment
and flags as `refhir`) and what starting this build would do to it. Nothing is changed, and a
SQLite file that does not exist is not created.

  --status    the versions recorded in the database, and which this build would apply
  --dry-run   the same, plus the SQL that would run

The server applies pending migrations itself at startup, in one transaction. Migrations are
not reversible: take a backup first. Exits 1 when this build would refuse to start, for
example because the database was migrated by a newer ReFHIR.";

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

/// Runs the subcommand. `Err` is printed by the caller and exits 1.
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut mode = None;
    let mut config_path = None;
    let mut sqlite = None;
    let mut postgres = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(());
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
            "--sqlite" => sqlite = Some(take(args, &mut index, "--sqlite")?),
            "--postgres" => postgres = Some(take(args, &mut index, "--postgres")?),
            other => {
                return Err(format!("unknown argument {other} (see refhir migrate --help)").into())
            }
        }
        index += 1;
    }
    let Some(mode) = mode else {
        return Err("pass --status or --dry-run (see refhir migrate --help)".into());
    };

    // The same sources as `refhir`, in the same order: file, then environment, then flags.
    let mut cfg = ResolvedConfig::defaults();
    if let Some(path) = discover_config_path(config_path.as_deref()) {
        cfg.apply_file(&load_file(&path)?)?;
    }
    apply_env(&mut cfg)?;
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
    if cfg.postgres.is_some() && (cfg.sqlite.is_some() || cfg.memory) {
        return Err("pass only one of --sqlite or --postgres".into());
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
    if report.can_start() {
        Ok(())
    } else {
        Err("this build would refuse to start on this database".into())
    }
}
