//! Project committed SearchParameter, ValueSet, CodeSystem, and ConceptMap
//! documents into the conformance tables.
//!
//! The document log stays the source of the resource. Search indexes and value
//! set expansions are filled from the same tables package install uses.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use refhir_database::{CommitError, Database, DocumentWrite, IndexPut};
use refhir_persistence::{
    ConformanceStore, ExpansionChange, NewExpansion, NewSearchParameter, NewTerminology,
    ParamStatus, StoredSearchParameter, StoredTerminology,
};
use refhir_types::{DocumentId, IndexId, Interval, Order, TabletId, Timestamp};
use refhir_validate::{winning_scopes, MemoryTerminology, TerminologyChange};
use serde_json::Value;

use crate::error::FhirError;
use crate::package::{self, parameter_rows, SearchScope};
use crate::run::{self, RunError, RunResult, MAX_OCC_FAILURES};

const CONFORMANCE: &[&str] = &["SearchParameter", "ValueSet", "CodeSystem", "ConceptMap"];

pub fn commit_operation(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &std::sync::RwLock<crate::catalog::Catalog>,
    path: &str,
    args: &Value,
) -> Result<RunResult, RunError> {
    if path == "ImplementationGuide/$install" {
        // Installing writes the conformance tables, which only the primary may do.
        if db.is_read_only() {
            return Err(RunError::Commit(CommitError::ReadOnly));
        }
        return crate::npm::install_operation(store, db, catalog, args).map_err(RunError::Fhir);
    }
    preflight(store, catalog, path, args).map_err(RunError::Fhir)?;
    let result = {
        let guard = catalog.read().expect("catalog");
        run::run(db, &guard, path, args)?
    };
    match project_writes(store, db, catalog, result.ts, &result.writes) {
        Ok(()) => Ok(result),
        // The document is committed. A failure of the store itself is repaired from the stored
        // resources straight away, so the request does not report a write that happened as one
        // that did not; a failure that is about the request still goes back to the client.
        Err(err @ FhirError::Storage(_)) => {
            tracing::error!(error = %err, "conformance projection failed after commit; repairing");
            match repair_conformance(store, db, catalog) {
                // The failure may have been the announcement to the other processes, after the
                // projection had gone through. A repair that finds nothing to fix says nothing,
                // so say it here, and say all of it: what was lost may have been either kind.
                Ok(()) => match package::announce_all(store, catalog) {
                    Ok(()) => Ok(result),
                    Err(announce) => Err(RunError::Fhir(FhirError::Storage(format!(
                        "the write was committed and the conformance tables are in order, but \
                         telling the other processes failed ({announce}); a replica or standby \
                         follows it when the catalog next changes, or at its next start"
                    )))),
                },
                Err(repair) => Err(RunError::Fhir(FhirError::Storage(format!(
                    "the write was committed, but projecting it into the conformance tables \
                     failed ({err}) and so did the repair ({repair}); it is repaired from the \
                     stored resources at the next start"
                )))),
            }
        }
        Err(err) => Err(RunError::Fhir(err)),
    }
}

pub fn repair_conformance(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &std::sync::RwLock<crate::catalog::Catalog>,
) -> Result<(), FhirError> {
    let local_id = local_package(store, catalog)?;
    let mut term_dirty = false;
    for type_name in ["ValueSet", "CodeSystem", "ConceptMap"] {
        if !catalog.read().expect("catalog").contains(type_name) {
            continue;
        }
        let live = package::live_ids(db, type_name)?;
        // The key each live resource is stored under now. A row under any other key for the same
        // resource is left over from a version it no longer has.
        let mut keys: BTreeMap<String, String> = BTreeMap::new();
        for id in &live {
            let Some(resource) = live_json(db, type_name, id)? else {
                continue;
            };
            if let Some(key) = row_key(&resource) {
                keys.insert(id.clone(), key);
            }
            if terminology_current(store, local_id, &resource)? {
                continue;
            }
            if project_terminology(store, local_id, &resource, false)? {
                term_dirty = true;
            }
        }
        if delete_orphan_terminology(store, local_id, type_name, &keys)? {
            term_dirty = true;
        }
    }
    // Expansions stored under other rules are rebuilt once, here: a database that ran the previous
    // version's rebuild has expansions read from one pool for every package (R5's encounter-status
    // codes for R4's value set), and nothing else would ever replace them. The rebuild writes the
    // current rules, so the next start finds nothing to do.
    if catalog.read().expect("catalog").expansions_stale() {
        term_dirty = true;
    }
    if term_dirty {
        sync_expansions(store)?;
        package::publish(store, catalog)?;
    }

    if catalog.read().expect("catalog").contains("SearchParameter") {
        let live = package::live_ids(db, "SearchParameter")?;
        // Resolving what each custom parameter refers to means parsing every installed
        // structure definition, which is slow and pointless when there are none.
        let scope = if live.is_empty() {
            SearchScope::default()
        } else {
            SearchScope::from_store(store)?
        };
        let mut jobs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut changed = false;
        for id in &live {
            let Some(resource) = live_json(db, "SearchParameter", id)? else {
                continue;
            };
            if parameter_current(store, local_id, &resource, &scope)? {
                continue;
            }
            let (dirty, more) = project_parameter(store, db, local_id, &resource, false, &scope)?;
            changed |= dirty;
            merge_jobs(&mut jobs, more);
        }
        if delete_orphan_parameters(store, db, local_id, &live)? {
            changed = true;
        }
        finish_parameters(store, db, catalog, local_id, changed, jobs)?;
    }
    Ok(())
}

fn preflight(
    store: &dyn ConformanceStore,
    catalog: &std::sync::RwLock<crate::catalog::Catalog>,
    path: &str,
    args: &Value,
) -> Result<(), FhirError> {
    let resources = candidate_resources(path, args);
    if resources.is_empty() {
        return Ok(());
    }
    let local_id = local_package(store, catalog)?;
    // A ValueSet is checked against the one row that could share its URL, and the search
    // parameter tables are read only for a request that carries a SearchParameter. Reading every
    // installed code system and every search parameter for each write is what made a one-line
    // ValueSet cost as much as the whole catalog.
    let terminology = store.load_terminology_for(&terminology_urls(&resources))?;
    let parameters_wanted = resources.iter().any(|resource| {
        resource
            .get("resourceType")
            .and_then(|value| value.as_str())
            == Some("SearchParameter")
    });
    let (parameters, scope) = if parameters_wanted {
        (
            store.load_search_parameters()?,
            Some(SearchScope::from_store(store)?.with_parameters(&resources)),
        )
    } else {
        (Vec::new(), None)
    };
    let mut seen = Vec::new();
    for resource in &resources {
        match resource
            .get("resourceType")
            .and_then(|value| value.as_str())
        {
            Some("SearchParameter") => {
                let scope = scope
                    .as_ref()
                    .expect("scope is built for search parameters");
                let rows = check_parameter(&parameters, &seen, resource, scope)?;
                seen.extend(rows);
            }
            Some("ValueSet" | "CodeSystem" | "ConceptMap") => {
                check_terminology(&terminology, local_id, resource)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn project_writes(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &std::sync::RwLock<crate::catalog::Catalog>,
    ts: Timestamp,
    writes: &[DocumentWrite],
) -> Result<(), FhirError> {
    let interesting: Vec<_> = writes
        .iter()
        .filter(|write| CONFORMANCE.contains(&write.tablet.as_str()))
        .collect();
    if interesting.is_empty() {
        return Ok(());
    }
    let local_id = local_package(store, catalog)?;
    let mut written: Vec<String> = Vec::new();
    let mut removed: Vec<(String, String)> = Vec::new();
    let mut rebuild = false;
    for write in &interesting {
        if !matches!(
            write.tablet.as_str(),
            "ValueSet" | "CodeSystem" | "ConceptMap"
        ) {
            continue;
        }
        let Some(resource) = write_resource(db, write, ts)? else {
            continue;
        };
        let Some(key) = row_key(&resource) else {
            if write.json.is_some() {
                return Err(FhirError::Invalid("url is required".into()));
            }
            continue;
        };
        if write.json.is_none() {
            store.delete_terminology(local_id, &key)?;
            // An installed package can hold the same canonical, and then the deletion is not the
            // end of it: what the catalog holds for that URL becomes the package's. Only a
            // rebuild from the stored rows says what that is.
            if store
                .load_terminology_for(std::slice::from_ref(&key))?
                .iter()
                .any(|row| row.package_id != local_id && row.url == key)
            {
                rebuild = true;
            } else {
                removed.push((write.tablet.clone(), key));
            }
            continue;
        }
        // An update that changes a code system's version makes it a different row: the row for
        // the version it had must not be left behind as if that version still existed.
        if let Some(previous) = previous_body(db, &write.tablet, &write.id, ts)? {
            if let Some(old_key) = serde_json::from_str::<Value>(&previous)
                .ok()
                .and_then(|old| row_key(&old))
                .filter(|old_key| *old_key != key)
            {
                store.delete_terminology(local_id, &old_key)?;
                removed.push((write.tablet.clone(), old_key));
            }
        }
        drop_unversioned_row(store, local_id, &resource, &key, &mut removed)?;
        if project_terminology(store, local_id, &resource, true)? {
            written.push(key);
        }
    }
    if rebuild || !written.is_empty() || !removed.is_empty() {
        // The terminology changed and nothing else did, so only what the change touches is
        // recomputed. If that cannot be done, the full rebuild gives the same answer.
        let applied = if rebuild {
            None
        } else {
            Some(apply_terminology(
                store, catalog, local_id, &written, &removed,
            ))
        };
        if matches!(applied, Some(Ok(()))) {
            // Nothing was loaded again here, so nothing told the other processes: a replica and a
            // standby follow counters, and only a publish moved them. This changed the
            // terminology alone, so that is what they are told, and what they load.
            package::announce_terminology(store)?;
        } else {
            if let Some(Err(err)) = &applied {
                tracing::warn!(error = %err, "incremental terminology update failed; rebuilding");
            }
            // Publishing announces the change as well, so this is told once either way.
            sync_expansions(store)?;
            package::publish(store, catalog)?;
        }
    }

    let parameters_written = interesting
        .iter()
        .any(|write| write.tablet == "SearchParameter");
    if !parameters_written {
        return Ok(());
    }
    let scope = SearchScope::from_store(store)?.with_parameters(
        &interesting
            .iter()
            .filter_map(|write| write.json.as_ref())
            .filter_map(|json| serde_json::from_str::<Value>(json).ok())
            .filter(|resource| {
                resource
                    .get("resourceType")
                    .and_then(|value| value.as_str())
                    == Some("SearchParameter")
            })
            .collect::<Vec<_>>(),
    );
    let mut jobs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut changed = false;
    for write in &interesting {
        if write.tablet != "SearchParameter" {
            continue;
        }
        let Some(resource) = write_resource(db, write, ts)? else {
            continue;
        };
        if write.json.is_none() {
            if delete_parameter(store, db, local_id, &resource)? {
                changed = true;
            }
            continue;
        }
        let (dirty, more) = project_parameter(store, db, local_id, &resource, true, &scope)?;
        changed |= dirty;
        merge_jobs(&mut jobs, more);
    }
    finish_parameters(store, db, catalog, local_id, changed, jobs)
}

fn finish_parameters(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &std::sync::RwLock<crate::catalog::Catalog>,
    local_id: i64,
    changed: bool,
    jobs: BTreeMap<String, BTreeSet<String>>,
) -> Result<(), FhirError> {
    if changed || !jobs.is_empty() {
        package::publish(store, catalog)?;
    }
    if jobs.is_empty() {
        return Ok(());
    }
    let watermark = db
        .latest_ts()
        .to_i64()
        .map_err(|_| FhirError::Storage("watermark timestamp does not fit".into()))?;
    for (base, urls) in &jobs {
        let urls: Vec<String> = urls.iter().cloned().collect();
        store.insert_backfill(local_id, watermark, base, &urls)?;
    }
    package::run_backfills(store, db, catalog)
}

fn project_terminology(
    store: &dyn ConformanceStore,
    local_id: i64,
    resource: &Value,
    strict: bool,
) -> Result<bool, FhirError> {
    let url = resource
        .get("url")
        .and_then(|value| value.as_str())
        .ok_or_else(|| FhirError::Invalid("url is required".into()))?;
    if terminology_owned_elsewhere(store, local_id, url)? {
        if strict {
            return Err(FhirError::Invalid(format!(
                "canonical {url} belongs to an installed package"
            )));
        }
        return Ok(false);
    }
    let resource_type = resource
        .get("resourceType")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    store.upsert_terminology(
        local_id,
        &NewTerminology {
            resource_type,
            url: row_key(resource).unwrap_or_else(|| url.to_string()),
            json: resource.to_string(),
        },
    )?;
    Ok(true)
}

/// The key a terminology resource is stored under. A code system with a version is stored as
/// `url|version`, so that two versions of one system are two rows, as a package's are; anything
/// else is stored under its URL.
fn row_key(resource: &Value) -> Option<String> {
    if resource
        .get("resourceType")
        .and_then(|value| value.as_str())
        == Some("CodeSystem")
    {
        return refhir_validate::code_system_row_key(resource);
    }
    resource
        .get("url")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// Before code systems were stored by version, one with a version was stored under its bare URL.
/// When that resource is written again its row moves to the key with the version, and the old
/// one goes, so it is not counted twice.
fn drop_unversioned_row(
    store: &dyn ConformanceStore,
    local_id: i64,
    resource: &Value,
    key: &str,
    removed: &mut Vec<(String, String)>,
) -> Result<(), FhirError> {
    if resource
        .get("resourceType")
        .and_then(|value| value.as_str())
        != Some("CodeSystem")
    {
        return Ok(());
    }
    let Some(url) = resource.get("url").and_then(|value| value.as_str()) else {
        return Ok(());
    };
    if url == key {
        return Ok(());
    }
    let id = resource.get("id").and_then(|value| value.as_str());
    for row in store.load_terminology_for(&[url.to_string()])? {
        if row.package_id != local_id || row.url != url {
            continue;
        }
        let Ok(old) = serde_json::from_str::<Value>(&row.json) else {
            continue;
        };
        if old.get("id").and_then(|value| value.as_str()) != id {
            continue;
        }
        store.delete_terminology(local_id, &row.url)?;
        if let Some(old_key) = row_key(&old) {
            removed.push(("CodeSystem".to_string(), old_key));
        }
    }
    Ok(())
}

fn project_parameter(
    store: &dyn ConformanceStore,
    db: &Database,
    local_id: i64,
    resource: &Value,
    strict: bool,
    scope: &SearchScope,
) -> Result<(bool, BTreeMap<String, BTreeSet<String>>), FhirError> {
    let rows = parameter_rows(resource, scope)?;
    if rows.is_empty() {
        return Err(FhirError::Invalid(
            "SearchParameter base is required".into(),
        ));
    }
    if rows
        .iter()
        .any(|row| row.status == ParamStatus::Unsupported)
    {
        if strict {
            return Err(FhirError::Invalid(format!(
                "search parameter {} expression is not supported",
                rows[0].code
            )));
        }
        return Ok((false, BTreeMap::new()));
    }
    let stored = store.load_search_parameters()?;
    if let Some(message) = expression_conflict(&stored, &[], &rows) {
        if strict {
            return Err(FhirError::Invalid(message));
        }
        return Ok((false, BTreeMap::new()));
    }
    let url = rows[0].url.clone();
    let priors: Vec<_> = stored
        .into_iter()
        .filter(|row| row.package_id == local_id && row.url == url)
        .collect();
    let removed: Vec<_> = priors
        .iter()
        .filter(|prior| !rows.iter().any(|row| row.base == prior.base))
        .cloned()
        .collect();
    for prior in &removed {
        tombstone_index(db, &prior.base, &prior.code)?;
    }
    for prior in &priors {
        if rows
            .iter()
            .any(|row| row.base == prior.base && row.code != prior.code)
        {
            tombstone_index(db, &prior.base, &prior.code)?;
        }
    }
    if !removed.is_empty() {
        store.delete_search_parameter(local_id, &url)?;
    }
    let mut jobs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut changed = !removed.is_empty();
    for row in &rows {
        let pending = priors.iter().any(|prior| {
            prior.base == row.base && prior.url == row.url && prior.status == ParamStatus::Pending
        });
        if store.upsert_search_parameter(local_id, row)? || pending {
            changed = true;
            jobs.entry(row.base.clone())
                .or_default()
                .insert(row.url.clone());
        }
    }
    Ok((changed, jobs))
}

fn delete_parameter(
    store: &dyn ConformanceStore,
    db: &Database,
    local_id: i64,
    resource: &Value,
) -> Result<bool, FhirError> {
    let Some(url) = resource.get("url").and_then(|value| value.as_str()) else {
        return Ok(false);
    };
    let stored = store.load_search_parameters()?;
    let priors: Vec<_> = stored
        .into_iter()
        .filter(|row| row.package_id == local_id && row.url == url)
        .collect();
    if priors.is_empty() {
        return Ok(false);
    }
    for prior in &priors {
        tombstone_index(db, &prior.base, &prior.code)?;
    }
    store.delete_search_parameter(local_id, url)?;
    Ok(true)
}

fn parameter_current(
    store: &dyn ConformanceStore,
    local_id: i64,
    resource: &Value,
    scope: &SearchScope,
) -> Result<bool, FhirError> {
    let rows = match parameter_rows(resource, scope) {
        Ok(rows) => rows,
        Err(_) => return Ok(false),
    };
    if rows.is_empty()
        || rows
            .iter()
            .any(|row| row.status == ParamStatus::Unsupported)
    {
        return Ok(true);
    }
    let stored = store.load_search_parameters()?;
    let local: Vec<_> = stored
        .iter()
        .filter(|row| row.package_id == local_id && row.url == rows[0].url)
        .collect();
    if local.len() != rows.len() {
        return Ok(false);
    }
    for row in &rows {
        let Some(prior) = local.iter().find(|prior| prior.base == row.base) else {
            return Ok(false);
        };
        if prior.status != ParamStatus::Ready || prior.code != row.code {
            return Ok(false);
        }
        if prior.expression.clone().unwrap_or_default()
            != row.expression.clone().unwrap_or_default()
        {
            return Ok(false);
        }
        if comparable(&prior.json) != comparable_value(resource) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn terminology_current(
    store: &dyn ConformanceStore,
    local_id: i64,
    resource: &Value,
) -> Result<bool, FhirError> {
    let Some(url) = resource.get("url").and_then(|value| value.as_str()) else {
        return Ok(true);
    };
    let Some(key) = row_key(resource) else {
        return Ok(true);
    };
    let stored = store.load_terminology_for(&[url.to_string()])?;
    let Some(row) = stored
        .iter()
        .find(|row| row.package_id == local_id && row.url == key)
    else {
        return Ok(false);
    };
    if comparable(&row.json) != comparable_value(resource) {
        return Ok(false);
    }
    if resource
        .get("resourceType")
        .and_then(|value| value.as_str())
        == Some("ValueSet")
    {
        let expansions = store.load_expansions()?;
        return Ok(expansions.iter().any(|row| {
            row.package_id == local_id && canonical_url(&row.url) == canonical_url(url)
        }));
    }
    Ok(true)
}

fn delete_orphan_terminology(
    store: &dyn ConformanceStore,
    local_id: i64,
    type_name: &str,
    live: &BTreeMap<String, String>,
) -> Result<bool, FhirError> {
    let stored = store.load_terminology_of(local_id)?;
    let mut dirty = false;
    for row in stored {
        if row.package_id != local_id || row.resource_type != type_name {
            continue;
        }
        let Ok(json) = serde_json::from_str::<Value>(&row.json) else {
            continue;
        };
        let Some(id) = json.get("id").and_then(|value| value.as_str()) else {
            continue;
        };
        if live.get(id) == Some(&row.url) {
            continue;
        }
        store.delete_terminology(local_id, &row.url)?;
        dirty = true;
    }
    Ok(dirty)
}

fn delete_orphan_parameters(
    store: &dyn ConformanceStore,
    db: &Database,
    local_id: i64,
    live: &[String],
) -> Result<bool, FhirError> {
    let live: BTreeSet<_> = live.iter().cloned().collect();
    let stored = store.load_search_parameters_of(local_id)?;
    let mut seen = BTreeSet::new();
    let mut dirty = false;
    for row in &stored {
        if row.package_id != local_id || !seen.insert(row.url.clone()) {
            continue;
        }
        let Ok(json) = serde_json::from_str::<Value>(&row.json) else {
            continue;
        };
        let Some(id) = json.get("id").and_then(|value| value.as_str()) else {
            continue;
        };
        if live.contains(id) {
            continue;
        }
        for item in stored
            .iter()
            .filter(|item| item.package_id == local_id && item.url == row.url)
        {
            tombstone_index(db, &item.base, &item.code)?;
        }
        store.delete_search_parameter(local_id, &row.url)?;
        dirty = true;
    }
    Ok(dirty)
}

/// The canonical URLs of the terminology resources in `resources`.
fn terminology_urls(resources: &[Value]) -> Vec<String> {
    let mut urls: Vec<String> = resources
        .iter()
        .filter(|resource| {
            matches!(
                resource
                    .get("resourceType")
                    .and_then(|value| value.as_str()),
                Some("ValueSet" | "CodeSystem" | "ConceptMap")
            )
        })
        .filter_map(|resource| resource.get("url").and_then(|value| value.as_str()))
        .map(|url| canonical_url(url).to_string())
        .collect();
    urls.sort();
    urls.dedup();
    urls
}

/// Bring the catalog and the stored expansions up to date with terminology that was written or
/// removed, recomputing only the expansions the change can alter: the value sets written, and
/// those that include them, or a code system written or removed, directly or through others.
/// What this does not touch is not read, not expanded and not rewritten.
///
/// The change is applied to the live catalog in place under its write lock, together with the
/// stored expansions, so two writers cannot each rebuild from what the other has not applied.
fn apply_terminology(
    store: &dyn ConformanceStore,
    catalog: &std::sync::RwLock<crate::catalog::Catalog>,
    local_id: i64,
    written: &[String],
    removed: &[(String, String)],
) -> Result<(), FhirError> {
    let mut upserted = Vec::new();
    for row in store.load_terminology_for(written)? {
        // The lookup also matches other versions of the same system; only what was written
        // is applied.
        if row.package_id != local_id || !written.contains(&row.url) {
            continue;
        }
        upserted.push(
            serde_json::from_str::<Value>(&row.json)
                .map_err(|err| FhirError::Invalid(format!("terminology json: {err}")))?,
        );
    }
    let change = TerminologyChange {
        upserted,
        removed: removed.to_vec(),
    };
    let urls = change.urls();
    // Stored rows of the value sets that depend on the change, by canonical URL, with the package
    // of each. A URL can be held by more than one package, and the package whose definition is in
    // use (a rebuild expands that one) is the one that holds the stored expansion: it is the row a
    // start reads.
    let release = catalog
        .read()
        .expect("catalog")
        .terms()
        .release()
        .map(str::to_string);
    let mut dependents: BTreeMap<String, Vec<(i64, Value)>> = BTreeMap::new();
    let mut owners: HashMap<String, i64> = HashMap::new();
    let mut holders: HashMap<String, Vec<i64>> = HashMap::new();
    for url in &urls {
        owners.insert(url.clone(), local_id);
    }
    loop {
        let wanted = catalog
            .read()
            .expect("catalog")
            .terms()
            .dependents_of(&urls);
        let missing: Vec<String> = wanted
            .iter()
            .filter(|url| !dependents.contains_key(*url))
            .cloned()
            .collect();
        if !missing.is_empty() {
            for row in store.load_terminology_for(&missing)? {
                if row.resource_type != "ValueSet" {
                    continue;
                }
                let url = canonical_url(&row.url).to_string();
                dependents.entry(url).or_default().push((
                    row.package_id,
                    serde_json::from_str::<Value>(&row.json)
                        .map_err(|err| FhirError::Invalid(format!("terminology json: {err}")))?,
                ));
            }
            // The catalog says these depend on the change, so their rows must exist. If one
            // does not, the catalog and the store disagree, and only a full rebuild settles it.
            if let Some(absent) = missing.iter().find(|url| !dependents.contains_key(*url)) {
                return Err(FhirError::Storage(format!(
                    "value set {absent} is in the catalog but not in the store"
                )));
            }
            let rows: Vec<(i64, &Value)> = missing
                .iter()
                .flat_map(|url| dependents[url].iter())
                .map(|(package, row)| (*package, row))
                .collect();
            owners.extend(winning_scopes(&rows, release.as_deref()));
            for url in &missing {
                holders.insert(
                    url.clone(),
                    dependents[url]
                        .iter()
                        .map(|(package, _)| *package)
                        .collect(),
                );
            }
        }
        let mut guard = catalog.write().expect("catalog");
        // Another writer may have changed what depends on this since it was read.
        let wanted = guard.terms().dependents_of(&urls);
        if wanted.iter().any(|url| !dependents.contains_key(url)) {
            continue;
        }
        let loaded: Vec<(i64, &Value)> = wanted
            .iter()
            .flat_map(|url| dependents.get(url).into_iter().flatten())
            .map(|(package, row)| (*package, row))
            .collect();
        let updates = guard.terms_mut().apply_scoped(&change, local_id, &loaded);
        let mut changes: Vec<ExpansionChange> = Vec::new();
        for update in updates {
            let owner = owners.get(&update.url).copied().unwrap_or(local_id);
            match update.document {
                Some(document) => changes.push(ExpansionChange {
                    package_id: owner,
                    url: update.url,
                    json: Some(document.to_string()),
                }),
                // Nothing answers for it now, so no package may keep an expansion that a start
                // would fall back to.
                None => {
                    let mut stored_in = holders.get(&update.url).cloned().unwrap_or_default();
                    if !stored_in.contains(&owner) {
                        stored_in.push(owner);
                    }
                    for package_id in stored_in {
                        changes.push(ExpansionChange {
                            package_id,
                            url: update.url.clone(),
                            json: None,
                        });
                    }
                }
            }
        }
        store.apply_expansions(&changes)?;
        return Ok(());
    }
}

fn sync_expansions(store: &dyn ConformanceStore) -> Result<(), FhirError> {
    let terminology = store.load_terminology()?;
    let existing = store.load_expansions()?;
    // The release the first package was written for, as the catalog takes it. Without it an
    // unversioned include would read the highest version of a code system, and a package that
    // carries another release's copy (`hl7.fhir.uv.xver-r5.r4` has `5.0.0` of R4's
    // `encounter-status`) would change what R4's own value sets mean.
    let release = store
        .load_packages()?
        .first()
        .map(|package| package.fhir_version.clone());
    let mut resources = Vec::new();
    let mut packages = Vec::new();
    let mut by_package: BTreeMap<i64, Vec<NewExpansion>> = BTreeMap::new();
    for row in &terminology {
        by_package.entry(row.package_id).or_default();
        let value: Value = serde_json::from_str(&row.json)
            .map_err(|err| FhirError::Invalid(format!("terminology json: {err}")))?;
        resources.push(value);
        packages.push(row.package_id);
    }
    for row in &existing {
        by_package.entry(row.package_id).or_default();
    }
    // Every package's resources together, but each value set read with the code systems and
    // value sets of its own package first, which is how installing the package expanded it. A
    // rebuild that read one pool would give a value set another package's copy of a code system.
    let scoped: Vec<(i64, &Value)> = packages.iter().copied().zip(&resources).collect();
    // The package whose definition of a value set is in use holds its stored expansion, because
    // that is the row a start reads for it.
    let owners = winning_scopes(&scoped, release.as_deref());
    for document in MemoryTerminology::expansion_documents_scoped(&scoped, release.as_deref()) {
        let Some(url) = document.get("url").and_then(|value| value.as_str()) else {
            continue;
        };
        let Some(package_id) = owners.get(canonical_url(url)).copied() else {
            continue;
        };
        by_package
            .entry(package_id)
            .or_default()
            .push(NewExpansion {
                url: canonical_url(url).to_string(),
                json: document.to_string(),
            });
    }
    for (package_id, expansions) in &by_package {
        store.replace_expansions(*package_id, expansions)?;
    }
    Ok(())
}

fn tombstone_index(db: &Database, base: &str, code: &str) -> Result<(), FhirError> {
    for _ in 0..=MAX_OCC_FAILURES {
        match tombstone_once(db, base, code) {
            Ok(()) => return Ok(()),
            Err(CommitError::Occ(_)) => continue,
            Err(err) => return Err(FhirError::Storage(err.to_string())),
        }
    }
    Err(FhirError::Conflict("index tombstone conflicted".into()))
}

fn tombstone_once(db: &Database, base: &str, code: &str) -> Result<(), CommitError> {
    let index = IndexId::new(format!("{base}/{code}"));
    let rows = db
        .reader()
        .index_scan(&index, db.latest_ts(), &Interval::all(), Order::Asc)
        .map_err(|err| CommitError::Persist(err.to_string()))?;
    let mut by_id: HashMap<String, Vec<Vec<u8>>> = HashMap::new();
    for row in rows {
        let Some(id) = row.document.id.id_str() else {
            continue;
        };
        by_id.entry(id.to_string()).or_default().push(row.key);
    }
    if by_id.is_empty() {
        return Ok(());
    }
    let tablet = TabletId::new(base);
    let entries: Vec<_> = by_id.into_iter().collect();
    for chunk in entries.chunks(16) {
        let mut tx = db.begin();
        for (id, keys) in chunk {
            let old_keys = keys
                .iter()
                .map(|key| (index.clone(), key.clone()))
                .collect();
            tx.stage_index(IndexPut {
                tablet: tablet.clone(),
                id: id.clone(),
                keys: Vec::new(),
                old_keys,
            })
            .map_err(|err| CommitError::Persist(err.to_string()))?;
        }
        db.commit(tx)?;
    }
    Ok(())
}

fn check_parameter(
    stored: &[StoredSearchParameter],
    seen: &[NewSearchParameter],
    resource: &Value,
    scope: &SearchScope,
) -> Result<Vec<NewSearchParameter>, FhirError> {
    let rows = parameter_rows(resource, scope)?;
    if rows.is_empty() {
        return Err(FhirError::Invalid(
            "SearchParameter base is required".into(),
        ));
    }
    if rows
        .iter()
        .any(|row| row.status == ParamStatus::Unsupported)
    {
        return Err(FhirError::Invalid(format!(
            "search parameter {} expression is not supported",
            rows[0].code
        )));
    }
    if let Some(message) = expression_conflict(stored, seen, &rows) {
        return Err(FhirError::Invalid(message));
    }
    Ok(rows)
}

fn expression_conflict(
    stored: &[StoredSearchParameter],
    seen: &[NewSearchParameter],
    rows: &[NewSearchParameter],
) -> Option<String> {
    for row in rows {
        let incoming = row.expression.clone().unwrap_or_default();
        for other in stored {
            if other.base == row.base && other.code == row.code && other.url != row.url {
                let existing = other.expression.clone().unwrap_or_default();
                if existing != incoming {
                    return Some(format!(
                        "search parameter {}.{} expression differs",
                        row.base, row.code
                    ));
                }
            }
        }
        for other in seen {
            if other.base == row.base && other.code == row.code && other.url != row.url {
                let existing = other.expression.clone().unwrap_or_default();
                if existing != incoming {
                    return Some(format!(
                        "search parameter {}.{} expression differs",
                        row.base, row.code
                    ));
                }
            }
        }
    }
    None
}

fn check_terminology(
    stored: &[StoredTerminology],
    local_id: i64,
    resource: &Value,
) -> Result<(), FhirError> {
    let url = resource
        .get("url")
        .and_then(|value| value.as_str())
        .ok_or_else(|| FhirError::Invalid("url is required".into()))?;
    if stored
        .iter()
        .any(|row| row.package_id != local_id && canonical_url(&row.url) == canonical_url(url))
    {
        return Err(FhirError::Invalid(format!(
            "canonical {url} belongs to an installed package"
        )));
    }
    Ok(())
}

fn terminology_owned_elsewhere(
    store: &dyn ConformanceStore,
    local_id: i64,
    url: &str,
) -> Result<bool, FhirError> {
    let stored = store.load_terminology_for(&[canonical_url(url).to_string()])?;
    Ok(stored
        .iter()
        .any(|row| row.package_id != local_id && canonical_url(&row.url) == canonical_url(url)))
}

fn candidate_resources(path: &str, args: &Value) -> Vec<Value> {
    if path == "Bundle/transaction" {
        let bundle = args
            .get("resource")
            .cloned()
            .unwrap_or_else(|| args.clone());
        let Some(entries) = bundle.get("entry").and_then(|value| value.as_array()) else {
            return Vec::new();
        };
        return entries
            .iter()
            .filter_map(|entry| entry.get("resource").cloned())
            .filter(|resource| {
                matches!(
                    resource
                        .get("resourceType")
                        .and_then(|value| value.as_str()),
                    Some("SearchParameter" | "ValueSet" | "CodeSystem" | "ConceptMap")
                )
            })
            .collect();
    }
    let Some((type_name, op)) = path.split_once('/') else {
        return Vec::new();
    };
    if !matches!(op, "create" | "update") || !CONFORMANCE.contains(&type_name) {
        return Vec::new();
    }
    args.get("resource").cloned().into_iter().collect()
}

fn write_resource(
    db: &Database,
    write: &DocumentWrite,
    ts: Timestamp,
) -> Result<Option<Value>, FhirError> {
    let text = if let Some(json) = &write.json {
        json.clone()
    } else {
        let Some(previous) = previous_body(db, &write.tablet, &write.id, ts)? else {
            return Ok(None);
        };
        previous
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| FhirError::Invalid(err.to_string()))
}

fn previous_body(
    db: &Database,
    tablet: &str,
    id: &str,
    at: Timestamp,
) -> Result<Option<String>, FhirError> {
    let document = DocumentId::new(TabletId::new(tablet), id.as_bytes());
    let previous = db.reader().previous_revision(&document, at)?;
    Ok(previous.and_then(|entry| entry.value))
}

fn live_json(db: &Database, type_name: &str, id: &str) -> Result<Option<Value>, FhirError> {
    let document = DocumentId::new(TabletId::new(type_name), id.as_bytes());
    let Some(entry) = db.reader().latest_document(&document, db.latest_ts())? else {
        return Ok(None);
    };
    let Some(text) = entry.value else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| FhirError::Invalid(err.to_string()))
}

fn local_package(
    store: &dyn ConformanceStore,
    catalog: &std::sync::RwLock<crate::catalog::Catalog>,
) -> Result<i64, FhirError> {
    let version = catalog.read().expect("catalog").fhir_version().to_string();
    store
        .ensure_local_package(&version)
        .map_err(FhirError::from)
}

fn merge_jobs(
    into: &mut BTreeMap<String, BTreeSet<String>>,
    more: BTreeMap<String, BTreeSet<String>>,
) {
    for (base, urls) in more {
        into.entry(base).or_default().extend(urls);
    }
}

fn canonical_url(url: &str) -> &str {
    url.split('|').next().unwrap_or(url)
}

fn comparable(text: &str) -> Value {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Value::String(text.to_string());
    };
    comparable_value(&value)
}

fn comparable_value(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        if let Some(meta) = object.get_mut("meta").and_then(|meta| meta.as_object_mut()) {
            meta.remove("versionId");
            if meta.is_empty() {
                object.remove("meta");
            }
        }
    }
    value
}
