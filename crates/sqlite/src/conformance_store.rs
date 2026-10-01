use refhir_persistence::{
    ConformanceStore, InstallError, InstallOutcome, NewPackage, ParamStatus, PendingParam,
    PersistError, StoredBackfill, StoredPackage, StoredSearchParameter, StoredStructureDefinition,
    StoredTerminology,
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
        Ok(guard.connection.last_insert_rowid())
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
        let prior: Option<(String, String)> = tx
            .query_row(
                "SELECT COALESCE(expression, ''), status FROM search_parameters \
                 WHERE base = ?1 AND code = ?2 LIMIT 1",
                params![param.base, param.code],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(definite)?;
        let incoming = param.expression.clone().unwrap_or_default();
        let status = if let Some((existing_expr, existing_status)) = prior {
            if existing_expr != incoming {
                return Err(InstallError::Expression {
                    base: param.base.clone(),
                    code: param.code.clone(),
                });
            }
            ParamStatus::parse(&existing_status)?
        } else {
            if param.status == ParamStatus::Pending {
                newly_pending.push(PendingParam {
                    base: param.base.clone(),
                    code: param.code.clone(),
                });
            }
            param.status
        };
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
