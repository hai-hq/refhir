use haifhir_persistence::{
    ConformanceStore, InstallError, InstallOutcome, NewPackage, ParamStatus, PendingParam,
    PersistError, StoredBackfill, StoredPackage, StoredSearchParameter, StoredStructureDefinition,
};
use postgres::Transaction;

use crate::{definite, PostgresPersistence};

impl ConformanceStore for PostgresPersistence {
    fn load_packages(&self) -> Result<Vec<StoredPackage>, PersistError> {
        let mut guard = self.inner.lock().expect("postgres");
        let rows = guard
            .client
            .query(
                "SELECT id, name, version, fhir_version FROM packages ORDER BY id",
                &[],
            )
            .map_err(definite)?;
        Ok(rows
            .iter()
            .map(|row| StoredPackage {
                id: row.get(0),
                name: row.get(1),
                version: row.get(2),
                fhir_version: row.get(3),
            })
            .collect())
    }

    fn load_structure_definitions(&self) -> Result<Vec<StoredStructureDefinition>, PersistError> {
        let mut guard = self.inner.lock().expect("postgres");
        let rows = guard
            .client
            .query(
                "SELECT package_id, url, type_name, kind, is_abstract, json \
                 FROM structure_definitions ORDER BY package_id, url",
                &[],
            )
            .map_err(definite)?;
        Ok(rows
            .iter()
            .map(|row| {
                let abstract_flag: i32 = row.get(4);
                StoredStructureDefinition {
                    package_id: row.get(0),
                    url: row.get(1),
                    type_name: row.get(2),
                    kind: row.get(3),
                    is_abstract: abstract_flag != 0,
                    json: row.get(5),
                }
            })
            .collect())
    }

    fn load_search_parameters(&self) -> Result<Vec<StoredSearchParameter>, PersistError> {
        let mut guard = self.inner.lock().expect("postgres");
        let rows = guard
            .client
            .query(
                "SELECT package_id, url, base, code, param_type, expression, status, json \
                 FROM search_parameters ORDER BY package_id, base, code",
                &[],
            )
            .map_err(definite)?;
        rows.iter()
            .map(|row| {
                let status: String = row.get(6);
                Ok(StoredSearchParameter {
                    package_id: row.get(0),
                    url: row.get(1),
                    base: row.get(2),
                    code: row.get(3),
                    param_type: row.get(4),
                    expression: row.get(5),
                    status: ParamStatus::parse(&status)?,
                    json: row.get(7),
                })
            })
            .collect()
    }

    fn install_package(&self, package: &NewPackage) -> Result<InstallOutcome, InstallError> {
        let mut guard = self.inner.lock().expect("postgres");
        let mut tx = guard.client.transaction().map_err(definite)?;
        let outcome = install_in_tx(&mut tx, package)?;
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
        let mut guard = self.inner.lock().expect("postgres");
        let status = status.as_str().to_string();
        let url = url.to_string();
        let base = base.to_string();
        guard
            .client
            .execute(
                "UPDATE search_parameters SET status = $1 \
                 WHERE package_id = $2 AND url = $3 AND base = $4 AND status = 'unsupported'",
                &[&status, &package_id, &url, &base],
            )
            .map_err(definite)?;
        Ok(())
    }

    fn mark_base_ready(&self, package_id: i64, base: &str) -> Result<(), PersistError> {
        let mut guard = self.inner.lock().expect("postgres");
        guard
            .client
            .execute(
                "UPDATE search_parameters SET status = 'ready' \
                 WHERE package_id = $1 AND base = $2 AND status = 'pending'",
                &[&package_id, &base],
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
        let mut guard = self.inner.lock().expect("postgres");
        let row = guard
            .client
            .query_one(
                "INSERT INTO index_backfills (package_id, watermark_ts, tablet, cursor_id, done) \
                 VALUES ($1, $2, $3, NULL, 0) RETURNING id",
                &[&package_id, &watermark_ts, &tablet],
            )
            .map_err(definite)?;
        Ok(row.get(0))
    }

    fn update_backfill(
        &self,
        id: i64,
        cursor_id: Option<&str>,
        done: bool,
    ) -> Result<(), PersistError> {
        let mut guard = self.inner.lock().expect("postgres");
        let done_flag: i32 = if done { 1 } else { 0 };
        guard
            .client
            .execute(
                "UPDATE index_backfills SET cursor_id = $1, done = $2 WHERE id = $3",
                &[&cursor_id, &done_flag, &id],
            )
            .map_err(definite)?;
        Ok(())
    }

    fn load_backfills(&self) -> Result<Vec<StoredBackfill>, PersistError> {
        let mut guard = self.inner.lock().expect("postgres");
        let rows = guard
            .client
            .query(
                "SELECT id, package_id, watermark_ts, tablet, cursor_id, done \
                 FROM index_backfills ORDER BY id",
                &[],
            )
            .map_err(definite)?;
        Ok(rows
            .iter()
            .map(|row| {
                let done: i32 = row.get(5);
                StoredBackfill {
                    id: row.get(0),
                    package_id: row.get(1),
                    watermark_ts: row.get(2),
                    tablet: row.get(3),
                    cursor_id: row.get(4),
                    done: done != 0,
                }
            })
            .collect())
    }
}

fn install_in_tx(
    tx: &mut Transaction<'_>,
    package: &NewPackage,
) -> Result<InstallOutcome, InstallError> {
    let existing = tx
        .query_opt(
            "SELECT id FROM packages WHERE name = $1 AND version = $2",
            &[&package.name, &package.version],
        )
        .map_err(definite)?;
    if let Some(row) = existing {
        return Ok(InstallOutcome {
            package_id: row.get(0),
            inserted: false,
            newly_pending: Vec::new(),
        });
    }
    let current = tx
        .query_opt("SELECT fhir_version FROM packages LIMIT 1", &[])
        .map_err(definite)?;
    if let Some(row) = current {
        let existing: String = row.get(0);
        if existing != package.fhir_version {
            return Err(InstallError::FhirVersion {
                existing,
                incoming: package.fhir_version.clone(),
            });
        }
    }
    let inserted = tx
        .query_one(
            "INSERT INTO packages (name, version, fhir_version) VALUES ($1, $2, $3) RETURNING id",
            &[&package.name, &package.version, &package.fhir_version],
        )
        .map_err(definite)?;
    let package_id: i64 = inserted.get(0);
    for definition in &package.structure_definitions {
        let abstract_flag: i32 = if definition.is_abstract { 1 } else { 0 };
        tx.execute(
            "INSERT INTO structure_definitions \
             (package_id, url, type_name, kind, is_abstract, json) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &package_id,
                &definition.url,
                &definition.type_name,
                &definition.kind,
                &abstract_flag,
                &definition.json,
            ],
        )
        .map_err(definite)?;
    }
    let mut newly_pending = Vec::new();
    for param in &package.search_parameters {
        let prior = tx
            .query_opt(
                "SELECT COALESCE(expression, ''), status FROM search_parameters \
                 WHERE base = $1 AND code = $2 LIMIT 1",
                &[&param.base, &param.code],
            )
            .map_err(definite)?;
        let incoming = param.expression.clone().unwrap_or_default();
        let status = if let Some(row) = prior {
            let existing_expr: String = row.get(0);
            let existing_status: String = row.get(1);
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
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[
                &package_id,
                &param.url,
                &param.base,
                &param.code,
                &param.param_type,
                &param.expression,
                &status.as_str(),
                &param.json,
            ],
        )
        .map_err(definite)?;
    }
    Ok(InstallOutcome {
        package_id,
        inserted: true,
        newly_pending,
    })
}
