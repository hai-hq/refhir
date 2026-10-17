//! The shared tenant layout: how a database is recognised as one, and the registry of its tenants.
//!
//! A database has exactly one of two layouts, decided when its first table is made and never
//! changed after. The classic layout (what ReFHIR has always made) has no tenant column and belongs
//! to one ReFHIR. The shared layout has `instance_id` on every table and a `refhir_tenants`
//! registry; each process serves one tenant of it. A process must say which it expects, and a
//! mismatch is refused before anything is read or written, so a process never reads another
//! layout's rows as its own.

use postgres::error::SqlState;
use postgres::GenericClient;
use refhir_persistence::PersistError;

use crate::migrate;
use crate::{connect_client, definite, off_runtime, ErrorText};

/// The registry table. Its presence is what makes a database shared.
const REGISTRY: &str = "refhir_tenants";

/// Longest tenant name. Matches the check on `refhir_tenants.name`.
const MAX_NAME: usize = 63;

/// Every table that holds a tenant's rows, in the shared layout. Each has `instance_id`. The
/// registry itself does not, and neither does `schema_migrations`.
pub(crate) const TENANT_TABLES: &[&str] = &[
    "documents",
    "indexes",
    "persistence_globals",
    "packages",
    "structure_definitions",
    "search_parameters",
    "index_backfills",
    "terminology",
    "value_set_expansion",
    "backfill_parameter",
    "smart_oauth",
    "jobs",
    "usage_daily",
    "request_log",
    "governance_members",
    "audit_log",
];

/// What a database holds, judged by its tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
    /// No ReFHIR tables at all.
    Empty,
    /// Tables without a tenant column.
    Classic,
    /// Tables with `instance_id`, and the registry.
    Shared,
}

pub(crate) fn detect_layout(client: &mut impl GenericClient) -> Result<Layout, PersistError> {
    let row = client
        .query_one(
            &format!(
                "SELECT to_regclass('{REGISTRY}') IS NOT NULL, \
                        to_regclass('documents') IS NOT NULL \
                        OR to_regclass('schema_migrations') IS NOT NULL"
            ),
            &[],
        )
        .map_err(definite)?;
    let shared: bool = row.get(0);
    let classic: bool = row.get(1);
    Ok(if shared {
        Layout::Shared
    } else if classic {
        Layout::Classic
    } else {
        Layout::Empty
    })
}

/// Whether the database at `url` has the shared tenant layout.
pub fn holds_tenants(url: &str) -> Result<bool, PersistError> {
    off_runtime(|| {
        let mut client = connect_client(url)?;
        Ok(detect_layout(&mut client)? == Layout::Shared)
    })
}

/// A tenant name is lowercase letters, digits and `-`, starting with a letter or digit, at most
/// 63 characters. It becomes part of URLs, directory names and log lines, so it stays boring.
pub fn validate_tenant_name(name: &str) -> Result<(), PersistError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(definite(format!(
            "`{name}` is not a tenant name: use lowercase letters, digits and `-`, starting with a \
             letter or digit, at most {MAX_NAME} characters"
        )))
    }
}

/// The id of tenant `name`, or `None` when the database has no such tenant. The reserved id 0 is
/// never a tenant a process can serve.
pub(crate) fn tenant_id(
    client: &mut impl GenericClient,
    name: &str,
) -> Result<Option<i32>, PersistError> {
    let row = client
        .query_opt(
            &format!("SELECT id FROM {REGISTRY} WHERE name = $1 AND id > 0"),
            &[&name],
        )
        .map_err(definite)?;
    Ok(row.map(|row| row.get(0)))
}

/// One tenant of a shared database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tenant {
    pub id: i32,
    pub name: String,
    /// ISO 8601, UTC.
    pub created_at: String,
    /// Whether some process holds this tenant's writer lease right now.
    pub primary_running: bool,
}

const CREATED_AT: &str = r#"to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')"#;

/// Registers tenant `name` in the database at `url`. In an empty database this first creates the
/// shared layout, so it is how a shared database comes to exist. It refuses a database made
/// without tenants, and a name already taken.
pub fn create_tenant(url: &str, name: &str) -> Result<Tenant, PersistError> {
    validate_tenant_name(name)?;
    off_runtime(|| {
        let mut client = connect_client(url)?;
        if detect_layout(&mut client)? == Layout::Classic {
            return Err(definite(
                "this database was created without tenants, and a database keeps its layout: \
                 create tenants in an empty database",
            ));
        }
        // Applies the shared migrations: all of them on an empty database, any that a newer
        // release added on one that exists.
        migrate::run(&mut client, migrate::SHARED_MIGRATIONS)?;
        let inserted = client.query_one(
            &format!("INSERT INTO {REGISTRY} (name) VALUES ($1) RETURNING id, {CREATED_AT}"),
            &[&name],
        );
        match inserted {
            Ok(row) => Ok(Tenant {
                id: row.get(0),
                name: name.to_string(),
                created_at: row.get(1),
                primary_running: false,
            }),
            Err(err)
                if err
                    .as_db_error()
                    .is_some_and(|db| db.code() == &SqlState::UNIQUE_VIOLATION) =>
            {
                Err(definite(format!("tenant `{name}` already exists")))
            }
            Err(err) => Err(definite(err.error_text())),
        }
    })
}

/// The tenants of the database at `url`, oldest first, each with whether a primary holds its
/// lease. Reads only; it takes a tenant's lease for an instant to ask and gives it back.
pub fn list_tenants(url: &str) -> Result<Vec<Tenant>, PersistError> {
    off_runtime(|| {
        let mut client = connect_client(url)?;
        match detect_layout(&mut client)? {
            Layout::Shared => {}
            Layout::Classic => {
                return Err(definite(
                    "this database was created without tenants, so it has none to list",
                ))
            }
            Layout::Empty => return Ok(Vec::new()),
        }
        let rows = client
            .query(
                &format!("SELECT id, name, {CREATED_AT} FROM {REGISTRY} WHERE id > 0 ORDER BY id"),
                &[],
            )
            .map_err(definite)?;
        let mut tenants = Vec::with_capacity(rows.len());
        for row in rows {
            let id: i32 = row.get(0);
            tenants.push(Tenant {
                id,
                name: row.get(1),
                created_at: row.get(2),
                primary_running: crate::lease::is_held(&mut client, id)?,
            });
        }
        Ok(tenants)
    })
}

/// Most rows one statement of `delete_tenant` removes. The log tables of a busy tenant are large,
/// so they go in pieces: no one statement holds locks, or writes WAL, for long.
const DELETE_BATCH: i64 = 10_000;

/// Removes tenant `name` and every row it holds, and returns how many rows that was.
///
/// It takes the tenant's writer lease first and keeps it until it is done, so it refuses while a
/// primary of the tenant runs (stop the tenant's processes first: a standby or replica left running
/// would serve what it already has until it noticed), and no primary can start halfway through. The
/// rows go in batches, one statement each, so a tenant with a great deal of history does not hold
/// the tables for the length of its deletion. Rows other tenants hold, and the shared catalog,
/// are not touched. There is no undo: take a backup first.
pub fn delete_tenant(url: &str, name: &str) -> Result<u64, PersistError> {
    validate_tenant_name(name)?;
    off_runtime(|| {
        let mut client = connect_client(url)?;
        match detect_layout(&mut client)? {
            Layout::Shared => {}
            Layout::Classic => {
                return Err(definite(
                    "this database was created without tenants, so it has none to delete",
                ))
            }
            Layout::Empty => {
                return Err(definite(format!(
                    "no tenant `{name}`: the database is empty"
                )))
            }
        }
        let id = tenant_id(&mut client, name)?
            .ok_or_else(|| definite(format!("no tenant `{name}` in this database")))?;
        let got: bool = client
            .query_one(
                "SELECT pg_try_advisory_lock($1, $2)",
                &[&crate::lease::LEASE_NAMESPACE, &id],
            )
            .map_err(definite)?
            .get(0);
        if !got {
            return Err(definite(format!(
                "tenant `{name}` has a primary running. Stop the tenant's processes first"
            )));
        }
        let mut removed: u64 = 0;
        for table in TENANT_TABLES {
            let batch = format!(
                "DELETE FROM {table} WHERE ctid IN \
                 (SELECT ctid FROM {table} WHERE instance_id = $1 LIMIT {DELETE_BATCH})"
            );
            loop {
                let rows = client.execute(&batch, &[&id]).map_err(definite)?;
                removed += rows;
                if rows == 0 {
                    break;
                }
            }
        }
        client
            .execute(&format!("DELETE FROM {REGISTRY} WHERE id = $1"), &[&id])
            .map_err(definite)?;
        // The lease is the session's, and goes with it.
        Ok(removed)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_lowercase_words() {
        for good in ["acme", "a", "st-marys", "clinic-42", "0abc"] {
            assert!(validate_tenant_name(good).is_ok(), "{good}");
        }
        let long = "a".repeat(64);
        for bad in [
            "",
            "-x",
            "Acme",
            "has space",
            "under_score",
            "dot.ted",
            "ünï",
            long.as_str(),
        ] {
            assert!(validate_tenant_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_tenant_name(&"a".repeat(63)).is_ok());
    }

    #[test]
    fn the_reserved_base_name_is_not_a_tenant_name() {
        // `_base` is how the registry names id 0, and the leading underscore is not allowed.
        assert!(validate_tenant_name("_base").is_err());
    }
}
