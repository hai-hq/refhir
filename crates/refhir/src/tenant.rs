//! `refhir tenant create NAME | list | delete NAME --yes`: the tenants of a Postgres database that
//! many tenants share. The server never creates or removes one: a process started with a tenant the
//! database does not have refuses to run.

use std::path::PathBuf;
use std::process::ExitCode;

use crate::config::{database_value, discover_config_path, load_file, ResolvedConfig};

pub const HELP: &str = "\
refhir tenant create NAME | list | delete NAME --yes [--config path] [--postgres url]

A Postgres database can hold many tenants. Each is a separate FHIR server with its own resources,
SMART clients and audit trail; they share the database's tables and schema. A process serves one
tenant, chosen with `refhir --tenant NAME`. The database is made shared by the first `create`: an
empty database becomes one, and a database that already holds a ReFHIR without tenants cannot (a
database keeps its layout).

  create NAME   add a tenant. A name is lowercase letters, digits and `-`, up to 63 characters
  list          the tenants, and whether a primary of each is running
  delete NAME   remove a tenant and everything it holds. It needs --yes, and it refuses while a
                primary of the tenant runs: stop the tenant's processes first. It cannot be
                undone, so take a backup first

The database is the one `refhir` would open: the same file, environment and flags.

Exit codes: 0 done; 1 refused or failed; 2 the arguments were wrong.";

enum Command {
    Create(String),
    List,
    Delete(String),
}

struct Parsed {
    command: Command,
    config_path: Option<PathBuf>,
    postgres: Option<String>,
    yes: bool,
}

/// The value after a flag.
fn take(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn parse(args: &[String]) -> Result<Option<Parsed>, String> {
    let mut words: Vec<String> = Vec::new();
    let mut config_path = None;
    let mut postgres = None;
    let mut yes = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => return Ok(None),
            "--yes" => yes = true,
            "--config" => config_path = Some(PathBuf::from(take(args, &mut index, "--config")?)),
            "--postgres" => {
                if postgres.is_some() {
                    return Err("pass --postgres once".into());
                }
                postgres = Some(database_value(
                    "--postgres",
                    take(args, &mut index, "--postgres")?,
                )?);
            }
            other if other.starts_with('-') => {
                return Err(format!(
                    "unknown argument {other} (see refhir tenant --help)"
                ))
            }
            other => words.push(other.to_string()),
        }
        index += 1;
    }
    let command = match words.as_slice() {
        [verb, name] if verb == "create" => Command::Create(name.clone()),
        [verb, name] if verb == "delete" => Command::Delete(name.clone()),
        [verb] if verb == "list" => Command::List,
        [verb] if verb == "create" || verb == "delete" => {
            return Err(format!("`{verb}` needs a tenant name"))
        }
        [] => return Err("pass create, list or delete (see refhir tenant --help)".into()),
        _ => return Err("too many arguments (see refhir tenant --help)".into()),
    };
    Ok(Some(Parsed {
        command,
        config_path,
        postgres,
        yes,
    }))
}

/// Runs the subcommand: 0 done, 1 refused or failed, 2 for wrong arguments.
pub fn run(args: &[String]) -> ExitCode {
    let parsed = match parse(args) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => {
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("refhir: {err}");
            return ExitCode::from(2);
        }
    };
    match execute(parsed) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("refhir: {err}");
            ExitCode::from(1)
        }
    }
}

/// The database, from the same sources as `refhir`: the file, then the environment, then the
/// flag. Only a Postgres database holds tenants.
fn database_url(config_path: Option<PathBuf>, flag: Option<String>) -> Result<String, String> {
    let mut cfg = ResolvedConfig::defaults();
    if let Some(path) = discover_config_path(config_path.as_deref()) {
        let file = load_file(&path).map_err(|err| err.to_string())?;
        cfg.apply_file(&file)?;
    }
    // Only the database setting is read from the environment, as in `refhir migrate`: the rest of
    // the settings have nothing to do with looking at the tenants.
    if let Some(url) = std::env::var("REFHIR_POSTGRES_URL")
        .ok()
        .filter(|url| !url.is_empty())
    {
        cfg.postgres = Some(url);
        cfg.memory = false;
        cfg.sqlite = None;
    }
    if let Some(url) = flag {
        cfg.postgres = Some(url);
        cfg.memory = false;
        cfg.sqlite = None;
    }
    cfg.postgres.ok_or_else(|| {
        "tenants live in a Postgres database: pass --postgres, or set REFHIR_POSTGRES_URL or \
         [database] postgres_url"
            .to_string()
    })
}

fn execute(parsed: Parsed) -> Result<(), Box<dyn std::error::Error>> {
    let Parsed {
        command,
        config_path,
        postgres,
        yes,
    } = parsed;
    // The URL carries the password, so it is never printed.
    let url = database_url(config_path, postgres)?;
    match command {
        Command::Create(name) => {
            let tenant = refhir_postgres::create_tenant(&url, &name)?;
            println!("created tenant {} (id {})", tenant.name, tenant.id);
            println!(
                "start a process for it with: refhir --tenant {}",
                tenant.name
            );
        }
        Command::List => {
            let tenants = refhir_postgres::list_tenants(&url)?;
            if tenants.is_empty() {
                println!("no tenants");
                return Ok(());
            }
            let width = tenants
                .iter()
                .map(|tenant| tenant.name.len())
                .max()
                .unwrap_or(4)
                .max(4);
            println!(
                "{:<width$}  {:>4}  {:<20}  PRIMARY",
                "NAME", "ID", "CREATED"
            );
            for tenant in tenants {
                println!(
                    "{:<width$}  {:>4}  {:<20}  {}",
                    tenant.name,
                    tenant.id,
                    tenant.created_at,
                    if tenant.primary_running {
                        "running"
                    } else {
                        "-"
                    }
                );
            }
        }
        Command::Delete(name) => {
            if !yes {
                return Err(format!(
                    "deleting tenant `{name}` removes everything it holds and cannot be undone: \
                     take a backup, then run it again with --yes"
                )
                .into());
            }
            let removed = refhir_postgres::delete_tenant(&url, &name)?;
            println!("deleted tenant {name}: {removed} rows removed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn the_verbs_and_their_arguments_are_checked() {
        let parsed = parse(&args(&["create", "acme"])).unwrap().unwrap();
        assert!(matches!(parsed.command, Command::Create(name) if name == "acme"));
        let parsed = parse(&args(&["list", "--postgres", "host=x"]))
            .unwrap()
            .unwrap();
        assert!(matches!(parsed.command, Command::List));
        assert_eq!(parsed.postgres.as_deref(), Some("host=x"));
        let parsed = parse(&args(&["delete", "acme", "--yes"])).unwrap().unwrap();
        assert!(matches!(parsed.command, Command::Delete(_)) && parsed.yes);

        assert!(parse(&args(&["--help"])).unwrap().is_none());
        for bad in [
            args(&[]),
            args(&["create"]),
            args(&["delete"]),
            args(&["list", "extra"]),
            args(&["create", "a", "b"]),
            args(&["rename", "a"]),
            args(&["list", "--nope"]),
            args(&["list", "--postgres"]),
            args(&["list", "--postgres", "a", "--postgres", "b"]),
        ] {
            assert!(parse(&bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_delete_without_yes_removes_nothing() {
        // No database is reached: the refusal comes before one is opened. (The database URL is
        // resolved first, so one is given; it is never connected to.)
        let parsed = Parsed {
            command: Command::Delete("acme".into()),
            config_path: None,
            postgres: Some("host=127.0.0.1 port=1 dbname=none".into()),
            yes: false,
        };
        let error = execute(parsed).unwrap_err().to_string();
        assert!(error.contains("--yes"), "{error}");
    }
}
