//! Forward-only schema migrations, shared by the SQLite and Postgres backends.
//!
//! A backend compiles an ordered list of [`Migration`]s into the binary and records each one it
//! has applied in a `schema_migrations` table (version, applied_at, checksum). Startup asks
//! [`plan`] what to do with what the database holds, then applies it inside one transaction.
//! The deciding is here and is pure, so both backends decide the same way; running SQL and
//! locking is the backend's job.
//!
//! Migrations are never undone, and an applied one is never edited: its checksum is recorded,
//! and a database whose recorded checksum differs from the binary's refuses to start.

use sha2::{Digest, Sha256};

use crate::PersistError;

/// One step of the schema. `sql` is applied in a transaction, so it must not contain
/// statements a backend refuses inside one (such as Postgres `CREATE INDEX CONCURRENTLY`).
#[derive(Debug)]
pub struct Migration {
    /// 1, 2, 3, ... with no gaps.
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

impl Migration {
    pub fn checksum(&self) -> String {
        checksum(self.sql)
    }
}

/// SHA-256 of the migration text, as hex. Line endings are normalised so a checkout with CRLF
/// does not change what an applied migration hashes to.
pub fn checksum(sql: &str) -> String {
    let normalised = sql.replace("\r\n", "\n");
    Sha256::digest(normalised.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Every table and index version 1 creates, the same in both backends. A backend test checks it
/// against the SQL, and it is how an unversioned database is recognised as `Existing`.
pub const BASELINE_OBJECTS: &[&str] = &[
    "documents",
    "documents_by_table_and_id",
    "indexes",
    "indexes_by_ts",
    "persistence_globals",
    "packages",
    "structure_definitions",
    "search_parameters",
    "search_parameters_by_base_code",
    "index_backfills",
    "terminology",
    "value_set_expansion",
    "backfill_parameter",
    "smart_oauth",
    "smart_oauth_by_exp",
    "jobs",
    "jobs_by_status",
];

/// A row of `schema_migrations`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedMigration {
    pub version: i64,
    /// ISO 8601, UTC.
    pub applied_at: String,
    pub checksum: String,
}

/// What a database without any `schema_migrations` rows holds. Only matters when none are
/// recorded: it separates a new database from one an earlier, unversioned release created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Existing {
    /// None of the tables: a new database.
    Empty,
    /// Every table, index and column version 1 creates.
    Complete,
    /// Some of them: made by a release older than the current schema.
    Partial,
}

/// How version 1 is dealt with on a database that was never versioned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Adoption {
    /// The database is versioned, or new.
    No,
    /// It has the current schema already: version 1 is recorded and not run.
    Record,
    /// It has an older schema. What is missing is added (additively; nothing is altered or
    /// dropped), then version 1 is recorded.
    CompleteThenRecord,
}

/// What startup does to a database.
#[derive(Debug)]
pub struct Plan {
    pub adoption: Adoption,
    /// Applied in order, after any adoption.
    pub run: &'static [Migration],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationError {
    /// The database has been migrated by a newer binary.
    Newer { database: i64, binary: i64 },
    /// An applied migration is not the one this binary has under that version.
    Checksum {
        version: i64,
        recorded: String,
        expected: String,
    },
    /// The recorded versions are not 1, 2, 3, ... in order.
    Gap { expected: i64, found: i64 },
}

impl std::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrationError::Newer { database, binary } => write!(
                f,
                "the database is at schema version {database}, but this build only knows \
                 versions up to {binary}: it was migrated by a newer ReFHIR. Refusing to start. \
                 Run the same ReFHIR version everywhere, or restore a backup taken before the \
                 upgrade (migrations are not reversible)"
            ),
            MigrationError::Checksum {
                version,
                recorded,
                expected,
            } => write!(
                f,
                "schema migration {version} was applied with checksum {recorded}, but this \
                 build's migration {version} has checksum {expected}: the database was \
                 migrated by a different build. Refusing to start"
            ),
            MigrationError::Gap { expected, found } => write!(
                f,
                "schema_migrations records version {found} where {expected} was expected: the \
                 table was edited by hand, or is damaged. Refusing to start"
            ),
        }
    }
}

impl std::error::Error for MigrationError {}

impl From<MigrationError> for PersistError {
    fn from(err: MigrationError) -> Self {
        PersistError::Definite(err.to_string())
    }
}

/// The migrations still to apply to a database that has recorded `applied`, if what it has
/// recorded is what this binary knows.
pub fn pending(
    known: &'static [Migration],
    applied: &[AppliedMigration],
) -> Result<&'static [Migration], MigrationError> {
    let newest = known.last().map_or(0, |m| m.version);
    if let Some(top) = applied.iter().map(|a| a.version).max() {
        if top > newest {
            return Err(MigrationError::Newer {
                database: top,
                binary: newest,
            });
        }
    }
    for (position, recorded) in applied.iter().enumerate() {
        let expected = position as i64 + 1;
        if recorded.version != expected {
            return Err(MigrationError::Gap {
                expected,
                found: recorded.version,
            });
        }
        let ours = known[position].checksum();
        if recorded.checksum != ours {
            return Err(MigrationError::Checksum {
                version: recorded.version,
                recorded: recorded.checksum.clone(),
                expected: ours,
            });
        }
    }
    Ok(&known[applied.len()..])
}

/// Decides what startup does. `applied` is in version order.
pub fn plan(
    known: &'static [Migration],
    applied: &[AppliedMigration],
    existing: Existing,
) -> Result<Plan, MigrationError> {
    if !applied.is_empty() {
        return Ok(Plan {
            adoption: Adoption::No,
            run: pending(known, applied)?,
        });
    }
    Ok(match existing {
        Existing::Empty => Plan {
            adoption: Adoption::No,
            run: known,
        },
        Existing::Complete => Plan {
            adoption: Adoption::Record,
            run: &known[1..],
        },
        Existing::Partial => Plan {
            adoption: Adoption::CompleteThenRecord,
            run: &known[1..],
        },
    })
}

/// What a database holds, read without changing it. Rendered by `refhir migrate`.
pub struct Report {
    pub known: &'static [Migration],
    pub applied: Vec<AppliedMigration>,
    pub existing: Existing,
    /// `Err` is a reason startup would refuse.
    pub plan: Result<Plan, MigrationError>,
}

impl Report {
    pub fn new(
        known: &'static [Migration],
        applied: Vec<AppliedMigration>,
        existing: Existing,
    ) -> Self {
        let plan = plan(known, &applied, existing);
        Self {
            known,
            applied,
            existing,
            plan,
        }
    }

    /// Whether startup would go ahead.
    pub fn can_start(&self) -> bool {
        self.plan.is_ok()
    }

    /// The text `refhir migrate` prints. `show_sql` adds the statements startup would run.
    pub fn render(&self, show_sql: bool) -> String {
        use std::fmt::Write;
        let newest = self.known.last().map_or(0, |m| m.version);
        let mut out = format!("this build knows schema versions 1..={newest}\n");
        match (&self.plan, self.applied.len()) {
            (Err(err), _) => {
                let _ = writeln!(out, "state: cannot start\n  {err}");
            }
            (Ok(plan), 0) => {
                let line = match plan.adoption {
                    Adoption::No => format!(
                        "new database: startup applies all {} migration(s)",
                        self.known.len()
                    ),
                    Adoption::Record => "unversioned, with the current tables: startup records \
                        version 1 without running it"
                        .to_string(),
                    Adoption::CompleteThenRecord => "unversioned, with an older schema: startup \
                        adds what is missing (additive only), then records version 1"
                        .to_string(),
                };
                let _ = writeln!(out, "state: {line}");
            }
            (Ok(plan), applied) if plan.run.is_empty() => {
                let _ = writeln!(out, "state: up to date at version {applied}");
            }
            (Ok(plan), applied) => {
                let _ = writeln!(
                    out,
                    "state: at version {applied}, {} migration(s) pending",
                    plan.run.len()
                );
            }
        }
        if self.applied.is_empty() {
            out.push_str("applied: none recorded\n");
        } else {
            out.push_str("applied:\n");
            for row in &self.applied {
                let name = self
                    .known
                    .iter()
                    .find(|m| m.version == row.version)
                    .map_or("(unknown to this build)", |m| m.name);
                let _ = writeln!(
                    out,
                    "  {:>3}  {name}  {}  sha256:{}",
                    row.version,
                    row.applied_at,
                    row.checksum.chars().take(12).collect::<String>()
                );
            }
        }
        if let Ok(plan) = &self.plan {
            let mut steps = Vec::new();
            if plan.adoption != Adoption::No {
                steps.push((&self.known[0], "adopt (recorded, not re-run)"));
            }
            steps.extend(plan.run.iter().map(|m| (m, "apply")));
            if steps.is_empty() {
                out.push_str("pending: none\n");
            } else {
                out.push_str("pending:\n");
                for (migration, what) in &steps {
                    let _ = writeln!(
                        out,
                        "  {:>3}  {}  {what}",
                        migration.version, migration.name
                    );
                }
            }
            if show_sql {
                out.push_str("\nwould run, in one transaction:\n");
                match plan.adoption {
                    Adoption::No => {}
                    Adoption::Record => out.push_str(
                        "-- version 1: recorded in schema_migrations; its SQL is not run\n",
                    ),
                    Adoption::CompleteThenRecord => {
                        out.push_str(
                            "-- version 1: the statements below add what an older release did \
                             not create (they only add, and skip what exists), then it is recorded\n",
                        );
                        let _ = writeln!(out, "{}", self.known[0].sql);
                    }
                }
                for migration in plan.run {
                    let _ = writeln!(
                        out,
                        "-- version {} {}\n{}",
                        migration.version, migration.name, migration.sql
                    );
                }
                if steps.is_empty() {
                    out.push_str("-- nothing\n");
                }
            }
        }
        out
    }
}

/// Panics unless `known` is 1, 2, 3, ... in order. For a backend's own test.
pub fn assert_well_formed(known: &[Migration]) {
    assert!(!known.is_empty(), "no migrations");
    for (position, migration) in known.iter().enumerate() {
        assert_eq!(
            migration.version,
            position as i64 + 1,
            "migration versions must be 1, 2, 3, ... in order"
        );
        assert!(
            !migration.sql.trim().is_empty(),
            "migration {} is empty",
            migration.version
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static KNOWN: [Migration; 3] = [
        Migration {
            version: 1,
            name: "baseline",
            sql: "CREATE TABLE a (x INT);",
        },
        Migration {
            version: 2,
            name: "second",
            sql: "CREATE TABLE b (x INT);",
        },
        Migration {
            version: 3,
            name: "third",
            sql: "CREATE TABLE c (x INT);",
        },
    ];

    fn row(version: i64) -> AppliedMigration {
        AppliedMigration {
            version,
            applied_at: "2026-01-01T00:00:00Z".into(),
            checksum: KNOWN[version as usize - 1].checksum(),
        }
    }

    #[test]
    fn a_new_database_runs_everything() {
        let plan = plan(&KNOWN, &[], Existing::Empty).unwrap();
        assert_eq!(plan.adoption, Adoption::No);
        assert_eq!(plan.run.len(), 3);
    }

    #[test]
    fn a_current_unversioned_database_records_version_one_and_runs_the_rest() {
        let plan = plan(&KNOWN, &[], Existing::Complete).unwrap();
        assert_eq!(plan.adoption, Adoption::Record);
        assert_eq!(
            plan.run.iter().map(|m| m.version).collect::<Vec<_>>(),
            [2, 3]
        );
    }

    #[test]
    fn an_older_unversioned_database_is_completed_before_it_is_recorded() {
        let plan = plan(&KNOWN, &[], Existing::Partial).unwrap();
        assert_eq!(plan.adoption, Adoption::CompleteThenRecord);
        assert_eq!(plan.run.len(), 2);
    }

    #[test]
    fn what_existing_tables_there_are_is_ignored_once_versions_are_recorded() {
        let plan = plan(&KNOWN, &[row(1)], Existing::Partial).unwrap();
        assert_eq!(plan.adoption, Adoption::No);
        assert_eq!(
            plan.run.iter().map(|m| m.version).collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(
            super::plan(&KNOWN, &[row(1), row(2), row(3)], Existing::Empty)
                .unwrap()
                .run
                .is_empty()
        );
    }

    #[test]
    fn a_newer_database_is_refused() {
        let mut newer = row(1);
        newer.version = 4;
        let err = plan(&KNOWN, &[row(1), row(2), row(3), newer], Existing::Complete).unwrap_err();
        assert_eq!(
            err,
            MigrationError::Newer {
                database: 4,
                binary: 3
            }
        );
        assert!(err.to_string().contains("newer ReFHIR"), "{err}");
    }

    #[test]
    fn an_edited_migration_is_refused() {
        let mut edited = row(2);
        edited.checksum = "0".repeat(64);
        let err = plan(&KNOWN, &[row(1), edited], Existing::Complete).unwrap_err();
        assert!(
            matches!(err, MigrationError::Checksum { version: 2, .. }),
            "{err}"
        );
    }

    #[test]
    fn a_gap_in_the_recorded_versions_is_refused() {
        let err = plan(&KNOWN, &[row(1), row(3)], Existing::Complete).unwrap_err();
        assert_eq!(
            err,
            MigrationError::Gap {
                expected: 2,
                found: 3
            }
        );
    }

    #[test]
    fn the_checksum_ignores_line_endings() {
        assert_eq!(checksum("a\nb\n"), checksum("a\r\nb\r\n"));
        assert_ne!(checksum("a\nb\n"), checksum("a\nc\n"));
        assert_eq!(checksum("").len(), 64);
    }

    #[test]
    fn a_damaged_checksum_with_multibyte_text_does_not_panic_the_report() {
        let mut damaged = row(1);
        damaged.checksum = "ééééééééé".into(); // 18 bytes: byte 12 is mid-character
        let report = Report::new(&KNOWN, vec![damaged], Existing::Complete);
        assert!(report.render(false).contains("sha256:"));
        assert!(!report.can_start());
    }

    #[test]
    fn the_report_says_what_startup_would_do() {
        let fresh = Report::new(&KNOWN, vec![], Existing::Empty).render(false);
        assert!(fresh.contains("new database"), "{fresh}");
        let current = Report::new(&KNOWN, vec![row(1), row(2), row(3)], Existing::Complete);
        assert!(current.render(false).contains("up to date at version 3"));
        let behind = Report::new(&KNOWN, vec![row(1)], Existing::Complete).render(true);
        assert!(behind.contains("2 migration(s) pending"), "{behind}");
        assert!(behind.contains("CREATE TABLE b"), "{behind}");
        let adopt = Report::new(&KNOWN, vec![], Existing::Complete).render(true);
        assert!(adopt.contains("without running it"), "{adopt}");
        assert!(adopt.contains("its SQL is not run"), "{adopt}");
        assert!(
            !adopt.contains("CREATE TABLE a"),
            "adoption must not print baseline SQL: {adopt}"
        );
        let completing = Report::new(&KNOWN, vec![], Existing::Partial).render(true);
        assert!(completing.contains("CREATE TABLE a"), "{completing}");
        let mut future = row(1);
        future.version = 9;
        let refused = Report::new(&KNOWN, vec![future], Existing::Complete);
        assert!(!refused.can_start());
        assert!(refused.render(false).contains("cannot start"));
    }
}
