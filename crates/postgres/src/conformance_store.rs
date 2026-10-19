use postgres::Transaction;
use refhir_persistence::{
    compare_versions, plan_package_parameters, ConformanceStore, ExpansionChange, InstallError,
    InstallOutcome, InstalledParam, NewExpansion, NewPackage, NewSearchParameter,
    NewStructureDefinition, NewTerminology, PackageRows, ParamStatus, PendingParam, PersistError,
    PersistenceGlobalKey, PoolGuard, RetiredParam, StoredBackfill, StoredExpansion, StoredPackage,
    StoredParameterRow, StoredSearchParameter, StoredStructureDefinition, StoredTerminology,
    CONFORMANCE_PAGE_ROWS,
};

use crate::scope::Scope;
use crate::{definite, PostgresPersistence};

/// The first half of the two-part advisory lock that serializes building one shared package. A
/// different space from the writer lease's, the audit chain's and the migrations'.
const SHARED_PACKAGE_LOCK: i32 = 0x7265_6663;

/// What asking for the shared copy of a package came to.
enum Shared {
    /// The tenant had the package already.
    Own,
    /// There is no shared copy of it yet.
    Missing,
    /// There is one, made from other content than the tenant asks for.
    Different,
    /// The tenant has it now.
    Activated(InstallOutcome),
}

/// A shared package being built inside a transaction. Definitions and terminology are rows under the
/// shared catalog's instance; search parameters and expansions go to the tables tenants copy from.
struct SharedRows<'a, 'c> {
    tx: &'a mut Transaction<'c>,
    package_id: i64,
    /// How many search parameters came before: the next one's place in the package's order.
    next_ord: i64,
    heaviest: usize,
}

impl SharedRows<'_, '_> {
    fn note(&mut self, bytes: usize) {
        self.heaviest = self.heaviest.max(bytes);
    }
}

impl PackageRows for SharedRows<'_, '_> {
    fn structure_definitions(
        &mut self,
        rows: &[NewStructureDefinition],
    ) -> Result<(), InstallError> {
        self.note(rows.iter().map(|row| row.json.len()).sum());
        insert_structure_definitions(self.tx, Scope::CATALOG, self.package_id, rows)
    }

    fn search_parameters(&mut self, rows: &[NewSearchParameter]) -> Result<(), InstallError> {
        self.note(rows.iter().map(|row| row.json.len()).sum());
        for run in runs(rows, |row| row.json.len() + row.url.len()) {
            let stored: Vec<(&NewSearchParameter, ParamStatus)> =
                run.iter().map(|row| (row, row.status)).collect();
            insert_parameters(
                self.tx,
                ParameterTable::Catalog {
                    first_ord: self.next_ord,
                },
                self.package_id,
                &stored,
            )?;
            self.next_ord += run.len() as i64;
        }
        Ok(())
    }

    fn terminology(&mut self, rows: &[NewTerminology]) -> Result<(), InstallError> {
        self.note(rows.iter().map(|row| row.json.len()).sum());
        insert_terminology(self.tx, Scope::CATALOG, self.package_id, rows)
    }

    fn expansions(&mut self, rows: &[NewExpansion]) -> Result<(), InstallError> {
        self.note(rows.iter().map(|row| row.json.len()).sum());
        // The catalog's copy has no tenant column: nothing leads its columns or values.
        insert_expansions(self.tx, "base_expansions", "", "", self.package_id, rows)
    }
}

/// How many definitions, terminology resources, search parameters and expansions the shared copy of
/// a package holds, as `d,t,p,e`: what is recorded when it is built and checked when a tenant is
/// given it.
fn shared_row_counts(tx: &mut Transaction<'_>, package_id: i64) -> Result<String, PersistError> {
    let row = tx
        .query_one(
            "SELECT (SELECT count(*) FROM structure_definitions \
                     WHERE instance_id = 0 AND package_id = $1)::text || ',' || \
                    (SELECT count(*) FROM terminology \
                     WHERE instance_id = 0 AND package_id = $1)::text || ',' || \
                    (SELECT count(*) FROM base_search_parameters WHERE package_id = $1)::text \
                    || ',' || (SELECT count(*) FROM base_expansions WHERE package_id = $1)::text",
            &[&package_id],
        )
        .map_err(definite)?;
    Ok(row.get(0))
}

/// Gives a tenant a package: it is the tenant's, in the order it got it. `owner` is whose rows hold
/// the package's definitions and terminology.
fn link_package(
    tx: &mut Transaction<'_>,
    tenant: i32,
    package_id: i64,
    owner: i32,
) -> Result<(), PersistError> {
    tx.execute(
        "INSERT INTO tenant_packages (instance_id, package_id, owner) VALUES ($1, $2, $3)",
        &[&tenant, &package_id, &owner],
    )
    .map_err(definite)?;
    Ok(())
}

// What a tenant reads differs from what a classic database does: its packages are the ones in its
// own list, in the order it got them, and the rows of a package that every tenant shares are held
// under the shared catalog. The statements are functions so a test can ask Postgres about them.

fn sql_load_packages(scope: Scope) -> String {
    match scope {
        Scope::Classic => "SELECT id, name, version, fhir_version FROM packages ORDER BY id".into(),
        Scope::Tenant(id) => format!(
            "SELECT p.id, p.name, p.version, p.fhir_version FROM tenant_packages t \
             JOIN packages p ON p.id = t.package_id WHERE t.instance_id = {id} ORDER BY t.seq"
        ),
    }
}

/// The release the first of the tenant's packages is for, which every other must match.
fn sql_first_fhir_version(scope: Scope) -> String {
    match scope {
        Scope::Classic => "SELECT fhir_version FROM packages LIMIT 1".into(),
        Scope::Tenant(id) => format!(
            "SELECT p.fhir_version FROM tenant_packages t JOIN packages p ON p.id = t.package_id \
             WHERE t.instance_id = {id} ORDER BY t.seq LIMIT 1"
        ),
    }
}

/// The package `$1` of version `$2` that the tenant has: its id.
fn sql_find_package(scope: Scope) -> String {
    match scope {
        Scope::Classic => "SELECT id FROM packages WHERE name = $1 AND version = $2".into(),
        Scope::Tenant(id) => format!(
            "SELECT p.id, t.owner FROM tenant_packages t JOIN packages p ON p.id = t.package_id \
             WHERE t.instance_id = {id} AND p.name = $1 AND p.version = $2"
        ),
    }
}

/// The id and version of each package named `$1` that the tenant has.
fn sql_package_versions(scope: Scope) -> String {
    match scope {
        Scope::Classic => "SELECT id, version FROM packages WHERE name = $1".into(),
        Scope::Tenant(id) => format!(
            "SELECT p.id, p.version FROM tenant_packages t JOIN packages p ON p.id = t.package_id \
             WHERE t.instance_id = {id} AND p.name = $1"
        ),
    }
}

fn sql_load_structure_definitions(scope: Scope) -> String {
    match scope {
        Scope::Classic => "SELECT package_id, url, type_name, kind, is_abstract, json \
                           FROM structure_definitions ORDER BY package_id, url"
            .into(),
        Scope::Tenant(id) => format!(
            "SELECT d.package_id, d.url, d.type_name, d.kind, d.is_abstract, d.json \
             FROM tenant_packages t JOIN structure_definitions d \
               ON d.instance_id = t.owner AND d.package_id = t.package_id \
             WHERE t.instance_id = {id} ORDER BY t.seq, d.url"
        ),
    }
}

fn sql_load_search_parameters(scope: Scope) -> String {
    match scope {
        Scope::Classic => "SELECT package_id, url, base, code, param_type, expression, status, json \
                           FROM search_parameters ORDER BY package_id, base, code"
            .into(),
        Scope::Tenant(id) => format!(
            "SELECT s.package_id, s.url, s.base, s.code, s.param_type, s.expression, s.status, s.json \
             FROM tenant_packages t JOIN search_parameters s \
               ON s.instance_id = t.instance_id AND s.package_id = t.package_id \
             WHERE t.instance_id = {id} ORDER BY t.seq, s.base, s.code"
        ),
    }
}

fn sql_load_terminology(scope: Scope) -> String {
    match scope {
        Scope::Classic => "SELECT package_id, resource_type, url, json FROM terminology \
                           ORDER BY package_id, url"
            .into(),
        Scope::Tenant(id) => format!(
            "SELECT m.package_id, m.resource_type, m.url, m.json \
             FROM tenant_packages t JOIN terminology m \
               ON m.instance_id = t.owner AND m.package_id = t.package_id \
             WHERE t.instance_id = {id} ORDER BY t.seq, m.url"
        ),
    }
}

/// The terminology rows with the urls in `$1`, a bare url also matching its versions.
fn sql_load_terminology_for(scope: Scope) -> String {
    match scope {
        Scope::Classic => "SELECT package_id, resource_type, url, json FROM terminology \
                           WHERE (url = ANY($1) OR split_part(url, '|', 1) = ANY($1)) \
                           ORDER BY package_id, url"
            .into(),
        Scope::Tenant(id) => format!(
            "SELECT m.package_id, m.resource_type, m.url, m.json \
             FROM tenant_packages t JOIN terminology m \
               ON m.instance_id = t.owner AND m.package_id = t.package_id \
             WHERE t.instance_id = {id} \
               AND (m.url = ANY($1) OR split_part(m.url, '|', 1) = ANY($1)) \
             ORDER BY t.seq, m.url"
        ),
    }
}

fn sql_load_expansions(scope: Scope) -> String {
    match scope {
        Scope::Classic => "SELECT package_id, url, json FROM value_set_expansion \
                           ORDER BY package_id, url"
            .into(),
        Scope::Tenant(id) => format!(
            "SELECT e.package_id, e.url, e.json FROM tenant_packages t JOIN value_set_expansion e \
               ON e.instance_id = t.instance_id AND e.package_id = t.package_id \
             WHERE t.instance_id = {id} ORDER BY t.seq, e.url"
        ),
    }
}

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
    ///
    /// For the classic layout, where every row of the table is the database's own. A tenant pages
    /// through its packages with [`PostgresPersistence::visit_tenant_pages`].
    fn visit_pages<T: Send>(
        &self,
        select: &str,
        read: impl Fn(&postgres::Row) -> T + Sync,
        key: impl Fn(&T) -> (i64, String),
        visit: &mut dyn FnMut(Vec<T>) -> Result<(), PersistError>,
    ) -> Result<(), PersistError> {
        debug_assert!(matches!(self.scope, Scope::Classic));
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

    fn install_shared(
        &self,
        tenant: i32,
        name: &str,
        version: &str,
        fhir_version: &str,
        content: &str,
        feed: &mut (dyn FnMut(&mut dyn PackageRows) -> Result<(), InstallError> + Send),
    ) -> Result<InstallOutcome, InstallError> {
        // Whether `feed` has been called: after that the package cannot be read again, so what
        // follows must be the shared copy that was just built.
        let mut fed = false;
        for _ in 0..3 {
            match self.activate_shared(tenant, name, version, fhir_version, content)? {
                Shared::Activated(outcome) => return Ok(outcome),
                Shared::Missing if !fed => {
                    fed = self.build_shared(name, version, fhir_version, content, feed)?;
                }
                // The tenant has the package: it is stored as any package is.
                Shared::Own if !fed => {
                    return self.install_package_in_pieces(name, version, fhir_version, feed);
                }
                // The shared copy is made from other content: the package is stored for the tenant
                // alone. Every tenant that lands here stores and reads a full copy of its own, so
                // it is said.
                Shared::Different if !fed => {
                    tracing::warn!(
                        target: "refhir",
                        "the shared copy of {name}@{version} was made from other content than \
                         this tenant's package, so the package is stored for this tenant alone"
                    );
                    return self.install_package_in_pieces(name, version, fhir_version, feed);
                }
                Shared::Own | Shared::Different | Shared::Missing => {
                    return Err(InstallError::Persist(PersistError::Definite(format!(
                        "the shared copy of {name}@{version} was not as expected right after it \
                         was built"
                    ))));
                }
            }
        }
        Err(InstallError::Persist(PersistError::Definite(format!(
            "the shared copy of {name}@{version} did not settle"
        ))))
    }

    /// Gives the tenant the shared copy of a package, if there is one made from `content`.
    fn activate_shared(
        &self,
        tenant: i32,
        name: &str,
        version: &str,
        fhir_version: &str,
        content: &str,
    ) -> Result<Shared, InstallError> {
        let scope = self.scope;
        let _gate = self.gate();
        let mut guard = self.inner.lock()?;
        let mut tx = guard.client.transaction().map_err(definite)?;
        self.fence(&mut tx)?;
        if tx
            .query_opt(&sql_find_package(scope), &[&name, &version])
            .map_err(definite)?
            .is_some()
        {
            return Ok(Shared::Own);
        }
        let Some(shared) = tx
            .query_opt(
                "SELECT id, digest, row_counts FROM packages \
                 WHERE instance_id = 0 AND name = $1 AND version = $2",
                &[&name, &version],
            )
            .map_err(definite)?
        else {
            return Ok(Shared::Missing);
        };
        let package_id: i64 = shared.get(0);
        let digest: Option<String> = shared.get(1);
        if digest.as_deref() != Some(content) {
            return Ok(Shared::Different);
        }
        // The copy is whole: it holds what it was built with. A tenant is not given a package that
        // is damaged, since every tenant added later would be given it too.
        let recorded: Option<String> = shared.get(2);
        let found = shared_row_counts(&mut tx, package_id)?;
        if recorded.as_deref() != Some(found.as_str()) {
            return Err(InstallError::Persist(PersistError::Definite(format!(
                "the shared copy of {name}@{version} is damaged: it was built with {} (definitions, \
                 terminology, search parameters, expansions) and holds {found}",
                recorded.as_deref().unwrap_or("no count")
            ))));
        }
        if let Some(row) = tx
            .query_opt(&sql_first_fhir_version(scope), &[])
            .map_err(definite)?
        {
            let existing: String = row.get(0);
            if existing != fhir_version {
                return Err(InstallError::FhirVersion {
                    existing,
                    incoming: fhir_version.to_string(),
                });
            }
        }
        // Read before the package is linked, so that the package is not among them.
        let superseded = older_versions(&mut tx, scope, name, version)?;
        link_package(&mut tx, tenant, package_id, 0)?;
        // The tenant's own search parameters, planned against what it has as an install would.
        let rows = tx
            .query(
                "SELECT url, base, code, param_type, expression, status, json \
                 FROM base_search_parameters WHERE package_id = $1 ORDER BY ord",
                &[&package_id],
            )
            .map_err(definite)?;
        let mut parameters = Vec::with_capacity(rows.len());
        for row in &rows {
            parameters.push(NewSearchParameter {
                url: row.get(0),
                base: row.get(1),
                code: row.get(2),
                param_type: row.get(3),
                expression: row.get(4),
                status: ParamStatus::parse(&row.get::<_, String>(5))?,
                json: row.get(6),
            });
        }
        let newly_pending =
            insert_search_parameters(&mut tx, scope, package_id, &parameters, &superseded)?;
        let retired = remove_superseded(&mut tx, scope, &superseded)?;
        tx.execute(
            &format!(
                "INSERT INTO value_set_expansion ({}package_id, url, json) \
                 SELECT {}package_id, url, json FROM base_expansions WHERE package_id = $1",
                scope.cols(),
                scope.vals()
            ),
            &[&package_id],
        )
        .map_err(definite)?;
        tx.commit().map_err(definite)?;
        Ok(Shared::Activated(InstallOutcome {
            package_id,
            inserted: true,
            newly_pending,
            retired,
            shared: true,
        }))
    }

    /// Builds the shared copy of a package, unless another process built it while this waited for
    /// the lock. Returns whether `feed` was called, and so whether the package was read.
    fn build_shared(
        &self,
        name: &str,
        version: &str,
        fhir_version: &str,
        content: &str,
        feed: &mut (dyn FnMut(&mut dyn PackageRows) -> Result<(), InstallError> + Send),
    ) -> Result<bool, InstallError> {
        let _gate = self.gate();
        let mut guard = self.inner.lock()?;
        let mut tx = guard.client.transaction().map_err(definite)?;
        self.fence(&mut tx)?;
        // Waits for as long as another process builds the same package, however long that takes
        // (a very large one over a slow link may take minutes): the lock is tried, not waited
        // for, so no lock timeout can end the wait with an error, and it is said now and then.
        let spec = format!("{name}@{version}");
        let waiting = std::time::Instant::now();
        let mut said = std::time::Duration::ZERO;
        loop {
            let got: bool = tx
                .query_one(
                    "SELECT pg_try_advisory_xact_lock($1, hashtext($2))",
                    &[&SHARED_PACKAGE_LOCK, &spec],
                )
                .map_err(definite)?
                .get(0);
            if got {
                break;
            }
            if waiting.elapsed() >= said + std::time::Duration::from_secs(30) {
                said = waiting.elapsed();
                tracing::info!(
                    target: "refhir",
                    "waiting for another process to build {spec} for the tenants to share ({}s so far)",
                    said.as_secs()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        // Decided again under the lock: another process may have just built it.
        let built = tx
            .query_opt(
                "SELECT 1 FROM packages WHERE instance_id = 0 AND name = $1 AND version = $2",
                &[&name, &version],
            )
            .map_err(definite)?;
        if built.is_some() {
            return Ok(false);
        }
        // Only this transaction may write the shared catalog, and only here.
        tx.batch_execute("SET LOCAL refhir.shared_writer = 'on'")
            .map_err(definite)?;
        let inserted = tx
            .query_one(
                "INSERT INTO packages (instance_id, name, version, fhir_version, digest) \
                 VALUES (0, $1, $2, $3, $4) RETURNING id",
                &[&name, &version, &fhir_version, &content],
            )
            .map_err(definite)?;
        let package_id: i64 = inserted.get(0);
        let mut rows = SharedRows {
            tx: &mut tx,
            package_id,
            next_ord: 0,
            heaviest: 0,
        };
        feed(&mut rows)?;
        let heaviest = rows.heaviest;
        // What the copy holds, so that activation can tell it is still whole.
        let counts = shared_row_counts(&mut tx, package_id)?;
        tx.execute(
            "UPDATE packages SET row_counts = $2 WHERE id = $1",
            &[&package_id, &counts],
        )
        .map_err(definite)?;
        tx.commit().map_err(definite)?;
        retire_if_swollen(&mut guard, heaviest);
        Ok(true)
    }

    /// The packages a tenant has, as (package id, owner), in the order it got them. Only a tenant has
    /// them: a classic database has packages and no such list.
    fn tenant_packages(&self, tenant: i32) -> Result<Vec<(i64, i32)>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let rows = guard
                .client
                .query(
                    &format!(
                        "SELECT package_id, owner FROM tenant_packages \
                         WHERE instance_id = {tenant} ORDER BY seq"
                    ),
                    &[],
                )
                .map_err(definite)?;
            Ok(rows.iter().map(|row| (row.get(0), row.get(1))).collect())
        })
    }

    /// [`PostgresPersistence::visit_pages`] for a tenant: the rows of each of its packages, in the
    /// order it got them and by url within one, a page at a time. `columns` are what to read, the
    /// first two being `package_id` and `url`. `shared` says whether the rows of a package that every
    /// tenant shares are held under the catalog's instance (definitions and terminology are) or are
    /// always the tenant's own (expansions).
    #[allow(clippy::too_many_arguments)]
    fn visit_tenant_pages<T: Send>(
        &self,
        tenant: i32,
        table: &str,
        columns: &str,
        shared: bool,
        read: impl Fn(&postgres::Row) -> T + Sync,
        url_of: impl Fn(&T) -> String,
        visit: &mut dyn FnMut(Vec<T>) -> Result<(), PersistError>,
    ) -> Result<(), PersistError> {
        for (package_id, owner) in self.tenant_packages(tenant)? {
            let instance = if shared { owner } else { tenant };
            let held = format!("instance_id = {instance} AND package_id = {package_id}");
            let first = format!(
                "SELECT {columns} FROM {table} WHERE {held} \
                 ORDER BY url LIMIT {CONFORMANCE_PAGE_ROWS}"
            );
            let next = format!(
                "SELECT {columns} FROM {table} WHERE {held} AND url > $1 \
                 ORDER BY url LIMIT {CONFORMANCE_PAGE_ROWS}"
            );
            let mut after: Option<String> = None;
            loop {
                let page: Vec<T> = crate::off_runtime(|| {
                    let mut guard = self.inner.lock()?;
                    let rows = match &after {
                        None => guard.client.query(first.as_str(), &[]),
                        Some(url) => guard.client.query(next.as_str(), &[url]),
                    }
                    .map_err(definite)?;
                    Ok::<_, PersistError>(rows.iter().map(&read).collect())
                })?;
                let Some(last) = page.last() else {
                    break;
                };
                after = Some(url_of(last));
                let done = page.len() < CONFORMANCE_PAGE_ROWS;
                visit(page)?;
                if done {
                    break;
                }
            }
        }
        Ok(())
    }

    fn row_json(
        &self,
        table: &str,
        shared: bool,
        package_id: i64,
        url: &str,
    ) -> Result<Option<String>, PersistError> {
        // A package every tenant shares has its rows under the catalog's instance, so for the
        // tables it may be in, a tenant reads its own and the catalog's.
        let held = if shared {
            self.scope.and_shared()
        } else {
            self.scope.and()
        };
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let row = guard
                .client
                .query_opt(
                    format!("SELECT json FROM {table} WHERE {held}package_id = $1 AND url = $2")
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
                .query(&sql_load_packages(self.scope), &[])
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
        self.row_json("structure_definitions", true, package_id, url)
    }

    fn load_terminology_json(
        &self,
        package_id: i64,
        url: &str,
    ) -> Result<Option<String>, PersistError> {
        self.row_json("terminology", true, package_id, url)
    }

    fn load_expansion_json(
        &self,
        package_id: i64,
        url: &str,
    ) -> Result<Option<String>, PersistError> {
        self.row_json("value_set_expansion", false, package_id, url)
    }

    fn shared(&self) -> Option<std::sync::Arc<dyn ConformanceStore>> {
        Some(std::sync::Arc::new(self.clone()))
    }

    fn load_structure_definitions(&self) -> Result<Vec<StoredStructureDefinition>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let rows = guard
                .client
                .query(&sql_load_structure_definitions(self.scope), &[])
                .map_err(definite)?;
            Ok(rows.iter().map(structure_definition_from).collect())
        })
    }

    fn visit_structure_definitions(
        &self,
        visit: &mut dyn FnMut(Vec<StoredStructureDefinition>) -> Result<(), PersistError>,
    ) -> Result<(), PersistError> {
        const COLUMNS: &str = "package_id, url, type_name, kind, is_abstract, json";
        match self.scope {
            Scope::Classic => self.visit_pages(
                "SELECT package_id, url, type_name, kind, is_abstract, json FROM structure_definitions",
                structure_definition_from,
                |row| (row.package_id, row.url.clone()),
                visit,
            ),
            Scope::Tenant(tenant) => self.visit_tenant_pages(
                tenant,
                "structure_definitions",
                COLUMNS,
                true,
                structure_definition_from,
                |row| row.url.clone(),
                visit,
            ),
        }
    }

    fn load_search_parameters(&self) -> Result<Vec<StoredSearchParameter>, PersistError> {
        crate::off_runtime(|| {
            let mut guard = self.inner.lock()?;
            let rows = guard
                .client
                .query(&sql_load_search_parameters(self.scope), &[])
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
                .query(&sql_load_terminology(self.scope), &[])
                .map_err(definite)?;
            Ok(rows.iter().map(terminology_from).collect())
        })
    }

    fn visit_terminology(
        &self,
        visit: &mut dyn FnMut(Vec<StoredTerminology>) -> Result<(), PersistError>,
    ) -> Result<(), PersistError> {
        match self.scope {
            Scope::Classic => self.visit_pages(
                "SELECT package_id, resource_type, url, json FROM terminology",
                terminology_from,
                |row| (row.package_id, row.url.clone()),
                visit,
            ),
            Scope::Tenant(tenant) => self.visit_tenant_pages(
                tenant,
                "terminology",
                "package_id, resource_type, url, json",
                true,
                terminology_from,
                |row| row.url.clone(),
                visit,
            ),
        }
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
                .query(&sql_load_terminology_for(self.scope), &[&urls])
                .map_err(definite)?;
            Ok(rows.iter().map(terminology_from).collect())
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
                        self.scope.and_shared()
                    ),
                    &[&package_id],
                )
                .map_err(definite)?;
            Ok(rows.iter().map(terminology_from).collect())
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
                .query(&sql_load_expansions(self.scope), &[])
                .map_err(definite)?;
            Ok(rows.iter().map(expansion_from).collect())
        })
    }

    fn visit_expansions(
        &self,
        visit: &mut dyn FnMut(Vec<StoredExpansion>) -> Result<(), PersistError>,
    ) -> Result<(), PersistError> {
        match self.scope {
            Scope::Classic => self.visit_pages(
                "SELECT package_id, url, json FROM value_set_expansion",
                expansion_from,
                |row| (row.package_id, row.url.clone()),
                visit,
            ),
            Scope::Tenant(tenant) => self.visit_tenant_pages(
                tenant,
                "value_set_expansion",
                "package_id, url, json",
                false,
                expansion_from,
                |row| row.url.clone(),
                visit,
            ),
        }
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

    fn install_shared_package_in_pieces(
        &self,
        name: &str,
        version: &str,
        fhir_version: &str,
        content: &str,
        feed: &mut (dyn FnMut(&mut dyn PackageRows) -> Result<(), InstallError> + Send),
    ) -> Result<InstallOutcome, InstallError> {
        match self.scope {
            Scope::Classic => self.install_package_in_pieces(name, version, fhir_version, feed),
            Scope::Tenant(tenant) => crate::off_runtime(|| {
                self.install_shared(tenant, name, version, fhir_version, content, feed)
            }),
        }
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
                .query_opt(&sql_first_fhir_version(self.scope), &[])
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
            if let Scope::Tenant(tenant) = self.scope {
                // The tenant's own package: its rows are the tenant's.
                link_package(&mut tx, tenant, id, tenant)?;
            }
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
/// installed, except those of older versions of the same package: it replaces them, and they are
/// removed when it is stored whole. Each call stores the rows it is given in statements of its
/// own, so what is held at once is the rows of one call, however large the package.
struct Installer<'a, 'c> {
    tx: &'a mut Transaction<'c>,
    scope: Scope,
    package_id: i64,
    /// Whose rows hold the package: 0 when it is one every tenant shares, which a tenant cannot add
    /// to. Only meaningful for a tenant.
    owner: i32,
    /// Whether the package was there before this install.
    existing: bool,
    newly_pending: Vec<PendingParam>,
    /// Whether the package's earlier expansions have been cleared for this install's.
    expansions_cleared: bool,
    /// The most JSON text any one call was given.
    heaviest: usize,
    /// The older versions of this package that the tenant has, which this install replaces. Empty
    /// for a package that was there before.
    superseded: Vec<i64>,
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
            .query_opt(&sql_find_package(scope), &[&name, &version])
            .map_err(definite)?;
        if let Some(row) = existing {
            let package_id: i64 = row.get(0);
            // Whose rows hold the package: a tenant's list says, and a classic database has no list.
            let owner: i32 = match scope {
                Scope::Classic => 0,
                Scope::Tenant(_) => row.get(1),
            };
            return Ok(Self {
                tx,
                scope,
                package_id,
                owner,
                existing: true,
                newly_pending: Vec::new(),
                expansions_cleared: false,
                heaviest: 0,
                superseded: Vec::new(),
            });
        }
        let current = tx
            .query_opt(&sql_first_fhir_version(scope), &[])
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
        let superseded = older_versions(tx, scope, name, version)?;
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
        if let Scope::Tenant(tenant) = scope {
            link_package(tx, tenant, package_id, tenant)?;
        }
        Ok(Self {
            tx,
            scope,
            package_id,
            owner: 0,
            existing: false,
            newly_pending: Vec::new(),
            expansions_cleared: false,
            heaviest: 0,
            superseded,
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
        let retired = remove_superseded(self.tx, self.scope, &self.superseded)?;
        Ok((
            InstallOutcome {
                package_id: self.package_id,
                inserted: !self.existing,
                newly_pending: self.newly_pending,
                retired,
                shared: false,
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
        let pending =
            insert_search_parameters(self.tx, self.scope, self.package_id, rows, &self.superseded)?;
        self.newly_pending.extend(pending);
        Ok(())
    }

    fn terminology(&mut self, rows: &[NewTerminology]) -> Result<(), InstallError> {
        // A package every tenant shares is the same for all of them: what an install of it brings
        // is not added to it.
        if matches!(self.scope, Scope::Tenant(_)) && self.owner == 0 && self.existing {
            return Ok(());
        }
        self.note(rows.iter().map(|row| row.json.len()).sum());
        insert_terminology(self.tx, self.scope, self.package_id, rows)
    }

    fn expansions(&mut self, rows: &[NewExpansion]) -> Result<(), InstallError> {
        self.clear_expansions()?;
        self.note(rows.iter().map(|row| row.json.len()).sum());
        insert_expansions(
            self.tx,
            "value_set_expansion",
            &self.scope.cols(),
            &self.scope.vals(),
            self.package_id,
            rows,
        )
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
/// query for all of them, not one per parameter. The rows of `superseded` packages are read as the
/// ones the package replaces.
fn insert_search_parameters(
    tx: &mut Transaction<'_>,
    scope: Scope,
    package_id: i64,
    parameters: &[NewSearchParameter],
    superseded: &[i64],
) -> Result<Vec<refhir_persistence::PendingParam>, InstallError> {
    if parameters.is_empty() {
        return Ok(Vec::new());
    }
    let bases: Vec<&str> = parameters.iter().map(|row| row.base.as_str()).collect();
    let codes: Vec<&str> = parameters.iter().map(|row| row.code.as_str()).collect();
    let stored = tx
        .query(
            &format!(
                "SELECT base, code, package_id, url, COALESCE(expression, ''), param_type, status, \
                        json \
                 FROM search_parameters \
                 WHERE {}(base, code) IN (SELECT * FROM UNNEST($1::text[], $2::text[]))",
                scope.and()
            ),
            &[&bases, &codes],
        )
        .map_err(definite)?
        .iter()
        .map(|row| {
            let package_id: i64 = row.get(2);
            StoredParameterRow {
                base: row.get(0),
                code: row.get(1),
                row: InstalledParam {
                    package_id,
                    url: row.get(3),
                    expression: row.get(4),
                    param_type: row.get(5),
                    status: row.get(6),
                    json: row.get(7),
                    superseded: superseded.contains(&package_id),
                },
            }
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
        insert_parameters(tx, ParameterTable::Own(scope), package_id, &rows)?;
    }
    Ok(plan.newly_pending)
}

/// The ids of the packages named `name` at a version older than `version` that the tenant has: the
/// ones a package of that version replaces.
fn older_versions(
    tx: &mut Transaction<'_>,
    scope: Scope,
    name: &str,
    version: &str,
) -> Result<Vec<i64>, InstallError> {
    Ok(tx
        .query(&sql_package_versions(scope), &[&name])
        .map_err(definite)?
        .iter()
        .filter(|row| compare_versions(&row.get::<_, String>(1), version).is_lt())
        .map(|row| row.get(0))
        .collect())
}

/// Remove the older versions of a package from the tenant, with everything they hold: definitions,
/// search parameters, terminology, expansions, and the backfills that were queued for them.
/// Whatever the new version has carried over is stored under it. Says which search parameters
/// nothing defines now.
///
/// Every statement is limited to the tenant's own rows. A package that every tenant shares keeps
/// its definitions and terminology under the shared catalog (instance 0), which no tenant's
/// statement reaches, so the tenant gives up its link to it and its own copies of what changes per
/// tenant, and the shared copy stays for the tenants that still have it.
fn remove_superseded(
    tx: &mut Transaction<'_>,
    scope: Scope,
    superseded: &[i64],
) -> Result<Vec<RetiredParam>, InstallError> {
    if superseded.is_empty() {
        return Ok(Vec::new());
    }
    let and = scope.and();
    let removed = tx
        .query(
            &format!(
                "DELETE FROM search_parameters WHERE {and}package_id = ANY($1) RETURNING base, code"
            ),
            &[&superseded],
        )
        .map_err(definite)?;
    let gone: std::collections::BTreeSet<(String, String)> =
        removed.iter().map(|row| (row.get(0), row.get(1))).collect();
    for statement in [
        format!(
            "DELETE FROM backfill_parameter WHERE {and}backfill_id IN \
             (SELECT id FROM index_backfills WHERE {and}package_id = ANY($1))"
        ),
        format!("DELETE FROM index_backfills WHERE {and}package_id = ANY($1)"),
        format!("DELETE FROM value_set_expansion WHERE {and}package_id = ANY($1)"),
        format!("DELETE FROM structure_definitions WHERE {and}package_id = ANY($1)"),
        format!("DELETE FROM terminology WHERE {and}package_id = ANY($1)"),
    ] {
        tx.execute(&statement, &[&superseded]).map_err(definite)?;
    }
    if let Scope::Tenant(tenant) = scope {
        tx.execute(
            &format!(
                "DELETE FROM tenant_packages WHERE instance_id = {tenant} AND package_id = ANY($1)"
            ),
            &[&superseded],
        )
        .map_err(definite)?;
    }
    let delete_rows = format!("DELETE FROM packages WHERE {and}id = ANY($1)");
    match scope {
        Scope::Classic => {
            tx.execute(&delete_rows, &[&superseded]).map_err(definite)?;
        }
        Scope::Tenant(_) => {
            // The row goes unless a link of another tenant still names it, which this crate never
            // makes but the database permits and a tenant cannot see through its row policy: that
            // tenant keeps its package, and the statement is undone alone. (A shared package's row
            // is instance 0's, which the filter leaves alone.)
            let mut attempt = tx.savepoint("drop_superseded").map_err(definite)?;
            match attempt.execute(&delete_rows, &[&superseded]) {
                Ok(_) => attempt.commit().map_err(definite)?,
                Err(err)
                    if err.code() == Some(&postgres::error::SqlState::FOREIGN_KEY_VIOLATION) => {}
                Err(err) => return Err(InstallError::Persist(definite(err))),
            }
        }
    }
    let mut retired = Vec::new();
    for (base, code) in gone {
        let defined: bool = tx
            .query_one(
                &format!(
                    "SELECT EXISTS (SELECT 1 FROM search_parameters \
                     WHERE {and}base = $1 AND code = $2)"
                ),
                &[&base, &code],
            )
            .map_err(definite)?
            .get(0);
        if !defined {
            retired.push(RetiredParam { base, code });
        }
    }
    Ok(retired)
}

/// Where a run of search parameters goes: a tenant's (or a classic database's) own table, or the
/// shared catalog's copy that tenants are given theirs from, which keeps each row's place in the
/// package (`ord`, counting from `first_ord`) since a package may define one parameter twice.
enum ParameterTable {
    Own(Scope),
    Catalog { first_ord: i64 },
}

/// Inserts a run of search parameters, each with the status to store it with, as one statement.
fn insert_parameters(
    tx: &mut Transaction<'_>,
    table: ParameterTable,
    package_id: i64,
    rows: &[(&NewSearchParameter, ParamStatus)],
) -> Result<(), PersistError> {
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
    match table {
        ParameterTable::Own(scope) => tx.execute(
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
        ),
        ParameterTable::Catalog { first_ord } => tx.execute(
            "INSERT INTO base_search_parameters \
             (package_id, ord, url, base, code, param_type, expression, status, json) \
             SELECT $1, $2::bigint + n, url, base, code, param_type, expression, status, json \
             FROM UNNEST($3::text[], $4::text[], $5::text[], $6::text[], $7::text[], \
                         $8::text[], $9::text[]) \
                  WITH ORDINALITY AS rows(url, base, code, param_type, expression, \
                                          status, json, n)",
            &[
                &package_id,
                &first_ord,
                &urls,
                &bases,
                &codes,
                &types,
                &expressions,
                &statuses,
                &documents,
            ],
        ),
    }
    .map_err(definite)?;
    Ok(())
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

/// Inserts expansions into `table`, with `cols` and `vals` (a tenant's `instance_id, ` and its id, or
/// nothing for a table that has no tenant column) in front of the package id.
fn insert_expansions(
    tx: &mut Transaction<'_>,
    table: &str,
    cols: &str,
    vals: &str,
    package_id: i64,
    rows: &[NewExpansion],
) -> Result<(), InstallError> {
    for run in runs(rows, |row| row.json.len() + row.url.len()) {
        let urls: Vec<&str> = run.iter().map(|row| row.url.as_str()).collect();
        let documents: Vec<&str> = run.iter().map(|row| row.json.as_str()).collect();
        tx.execute(
            &format!(
                "INSERT INTO {table} ({cols}package_id, url, json) \
                 SELECT {vals}$1, url, json FROM UNNEST($2::text[], $3::text[]) AS rows(url, json)"
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
