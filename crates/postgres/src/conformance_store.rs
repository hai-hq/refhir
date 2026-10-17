use postgres::Transaction;
use refhir_persistence::{
    plan_package_parameters, ConformanceStore, ExpansionChange, InstallError, InstallOutcome,
    InstalledParam, NewExpansion, NewPackage, NewSearchParameter, NewStructureDefinition,
    NewTerminology, PackageRows, ParamStatus, PendingParam, PersistError, PersistenceGlobalKey,
    PoolGuard, StoredBackfill, StoredExpansion, StoredPackage, StoredParameterRow,
    StoredSearchParameter, StoredStructureDefinition, StoredTerminology, CONFORMANCE_PAGE_ROWS,
};

use crate::scope::Scope;
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
        let first = format!(
            "{select}{} ORDER BY package_id, url LIMIT {CONFORMANCE_PAGE_ROWS}",
            self.scope.where_only()
        );
        let next = format!(
            "{select} WHERE {}(package_id, url) > ($1, $2) \
             ORDER BY package_id, url LIMIT {CONFORMANCE_PAGE_ROWS}",
            self.scope.and()
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
                    format!(
                        "SELECT json FROM {table} WHERE {}package_id = $1 AND url = $2",
                        self.scope.and()
                    )
                    .as_str(),
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
                &format!(
                    "SELECT json_value FROM persistence_globals WHERE {}key = $1",
                    self.scope.and()
                ),
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
                &format!(
                    "INSERT INTO persistence_globals ({}key, json_value) VALUES ({}$1, '1')
                     ON CONFLICT {} DO UPDATE
                     SET json_value = (persistence_globals.json_value::bigint + 1)::text
                     RETURNING json_value",
                    self.scope.cols(),
                    self.scope.vals(),
                    self.scope.conflict("key")
                ),
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
            Some(epoch) => crate::lease::check_epoch(tx, self.scope, epoch),
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

    fn terminology_version(&self) -> Result<u64, PersistError> {
        crate::off_runtime(|| self.read_counter(PersistenceGlobalKey::TerminologyVersion))
    }

    fn bump_terminology_version(&self) -> Result<u64, PersistError> {
        crate::off_runtime(|| self.bump_counter(PersistenceGlobalKey::TerminologyVersion))
    }

    fn load_packages(&self) -> Result<Vec<StoredPackage>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let rows = guard
                .client
                .query(
                    &format!(
                        "SELECT id, name, version, fhir_version FROM packages{} ORDER BY id",
                        self.scope.where_only()
                    ),
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
                    &format!(
                        "SELECT package_id, url, type_name, kind, is_abstract, json \
                         FROM structure_definitions{} ORDER BY package_id, url",
                        self.scope.where_only()
                    ),
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
                    &format!(
                        "SELECT package_id, url, base, code, param_type, expression, status, json \
                         FROM search_parameters{} ORDER BY package_id, base, code",
                        self.scope.where_only()
                    ),
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
                    &format!(
                        "SELECT package_id, resource_type, url, json FROM terminology{} \
                     ORDER BY package_id, url",
                        self.scope.where_only()
                    ),
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
                    &format!(
                        "SELECT package_id, resource_type, url, json FROM terminology \
                         WHERE {}(url = ANY($1) OR split_part(url, '|', 1) = ANY($1)) \
                         ORDER BY package_id, url",
                        self.scope.and()
                    ),
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
                        &format!(
                            "INSERT INTO value_set_expansion ({}package_id, url, json) \
                         VALUES ({}$1, $2, $3) \
                         ON CONFLICT {} DO UPDATE SET json = excluded.json",
                            self.scope.cols(),
                            self.scope.vals(),
                            self.scope.conflict("package_id, url")
                        ),
                        &[&change.package_id, &change.url, json],
                    ),
                    None => tx.execute(
                        &format!(
                            "DELETE FROM value_set_expansion WHERE {}package_id = $1 AND url = $2",
                            self.scope.and()
                        ),
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
                    &format!(
                        "SELECT package_id, resource_type, url, json FROM terminology \
                         WHERE {}package_id = $1 ORDER BY url",
                        self.scope.and()
                    ),
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
                    &format!(
                        "SELECT package_id, url, base, code, param_type, expression, status, json \
                         FROM search_parameters WHERE {}package_id = $1 ORDER BY base, code",
                        self.scope.and()
                    ),
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
                    &format!(
                        "SELECT package_id, url, json FROM value_set_expansion{} \
                     ORDER BY package_id, url",
                        self.scope.where_only()
                    ),
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
            let (outcome, heaviest) = install_in_tx(&mut tx, self.scope, package)?;
            tx.commit().map_err(definite)?;
            retire_if_swollen(&mut guard, heaviest);
            Ok(outcome)
        })
    }

    fn install_package_in_pieces(
        &self,
        name: &str,
        version: &str,
        fhir_version: &str,
        feed: &mut (dyn FnMut(&mut dyn PackageRows) -> Result<(), InstallError> + Send),
    ) -> Result<InstallOutcome, InstallError> {
        crate::off_runtime(|| {
            let _gate = self.gate();
            let mut guard = self.inner.lock()?;
            let mut tx = guard.client.transaction().map_err(definite)?;
            self.fence(&mut tx)?;
            let mut installer = Installer::begin(&mut tx, self.scope, name, version, fhir_version)?;
            feed(&mut installer)?;
            let (outcome, heaviest) = installer.finish()?;
            tx.commit().map_err(definite)?;
            retire_if_swollen(&mut guard, heaviest);
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
                &format!(
                    "UPDATE search_parameters SET status = $1 \
                     WHERE {}package_id = $2 AND url = $3 AND base = $4 AND status = 'unsupported'",
                    self.scope.and()
                ),
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
                    &format!(
                        "SELECT id FROM packages WHERE {}name = 'refhir.local' AND version = '0'",
                        self.scope.and()
                    ),
                    &[],
                )
                .map_err(definite)?;
            if let Some(row) = existing {
                return Ok(row.get(0));
            }
            let current = tx
                .query_opt(
                    &format!(
                        "SELECT fhir_version FROM packages{} LIMIT 1",
                        self.scope.where_only()
                    ),
                    &[],
                )
                .map_err(definite)?;
            let version = current
                .map(|row| row.get::<_, String>(0))
                .unwrap_or_else(|| fhir_version.to_string());
            let inserted = tx
                .query_one(
                    &format!(
                        "INSERT INTO packages ({}name, version, fhir_version) \
                         VALUES ({}'refhir.local', '0', $1) RETURNING id",
                        self.scope.cols(),
                        self.scope.vals()
                    ),
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
                &format!(
                    "SELECT code, param_type, COALESCE(expression, ''), status FROM search_parameters \
                     WHERE {}package_id = $1 AND url = $2 AND base = $3",
                    self.scope.and()
                ),
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
                        &format!(
                            "UPDATE search_parameters SET json = $1 \
                             WHERE {}package_id = $2 AND url = $3 AND base = $4",
                            self.scope.and()
                        ),
                        &[&param.json, &package_id, &param.url, &param.base],
                    )
                    .map_err(definite)?;
                    tx.commit().map_err(definite)?;
                    return Ok(false);
                }
            }
            let status = param.status.as_str();
            tx.execute(
                &format!(
                    "INSERT INTO search_parameters \
                     ({}package_id, url, base, code, param_type, expression, status, json) \
                     VALUES ({}$1, $2, $3, $4, $5, $6, $7, $8) \
                     ON CONFLICT {} DO UPDATE SET \
                     code = EXCLUDED.code, param_type = EXCLUDED.param_type, \
                     expression = EXCLUDED.expression, status = EXCLUDED.status, \
                     json = EXCLUDED.json",
                    self.scope.cols(),
                    self.scope.vals(),
                    self.scope.conflict("package_id, url, base")
                ),
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
                &format!(
                    "DELETE FROM search_parameters WHERE {}package_id = $1 AND url = $2",
                    self.scope.and()
                ),
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
                &format!(
                    "INSERT INTO terminology ({}package_id, resource_type, url, json) \
                     VALUES ({}$1, $2, $3, $4) \
                     ON CONFLICT {} DO UPDATE SET \
                     resource_type = EXCLUDED.resource_type, json = EXCLUDED.json",
                    self.scope.cols(),
                    self.scope.vals(),
                    self.scope.conflict("package_id, url")
                ),
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
                &format!(
                    "DELETE FROM terminology WHERE {}package_id = $1 AND url = $2",
                    self.scope.and()
                ),
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
                &format!(
                    "DELETE FROM value_set_expansion WHERE {}package_id = $1",
                    self.scope.and()
                ),
                &[&package_id],
            )
            .map_err(definite)?;
            let insert = format!(
                "INSERT INTO value_set_expansion ({}package_id, url, json) VALUES ({}$1, $2, $3)",
                self.scope.cols(),
                self.scope.vals()
            );
            for expansion in expansions {
                tx.execute(&insert, &[&package_id, &expansion.url, &expansion.json])
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
                    &format!(
                        "SELECT url FROM backfill_parameter WHERE {}backfill_id = $1",
                        self.scope.and()
                    ),
                    &[&backfill_id],
                )
                .map_err(definite)?
                .iter()
                .map(|row| row.get(0))
                .collect();
            if urls.is_empty() {
                tx.execute(
                    &format!(
                        "UPDATE search_parameters SET status = 'ready' \
                         WHERE {}package_id = $1 AND base = $2 AND status = 'pending'",
                        self.scope.and()
                    ),
                    &[&package_id, &base],
                )
                .map_err(definite)?;
            }
            for url in urls {
                tx.execute(
                    &format!(
                        "UPDATE search_parameters SET status = 'ready' \
                         WHERE {}package_id = $1 AND base = $2 AND url = $3 AND status = 'pending'",
                        self.scope.and()
                    ),
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
                &format!(
                    "UPDATE search_parameters SET status = 'ready' \
                     WHERE {}package_id = $1 AND base = $2 AND status = 'pending'",
                    self.scope.and()
                ),
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
                    &format!(
                        "INSERT INTO index_backfills \
                     ({}package_id, watermark_ts, tablet, cursor_id, done) \
                     VALUES ({}$1, $2, $3, NULL, 0) RETURNING id",
                        self.scope.cols(),
                        self.scope.vals()
                    ),
                    &[&package_id, &watermark_ts, &tablet],
                )
                .map_err(definite)?;
            let backfill_id: i64 = row.get(0);
            if urls.is_empty() {
                tx.execute(
                    &format!(
                        "INSERT INTO backfill_parameter ({}backfill_id, url) \
                         SELECT {}$1, url FROM search_parameters \
                         WHERE {}package_id = $2 AND base = $3 AND status = 'pending'",
                        self.scope.cols(),
                        self.scope.vals(),
                        self.scope.and()
                    ),
                    &[&backfill_id, &package_id, &tablet],
                )
                .map_err(definite)?;
            } else {
                let insert = format!(
                    "INSERT INTO backfill_parameter ({}backfill_id, url) VALUES ({}$1, $2) \
                     ON CONFLICT {} DO NOTHING",
                    self.scope.cols(),
                    self.scope.vals(),
                    self.scope.conflict("backfill_id, url")
                );
                for url in urls {
                    tx.execute(&insert, &[&backfill_id, url])
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
                    &format!(
                        "INSERT INTO index_backfills \
                         ({}package_id, watermark_ts, tablet, cursor_id, done) \
                         SELECT {}$1, $2, tablet, NULL, 0 FROM UNNEST($3::text[]) AS tablets(tablet) \
                         RETURNING id",
                        self.scope.cols(),
                        self.scope.vals()
                    ),
                    &[&package_id, &watermark_ts, &tablets],
                )
                .map_err(definite)?
                .iter()
                .map(|row| row.get(0))
                .collect();
            tx.execute(
                &format!(
                    "INSERT INTO backfill_parameter ({}backfill_id, url) \
                     SELECT {}job.id, parameter.url FROM index_backfills job \
                     JOIN search_parameters parameter \
                       ON {}parameter.package_id = job.package_id \
                      AND parameter.base = job.tablet AND parameter.status = 'pending' \
                     WHERE {}job.id = ANY($1)",
                    self.scope.cols(),
                    self.scope.vals(),
                    self.scope.and_in("parameter"),
                    self.scope.and_in("job")
                ),
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
                    &format!(
                        "SELECT id, done, claimed_epoch FROM index_backfills \
                         WHERE {}id = ANY($1) ORDER BY id FOR UPDATE",
                        self.scope.and()
                    ),
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
                &format!(
                    "UPDATE search_parameters parameter SET status = 'ready' \
                     FROM index_backfills job \
                     WHERE {job}job.id = ANY($1) AND {parameter}parameter.package_id = job.package_id \
                       AND parameter.base = job.tablet AND parameter.status = 'pending' \
                       AND (NOT EXISTS (SELECT 1 FROM backfill_parameter snapshot \
                                        WHERE {snapshot}snapshot.backfill_id = job.id) \
                            OR EXISTS (SELECT 1 FROM backfill_parameter snapshot \
                                       WHERE {snapshot}snapshot.backfill_id = job.id \
                                         AND snapshot.url = parameter.url))",
                    job = self.scope.and_in("job"),
                    parameter = self.scope.and_in("parameter"),
                    snapshot = self.scope.and_in("snapshot"),
                ),
                &[&open],
            )
            .map_err(definite)?;
            tx.execute(
                &format!(
                    "UPDATE index_backfills SET claimed_epoch = $1, done = 1 \
                     WHERE {}id = ANY($2)",
                    self.scope.and()
                ),
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
                &format!(
                    "UPDATE index_backfills SET cursor_id = $1, done = $2 WHERE {}id = $3",
                    self.scope.and()
                ),
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
                    &format!(
                        "SELECT id, package_id, watermark_ts, tablet, cursor_id, done, \
                         claimed_epoch FROM index_backfills{} ORDER BY id",
                        self.scope.where_only()
                    ),
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
                    &format!(
                        "SELECT done, claimed_epoch FROM index_backfills \
                         WHERE {}id = $1 FOR UPDATE",
                        self.scope.and()
                    ),
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
                &format!(
                    "UPDATE index_backfills SET claimed_epoch = $1 WHERE {}id = $2",
                    self.scope.and()
                ),
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
    scope: Scope,
    package: &NewPackage,
) -> Result<(InstallOutcome, usize), InstallError> {
    let mut installer = Installer::begin(
        tx,
        scope,
        &package.name,
        &package.version,
        &package.fhir_version,
    )?;
    installer.structure_definitions(&package.structure_definitions)?;
    installer.search_parameters(&package.search_parameters)?;
    installer.terminology(&package.terminology)?;
    installer.expansions(&package.expansions)?;
    installer.finish()
}

/// A package being stored inside a transaction, a few rows at a time.
///
/// A package the store already has (same name and version) takes only terminology, which is added
/// to what it has, and expansions, which replace what it has. A package it does not have gets its
/// definitions and search parameters too, and the search parameters are checked against the ones
/// installed. Each call stores the rows it is given in statements of its own, so what is held at
/// once is the rows of one call, however large the package.
struct Installer<'a, 'c> {
    tx: &'a mut Transaction<'c>,
    scope: Scope,
    package_id: i64,
    /// Whether the package was there before this install.
    existing: bool,
    newly_pending: Vec<PendingParam>,
    /// Whether the package's earlier expansions have been cleared for this install's.
    expansions_cleared: bool,
    /// The most JSON text any one call was given.
    heaviest: usize,
}

impl<'a, 'c> Installer<'a, 'c> {
    fn begin(
        tx: &'a mut Transaction<'c>,
        scope: Scope,
        name: &str,
        version: &str,
        fhir_version: &str,
    ) -> Result<Self, InstallError> {
        let existing = tx
            .query_opt(
                &format!(
                    "SELECT id FROM packages WHERE {}name = $1 AND version = $2",
                    scope.and()
                ),
                &[&name, &version],
            )
            .map_err(definite)?;
        if let Some(row) = existing {
            let package_id: i64 = row.get(0);
            return Ok(Self {
                tx,
                scope,
                package_id,
                existing: true,
                newly_pending: Vec::new(),
                expansions_cleared: false,
                heaviest: 0,
            });
        }
        let current = tx
            .query_opt(
                &format!(
                    "SELECT fhir_version FROM packages{} LIMIT 1",
                    scope.where_only()
                ),
                &[],
            )
            .map_err(definite)?;
        if let Some(row) = current {
            let existing: String = row.get(0);
            if existing != fhir_version {
                return Err(InstallError::FhirVersion {
                    existing,
                    incoming: fhir_version.to_string(),
                });
            }
        }
        let inserted = tx
            .query_one(
                &format!(
                    "INSERT INTO packages ({}name, version, fhir_version) VALUES ({}$1, $2, $3) \
                     RETURNING id",
                    scope.cols(),
                    scope.vals()
                ),
                &[&name, &version, &fhir_version],
            )
            .map_err(definite)?;
        let package_id: i64 = inserted.get(0);
        Ok(Self {
            tx,
            scope,
            package_id,
            existing: false,
            newly_pending: Vec::new(),
            expansions_cleared: false,
            heaviest: 0,
        })
    }

    fn note(&mut self, bytes: usize) {
        self.heaviest = self.heaviest.max(bytes);
    }

    fn clear_expansions(&mut self) -> Result<(), InstallError> {
        if !self.expansions_cleared {
            self.tx
                .execute(
                    &format!(
                        "DELETE FROM value_set_expansion WHERE {}package_id = $1",
                        self.scope.and()
                    ),
                    &[&self.package_id],
                )
                .map_err(definite)?;
            self.expansions_cleared = true;
        }
        Ok(())
    }

    /// The outcome, and the most JSON text any one call was given.
    fn finish(mut self) -> Result<(InstallOutcome, usize), InstallError> {
        // A package that is installed again and brings no expansions has none afterwards.
        self.clear_expansions()?;
        Ok((
            InstallOutcome {
                package_id: self.package_id,
                inserted: !self.existing,
                newly_pending: self.newly_pending,
            },
            self.heaviest,
        ))
    }
}

impl PackageRows for Installer<'_, '_> {
    fn structure_definitions(
        &mut self,
        rows: &[NewStructureDefinition],
    ) -> Result<(), InstallError> {
        if self.existing {
            return Ok(());
        }
        self.note(rows.iter().map(|row| row.json.len()).sum());
        insert_structure_definitions(self.tx, self.scope, self.package_id, rows)
    }

    fn search_parameters(&mut self, rows: &[NewSearchParameter]) -> Result<(), InstallError> {
        if self.existing {
            return Ok(());
        }
        self.note(rows.iter().map(|row| row.json.len()).sum());
        let pending = insert_search_parameters(self.tx, self.scope, self.package_id, rows)?;
        self.newly_pending.extend(pending);
        Ok(())
    }

    fn terminology(&mut self, rows: &[NewTerminology]) -> Result<(), InstallError> {
        self.note(rows.iter().map(|row| row.json.len()).sum());
        insert_terminology(self.tx, self.scope, self.package_id, rows)
    }

    fn expansions(&mut self, rows: &[NewExpansion]) -> Result<(), InstallError> {
        self.clear_expansions()?;
        self.note(rows.iter().map(|row| row.json.len()).sum());
        insert_expansions(self.tx, self.scope, self.package_id, rows)
    }
}

/// How much JSON text one call of an install may carry before the connection it went over is
/// closed once the install is stored. A connection copies each statement into a buffer that it
/// keeps at the size of the largest it has sent, for as long as it lives, so a connection that
/// has stored a package of 100 MB in pieces of several megabytes keeps about 16 MB, which the
/// server would then hold for as long as it ran. A small install is not worth a new connection.
const RETIRE_AFTER_BYTES: usize = 1024 * 1024;

/// Close the connection of an install that was large (see [`RETIRE_AFTER_BYTES`]); the pool
/// opens another when it is next asked for one.
fn retire_if_swollen(connection: &mut PoolGuard<'_, crate::Inner>, heaviest: usize) {
    if heaviest > RETIRE_AFTER_BYTES {
        connection.discard();
    }
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
    scope: Scope,
    package_id: i64,
    definitions: &[NewStructureDefinition],
) -> Result<(), InstallError> {
    let insert = format!(
        "INSERT INTO structure_definitions \
         ({}package_id, url, type_name, kind, is_abstract, json) \
         SELECT {}$1, url, type_name, kind, is_abstract, json \
         FROM UNNEST($2::text[], $3::text[], $4::text[], $5::int4[], $6::text[]) \
              AS rows(url, type_name, kind, is_abstract, json)",
        scope.cols(),
        scope.vals()
    );
    for run in runs(definitions, |row| row.json.len() + row.url.len()) {
        let urls: Vec<&str> = run.iter().map(|row| row.url.as_str()).collect();
        let types: Vec<&str> = run.iter().map(|row| row.type_name.as_str()).collect();
        let kinds: Vec<&str> = run.iter().map(|row| row.kind.as_str()).collect();
        let flags: Vec<i32> = run.iter().map(|row| i32::from(row.is_abstract)).collect();
        let documents: Vec<&str> = run.iter().map(|row| row.json.as_str()).collect();
        tx.execute(
            &insert,
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
    scope: Scope,
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
            &format!(
                "SELECT base, code, package_id, url, COALESCE(expression, ''), status, json \
                 FROM search_parameters \
                 WHERE {}(base, code) IN (SELECT * FROM UNNEST($1::text[], $2::text[]))",
                scope.and()
            ),
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
            &format!(
                "DELETE FROM search_parameters \
                 WHERE {}package_id = $1 AND url = $2 AND base = $3 AND code = $4",
                scope.and()
            ),
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
            &format!(
                "INSERT INTO search_parameters \
                 ({}package_id, url, base, code, param_type, expression, status, json) \
                 SELECT {}$1, url, base, code, param_type, expression, status, json \
                 FROM UNNEST($2::text[], $3::text[], $4::text[], $5::text[], $6::text[], \
                             $7::text[], $8::text[]) \
                      AS rows(url, base, code, param_type, expression, status, json)",
                scope.cols(),
                scope.vals()
            ),
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
    scope: Scope,
    package_id: i64,
    rows: &[NewTerminology],
) -> Result<(), InstallError> {
    for run in runs(rows, |row| row.json.len() + row.url.len()) {
        let types: Vec<&str> = run.iter().map(|row| row.resource_type.as_str()).collect();
        let urls: Vec<&str> = run.iter().map(|row| row.url.as_str()).collect();
        let documents: Vec<&str> = run.iter().map(|row| row.json.as_str()).collect();
        // The first of two rows for one URL stays, as it did when they went in one at a time.
        tx.execute(
            &format!(
                "INSERT INTO terminology ({}package_id, resource_type, url, json) \
                 SELECT {}$1, resource_type, url, json \
                 FROM UNNEST($2::text[], $3::text[], $4::text[]) AS rows(resource_type, url, json) \
                 ON CONFLICT {} DO NOTHING",
                scope.cols(),
                scope.vals(),
                scope.conflict("package_id, url")
            ),
            &[&package_id, &types, &urls, &documents],
        )
        .map_err(definite)?;
    }
    Ok(())
}

fn insert_expansions(
    tx: &mut Transaction<'_>,
    scope: Scope,
    package_id: i64,
    rows: &[NewExpansion],
) -> Result<(), InstallError> {
    for run in runs(rows, |row| row.json.len() + row.url.len()) {
        let urls: Vec<&str> = run.iter().map(|row| row.url.as_str()).collect();
        let documents: Vec<&str> = run.iter().map(|row| row.json.as_str()).collect();
        tx.execute(
            &format!(
                "INSERT INTO value_set_expansion ({}package_id, url, json) \
                 SELECT {}$1, url, json FROM UNNEST($2::text[], $3::text[]) AS rows(url, json)",
                scope.cols(),
                scope.vals()
            ),
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
