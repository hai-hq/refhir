use refhir_persistence::{
    plan_search_parameter, ConformanceStore, ExpansionChange, InstallError, InstallOutcome,
    InstalledParam, NewExpansion, NewPackage, NewSearchParameter, NewTerminology, ParamPlan,
    ParamStatus, PendingParam, PersistError, StoredBackfill, StoredExpansion, StoredPackage,
    StoredSearchParameter, StoredStructureDefinition, StoredTerminology,
};
use rusqlite::{params, OptionalExtension, Transaction};

use crate::{definite, SqlitePersistence};

impl ConformanceStore for SqlitePersistence {
    fn load_packages(&self) -> Result<Vec<StoredPackage>, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        let mut stmt = guard
            .connection
            .prepare("SELECT id, name, version, fhir_version FROM packages ORDER BY id")
            .map_err(definite)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(StoredPackage {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    version: row.get(2)?,
                    fhir_version: row.get(3)?,
                })
            })
            .map_err(definite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(definite)?;
        Ok(rows)
    }

    fn load_structure_definitions(&self) -> Result<Vec<StoredStructureDefinition>, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        let mut stmt = guard
            .connection
            .prepare(
                "SELECT package_id, url, type_name, kind, is_abstract, json \
                 FROM structure_definitions ORDER BY package_id, url",
            )
            .map_err(definite)?;
        let rows = stmt
            .query_map([], |row| {
                let abstract_flag: i64 = row.get(4)?;
                Ok(StoredStructureDefinition {
                    package_id: row.get(0)?,
                    url: row.get(1)?,
                    type_name: row.get(2)?,
                    kind: row.get(3)?,
                    is_abstract: abstract_flag != 0,
                    json: row.get(5)?,
                })
            })
            .map_err(definite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(definite)?;
        Ok(rows)
    }

    fn load_search_parameters(&self) -> Result<Vec<StoredSearchParameter>, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        let mut stmt = guard
            .connection
            .prepare(
                "SELECT package_id, url, base, code, param_type, expression, status, json \
                 FROM search_parameters ORDER BY package_id, base, code",
            )
            .map_err(definite)?;
        let rows = stmt
            .query_map([], |row| {
                let status: String = row.get(6)?;
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    status,
                    row.get(7)?,
                ))
            })
            .map_err(definite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(definite)?;
        rows.into_iter()
            .map(
                |(package_id, url, base, code, param_type, expression, status, json)| {
                    Ok(StoredSearchParameter {
                        package_id,
                        url,
                        base,
                        code,
                        param_type,
                        expression,
                        status: ParamStatus::parse(&status)?,
                        json,
                    })
                },
            )
            .collect()
    }

    fn load_terminology(&self) -> Result<Vec<StoredTerminology>, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        let mut stmt = guard
            .connection
            .prepare(
                "SELECT package_id, resource_type, url, json FROM terminology ORDER BY package_id, url",
            )
            .map_err(definite)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(StoredTerminology {
                    package_id: row.get(0)?,
                    resource_type: row.get(1)?,
                    url: row.get(2)?,
                    json: row.get(3)?,
                })
            })
            .map_err(definite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(definite)?;
        Ok(rows)
    }

    fn load_terminology_for(
        &self,
        urls: &[String],
    ) -> Result<Vec<StoredTerminology>, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        let mut rows = Vec::new();
        // SQLite caps the number of bound parameters, so a long list goes in pieces.
        for chunk in urls.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(", ");
            // A row matches the URL as it is stored, or the URL without its `|version`.
            let mut stmt = guard
                .connection
                .prepare(&format!(
                    "SELECT package_id, resource_type, url, json FROM terminology \
                     WHERE url IN ({marks}) \
                        OR (CASE WHEN instr(url, '|') > 0 \
                                 THEN substr(url, 1, instr(url, '|') - 1) ELSE url END) IN ({marks}) \
                     ORDER BY package_id, url"
                ))
                .map_err(definite)?;
            let found = stmt
                .query_map(
                    rusqlite::params_from_iter(chunk.iter().chain(chunk.iter())),
                    |row| {
                        Ok(StoredTerminology {
                            package_id: row.get(0)?,
                            resource_type: row.get(1)?,
                            url: row.get(2)?,
                            json: row.get(3)?,
                        })
                    },
                )
                .map_err(definite)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(definite)?;
            rows.extend(found);
        }
        Ok(rows)
    }

    fn apply_expansions(&self, changes: &[ExpansionChange]) -> Result<(), PersistError> {
        if changes.is_empty() {
            return Ok(());
        }
        let guard = self.inner.lock().expect("sqlite");
        let tx = guard.connection.unchecked_transaction().map_err(definite)?;
        for change in changes {
            match &change.json {
                Some(json) => tx.execute(
                    "INSERT INTO value_set_expansion (package_id, url, json) VALUES (?1, ?2, ?3) \
                     ON CONFLICT (package_id, url) DO UPDATE SET json = excluded.json",
                    params![change.package_id, change.url, json],
                ),
                None => tx.execute(
                    "DELETE FROM value_set_expansion WHERE package_id = ?1 AND url = ?2",
                    params![change.package_id, change.url],
                ),
            }
            .map_err(definite)?;
        }
        tx.commit().map_err(definite)?;
        Ok(())
    }

    fn load_expansions(&self) -> Result<Vec<StoredExpansion>, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        let mut stmt = guard
            .connection
            .prepare(
                "SELECT package_id, url, json FROM value_set_expansion ORDER BY package_id, url",
            )
            .map_err(definite)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(StoredExpansion {
                    package_id: row.get(0)?,
                    url: row.get(1)?,
                    json: row.get(2)?,
                })
            })
            .map_err(definite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(definite)?;
        Ok(rows)
    }

    fn install_package(&self, package: &NewPackage) -> Result<InstallOutcome, InstallError> {
        let guard = self.inner.lock().expect("sqlite");
        let tx = guard.connection.unchecked_transaction().map_err(definite)?;
        let outcome = install_in_tx(&tx, package)?;
        tx.commit().map_err(definite)?;
        Ok(outcome)
    }

    fn set_search_parameter_status(
        &self,
        package_id: i64,
        url: &str,
        base: &str,
        status: ParamStatus,
    ) -> Result<(), PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        guard
            .connection
            .execute(
                "UPDATE search_parameters SET status = ?1 \
                 WHERE package_id = ?2 AND url = ?3 AND base = ?4 AND status = 'unsupported'",
                params![status.as_str(), package_id, url, base],
            )
            .map_err(definite)?;
        Ok(())
    }

    fn ensure_local_package(&self, fhir_version: &str) -> Result<i64, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        let existing: Option<i64> = guard
            .connection
            .query_row(
                "SELECT id FROM packages WHERE name = 'refhir.local' AND version = '0'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(definite)?;
        if let Some(id) = existing {
            return Ok(id);
        }
        let current: Option<String> = guard
            .connection
            .query_row("SELECT fhir_version FROM packages LIMIT 1", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(definite)?;
        let version = current.unwrap_or_else(|| fhir_version.to_string());
        guard
            .connection
            .execute(
                "INSERT INTO packages (name, version, fhir_version) VALUES ('refhir.local', '0', ?1)",
                params![version],
            )
            .map_err(definite)?;
        Ok(guard.connection.last_insert_rowid())
    }

    fn upsert_search_parameter(
        &self,
        package_id: i64,
        param: &NewSearchParameter,
    ) -> Result<bool, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        let prior: Option<(String, String, String, String)> = guard
            .connection
            .query_row(
                "SELECT code, param_type, COALESCE(expression, ''), status FROM search_parameters \
                 WHERE package_id = ?1 AND url = ?2 AND base = ?3",
                params![package_id, param.url, param.base],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(definite)?;
        let incoming = param.expression.clone().unwrap_or_default();
        if let Some((code, param_type, expression, _status)) = &prior {
            if code == &param.code && param_type == &param.param_type && expression == &incoming {
                guard
                    .connection
                    .execute(
                        "UPDATE search_parameters SET json = ?1 \
                         WHERE package_id = ?2 AND url = ?3 AND base = ?4",
                        params![param.json, package_id, param.url, param.base],
                    )
                    .map_err(definite)?;
                return Ok(false);
            }
        }
        guard
            .connection
            .execute(
                "INSERT INTO search_parameters \
                 (package_id, url, base, code, param_type, expression, status, json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT (package_id, url, base) DO UPDATE SET \
                 code = excluded.code, param_type = excluded.param_type, \
                 expression = excluded.expression, status = excluded.status, json = excluded.json",
                params![
                    package_id,
                    param.url,
                    param.base,
                    param.code,
                    param.param_type,
                    param.expression,
                    param.status.as_str(),
                    param.json
                ],
            )
            .map_err(definite)?;
        Ok(true)
    }

    fn delete_search_parameter(&self, package_id: i64, url: &str) -> Result<(), PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        guard
            .connection
            .execute(
                "DELETE FROM search_parameters WHERE package_id = ?1 AND url = ?2",
                params![package_id, url],
            )
            .map_err(definite)?;
        Ok(())
    }

    fn upsert_terminology(
        &self,
        package_id: i64,
        row: &NewTerminology,
    ) -> Result<(), PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        guard
            .connection
            .execute(
                "INSERT INTO terminology (package_id, resource_type, url, json) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (package_id, url) DO UPDATE SET \
                 resource_type = excluded.resource_type, json = excluded.json",
                params![package_id, row.resource_type, row.url, row.json],
            )
            .map_err(definite)?;
        Ok(())
    }

    fn delete_terminology(&self, package_id: i64, url: &str) -> Result<(), PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        guard
            .connection
            .execute(
                "DELETE FROM terminology WHERE package_id = ?1 AND url = ?2",
                params![package_id, url],
            )
            .map_err(definite)?;
        Ok(())
    }

    fn replace_expansions(
        &self,
        package_id: i64,
        expansions: &[NewExpansion],
    ) -> Result<(), PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        let tx = guard.connection.unchecked_transaction().map_err(definite)?;
        tx.execute(
            "DELETE FROM value_set_expansion WHERE package_id = ?1",
            params![package_id],
        )
        .map_err(definite)?;
        for expansion in expansions {
            tx.execute(
                "INSERT INTO value_set_expansion (package_id, url, json) VALUES (?1, ?2, ?3)",
                params![package_id, expansion.url, expansion.json],
            )
            .map_err(definite)?;
        }
        tx.commit().map_err(definite)?;
        Ok(())
    }

    fn complete_backfill(
        &self,
        backfill_id: i64,
        package_id: i64,
        base: &str,
    ) -> Result<(), PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        let urls = {
            let mut stmt = guard
                .connection
                .prepare("SELECT url FROM backfill_parameter WHERE backfill_id = ?1")
                .map_err(definite)?;
            let mapped = stmt
                .query_map(params![backfill_id], |row| row.get::<_, String>(0))
                .map_err(definite)?;
            mapped.collect::<Result<Vec<_>, _>>().map_err(definite)?
        };
        if urls.is_empty() {
            guard
                .connection
                .execute(
                    "UPDATE search_parameters SET status = 'ready' \
                     WHERE package_id = ?1 AND base = ?2 AND status = 'pending'",
                    params![package_id, base],
                )
                .map_err(definite)?;
            return Ok(());
        }
        for url in urls {
            guard
                .connection
                .execute(
                    "UPDATE search_parameters SET status = 'ready' \
                     WHERE package_id = ?1 AND base = ?2 AND url = ?3 AND status = 'pending'",
                    params![package_id, base, url],
                )
                .map_err(definite)?;
        }
        Ok(())
    }

    fn mark_base_ready(&self, package_id: i64, base: &str) -> Result<(), PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        guard
            .connection
            .execute(
                "UPDATE search_parameters SET status = 'ready' \
                 WHERE package_id = ?1 AND base = ?2 AND status = 'pending'",
                params![package_id, base],
            )
            .map_err(definite)?;
        Ok(())
    }

    fn insert_backfill(
        &self,
        package_id: i64,
        watermark_ts: i64,
        tablet: &str,
        urls: &[String],
    ) -> Result<i64, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        guard
            .connection
            .execute(
                "INSERT INTO index_backfills (package_id, watermark_ts, tablet, cursor_id, done) \
                 VALUES (?1, ?2, ?3, NULL, 0)",
                params![package_id, watermark_ts, tablet],
            )
            .map_err(definite)?;
        let backfill_id = guard.connection.last_insert_rowid();
        if urls.is_empty() {
            guard
                .connection
                .execute(
                    "INSERT INTO backfill_parameter (backfill_id, url) \
                     SELECT ?1, url FROM search_parameters \
                     WHERE package_id = ?2 AND base = ?3 AND status = 'pending'",
                    params![backfill_id, package_id, tablet],
                )
                .map_err(definite)?;
        } else {
            for url in urls {
                guard
                    .connection
                    .execute(
                        "INSERT OR IGNORE INTO backfill_parameter (backfill_id, url) VALUES (?1, ?2)",
                        params![backfill_id, url],
                    )
                    .map_err(definite)?;
            }
        }
        Ok(backfill_id)
    }

    fn update_backfill(
        &self,
        id: i64,
        cursor_id: Option<&str>,
        done: bool,
    ) -> Result<(), PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        let done_flag: i64 = if done { 1 } else { 0 };
        guard
            .connection
            .execute(
                "UPDATE index_backfills SET cursor_id = ?1, done = ?2 WHERE id = ?3",
                params![cursor_id, done_flag, id],
            )
            .map_err(definite)?;
        Ok(())
    }

    fn load_backfills(&self) -> Result<Vec<StoredBackfill>, PersistError> {
        let guard = self.inner.lock().expect("sqlite");
        let mut stmt = guard
            .connection
            .prepare(
                "SELECT id, package_id, watermark_ts, tablet, cursor_id, done \
                 FROM index_backfills ORDER BY id",
            )
            .map_err(definite)?;
        let rows = stmt
            .query_map([], |row| {
                let done: i64 = row.get(5)?;
                Ok(StoredBackfill {
                    id: row.get(0)?,
                    package_id: row.get(1)?,
                    watermark_ts: row.get(2)?,
                    tablet: row.get(3)?,
                    cursor_id: row.get(4)?,
                    done: done != 0,
                })
            })
            .map_err(definite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(definite)?;
        Ok(rows)
    }
}

fn install_in_tx(
    tx: &Transaction<'_>,
    package: &NewPackage,
) -> Result<InstallOutcome, InstallError> {
    let existing_id: Option<i64> = tx
        .query_row(
            "SELECT id FROM packages WHERE name = ?1 AND version = ?2",
            params![package.name, package.version],
            |row| row.get(0),
        )
        .optional()
        .map_err(definite)?;
    if let Some(package_id) = existing_id {
        insert_terminology(tx, package_id, package)?;
        replace_expansions(tx, package_id, package)?;
        return Ok(InstallOutcome {
            package_id,
            inserted: false,
            newly_pending: Vec::new(),
        });
    }
    let current: Option<String> = tx
        .query_row("SELECT fhir_version FROM packages LIMIT 1", [], |row| {
            row.get(0)
        })
        .optional()
        .map_err(definite)?;
    if let Some(existing) = current {
        if existing != package.fhir_version {
            return Err(InstallError::FhirVersion {
                existing,
                incoming: package.fhir_version.clone(),
            });
        }
    }
    tx.execute(
        "INSERT INTO packages (name, version, fhir_version) VALUES (?1, ?2, ?3)",
        params![package.name, package.version, package.fhir_version],
    )
    .map_err(definite)?;
    let package_id = tx.last_insert_rowid();
    for definition in &package.structure_definitions {
        let abstract_flag: i64 = if definition.is_abstract { 1 } else { 0 };
        tx.execute(
            "INSERT INTO structure_definitions \
             (package_id, url, type_name, kind, is_abstract, json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                package_id,
                definition.url,
                definition.type_name,
                definition.kind,
                abstract_flag,
                definition.json
            ],
        )
        .map_err(definite)?;
    }
    let mut newly_pending = Vec::new();
    for param in &package.search_parameters {
        let installed = tx
            .prepare(
                "SELECT package_id, url, COALESCE(expression, ''), status, json \
                 FROM search_parameters WHERE base = ?1 AND code = ?2",
            )
            .map_err(definite)?
            .query_map(params![param.base, param.code], |row| {
                Ok(InstalledParam {
                    package_id: row.get(0)?,
                    url: row.get(1)?,
                    expression: row.get(2)?,
                    status: row.get(3)?,
                    json: row.get(4)?,
                })
            })
            .map_err(definite)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(definite)?;
        let (existing, remove) = match plan_search_parameter(param, &installed)? {
            ParamPlan::Conflict => {
                return Err(InstallError::Expression {
                    base: param.base.clone(),
                    code: param.code.clone(),
                });
            }
            ParamPlan::Skip => continue,
            ParamPlan::Insert { status, remove } => (status, remove),
        };
        for (owner, url) in remove {
            tx.execute(
                "DELETE FROM search_parameters \
                 WHERE package_id = ?1 AND url = ?2 AND base = ?3 AND code = ?4",
                params![owner, url, param.base, param.code],
            )
            .map_err(definite)?;
        }
        let is_new = existing.is_none();
        let status = existing.unwrap_or(param.status);
        if is_new && status == ParamStatus::Pending {
            newly_pending.push(PendingParam {
                base: param.base.clone(),
                code: param.code.clone(),
            });
        }
        tx.execute(
            "INSERT INTO search_parameters \
             (package_id, url, base, code, param_type, expression, status, json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                package_id,
                param.url,
                param.base,
                param.code,
                param.param_type,
                param.expression,
                status.as_str(),
                param.json
            ],
        )
        .map_err(definite)?;
    }
    insert_terminology(tx, package_id, package)?;
    replace_expansions(tx, package_id, package)?;
    Ok(InstallOutcome {
        package_id,
        inserted: true,
        newly_pending,
    })
}

fn insert_terminology(
    tx: &Transaction<'_>,
    package_id: i64,
    package: &NewPackage,
) -> Result<(), InstallError> {
    for resource in &package.terminology {
        tx.execute(
            "INSERT OR IGNORE INTO terminology (package_id, resource_type, url, json) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                package_id,
                resource.resource_type,
                resource.url,
                resource.json
            ],
        )
        .map_err(definite)?;
    }
    Ok(())
}

fn replace_expansions(
    tx: &Transaction<'_>,
    package_id: i64,
    package: &NewPackage,
) -> Result<(), InstallError> {
    tx.execute(
        "DELETE FROM value_set_expansion WHERE package_id = ?1",
        params![package_id],
    )
    .map_err(definite)?;
    for expansion in &package.expansions {
        tx.execute(
            "INSERT INTO value_set_expansion (package_id, url, json) VALUES (?1, ?2, ?3)",
            params![package_id, expansion.url, expansion.json],
        )
        .map_err(definite)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminology_is_read_by_url_and_expansions_change_one_at_a_time() {
        let store = SqlitePersistence::open_in_memory().unwrap();
        refhir_persistence::conformance::exercise_terminology_changes(&store, "sqlite");
    }

    #[test]
    fn parameters_from_different_packages_resolve_clashes() {
        let store = SqlitePersistence::open_in_memory().unwrap();
        refhir_persistence::conformance::exercise_parameter_clashes(&store, "sqlite");
    }
}
