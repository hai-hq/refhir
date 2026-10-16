use postgres::Transaction;
use refhir_persistence::{
    plan_package_parameters, ConformanceStore, ExpansionChange, InstallError, InstallOutcome,
    InstalledParam, NewExpansion, NewPackage, NewSearchParameter, NewStructureDefinition,
    NewTerminology, ParamStatus, PersistError, PersistenceGlobalKey, StoredBackfill,
    StoredExpansion, StoredPackage, StoredParameterRow, StoredSearchParameter,
    StoredStructureDefinition, StoredTerminology, CONFORMANCE_PAGE_ROWS,
};

use crate::{definite, PostgresPersistence};

fn structure_definition_from(row: &postgres::Row) -> StoredStructureDefinition {
    let abstract_flag: i32 = row.get(4);
    StoredStructureDefinition {
        package_id: row.get(0),
        url: row.get(1),
        type_name: row.get(2),
        kind: row.get(3),
        is_abstract: abstract_flag != 0,
        json: row.get(5),
    }
}

fn terminology_from(row: &postgres::Row) -> StoredTerminology {
    StoredTerminology {
        package_id: row.get(0),
        resource_type: row.get(1),
        url: row.get(2),
        json: row.get(3),
    }
}

fn expansion_from(row: &postgres::Row) -> StoredExpansion {
    StoredExpansion {
        package_id: row.get(0),
        url: row.get(1),
        json: row.get(2),
    }
}

impl PostgresPersistence {
    /// Read `select` (a `SELECT` of a table keyed by `(package_id, url)`) a page at a time, in key
    /// order, and hand each page to `visit`. A connection is taken for the query of each page
    /// only, so none is held while `visit` works on the rows, which for a catalog load is seconds
    /// of work, and the client never holds more than a page of the wire data at once.
    fn visit_pages<T: Send>(
        &self,
        select: &str,
        read: impl Fn(&postgres::Row) -> T + Sync,
        key: impl Fn(&T) -> (i64, String),
        visit: &mut dyn FnMut(Vec<T>) -> Result<(), PersistError>,
    ) -> Result<(), PersistError> {
        let first = format!("{select} ORDER BY package_id, url LIMIT {CONFORMANCE_PAGE_ROWS}");
        let next = format!(
            "{select} WHERE (package_id, url) > ($1, $2) \
             ORDER BY package_id, url LIMIT {CONFORMANCE_PAGE_ROWS}"
        );
        let mut after: Option<(i64, String)> = None;
        loop {
            let page: Vec<T> = crate::off_runtime(|| {
                let mut guard = self.inner.lock()?;
                let rows = match &after {
                    None => guard.client.query(first.as_str(), &[]),
                    Some((package_id, url)) => {
                        guard.client.query(next.as_str(), &[package_id, url])
                    }
                }
                .map_err(definite)?;
                Ok::<_, PersistError>(rows.iter().map(&read).collect())
            })?;
            let Some(last) = page.last() else {
                return Ok(());
            };
            after = Some(key(last));
            let done = page.len() < CONFORMANCE_PAGE_ROWS;
            visit(page)?;
            if done {
                return Ok(());
            }
        }
    }

    /// The `json` of the row of `table` (a conformance table keyed by `(package_id, url)`) with
    /// this key.
    fn row_json(
        &self,
        table: &str,
        package_id: i64,
        url: &str,
    ) -> Result<Option<String>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let row = guard
                .client
                .query_opt(
                    format!("SELECT json FROM {table} WHERE package_id = $1 AND url = $2").as_str(),
                    &[&package_id, &url],
                )
                .map_err(definite)?;
            Ok(row.map(|row| row.get(0)))
        })
    }

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

    fn load_structure_definition_json(
        &self,
        package_id: i64,
        url: &str,
    ) -> Result<Option<String>, PersistError> {
        self.row_json("structure_definitions", package_id, url)
    }

    fn load_terminology_json(
        &self,
        package_id: i64,
        url: &str,
    ) -> Result<Option<String>, PersistError> {
        self.row_json("terminology", package_id, url)
    }

    fn load_expansion_json(
        &self,
        package_id: i64,
        url: &str,
    ) -> Result<Option<String>, PersistError> {
        self.row_json("value_set_expansion", package_id, url)
    }

    fn shared(&self) -> Option<std::sync::Arc<dyn ConformanceStore>> {
        Some(std::sync::Arc::new(self.clone()))
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

    fn visit_structure_definitions(
        &self,
        visit: &mut dyn FnMut(Vec<StoredStructureDefinition>) -> Result<(), PersistError>,
    ) -> Result<(), PersistError> {
        self.visit_pages(
            "SELECT package_id, url, type_name, kind, is_abstract, json FROM structure_definitions",
            structure_definition_from,
            |row| (row.package_id, row.url.clone()),
            visit,
        )
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

    fn visit_terminology(
        &self,
        visit: &mut dyn FnMut(Vec<StoredTerminology>) -> Result<(), PersistError>,
    ) -> Result<(), PersistError> {
        self.visit_pages(
            "SELECT package_id, resource_type, url, json FROM terminology",
            terminology_from,
            |row| (row.package_id, row.url.clone()),
            visit,
        )
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

    fn visit_expansions(
        &self,
        visit: &mut dyn FnMut(Vec<StoredExpansion>) -> Result<(), PersistError>,
    ) -> Result<(), PersistError> {
        self.visit_pages(
            "SELECT package_id, url, json FROM value_set_expansion",
            expansion_from,
            |row| (row.package_id, row.url.clone()),
            visit,
        )
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

    fn insert_backfills(
        &self,
        package_id: i64,
        watermark_ts: i64,
        tablets: &[String],
    ) -> Result<(), PersistError> {
        if tablets.is_empty() {
            return Ok(());
        }
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            self.fence(&mut tx)?;
            // The jobs are numbered in the order of `tablets`, which is the order they run in.
            let ids: Vec<i64> = tx
                .query(
                    "INSERT INTO index_backfills (package_id, watermark_ts, tablet, cursor_id, done) \
                     SELECT $1, $2, tablet, NULL, 0 FROM UNNEST($3::text[]) AS tablets(tablet) \
                     RETURNING id",
                    &[&package_id, &watermark_ts, &tablets],
                )
                .map_err(definite)?
                .iter()
                .map(|row| row.get(0))
                .collect();
            tx.execute(
                "INSERT INTO backfill_parameter (backfill_id, url) \
                 SELECT job.id, parameter.url FROM index_backfills job \
                 JOIN search_parameters parameter ON parameter.package_id = job.package_id \
                  AND parameter.base = job.tablet AND parameter.status = 'pending' \
                 WHERE job.id = ANY($1)",
                &[&ids],
            )
            .map_err(definite)?;
            tx.commit().map_err(definite)
        })
    }

    fn complete_empty_backfills(&self, jobs: &[StoredBackfill]) -> Result<(), PersistError> {
        if jobs.is_empty() {
            return Ok(());
        }
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            self.fence(&mut tx)?;
            let ours = self.lease.epoch().unwrap_or(0) as i64;
            let ids: Vec<i64> = jobs.iter().map(|job| job.id).collect();
            // The same checks as claiming each job, on all of them under one lock.
            let rows = tx
                .query(
                    "SELECT id, done, claimed_epoch FROM index_backfills \
                     WHERE id = ANY($1) ORDER BY id FOR UPDATE",
                    &[&ids],
                )
                .map_err(definite)?;
            let mut open: Vec<i64> = Vec::new();
            for row in &rows {
                let id: i64 = row.get(0);
                let done: i32 = row.get(1);
                let held: Option<i64> = row.get(2);
                if done != 0 {
                    continue;
                }
                if let Some(held) = held.filter(|held| *held > ours) {
                    return Err(PersistError::Fenced(format!(
                        "backfill {id} is claimed by writer epoch {held}, this process holds {ours}"
                    )));
                }
                open.push(id);
            }
            if open.is_empty() {
                return Ok(());
            }
            // What `complete_backfill` does for each job: the parameters snapshotted onto it
            // become ready, or every pending one of its package and type when it has no snapshot.
            tx.execute(
                "UPDATE search_parameters parameter SET status = 'ready' \
                 FROM index_backfills job \
                 WHERE job.id = ANY($1) AND parameter.package_id = job.package_id \
                   AND parameter.base = job.tablet AND parameter.status = 'pending' \
                   AND (NOT EXISTS (SELECT 1 FROM backfill_parameter snapshot \
                                    WHERE snapshot.backfill_id = job.id) \
                        OR EXISTS (SELECT 1 FROM backfill_parameter snapshot \
                                   WHERE snapshot.backfill_id = job.id \
                                     AND snapshot.url = parameter.url))",
                &[&open],
            )
            .map_err(definite)?;
            tx.execute(
                "UPDATE index_backfills SET claimed_epoch = $1, done = 1 WHERE id = ANY($2)",
                &[&ours, &open],
            )
            .map_err(definite)?;
            tx.commit().map_err(definite)
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
    insert_structure_definitions(tx, package_id, &package.structure_definitions)?;
    let newly_pending = insert_search_parameters(tx, package_id, &package.search_parameters)?;
    insert_terminology(tx, package_id, package)?;
    replace_expansions(tx, package_id, package)?;
    Ok(InstallOutcome {
        package_id,
        inserted: true,
        newly_pending,
    })
}

/// The most JSON text one statement carries. A package is thousands of rows and tens of
/// megabytes, and every round trip to the server costs the same however small the row, so rows
/// go in a statement at a time, as many as fit under this, and no more of them are copied into
/// a statement than that.
const INSERT_BYTES: usize = 8 * 1024 * 1024;

/// Split `rows` into runs whose `size` adds up to about [`INSERT_BYTES`]. A row larger than that
/// is a run of its own.
fn runs<T>(rows: &[T], size: impl Fn(&T) -> usize) -> Vec<&[T]> {
    runs_within(rows, INSERT_BYTES, size)
}

fn runs_within<T>(rows: &[T], limit: usize, size: impl Fn(&T) -> usize) -> Vec<&[T]> {
    let mut runs = Vec::new();
    let (mut start, mut held) = (0, 0);
    for (at, row) in rows.iter().enumerate() {
        let bytes = size(row);
        if at > start && held + bytes > limit {
            runs.push(&rows[start..at]);
            (start, held) = (at, 0);
        }
        held += bytes;
    }
    if start < rows.len() {
        runs.push(&rows[start..]);
    }
    runs
}

fn insert_structure_definitions(
    tx: &mut Transaction<'_>,
    package_id: i64,
    definitions: &[NewStructureDefinition],
) -> Result<(), InstallError> {
    for run in runs(definitions, |row| row.json.len() + row.url.len()) {
        let urls: Vec<&str> = run.iter().map(|row| row.url.as_str()).collect();
        let types: Vec<&str> = run.iter().map(|row| row.type_name.as_str()).collect();
        let kinds: Vec<&str> = run.iter().map(|row| row.kind.as_str()).collect();
        let flags: Vec<i32> = run.iter().map(|row| i32::from(row.is_abstract)).collect();
        let documents: Vec<&str> = run.iter().map(|row| row.json.as_str()).collect();
        tx.execute(
            "INSERT INTO structure_definitions \
             (package_id, url, type_name, kind, is_abstract, json) \
             SELECT $1, url, type_name, kind, is_abstract, json \
             FROM UNNEST($2::text[], $3::text[], $4::text[], $5::int4[], $6::text[]) \
                  AS rows(url, type_name, kind, is_abstract, json)",
            &[&package_id, &urls, &types, &kinds, &flags, &documents],
        )
        .map_err(definite)?;
    }
    Ok(())
}

/// Store the search parameters of a package, and say which of them wait for a backfill. What the
/// rows already stored decide about each one (see [`plan_package_parameters`]) is read in one
/// query for all of them, not one per parameter.
fn insert_search_parameters(
    tx: &mut Transaction<'_>,
    package_id: i64,
    parameters: &[NewSearchParameter],
) -> Result<Vec<refhir_persistence::PendingParam>, InstallError> {
    if parameters.is_empty() {
        return Ok(Vec::new());
    }
    let bases: Vec<&str> = parameters.iter().map(|row| row.base.as_str()).collect();
    let codes: Vec<&str> = parameters.iter().map(|row| row.code.as_str()).collect();
    let stored = tx
        .query(
            "SELECT base, code, package_id, url, COALESCE(expression, ''), status, json \
             FROM search_parameters \
             WHERE (base, code) IN (SELECT * FROM UNNEST($1::text[], $2::text[]))",
            &[&bases, &codes],
        )
        .map_err(definite)?
        .iter()
        .map(|row| StoredParameterRow {
            base: row.get(0),
            code: row.get(1),
            row: InstalledParam {
                package_id: row.get(2),
                url: row.get(3),
                expression: row.get(4),
                status: row.get(5),
                json: row.get(6),
            },
        })
        .collect();
    let plan = plan_package_parameters(package_id, parameters, stored)?;
    for removal in &plan.removals {
        tx.execute(
            "DELETE FROM search_parameters \
             WHERE package_id = $1 AND url = $2 AND base = $3 AND code = $4",
            &[
                &removal.package_id,
                &removal.url,
                &removal.base,
                &removal.code,
            ],
        )
        .map_err(definite)?;
    }
    for run in runs(&plan.inserts, |(at, _)| {
        parameters[*at].json.len() + parameters[*at].url.len()
    }) {
        let rows: Vec<(&NewSearchParameter, ParamStatus)> = run
            .iter()
            .map(|(at, status)| (&parameters[*at], *status))
            .collect();
        let urls: Vec<&str> = rows.iter().map(|(row, _)| row.url.as_str()).collect();
        let bases: Vec<&str> = rows.iter().map(|(row, _)| row.base.as_str()).collect();
        let codes: Vec<&str> = rows.iter().map(|(row, _)| row.code.as_str()).collect();
        let types: Vec<&str> = rows
            .iter()
            .map(|(row, _)| row.param_type.as_str())
            .collect();
        let expressions: Vec<Option<&str>> = rows
            .iter()
            .map(|(row, _)| row.expression.as_deref())
            .collect();
        let statuses: Vec<&str> = rows.iter().map(|(_, status)| status.as_str()).collect();
        let documents: Vec<&str> = rows.iter().map(|(row, _)| row.json.as_str()).collect();
        tx.execute(
            "INSERT INTO search_parameters \
             (package_id, url, base, code, param_type, expression, status, json) \
             SELECT $1, url, base, code, param_type, expression, status, json \
             FROM UNNEST($2::text[], $3::text[], $4::text[], $5::text[], $6::text[], \
                         $7::text[], $8::text[]) \
                  AS rows(url, base, code, param_type, expression, status, json)",
            &[
                &package_id,
                &urls,
                &bases,
                &codes,
                &types,
                &expressions,
                &statuses,
                &documents,
            ],
        )
        .map_err(definite)?;
    }
    Ok(plan.newly_pending)
}

fn insert_terminology(
    tx: &mut Transaction<'_>,
    package_id: i64,
    package: &NewPackage,
) -> Result<(), InstallError> {
    for run in runs(&package.terminology, |row| row.json.len() + row.url.len()) {
        let types: Vec<&str> = run.iter().map(|row| row.resource_type.as_str()).collect();
        let urls: Vec<&str> = run.iter().map(|row| row.url.as_str()).collect();
        let documents: Vec<&str> = run.iter().map(|row| row.json.as_str()).collect();
        // The first of two rows for one URL stays, as it did when they went in one at a time.
        tx.execute(
            "INSERT INTO terminology (package_id, resource_type, url, json) \
             SELECT $1, resource_type, url, json \
             FROM UNNEST($2::text[], $3::text[], $4::text[]) AS rows(resource_type, url, json) \
             ON CONFLICT (package_id, url) DO NOTHING",
            &[&package_id, &types, &urls, &documents],
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
    for run in runs(&package.expansions, |row| row.json.len() + row.url.len()) {
        let urls: Vec<&str> = run.iter().map(|row| row.url.as_str()).collect();
        let documents: Vec<&str> = run.iter().map(|row| row.json.as_str()).collect();
        tx.execute(
            "INSERT INTO value_set_expansion (package_id, url, json) \
             SELECT $1, url, json FROM UNNEST($2::text[], $3::text[]) AS rows(url, json)",
            &[&package_id, &urls, &documents],
        )
        .map_err(definite)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::runs_within;

    #[test]
    fn rows_go_in_runs_that_stay_under_the_limit_except_for_one_row_that_is_larger() {
        let sizes = [4usize, 4, 4, 30, 1, 1, 9, 2];
        let runs = runs_within(&sizes, 10, |size| *size);
        let shape: Vec<&[usize]> = runs.to_vec();
        assert_eq!(
            shape,
            [
                &[4, 4][..],
                &[4][..],
                &[30][..],
                &[1, 1][..],
                &[9][..],
                &[2][..]
            ]
        );
        // Nothing is dropped or reordered, whatever the limit.
        for limit in [1, 3, 10, 1000] {
            let joined: Vec<usize> = runs_within(&sizes, limit, |size| *size).concat();
            assert_eq!(joined, sizes);
        }
        assert!(runs_within::<usize>(&[], 10, |size| *size).is_empty());
    }
}
