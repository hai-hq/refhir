//! `refhir tenant create | list | show | delete | restore | purge`: the tenants of a Postgres
//! database that many tenants share. A process started with `--tenant` never creates or removes
//! one, and refuses to run for a tenant the database does not have. A process started with
//! `--all-tenants` does the same over HTTP (`/admin/tenants`); the two change the same registry, so
//! either can be used, and a process that serves every tenant follows what this command changes.

use std::path::PathBuf;
use std::process::ExitCode;

use crate::config::{database_value, discover_config_path, load_file, ResolvedConfig};

pub const HELP: &str = "\
refhir tenant create NAME [settings] | list | show NAME | delete NAME --yes [--grace D] [--export]
              | restore NAME | purge   [--config path] [--postgres url]

A Postgres database can hold many tenants. Each is a separate FHIR server with its own resources,
SMART clients and audit trail; they share the database's tables and schema. A process serves one
tenant (`refhir --tenant NAME`) or every one (`refhir --all-tenants`). The database is made shared
by the first `create`: an empty database becomes one, and a database that already holds a ReFHIR
without tenants cannot (a database keeps its layout).

  create NAME   add a tenant. A name is lowercase letters, digits and `-`, up to 63 characters.
                What it is created with is kept, and a process that serves every tenant serves
                it so (a process started with --tenant takes its own flags instead):
                  --package SPEC        a package to install, as --package takes it (repeat)
                  --update-as-create    PUT may create a resource under the id it names
                  --smart, --no-smart   SMART App Launch on or off (default: the process's own)
                  --health-cards        the tenant issues SMART Health Cards with a key of its own
                  --rate N [--burst N]  requests a second, per process
                  --storage BYTES       most bytes it may store
                  --connections N       most database connections it may hold at once
  list          the tenants, whether each is deleted, and whether a primary of each is running
  show NAME     one tenant, with what it was created with
  delete NAME   delete a tenant. It needs --yes. It is no longer served (at once by a process that
                serves every tenant; a process started with --tenant NAME serves it until it
                stops), and its rows are removed once its grace period is over: --grace (30s,
                15m, 24h, 7d; default [tenants] delete_grace, 7d). Until then `restore` brings it
                back. --grace 0 removes it at once, and then refuses while a primary of the tenant
                runs; that cannot be undone, so take a backup first. --export DIR writes every
                resource it holds to DIR/NAME-<time>.ndjson first
  restore NAME  bring back a deleted tenant whose rows have not been removed yet
  purge         remove the rows of every deleted tenant whose grace period is over

The database is the one `refhir` would open: the same file, environment and flags.

Exit codes: 0 done; 1 refused or failed; 2 the arguments were wrong.";

enum Command {
    Create(String),
    List,
    Show(String),
    Delete(String),
    Restore(String),
    Purge,
}

struct Parsed {
    command: Command,
    config_path: Option<PathBuf>,
    postgres: Option<String>,
    yes: bool,
    settings: CreateFlags,
    grace: Option<std::time::Duration>,
    export: Option<PathBuf>,
}

/// What `create` was asked to create the tenant with.
#[derive(Default)]
struct CreateFlags {
    packages: Vec<String>,
    update_as_create: bool,
    smart: Option<bool>,
    health_cards: bool,
    limits: refhir_server::tenancy::TenantLimits,
    /// Whether any was given, so that `create NAME` alone keeps the old meaning.
    any: bool,
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
    let mut settings = CreateFlags::default();
    let mut grace = None;
    let mut export = None;
    let number = |flag: &str, value: String| -> Result<u64, String> {
        value
            .parse::<u64>()
            .ok()
            .filter(|number| *number > 0)
            .ok_or_else(|| format!("{flag} must be a positive number, not `{value}`"))
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => return Ok(None),
            "--yes" => yes = true,
            "--package" => {
                settings.packages.push(take(args, &mut index, "--package")?);
                settings.any = true;
            }
            "--update-as-create" => {
                settings.update_as_create = true;
                settings.any = true;
            }
            "--smart" => {
                settings.smart = Some(true);
                settings.any = true;
            }
            "--no-smart" => {
                settings.smart = Some(false);
                settings.any = true;
            }
            "--health-cards" => {
                settings.health_cards = true;
                settings.any = true;
            }
            "--rate" => {
                let value = take(args, &mut index, "--rate")?;
                let rate: f64 = value
                    .parse()
                    .ok()
                    .filter(|rate: &f64| rate.is_finite() && *rate > 0.0)
                    .ok_or_else(|| format!("--rate must be a positive number, not `{value}`"))?;
                settings.limits.requests_per_second = Some(rate);
                settings.any = true;
            }
            "--burst" => {
                let value = take(args, &mut index, "--burst")?;
                settings.limits.burst =
                    Some(u32::try_from(number("--burst", value)?).unwrap_or(u32::MAX));
                settings.any = true;
            }
            "--storage" => {
                let value = take(args, &mut index, "--storage")?;
                settings.limits.storage_bytes = Some(number("--storage", value)?);
                settings.any = true;
            }
            "--connections" => {
                let value = take(args, &mut index, "--connections")?;
                settings.limits.connections =
                    Some(usize::try_from(number("--connections", value)?).unwrap_or(usize::MAX));
                settings.any = true;
            }
            "--grace" => {
                let value = take(args, &mut index, "--grace")?;
                grace = Some(refhir_server::tenancy::admin::parse_duration(&value)?);
            }
            "--export" => export = Some(PathBuf::from(take(args, &mut index, "--export")?)),
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
        [verb, name] if verb == "show" => Command::Show(name.clone()),
        [verb, name] if verb == "delete" => Command::Delete(name.clone()),
        [verb, name] if verb == "restore" => Command::Restore(name.clone()),
        [verb] if verb == "list" => Command::List,
        [verb] if verb == "purge" => Command::Purge,
        [verb] if ["create", "show", "delete", "restore"].contains(&verb.as_str()) => {
            return Err(format!("`{verb}` needs a tenant name"))
        }
        [] => {
            return Err(
                "pass create, list, show, delete, restore or purge (see refhir tenant --help)"
                    .into(),
            )
        }
        _ => return Err("too many arguments (see refhir tenant --help)".into()),
    };
    if settings.any && !matches!(command, Command::Create(_)) {
        return Err("the settings flags are for `create`".into());
    }
    if (grace.is_some() || export.is_some()) && !matches!(command, Command::Delete(_)) {
        return Err("--grace and --export are for `delete`".into());
    }
    settings
        .limits
        .validate()
        .map_err(|err| err.replace("limits.", "--"))?;
    Ok(Some(Parsed {
        command,
        config_path,
        postgres,
        yes,
        settings,
        grace,
        export,
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
fn database_url(
    config_path: Option<PathBuf>,
    flag: Option<String>,
) -> Result<(String, ResolvedConfig), String> {
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
    let url = cfg.postgres.clone().ok_or_else(|| {
        "tenants live in a Postgres database: pass --postgres, or set REFHIR_POSTGRES_URL or \
         [database] postgres_url"
            .to_string()
    })?;
    Ok((url, cfg))
}

fn execute(parsed: Parsed) -> Result<(), Box<dyn std::error::Error>> {
    let Parsed {
        command,
        config_path,
        postgres,
        yes,
        settings,
        grace,
        export,
    } = parsed;
    if let Command::Delete(name) = &command {
        // Refused before the database is reached.
        if !yes {
            return Err(format!(
                "deleting tenant `{name}` stops it being served and, once its grace period is \
                 over, removes everything it holds: run it again with --yes"
            )
            .into());
        }
    }
    // The URL carries the password, so it is never printed.
    let (url, cfg) = database_url(config_path, postgres)?;
    match command {
        Command::Create(name) => {
            let tenant = if settings.any {
                let settings = refhir_server::tenancy::TenantSettings {
                    packages: settings.packages,
                    update_as_create: settings.update_as_create,
                    smart: settings.smart.unwrap_or(cfg.auth),
                    health_cards: settings.health_cards,
                    limits: settings.limits,
                };
                let settings = serde_json::to_value(settings)?;
                match refhir_postgres::create_tenant_with(&url, &name, &settings)? {
                    refhir_postgres::Created::New(tenant) => tenant,
                    refhir_postgres::Created::Existing(_) => {
                        return Err(format!("tenant `{name}` already exists").into())
                    }
                }
            } else {
                refhir_postgres::create_tenant(&url, &name)?
            };
            println!("created tenant {} (id {})", tenant.name, tenant.id);
            println!(
                "a process started with --all-tenants serves it now; start one for it alone with: \
                 refhir --tenant {}",
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
                "{:<width$}  {:>4}  {:<20}  {:<7}  PRIMARY",
                "NAME", "ID", "CREATED", "STATUS"
            );
            for tenant in tenants {
                println!(
                    "{:<width$}  {:>4}  {:<20}  {:<7}  {}",
                    tenant.name,
                    tenant.id,
                    tenant.created_at,
                    if tenant.is_deleted() {
                        "deleted"
                    } else {
                        "active"
                    },
                    if tenant.primary_running {
                        "running"
                    } else {
                        "-"
                    }
                );
            }
        }
        Command::Show(name) => {
            let tenant = refhir_postgres::get_tenant(&url, &name)?
                .ok_or_else(|| format!("no tenant `{name}` in this database"))?;
            println!("name:      {}", tenant.name);
            println!("id:        {}", tenant.id);
            println!("created:   {}", tenant.created_at);
            match (&tenant.deleted_at, &tenant.purge_after) {
                (Some(at), Some(purge)) => {
                    println!("status:    deleted at {at}, removed after {purge}")
                }
                _ => println!("status:    active"),
            }
            println!(
                "primary:   {}",
                if tenant.primary_running {
                    "running"
                } else {
                    "-"
                }
            );
            println!("settings:  {}", tenant.settings);
        }
        Command::Delete(name) => {
            let grace = grace.unwrap_or(cfg.tenants.delete_grace);
            if let Some(dir) = &export {
                let file = export_tenant(&url, &name, dir)?;
                println!("wrote the resources of tenant {name} to {}", file.display());
            }
            if grace.is_zero() {
                // At once, as `delete` always did: refused while a primary runs, never undone.
                let removed = refhir_postgres::delete_tenant(&url, &name)?;
                println!("deleted tenant {name}: {removed} rows removed");
                return Ok(());
            }
            let tenant = refhir_postgres::soft_delete_tenant(&url, &name, grace.as_secs())?
                .ok_or_else(|| format!("no tenant `{name}` in this database"))?;
            println!(
                "deleted tenant {name}: no longer served, and removed after {} unless restored \
                 (`refhir tenant restore {name}`)",
                tenant.purge_after.unwrap_or_default()
            );
            if tenant.primary_running {
                println!(
                    "a primary of it is running: a process that serves every tenant stops serving \
                     it now, and one started with --tenant {name} when it stops"
                );
            }
        }
        Command::Restore(name) => {
            let tenant = refhir_postgres::restore_tenant(&url, &name)?.ok_or_else(|| {
                format!("no tenant `{name}` in this database (it may be removed)")
            })?;
            println!("restored tenant {}", tenant.name);
        }
        Command::Purge => {
            let removed = refhir_postgres::purge_due_tenants(&url)?;
            if removed.is_empty() {
                println!("no deleted tenant is due to be removed");
            }
            for (name, rows) in removed {
                println!("removed tenant {name}: {rows} rows");
            }
        }
    }
    Ok(())
}

/// Writes every resource tenant `name` holds to `dir`, as a process that serves every tenant does
/// before it deletes one. Reads only: it opens the tenant as a replica would.
fn export_tenant(
    url: &str,
    name: &str,
    dir: &std::path::Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    use std::sync::{Arc, RwLock};
    let persistence = Arc::new(refhir_postgres::PostgresPersistence::connect_with(
        url,
        &refhir_postgres::ConnectOptions {
            create_schema: false,
            tenant: Some(name.to_string()),
            ..refhir_postgres::ConnectOptions::default()
        },
    )?);
    let db = refhir_database::Database::open_read_only(persistence.clone())?;
    let (catalog, _) = refhir_fhir::Catalog::open_consistent(
        persistence.as_ref(),
        refhir_fhir::CatalogClock::Stable,
    )?;
    let catalog = RwLock::new(catalog);
    std::fs::create_dir_all(dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let file = dir.join(format!("{name}-{stamp}.ndjson"));
    refhir_server::export_everything(&db, &catalog, &file)?;
    Ok(file)
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

        let parsed = parse(&args(&[
            "create",
            "acme",
            "--package",
            "a@1",
            "--package",
            "./b",
            "--update-as-create",
            "--no-smart",
            "--health-cards",
            "--rate",
            "2.5",
            "--burst",
            "10",
            "--storage",
            "1000",
            "--connections",
            "3",
        ]))
        .unwrap()
        .unwrap();
        let flags = parsed.settings;
        assert!(flags.any && flags.update_as_create && flags.health_cards);
        assert_eq!(flags.packages, ["a@1", "./b"]);
        assert_eq!(flags.smart, Some(false));
        assert_eq!(flags.limits.requests_per_second, Some(2.5));
        assert_eq!(flags.limits.burst, Some(10));
        assert_eq!(flags.limits.storage_bytes, Some(1000));
        assert_eq!(flags.limits.connections, Some(3));
        let parsed = parse(&args(&[
            "delete", "acme", "--yes", "--grace", "36h", "--export", "out",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            parsed.grace,
            Some(std::time::Duration::from_secs(36 * 3600))
        );
        assert_eq!(parsed.export, Some(PathBuf::from("out")));
        for verb in ["show", "restore"] {
            assert!(parse(&args(&[verb, "acme"])).unwrap().is_some());
        }
        assert!(matches!(
            parse(&args(&["purge"])).unwrap().unwrap().command,
            Command::Purge
        ));
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
            args(&["list", "--smart"]),
            args(&["show"]),
            args(&["create", "a", "--rate", "-1"]),
            args(&["create", "a", "--connections", "0"]),
            args(&["create", "a", "--grace", "1d"]),
            args(&["delete", "a", "--yes", "--grace", "soon"]),
            args(&["purge", "extra"]),
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
            settings: CreateFlags::default(),
            grace: None,
            export: None,
        };
        let error = execute(parsed).unwrap_err().to_string();
        assert!(error.contains("--yes"), "{error}");
    }
}
