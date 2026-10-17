//! Which tenant a statement belongs to.
//!
//! A database is in one of two layouts. In the classic layout one ReFHIR owns the whole database
//! and the tables have no tenant column. In the shared layout many tenants share one set of tables
//! and an integer `instance_id` leads every primary key and index; a process serves one tenant,
//! chosen when it starts. [`Scope`] is that choice, and every statement in this crate builds its
//! tenant-dependent parts from it.
//!
//! The helpers return the empty string for [`Scope::Classic`], so a classic statement is the same
//! text it was before tenants existed. For a tenant they write the id into the statement as a
//! literal. That is safe because the id is an `i32` the registry handed out, never text from a
//! caller, and it keeps every `$n` placeholder where it was (some statements add a placeholder only
//! when a bound is given, which a leading `$1` for the tenant would shift).

/// The tenant a [`PostgresPersistence`](crate::PostgresPersistence) reads and writes as.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Scope {
    /// A database with no tenant column: the only layout before tenants existed.
    #[default]
    Classic,
    /// One tenant of a database in the shared layout.
    Tenant(i32),
}

impl Scope {
    /// For the front of a column list: `instance_id, `.
    pub(crate) fn cols(self) -> String {
        match self {
            Scope::Classic => String::new(),
            Scope::Tenant(_) => "instance_id, ".to_string(),
        }
    }

    /// For the front of a `VALUES` list, matching [`Scope::cols`]: `7, `.
    pub(crate) fn vals(self) -> String {
        match self {
            Scope::Classic => String::new(),
            Scope::Tenant(id) => format!("{id}, "),
        }
    }

    /// For the front of a condition list that already has a first condition:
    /// `instance_id = 7 AND `.
    pub(crate) fn and(self) -> String {
        match self {
            Scope::Classic => String::new(),
            Scope::Tenant(id) => format!("instance_id = {id} AND "),
        }
    }

    /// [`Scope::and`] for a table known by an alias: `A.instance_id = 7 AND `.
    pub(crate) fn and_in(self, alias: &str) -> String {
        match self {
            Scope::Classic => String::new(),
            Scope::Tenant(id) => format!("{alias}.instance_id = {id} AND "),
        }
    }

    /// A whole `WHERE` for a statement that has no other condition: ` WHERE instance_id = 7`,
    /// with the leading space, or nothing.
    pub(crate) fn where_only(self) -> String {
        match self {
            Scope::Classic => String::new(),
            Scope::Tenant(id) => format!(" WHERE instance_id = {id}"),
        }
    }

    /// An `ON CONFLICT` target: `(instance_id, a, b)` for a tenant, `(a, b)` otherwise.
    pub(crate) fn conflict(self, columns: &str) -> String {
        match self {
            Scope::Classic => format!("({columns})"),
            Scope::Tenant(_) => format!("(instance_id, {columns})"),
        }
    }

    /// The `NOTIFY` channel a tenant's primary announces on and its replicas listen to. A
    /// tenant's own channel keeps one tenant's commits from waking every other tenant's replicas.
    pub(crate) fn channel(self) -> String {
        match self {
            Scope::Classic => "refhir_published".to_string(),
            Scope::Tenant(id) => format!("refhir_published_{id}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_adds_nothing_to_a_statement() {
        let scope = Scope::Classic;
        assert_eq!(scope.cols(), "");
        assert_eq!(scope.vals(), "");
        assert_eq!(scope.and(), "");
        assert_eq!(scope.and_in("B"), "");
        assert_eq!(scope.where_only(), "");
        assert_eq!(scope.conflict("ts, table_id, id"), "(ts, table_id, id)");
        assert_eq!(scope.channel(), "refhir_published");
    }

    #[test]
    fn a_tenant_leads_every_part_with_its_id() {
        let scope = Scope::Tenant(7);
        assert_eq!(scope.cols(), "instance_id, ");
        assert_eq!(scope.vals(), "7, ");
        assert_eq!(scope.and(), "instance_id = 7 AND ");
        assert_eq!(scope.and_in("B"), "B.instance_id = 7 AND ");
        assert_eq!(scope.where_only(), " WHERE instance_id = 7");
        assert_eq!(
            scope.conflict("ts, table_id, id"),
            "(instance_id, ts, table_id, id)"
        );
        assert_eq!(scope.channel(), "refhir_published_7");
    }

    #[test]
    fn a_statement_built_from_a_scope_reads_as_one_sentence() {
        let sql = |scope: Scope| {
            format!(
                "INSERT INTO persistence_globals ({}key, json_value) VALUES ({}$1, $2) \
                 ON CONFLICT {} DO UPDATE SET json_value = EXCLUDED.json_value",
                scope.cols(),
                scope.vals(),
                scope.conflict("key")
            )
        };
        assert_eq!(
            sql(Scope::Classic),
            "INSERT INTO persistence_globals (key, json_value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET json_value = EXCLUDED.json_value"
        );
        assert_eq!(
            sql(Scope::Tenant(3)),
            "INSERT INTO persistence_globals (instance_id, key, json_value) VALUES (3, $1, $2) \
             ON CONFLICT (instance_id, key) DO UPDATE SET json_value = EXCLUDED.json_value"
        );
    }
}
