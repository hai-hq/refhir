use postgres::Transaction;
use refhir_persistence::{
    plan_search_parameter, ConformanceStore, ExpansionChange, InstallError, InstallOutcome,
    InstalledParam, NewExpansion, NewPackage, NewSearchParameter, NewTerminology, ParamPlan,
    ParamStatus, PendingParam, PersistError, PersistenceGlobalKey, StoredBackfill, StoredExpansion,
    StoredPackage, StoredSearchParameter, StoredStructureDefinition, StoredTerminology,
};

use crate::{definite, PostgresPersistence};

impl PostgresPersistence {
    fn read_counter(&self, key: PersistenceGlobalKey) -> Result<u64, PersistError> {
        let mut guard = self.inner.lock()?;
        let row = guard
            .client
            .query_opt(
                "SELECT json_value FROM persistence_globals WHERE key = $1",
                &[&key.as_str()],
            )
            .map_err(definite)?;
        Ok(row
            .and_then(|row| row.get::<_, String>(0).trim().parse().ok())
            .unwrap_or(0))
    }

    /// Adds one, fenced like every other conformance write.
    fn bump_counter(&self, key: PersistenceGlobalKey) -> Result<u64, PersistError> {
        let mut guard = self.inner.lock()?;
        let mut tx = guard.client.transaction().map_err(definite)?;
        self.fence(&mut tx)?;
        let text: String = tx
            .query_one(
                "INSERT INTO persistence_globals (key, json_value) VALUES ($1, '1')
                 ON CONFLICT (key) DO UPDATE
                 SET json_value = (persistence_globals.json_value::bigint + 1)::text
                 RETURNING json_value",
                &[&key.as_str()],
            )
            .map_err(definite)?
            .get(0);
        tx.commit().map_err(definite)?;
        text.trim()
            .parse()
            .map_err(|_| PersistError::Definite(format!("bad {} {text:?}", key.as_str())))
    }

    /// The writer epoch check, inside the transaction of a conformance write. Document commits
    /// are fenced in `write`; these tables are written outside it, so without this a primary
    /// that was paused and replaced could wake up and mark a backfill done or a parameter ready
    /// behind its successor's back, in the moments before its lease keepalive stops it.
    fn fence(&self, tx: &mut Transaction<'_>) -> Result<(), PersistError> {
        match self.lease.epoch() {
            Some(epoch) => crate::lease::check_epoch(tx, epoch),
            None => Ok(()),
        }
    }
}

impl ConformanceStore for PostgresPersistence {
    fn catalog_version(&self) -> Result<u64, PersistError> {
        crate::off_runtime(|| self.read_counter(PersistenceGlobalKey::CatalogVersion))
    }

    fn bump_catalog_version(&self) -> Result<u64, PersistError> {
        crate::off_runtime(|| self.bump_counter(PersistenceGlobalKey::CatalogVersion))
    }

    fn catalog_revision(&self) -> Result<u64, PersistError> {
        crate::off_runtime(|| self.read_counter(PersistenceGlobalKey::CatalogRevision))
    }

    fn bump_catalog_revision(&self) -> Result<u64, PersistError> {
        crate::off_runtime(|| self.bump_counter(PersistenceGlobalKey::CatalogRevision))
    }

    fn load_packages(&self) -> Result<Vec<StoredPackage>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
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
        })
    }

    fn load_structure_definitions(&self) -> Result<Vec<StoredStructureDefinition>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
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
        })
    }

    fn load_search_parameters(&self) -> Result<Vec<StoredSearchParameter>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
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
        })
    }

    fn load_terminology(&self) -> Result<Vec<StoredTerminology>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let rows = guard
            .client
            .query(
                "SELECT package_id, resource_type, url, json FROM terminology ORDER BY package_id, url",
                &[],
            )
            .map_err(definite)?;
            Ok(rows
                .iter()
                .map(|row| StoredTerminology {
                    package_id: row.get(0),
                    resource_type: row.get(1),
                    url: row.get(2),
                    json: row.get(3),
                })
                .collect())
        })
    }

    fn load_terminology_for(
        &self,
        urls: &[String],
    ) -> Result<Vec<StoredTerminology>, PersistError> {
        crate::off_runtime(|| {
            if urls.is_empty() {
                return Ok(Vec::new());
            }
            let mut guard = self.inner.lock()?;
            let rows = guard
                .client
                .query(
                    "SELECT package_id, resource_type, url, json FROM terminology \
                 WHERE url = ANY($1) OR split_part(url, '|', 1) = ANY($1) \
                 ORDER BY package_id, url",
                    &[&urls],
                )
                .map_err(definite)?;
            Ok(rows
                .iter()
                .map(|row| StoredTerminology {
                    package_id: row.get(0),
                    resource_type: row.get(1),
                    url: row.get(2),
                    json: row.get(3),
                })
                .collect())
        })
    }

    fn apply_expansions(&self, changes: &[ExpansionChange]) -> Result<(), PersistError> {
        crate::off_runtime(|| {
            if changes.is_empty() {
                return Ok(());
            }
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            self.fence(&mut tx)?;
            for change in changes {
                match &change.json {
                Some(json) => tx.execute(
                    "INSERT INTO value_set_expansion (package_id, url, json) VALUES ($1, $2, $3) \
                     ON CONFLICT (package_id, url) DO UPDATE SET json = excluded.json",
                    &[&change.package_id, &change.url, json],
                ),
                None => tx.execute(
                    "DELETE FROM value_set_expansion WHERE package_id = $1 AND url = $2",
                    &[&change.package_id, &change.url],
                ),
            }
            .map_err(definite)?;
            }
            tx.commit().map_err(definite)?;
            Ok(())
        })
    }

    fn load_terminology_of(&self, package_id: i64) -> Result<Vec<StoredTerminology>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let rows = guard
                .client
                .query(
                    "SELECT package_id, resource_type, url, json FROM terminology \
                 WHERE package_id = $1 ORDER BY url",
                    &[&package_id],
                )
                .map_err(definite)?;
            Ok(rows
                .iter()
                .map(|row| StoredTerminology {
                    package_id: row.get(0),
                    resource_type: row.get(1),
                    url: row.get(2),
                    json: row.get(3),
                })
                .collect())
        })
    }

    fn load_search_parameters_of(
        &self,
        package_id: i64,
    ) -> Result<Vec<StoredSearchParameter>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let rows = guard
                .client
                .query(
                    "SELECT package_id, url, base, code, param_type, expression, status, json \
                 FROM search_parameters WHERE package_id = $1 ORDER BY base, code",
                    &[&package_id],
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
        })
    }

    fn load_expansions(&self) -> Result<Vec<StoredExpansion>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let rows = guard
            .client
            .query(
                "SELECT package_id, url, json FROM value_set_expansion ORDER BY package_id, url",
                &[],
            )
            .map_err(definite)?;
            Ok(rows
                .iter()
                .map(|row| StoredExpansion {
                    package_id: row.get(0),
                    url: row.get(1),
                    json: row.get(2),
                })
                .collect())
        })
    }

    fn install_package(&self, package: &NewPackage) -> Result<InstallOutcome, InstallError> {
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            self.fence(&mut tx)?;
            let outcome = install_in_tx(&mut tx, package)?;
            tx.commit().map_err(definite)?;
            Ok(outcome)
        })
    }

    fn set_search_parameter_status(
        &self,
        package_id: i64,
        url: &str,
        base: &str,
        status: ParamStatus,
    ) -> Result<(), PersistError> {
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            self.fence(&mut tx)?;
            let status = status.as_str().to_string();
            let url = url.to_string();
            let base = base.to_string();
            tx.execute(
                "UPDATE search_parameters SET status = $1 \
             WHERE package_id = $2 AND url = $3 AND base = $4 AND status = 'unsupported'",
                &[&status, &package_id, &url, &base],
            )
            .map_err(definite)?;
            tx.commit().map_err(definite)
        })
    }

    fn ensure_local_package(&self, fhir_version: &str) -> Result<i64, PersistError> {
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            self.fence(&mut tx)?;
            let existing = tx
                .query_opt(
                    "SELECT id FROM packages WHERE name = 'refhir.local' AND version = '0'",
                    &[],
                )
                .map_err(definite)?;
            if let Some(row) = existing {
                return Ok(row.get(0));
            }
            let current = tx
                .query_opt("SELECT fhir_version FROM packages LIMIT 1", &[])
                .map_err(definite)?;
            let version = current
                .map(|row| row.get::<_, String>(0))
                .unwrap_or_else(|| fhir_version.to_string());
            let inserted = tx
                .query_one(
                    "INSERT INTO packages (name, version, fhir_version) \
                 VALUES ('refhir.local', '0', $1) RETURNING id",
                    &[&version],
                )
                .map_err(definite)?;
            let id: i64 = inserted.get(0);
            tx.commit().map_err(definite)?;
            Ok(id)
        })
    }

    fn upsert_search_parameter(
        &self,
        package_id: i64,
        param: &NewSearchParameter,
    ) -> Result<bool, PersistError> {
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            self.fence(&mut tx)?;
            let prior = tx
            .query_opt(
                "SELECT code, param_type, COALESCE(expression, ''), status FROM search_parameters \
                 WHERE package_id = $1 AND url = $2 AND base = $3",
                &[&package_id, &param.url, &param.base],
            )
            .map_err(definite)?;
            let incoming = param.expression.clone().unwrap_or_default();
            if let Some(row) = prior {
                let code: String = row.get(0);
                let param_type: String = row.get(1);
                let expression: String = row.get(2);
                if code == param.code && param_type == param.param_type && expression == incoming {
                    tx.execute(
                        "UPDATE search_parameters SET json = $1 \
                     WHERE package_id = $2 AND url = $3 AND base = $4",
                        &[&param.json, &package_id, &param.url, &param.base],
                    )
                    .map_err(definite)?;
                    tx.commit().map_err(definite)?;
                    return Ok(false);
                }
            }
            let status = param.status.as_str();
            tx.execute(
                "INSERT INTO search_parameters \
             (package_id, url, base, code, param_type, expression, status, json) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (package_id, url, base) DO UPDATE SET \
             code = EXCLUDED.code, param_type = EXCLUDED.param_type, \
             expression = EXCLUDED.expression, status = EXCLUDED.status, json = EXCLUDED.json",
                &[
                    &package_id,
                    &param.url,
                    &param.base,
                    &param.code,
                    &param.param_type,
                    &param.expression,
                    &status,
                    &param.json,
                ],
            )
            .map_err(definite)?;
            tx.commit().map_err(definite)?;
            Ok(true)
        })
    }

    fn delete_search_parameter(&self, package_id: i64, url: &str) -> Result<(), PersistError> {
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            self.fence(&mut tx)?;
            let url = url.to_string();
            tx.execute(
                "DELETE FROM search_parameters WHERE package_id = $1 AND url = $2",
                &[&package_id, &url],
            )
            .map_err(definite)?;
            tx.commit().map_err(definite)
        })
    }

    fn upsert_terminology(
        &self,
        package_id: i64,
        row: &NewTerminology,
    ) -> Result<(), PersistError> {
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            self.fence(&mut tx)?;
            tx.execute(
                "INSERT INTO terminology (package_id, resource_type, url, json) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (package_id, url) DO UPDATE SET \
             resource_type = EXCLUDED.resource_type, json = EXCLUDED.json",
                &[&package_id, &row.resource_type, &row.url, &row.json],
            )
            .map_err(definite)?;
            tx.commit().map_err(definite)
        })
    }

    fn delete_terminology(&self, package_id: i64, url: &str) -> Result<(), PersistError> {
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            self.fence(&mut tx)?;
            let url = url.to_string();
            tx.execute(
                "DELETE FROM terminology WHERE package_id = $1 AND url = $2",
                &[&package_id, &url],
            )
            .map_err(definite)?;
            tx.commit().map_err(definite)
        })
    }

    fn replace_expansions(
        &self,
        package_id: i64,
        expansions: &[NewExpansion],
    ) -> Result<(), PersistError> {
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            self.fence(&mut tx)?;
            tx.execute(
                "DELETE FROM value_set_expansion WHERE package_id = $1",
                &[&package_id],
            )
            .map_err(definite)?;
            for expansion in expansions {
                tx.execute(
                    "INSERT INTO value_set_expansion (package_id, url, json) VALUES ($1, $2, $3)",
                    &[&package_id, &expansion.url, &expansion.json],
                )
                .map_err(definite)?;
            }
            tx.commit().map_err(definite)?;
            Ok(())
        })
    }

    fn complete_backfill(
        &self,
        backfill_id: i64,
        package_id: i64,
        base: &str,
    ) -> Result<(), PersistError> {
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            // Marking parameters ready changes what search may use, so a stale primary must not
            // be able to do it after a successor took over.
            self.fence(&mut tx)?;
            let urls: Vec<String> = tx
                .query(
                    "SELECT url FROM backfill_parameter WHERE backfill_id = $1",
                    &[&backfill_id],
                )
                .map_err(definite)?
                .iter()
                .map(|row| row.get(0))
                .collect();
            if urls.is_empty() {
                tx.execute(
                    "UPDATE search_parameters SET status = 'ready' \
                 WHERE package_id = $1 AND base = $2 AND status = 'pending'",
                    &[&package_id, &base],
                )
                .map_err(definite)?;
            }
            for url in urls {
                tx.execute(
                    "UPDATE search_parameters SET status = 'ready' \
                 WHERE package_id = $1 AND base = $2 AND url = $3 AND status = 'pending'",
                    &[&package_id, &base, &url],
                )
                .map_err(definite)?;
            }
            tx.commit().map_err(definite)
        })
    }

    fn mark_base_ready(&self, package_id: i64, base: &str) -> Result<(), PersistError> {
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            self.fence(&mut tx)?;
            tx.execute(
                "UPDATE search_parameters SET status = 'ready' \
             WHERE package_id = $1 AND base = $2 AND status = 'pending'",
                &[&package_id, &base],
            )
            .map_err(definite)?;
            tx.commit().map_err(definite)
        })
    }

    fn insert_backfill(
        &self,
        package_id: i64,
        watermark_ts: i64,
        tablet: &str,
        urls: &[String],
    ) -> Result<i64, PersistError> {
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            self.fence(&mut tx)?;
            let row = tx
            .query_one(
                "INSERT INTO index_backfills (package_id, watermark_ts, tablet, cursor_id, done) \
                 VALUES ($1, $2, $3, NULL, 0) RETURNING id",
                &[&package_id, &watermark_ts, &tablet],
            )
            .map_err(definite)?;
            let backfill_id: i64 = row.get(0);
            if urls.is_empty() {
                tx.execute(
                    "INSERT INTO backfill_parameter (backfill_id, url) \
                 SELECT $1, url FROM search_parameters \
                 WHERE package_id = $2 AND base = $3 AND status = 'pending'",
                    &[&backfill_id, &package_id, &tablet],
                )
                .map_err(definite)?;
            } else {
                for url in urls {
                    tx.execute(
                        "INSERT INTO backfill_parameter (backfill_id, url) VALUES ($1, $2) \
                     ON CONFLICT (backfill_id, url) DO NOTHING",
                        &[&backfill_id, url],
                    )
                    .map_err(definite)?;
                }
            }
            tx.commit().map_err(definite)?;
            Ok(backfill_id)
        })
    }

    fn update_backfill(
        &self,
        id: i64,
        cursor_id: Option<&str>,
        done: bool,
    ) -> Result<(), PersistError> {
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            // The cursor is what a takeover resumes from, so only the lease holder may move it.
            self.fence(&mut tx)?;
            let done_flag: i32 = if done { 1 } else { 0 };
            tx.execute(
                "UPDATE index_backfills SET cursor_id = $1, done = $2 WHERE id = $3",
                &[&cursor_id, &done_flag, &id],
            )
            .map_err(definite)?;
            tx.commit().map_err(definite)
        })
    }

    fn load_backfills(&self) -> Result<Vec<StoredBackfill>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let rows = guard
                .client
                .query(
                    "SELECT id, package_id, watermark_ts, tablet, cursor_id, done, claimed_epoch \
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
                        claimed_epoch: row.get(6),
                    }
                })
                .collect())
        })
    }

    fn claim_backfill(&self, id: i64) -> Result<bool, PersistError> {
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            self.fence(&mut tx)?;
            let ours = self.lease.epoch().unwrap_or(0) as i64;
            let row = tx
                .query_opt(
                    "SELECT done, claimed_epoch FROM index_backfills WHERE id = $1 FOR UPDATE",
                    &[&id],
                )
                .map_err(definite)?;
            let Some(row) = row else {
                return Ok(false);
            };
            let done: i32 = row.get(0);
            let held: Option<i64> = row.get(1);
            if done != 0 {
                return Ok(false);
            }
            if let Some(held) = held.filter(|held| *held > ours) {
                return Err(PersistError::Fenced(format!(
                    "backfill {id} is claimed by writer epoch {held}, this process holds {ours}"
                )));
            }
            tx.execute(
                "UPDATE index_backfills SET claimed_epoch = $1 WHERE id = $2",
                &[&ours, &id],
            )
            .map_err(definite)?;
            tx.commit().map_err(definite)?;
            Ok(true)
        })
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
        let package_id: i64 = row.get(0);
        insert_terminology(tx, package_id, package)?;
        replace_expansions(tx, package_id, package)?;
        return Ok(InstallOutcome {
            package_id,
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
        let installed = tx
            .query(
                "SELECT package_id, url, COALESCE(expression, ''), status, json \
                 FROM search_parameters WHERE base = $1 AND code = $2",
                &[&param.base, &param.code],
            )
            .map_err(definite)?
            .iter()
            .map(|row| InstalledParam {
                package_id: row.get(0),
                url: row.get(1),
                expression: row.get(2),
                status: row.get(3),
                json: row.get(4),
            })
            .collect::<Vec<_>>();
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
                 WHERE package_id = $1 AND url = $2 AND base = $3 AND code = $4",
                &[&owner, &url, &param.base, &param.code],
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
    insert_terminology(tx, package_id, package)?;
    replace_expansions(tx, package_id, package)?;
    Ok(InstallOutcome {
        package_id,
        inserted: true,
        newly_pending,
    })
}

fn insert_terminology(
    tx: &mut Transaction<'_>,
    package_id: i64,
    package: &NewPackage,
) -> Result<(), InstallError> {
    for resource in &package.terminology {
        tx.execute(
            "INSERT INTO terminology (package_id, resource_type, url, json) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (package_id, url) DO NOTHING",
            &[
                &package_id,
                &resource.resource_type,
                &resource.url,
                &resource.json,
            ],
        )
        .map_err(definite)?;
    }
    Ok(())
}

fn replace_expansions(
    tx: &mut Transaction<'_>,
    package_id: i64,
    package: &NewPackage,
) -> Result<(), InstallError> {
    tx.execute(
        "DELETE FROM value_set_expansion WHERE package_id = $1",
        &[&package_id],
    )
    .map_err(definite)?;
    for expansion in &package.expansions {
        tx.execute(
            "INSERT INTO value_set_expansion (package_id, url, json) VALUES ($1, $2, $3)",
            &[&package_id, &expansion.url, &expansion.json],
        )
        .map_err(definite)?;
    }
    Ok(())
}
