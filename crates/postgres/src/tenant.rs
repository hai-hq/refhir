//! The shared tenant layout: how a database is recognised as one, and the registry of its tenants.
//!
//! A database has exactly one of two layouts, decided when its first table is made and never
//! changed after. The classic layout (what ReFHIR has always made) has no tenant column and belongs
//! to one ReFHIR. The shared layout has `instance_id` on every table and a `refhir_tenants`
//! registry; each process serves one tenant of it. A process must say which it expects, and a
//! mismatch is refused before anything is read or written, so a process never reads another
//! layout's rows as its own.

use postgres::GenericClient;
use refhir_persistence::PersistError;
use serde_json::Value as JsonValue;

use crate::migrate;
use crate::{connect_client, definite, off_runtime, SharedConnections};

/// The registry table. Its presence is what makes a database shared.
const REGISTRY: &str = "refhir_tenants";

/// The channel every change to the registry is announced on, so that the processes that serve
/// many tenants pick a new one up, or stop serving a deleted one, at once.
pub(crate) const REGISTRY_CHANNEL: &str = "refhir_tenants";

/// Longest tenant name. Matches the check on `refhir_tenants.name`.
const MAX_NAME: usize = 63;

/// Every table that holds a tenant's rows, in the shared layout. Each has `instance_id`. The
/// registry itself does not, and neither does `schema_migrations`.
pub(crate) const TENANT_TABLES: &[&str] = &[
    "documents",
    "indexes",
    "persistence_globals",
    "tenant_packages",
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

/// The name of the tenant a process that serves every tenant makes at its start, to have what a
/// new tenant needs ready before the first one is asked for, and removes again. It is a tenant like
/// any other while it lasts, so that making it is exactly what making a tenant is, and nobody can
/// make, read, change or delete one of this name: it is not listed, and not served.
pub const WARM_TENANT: &str = "refhir-warm";

/// A tenant name is lowercase letters, digits and `-`, starting with a letter or digit, at most
/// 63 characters. It becomes part of URLs, directory names and log lines, so it stays boring.
/// [`WARM_TENANT`] is a name no one may have.
pub fn validate_tenant_name(name: &str) -> Result<(), PersistError> {
    if name == WARM_TENANT {
        return Err(definite(format!(
            "`{name}` is reserved: use another tenant name"
        )));
    }
    validate_name_syntax(name)
}

/// What a name must look like, whoever owns it.
pub(crate) fn validate_name_syntax(name: &str) -> Result<(), PersistError> {
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

/// The id of tenant `name`, or `None` when the database has no such tenant or it is deleted (a
/// deleted tenant is not served while it waits to be removed). The reserved id 0 is never a tenant
/// a process can serve.
pub(crate) fn tenant_id(
    client: &mut impl GenericClient,
    name: &str,
) -> Result<Option<i32>, PersistError> {
    let row = client
        .query_opt(
            &format!("SELECT id FROM {REGISTRY} WHERE name = $1 AND id > 0 AND deleted_at IS NULL"),
            &[&name],
        )
        .map_err(definite)?;
    Ok(row.map(|row| row.get(0)))
}

/// The id of tenant `name`, deleted or not.
fn any_tenant_id(client: &mut impl GenericClient, name: &str) -> Result<Option<i32>, PersistError> {
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
    /// What the tenant was created with, as the server understands it. `{}` for a tenant made
    /// before the registry kept settings, or made with none.
    pub settings: JsonValue,
    /// When the tenant was deleted (ISO 8601, UTC). A deleted tenant is not served.
    pub deleted_at: Option<String>,
    /// When a deleted tenant's rows are removed, and until when it can be restored.
    pub purge_after: Option<String>,
    /// When the registry last changed this tenant (created, settings changed, deleted, restored,
    /// or [touched](SharedConnections::touch_tenant)), as the database writes it. A process that
    /// serves the tenant compares it with the one it last acted on.
    pub updated_at: String,
}

impl Tenant {
    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }
}

/// What [`SharedConnections::create_tenant`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Created {
    /// The tenant is new.
    New(Tenant),
    /// A tenant of that name was already there, with the settings it has.
    Existing(Tenant),
}

const CREATED_AT: &str = r#"to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')"#;

/// The columns [`tenant_from_row`] reads, in order.
fn tenant_columns() -> String {
    let at = |column: &str| {
        format!(r#"to_char({column} AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"')"#)
    };
    format!(
        "id, name, {CREATED_AT}, settings::text, {}, {}, updated_at::text",
        at("deleted_at"),
        at("purge_after")
    )
}

fn tenant_from_row(row: &postgres::Row, primary_running: bool) -> Tenant {
    let settings: String = row.get(3);
    Tenant {
        id: row.get(0),
        name: row.get(1),
        created_at: row.get(2),
        primary_running,
        settings: serde_json::from_str(&settings).unwrap_or(JsonValue::Null),
        deleted_at: row.get(4),
        purge_after: row.get(5),
        updated_at: row.get(6),
    }
}

/// Inserts tenant `name`, or finds the one already there. The registry is announced as changed
/// when a tenant is added.
fn insert_tenant(
    client: &mut impl GenericClient,
    name: &str,
    settings: &JsonValue,
) -> Result<Created, PersistError> {
    validate_name_syntax(name)?;
    let columns = tenant_columns();
    let inserted = client
        .query_opt(
            &format!(
                "INSERT INTO {REGISTRY} (name, settings) VALUES ($1, $2::text::jsonb)                  ON CONFLICT (name) DO NOTHING RETURNING {columns}"
            ),
            &[&name, &settings.to_string()],
        )
        .map_err(definite)?;
    if let Some(row) = inserted {
        client
            .batch_execute(&format!("NOTIFY {REGISTRY_CHANNEL}"))
            .map_err(definite)?;
        return Ok(Created::New(tenant_from_row(&row, false)));
    }
    let row = client
        .query_one(
            &format!("SELECT {columns} FROM {REGISTRY} WHERE name = $1"),
            &[&name],
        )
        .map_err(definite)?;
    let id: i32 = row.get(0);
    let running = crate::lease::is_held(client, id)?;
    Ok(Created::Existing(tenant_from_row(&row, running)))
}

/// Every tenant of the registry, oldest first, deleted ones included.
fn read_tenants(client: &mut impl GenericClient) -> Result<Vec<Tenant>, PersistError> {
    let rows = client
        .query(
            &format!(
                "SELECT {} FROM {REGISTRY} WHERE id > 0 AND name <> '{WARM_TENANT}' ORDER BY id",
                tenant_columns()
            ),
            &[],
        )
        .map_err(definite)?;
    let mut tenants = Vec::with_capacity(rows.len());
    for row in rows {
        let id: i32 = row.get(0);
        let running = crate::lease::is_held(client, id)?;
        tenants.push(tenant_from_row(&row, running));
    }
    Ok(tenants)
}

fn read_tenant(
    client: &mut impl GenericClient,
    name: &str,
) -> Result<Option<Tenant>, PersistError> {
    let row = client
        .query_opt(
            &format!(
                "SELECT {} FROM {REGISTRY} WHERE name = $1 AND id > 0 AND name <> '{WARM_TENANT}'",
                tenant_columns()
            ),
            &[&name],
        )
        .map_err(definite)?;
    match row {
        Some(row) => {
            let id: i32 = row.get(0);
            let running = crate::lease::is_held(client, id)?;
            Ok(Some(tenant_from_row(&row, running)))
        }
        None => Ok(None),
    }
}

/// Marks tenant `name` deleted, its rows to be removed once `grace_secs` have passed. A tenant
/// that is already deleted keeps the time it was given. `None` when there is no such tenant.
fn mark_deleted(
    client: &mut impl GenericClient,
    name: &str,
    grace_secs: u64,
) -> Result<Option<Tenant>, PersistError> {
    let grace = i64::try_from(grace_secs).unwrap_or(i64::MAX / 1_000_000);
    let changed = client
        .execute(
            &format!(
                "UPDATE {REGISTRY} SET deleted_at = now(),                  purge_after = now() + make_interval(secs => $2::bigint::double precision),                  updated_at = now()                  WHERE name = $1 AND id > 0 AND name <> '{WARM_TENANT}' AND deleted_at IS NULL"
            ),
            &[&name, &grace],
        )
        .map_err(definite)?;
    if changed > 0 {
        client
            .batch_execute(&format!("NOTIFY {REGISTRY_CHANNEL}"))
            .map_err(definite)?;
    }
    read_tenant(client, name)
}

/// Undoes [`mark_deleted`] while the tenant's rows are still there. `None` when there is no such
/// tenant (it may have been removed already).
fn mark_restored(
    client: &mut impl GenericClient,
    name: &str,
) -> Result<Option<Tenant>, PersistError> {
    let changed = client
        .execute(
            &format!(
                "UPDATE {REGISTRY} SET deleted_at = NULL, purge_after = NULL, updated_at = now()                  WHERE name = $1 AND id > 0 AND name <> '{WARM_TENANT}' AND deleted_at IS NOT NULL"
            ),
            &[&name],
        )
        .map_err(definite)?;
    if changed > 0 {
        client
            .batch_execute(&format!("NOTIFY {REGISTRY_CHANNEL}"))
            .map_err(definite)?;
    }
    read_tenant(client, name)
}

/// Replaces the settings of tenant `name`, or only marks it changed when `settings` is `None`, and
/// announces the registry as changed so that every process that serves the tenant looks again. A
/// deleted tenant is not changed. `None` when there is no such tenant.
fn mark_changed(
    client: &mut impl GenericClient,
    name: &str,
    settings: Option<&JsonValue>,
) -> Result<Option<Tenant>, PersistError> {
    let changed = match settings {
        Some(settings) => client.execute(
            &format!(
                "UPDATE {REGISTRY} SET settings = $2::text::jsonb, updated_at = now() \
                 WHERE name = $1 AND id > 0 AND name <> '{WARM_TENANT}' AND deleted_at IS NULL"
            ),
            &[&name, &settings.to_string()],
        ),
        None => client.execute(
            &format!(
                "UPDATE {REGISTRY} SET updated_at = now() \
                 WHERE name = $1 AND id > 0 AND name <> '{WARM_TENANT}' AND deleted_at IS NULL"
            ),
            &[&name],
        ),
    }
    .map_err(definite)?;
    if changed > 0 {
        client
            .batch_execute(&format!("NOTIFY {REGISTRY_CHANNEL}"))
            .map_err(definite)?;
    }
    read_tenant(client, name)
}

/// The deleted tenants whose grace period is over.
fn due(client: &mut impl GenericClient) -> Result<Vec<String>, PersistError> {
    let rows = client
        .query(
            &format!(
                "SELECT name FROM {REGISTRY} WHERE id > 0 AND deleted_at IS NOT NULL                  AND purge_after <= now() ORDER BY id"
            ),
            &[],
        )
        .map_err(definite)?;
    Ok(rows.into_iter().map(|row| row.get(0)).collect())
}

/// Something that moves with every change to the registry: the number of tenants and the newest
/// change. Equal values mean nothing was added, deleted, restored or removed in between.
fn signature(client: &mut impl GenericClient) -> Result<String, PersistError> {
    let row = client
        .query_one(
            &format!(
                "SELECT count(*)::text || '/' || COALESCE(max(updated_at)::text, '') \
                 FROM {REGISTRY} WHERE name <> '{WARM_TENANT}'"
            ),
            &[],
        )
        .map_err(definite)?;
    Ok(row.get(0))
}

impl SharedConnections {
    /// Registers tenant `name` with `settings`, or finds the tenant of that name that is there:
    /// asking twice is safe. A deleted tenant that is still waiting to be removed is found, not
    /// replaced; the caller decides what to make of a tenant that differs from what it asked for.
    pub fn create_tenant(&self, name: &str, settings: &JsonValue) -> Result<Created, PersistError> {
        validate_tenant_name(name)?;
        self.with_registry(|client| insert_tenant(client, name, settings))
    }

    /// Every tenant of the database, oldest first, deleted ones included.
    pub fn list_tenants(&self) -> Result<Vec<Tenant>, PersistError> {
        self.with_registry(read_tenants)
    }

    /// Tenant `name`, deleted or not.
    pub fn get_tenant(&self, name: &str) -> Result<Option<Tenant>, PersistError> {
        self.with_registry(|client| read_tenant(client, name))
    }

    /// Deletes tenant `name`: no process serves it from now on, and its rows are removed once
    /// `grace_secs` have passed (see [`SharedConnections::purge_due`]).
    pub fn delete_tenant(
        &self,
        name: &str,
        grace_secs: u64,
    ) -> Result<Option<Tenant>, PersistError> {
        self.with_registry(|client| mark_deleted(client, name, grace_secs))
    }

    /// Brings back a deleted tenant whose rows have not been removed yet.
    pub fn restore_tenant(&self, name: &str) -> Result<Option<Tenant>, PersistError> {
        self.with_registry(|client| mark_restored(client, name))
    }

    /// Replaces the settings of tenant `name` and announces the change, so every process that
    /// serves it applies them. The tenant as it is now; `None` when there is no such tenant. A
    /// deleted tenant keeps the settings it has.
    pub fn update_tenant_settings(
        &self,
        name: &str,
        settings: &JsonValue,
    ) -> Result<Option<Tenant>, PersistError> {
        self.with_registry(|client| mark_changed(client, name, Some(settings)))
    }

    /// Marks tenant `name` changed without changing its settings, and announces it: what a tenant
    /// keeps in its own rows (a key rotated, say) reaches every process that serves it.
    pub fn touch_tenant(&self, name: &str) -> Result<Option<Tenant>, PersistError> {
        self.with_registry(|client| mark_changed(client, name, None))
    }

    /// The deleted tenants whose grace period is over.
    pub fn tenants_due_for_purge(&self) -> Result<Vec<String>, PersistError> {
        self.with_registry(due)
    }

    /// Removes the rows of tenant `name`, which must be deleted and past its grace period, and
    /// whose lease no process holds. Returns how many rows went.
    pub fn purge_tenant(&self, name: &str) -> Result<u64, PersistError> {
        purge(&self.url, name, true)
    }

    /// Makes the [`WARM_TENANT`], with the default settings, after removing one that an earlier
    /// process left behind. Whoever makes it opens it as any tenant is opened and then removes it
    /// with [`SharedConnections::remove_warm_tenant`]. Fails if a process still holds the one that
    /// is there.
    pub fn make_warm_tenant(&self) -> Result<(), PersistError> {
        self.remove_warm_tenant()?;
        self.with_registry(|client| {
            insert_tenant(client, WARM_TENANT, &JsonValue::Object(Default::default()))
        })
        .map(|_| ())
    }

    /// Removes the [`WARM_TENANT`] and every row it holds, if there is one. Returns how many rows
    /// went.
    pub fn remove_warm_tenant(&self) -> Result<u64, PersistError> {
        let there = self.with_registry(|client| any_tenant_id(client, WARM_TENANT))?;
        match there {
            Some(_) => purge_rows(&self.url, WARM_TENANT, false),
            None => Ok(0),
        }
    }

    /// See [`signature`].
    pub fn registry_signature(&self) -> Result<String, PersistError> {
        self.with_registry(signature)
    }
}

/// Registers tenant `name` with `settings` in the database at `url`, or finds the tenant of that
/// name that is there. In an empty database this first creates the shared layout, so it is how a
/// shared database comes to exist. It refuses a database made without tenants.
pub fn create_tenant_with(
    url: &str,
    name: &str,
    settings: &JsonValue,
) -> Result<Created, PersistError> {
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
        insert_tenant(&mut client, name, settings)
    })
}

/// [`create_tenant_with`] with no settings, refusing a name that is taken.
pub fn create_tenant(url: &str, name: &str) -> Result<Tenant, PersistError> {
    match create_tenant_with(url, name, &JsonValue::Object(Default::default()))? {
        Created::New(tenant) => Ok(tenant),
        Created::Existing(_) => Err(definite(format!("tenant `{name}` already exists"))),
    }
}

/// Tenant `name` of the database at `url`, deleted or not.
pub fn get_tenant(url: &str, name: &str) -> Result<Option<Tenant>, PersistError> {
    off_runtime(|| {
        let mut client = connect_client(url)?;
        match detect_layout(&mut client)? {
            Layout::Shared => read_tenant(&mut client, name),
            _ => Ok(None),
        }
    })
}

/// See [`SharedConnections::delete_tenant`]. `None` when there is no such tenant.
pub fn soft_delete_tenant(
    url: &str,
    name: &str,
    grace_secs: u64,
) -> Result<Option<Tenant>, PersistError> {
    validate_tenant_name(name)?;
    off_runtime(|| {
        let mut client = shared_client(url)?;
        mark_deleted(&mut client, name, grace_secs)
    })
}

/// See [`SharedConnections::restore_tenant`]. `None` when there is no such tenant.
pub fn restore_tenant(url: &str, name: &str) -> Result<Option<Tenant>, PersistError> {
    validate_tenant_name(name)?;
    off_runtime(|| {
        let mut client = shared_client(url)?;
        mark_restored(&mut client, name)
    })
}

/// Removes every deleted tenant whose grace period is over and that no process serves, and
/// returns each one removed with how many rows went. One that a process still holds is left for
/// the next time, and said so in the log.
pub fn purge_due_tenants(url: &str) -> Result<Vec<(String, u64)>, PersistError> {
    let names = off_runtime(|| {
        let mut client = shared_client(url)?;
        due(&mut client)
    })?;
    let mut removed = Vec::new();
    for name in names {
        match purge(url, &name, true) {
            Ok(rows) => removed.push((name, rows)),
            Err(err) => tracing::warn!(target: "refhir", "tenant `{name}` not removed yet: {err}"),
        }
    }
    Ok(removed)
}

/// A connection to a database that must have the shared layout.
fn shared_client(url: &str) -> Result<postgres::Client, PersistError> {
    let mut client = connect_client(url)?;
    match detect_layout(&mut client)? {
        Layout::Shared => Ok(client),
        Layout::Classic => Err(definite(
            "this database was created without tenants, so it has none",
        )),
        Layout::Empty => Err(definite("the database is empty: it has no tenants")),
    }
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
        read_tenants(&mut client)
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
    purge(url, name, false)
}

/// The rows of tenant `name` and its registry row, removed; with `only_deleted`, only a tenant
/// that is deleted and past its grace period.
fn purge(url: &str, name: &str, only_deleted: bool) -> Result<u64, PersistError> {
    validate_tenant_name(name)?;
    purge_rows(url, name, only_deleted)
}

/// [`purge`] for a name that has been checked, or is not one a person chose.
fn purge_rows(url: &str, name: &str, only_deleted: bool) -> Result<u64, PersistError> {
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
        let id = any_tenant_id(&mut client, name)?
            .ok_or_else(|| definite(format!("no tenant `{name}` in this database")))?;
        if only_deleted {
            let due: bool = client
                .query_one(
                    &format!(
                        "SELECT EXISTS (SELECT 1 FROM {REGISTRY} WHERE id = $1                          AND deleted_at IS NOT NULL AND purge_after <= now())"
                    ),
                    &[&id],
                )
                .map_err(definite)?
                .get(0);
            if !due {
                return Err(definite(format!(
                    "tenant `{name}` is not deleted, or its grace period is not over"
                )));
            }
        }
        // Row-level security shows a connection only the rows of the tenant it serves, and these
        // are the rows to remove.
        crate::set_tenant(&mut client, crate::Scope::Tenant(id))?;
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
        client
            .batch_execute(&format!("NOTIFY {REGISTRY_CHANNEL}"))
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
        // The name the warm-up tenant has is not one a person can give.
        let err = validate_tenant_name(WARM_TENANT).unwrap_err().to_string();
        assert!(err.contains("reserved"), "{err}");
        assert!(validate_name_syntax(WARM_TENANT).is_ok());
    }
}
