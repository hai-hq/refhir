//! Load a FHIR package directory into the conformance tables, then backfill
//! search indexes for parameters that were not indexed yet.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use refhir_database::{CommitError, Database, IndexPut};
use refhir_persistence::{
    ConformanceStore, InstallError, NewPackage, NewSearchParameter, NewStructureDefinition,
    ParamStatus,
};
use refhir_types::{IndexId, Interval, Order, Timestamp};
use serde_json::Value;

use crate::catalog::{pending_index_keys, Catalog};
use crate::error::FhirError;
use crate::extract;
use crate::run::MAX_OCC_FAILURES;

const BATCH: usize = 16;

pub fn minir4_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/minir4"))
}

pub fn r4_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/r4"))
}

pub fn extra_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/extra"))
}

pub fn version_clash_dir() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/version-clash"
    ))
}

pub fn expression_clash_dir() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/expression-clash"
    ))
}

pub fn install_package_dir(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    path: &Path,
) -> Result<(), FhirError> {
    stage_package_dir(store, db, catalog, path)?;
    run_backfills(store, db, catalog)
}

pub fn stage_package_dir(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    path: &Path,
) -> Result<(), FhirError> {
    let (name, version) = package_identity(path)?;
    let installed = store.load_packages().map_err(FhirError::from)?;
    if let Some(package) = installed
        .iter()
        .find(|package| package.name == name && package.version == version)
    {
        reopen_unsupported(store, db, package.id)?;
        return publish(store, catalog);
    }
    let package = read_package(path)?;
    let outcome = store.install_package(&package).map_err(install_error)?;
    publish(store, catalog)?;
    if outcome.inserted {
        let watermark = db
            .latest_ts()
            .to_i64()
            .map_err(|_| FhirError::Storage("watermark timestamp does not fit".into()))?;
        let mut bases = BTreeSet::new();
        for param in &outcome.newly_pending {
            bases.insert(param.base.clone());
        }
        for base in bases {
            store
                .insert_backfill(outcome.package_id, watermark, &base)
                .map_err(FhirError::from)?;
        }
    }
    Ok(())
}

pub fn run_backfills(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
) -> Result<(), FhirError> {
    let jobs = store.load_backfills().map_err(FhirError::from)?;
    let mut refreshed = false;
    for job in jobs.into_iter().filter(|job| !job.done) {
        let watermark = Timestamp::from_i64(job.watermark_ts)
            .map_err(|_| FhirError::Storage("bad backfill watermark".into()))?;
        let mut ids = live_ids(db, &job.tablet)?;
        if let Some(cursor) = &job.cursor_id {
            ids.retain(|id| id.as_str() > cursor.as_str());
        }
        let mut start = 0;
        while start < ids.len() {
            let end = (start + BATCH).min(ids.len());
            let batch = &ids[start..end];
            match backfill_batch(db, catalog, &job.tablet, batch, watermark) {
                Ok(()) => {}
                Err(CommitError::Occ(_)) => {
                    for id in batch {
                        backfill_id(db, catalog, &job.tablet, id, watermark)?;
                    }
                }
                Err(err) => return Err(FhirError::Storage(err.to_string())),
            }
            store
                .update_backfill(job.id, Some(batch[batch.len() - 1].as_str()), false)
                .map_err(FhirError::from)?;
            start = end;
        }
        store
            .mark_base_ready(job.package_id, &job.tablet)
            .map_err(FhirError::from)?;
        let cursor = ids.last().map(String::as_str).or(job.cursor_id.as_deref());
        store
            .update_backfill(job.id, cursor, true)
            .map_err(FhirError::from)?;
        refreshed = true;
    }
    if refreshed {
        publish(store, catalog)?;
    }
    Ok(())
}

fn publish(store: &dyn ConformanceStore, catalog: &RwLock<Catalog>) -> Result<(), FhirError> {
    let loaded = Catalog::open(store)?;
    *catalog.write().expect("catalog") = loaded;
    Ok(())
}

fn live_ids(db: &Database, type_name: &str) -> Result<Vec<String>, FhirError> {
    let index = IndexId::new(format!("{type_name}/by_id"));
    let rows = db
        .reader()
        .index_scan(&index, db.latest_ts(), &Interval::all(), Order::Asc)?;
    let mut ids = Vec::new();
    for row in rows {
        if let Some(id) = row.document.id.id_str() {
            ids.push(id.to_string());
        }
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

fn backfill_batch(
    db: &Database,
    catalog: &RwLock<Catalog>,
    type_name: &str,
    ids: &[String],
    watermark: Timestamp,
) -> Result<(), CommitError> {
    let mut tx = db.begin();
    let guard = catalog.read().expect("catalog");
    let model = guard
        .get(type_name)
        .map_err(|err| CommitError::Invalid(err.to_string()))?;
    let mut staged = false;
    for id in ids {
        let Some(stored) = tx
            .get(&model.tablet(), id, &model.by_id())
            .map_err(|err| CommitError::Invalid(err.to_string()))?
        else {
            continue;
        };
        let Some(ts) = stored.ts else {
            continue;
        };
        if ts > watermark {
            continue;
        }
        let keys = {
            let host = crate::resolve::TxHost::new(&mut tx, &guard);
            pending_index_keys(guard.path_model(), model, &stored.json, &host).unwrap_or_default()
        };
        if keys.is_empty() {
            continue;
        }
        tx.stage_index(IndexPut {
            tablet: model.tablet(),
            id: id.clone(),
            keys,
        })
        .map_err(|err| CommitError::Invalid(err.to_string()))?;
        staged = true;
    }
    drop(guard);
    if staged {
        db.commit(tx)?;
    }
    Ok(())
}

fn backfill_id(
    db: &Database,
    catalog: &RwLock<Catalog>,
    type_name: &str,
    id: &str,
    watermark: Timestamp,
) -> Result<(), FhirError> {
    for attempt in 0..=MAX_OCC_FAILURES {
        match backfill_batch(db, catalog, type_name, &[id.to_string()], watermark) {
            Ok(()) => return Ok(()),
            Err(CommitError::Occ(_)) if attempt < MAX_OCC_FAILURES => continue,
            Err(CommitError::Occ(err)) => return Err(FhirError::Conflict(format!("{err:?}"))),
            Err(err) => return Err(FhirError::Storage(err.to_string())),
        }
    }
    Err(FhirError::Conflict("backfill OCC retries exhausted".into()))
}

fn read_manifest(root: &Path) -> Result<Value, FhirError> {
    let manifest_text = fs::read_to_string(root.join("package.json"))
        .map_err(|err| FhirError::Invalid(format!("package.json: {err}")))?;
    serde_json::from_str(&manifest_text)
        .map_err(|err| FhirError::Invalid(format!("package.json: {err}")))
}

fn package_identity(path: &Path) -> Result<(String, String), FhirError> {
    let root = package_root(path)?;
    let manifest = read_manifest(&root)?;
    let name = manifest
        .get("name")
        .and_then(|value| value.as_str())
        .ok_or_else(|| FhirError::Invalid("package.json name is required".into()))?
        .to_string();
    let version = manifest
        .get("version")
        .and_then(|value| value.as_str())
        .ok_or_else(|| FhirError::Invalid("package.json version is required".into()))?
        .to_string();
    Ok((name, version))
}

fn read_package(path: &Path) -> Result<NewPackage, FhirError> {
    let root = package_root(path)?;
    let manifest = read_manifest(&root)?;
    let name = manifest
        .get("name")
        .and_then(|value| value.as_str())
        .ok_or_else(|| FhirError::Invalid("package.json name is required".into()))?
        .to_string();
    let version = manifest
        .get("version")
        .and_then(|value| value.as_str())
        .ok_or_else(|| FhirError::Invalid("package.json version is required".into()))?
        .to_string();
    let mut definitions = Vec::new();
    let mut parameters = Vec::new();
    for entry in fs::read_dir(&root).map_err(|err| FhirError::Invalid(err.to_string()))? {
        let entry = entry.map_err(|err| FhirError::Invalid(err.to_string()))?;
        let file = entry.path();
        if file.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        if file.file_name().and_then(|name| name.to_str()) == Some("package.json") {
            continue;
        }
        let text = fs::read_to_string(&file)
            .map_err(|err| FhirError::Invalid(format!("{}: {err}", file.display())))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|err| FhirError::Invalid(format!("{}: {err}", file.display())))?;
        match value.get("resourceType").and_then(|item| item.as_str()) {
            Some("StructureDefinition") => definitions.push(value),
            Some("SearchParameter") => parameters.push(value),
            _ => {}
        }
    }
    let fhir_version = fhir_version(&manifest, &definitions)?;
    let mut structure_definitions = Vec::new();
    for definition in &definitions {
        structure_definitions.push(structure_row(definition)?);
    }
    let mut search_parameters = Vec::new();
    for parameter in &parameters {
        search_parameters.extend(parameter_rows(parameter)?);
    }
    Ok(NewPackage {
        name,
        version,
        fhir_version,
        structure_definitions,
        search_parameters,
    })
}

fn package_root(path: &Path) -> Result<PathBuf, FhirError> {
    let nested = path.join("package").join("package.json");
    if nested.is_file() {
        return Ok(path.join("package"));
    }
    if path.join("package.json").is_file() {
        return Ok(path.to_path_buf());
    }
    Err(FhirError::Invalid(format!(
        "no package.json in {}",
        path.display()
    )))
}

fn fhir_version(manifest: &Value, definitions: &[Value]) -> Result<String, FhirError> {
    if let Some(version) = manifest.get("fhirVersion").and_then(|value| value.as_str()) {
        return Ok(version.to_string());
    }
    if let Some(version) = manifest
        .get("fhirVersions")
        .and_then(|value| value.as_array())
        .and_then(|items| items.first())
        .and_then(|value| value.as_str())
    {
        return Ok(version.to_string());
    }
    for definition in definitions {
        if let Some(version) = definition
            .get("fhirVersion")
            .and_then(|value| value.as_str())
        {
            return Ok(version.to_string());
        }
    }
    Err(FhirError::Invalid("package fhirVersion is required".into()))
}

fn structure_row(definition: &Value) -> Result<NewStructureDefinition, FhirError> {
    let url = required_str(definition, "url")?;
    let kind = required_str(definition, "kind")?;
    let type_name = definition
        .get("type")
        .and_then(|value| value.as_str())
        .or_else(|| definition.get("name").and_then(|value| value.as_str()))
        .ok_or_else(|| FhirError::Invalid("StructureDefinition type is required".into()))?
        .to_string();
    Ok(NewStructureDefinition {
        url,
        type_name,
        kind,
        is_abstract: definition
            .get("abstract")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        json: definition.to_string(),
    })
}

fn reopen_unsupported(
    store: &dyn ConformanceStore,
    db: &Database,
    package_id: i64,
) -> Result<(), FhirError> {
    let parameters = store.load_search_parameters().map_err(FhirError::from)?;
    let mut bases = BTreeSet::new();
    for parameter in parameters
        .iter()
        .filter(|parameter| parameter.package_id == package_id && parameter.status == ParamStatus::Unsupported)
    {
        if extract::compile(&parameter.param_type, parameter.expression.as_deref()).is_none() {
            continue;
        }
        store
            .set_search_parameter_status(package_id, &parameter.url, &parameter.base, ParamStatus::Pending)
            .map_err(FhirError::from)?;
        bases.insert(parameter.base.clone());
    }
    if bases.is_empty() {
        return Ok(());
    }
    let watermark = db
        .latest_ts()
        .to_i64()
        .map_err(|_| FhirError::Storage("watermark timestamp does not fit".into()))?;
    for base in bases {
        store
            .insert_backfill(package_id, watermark, &base)
            .map_err(FhirError::from)?;
    }
    Ok(())
}

fn parameter_rows(parameter: &Value) -> Result<Vec<NewSearchParameter>, FhirError> {
    let url = required_str(parameter, "url")?;
    let code = required_str(parameter, "code")?;
    let param_type = required_str(parameter, "type")?;
    let expression = parameter
        .get("expression")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string());
    let bases = parameter
        .get("base")
        .and_then(|value| value.as_array())
        .ok_or_else(|| FhirError::Invalid(format!("SearchParameter {code} base is required")))?;
    let mut rows = Vec::new();
    for base in bases {
        let Some(base) = base.as_str() else {
            continue;
        };
        let status = if extract::compile(&param_type, expression.as_deref()).is_some() {
            ParamStatus::Pending
        } else {
            ParamStatus::Unsupported
        };
        rows.push(NewSearchParameter {
            url: url.clone(),
            base: base.to_string(),
            code: code.clone(),
            param_type: param_type.clone(),
            expression: expression.clone(),
            status,
            json: parameter.to_string(),
        });
    }
    Ok(rows)
}

fn required_str(value: &Value, field: &str) -> Result<String, FhirError> {
    value
        .get(field)
        .and_then(|item| item.as_str())
        .map(|item| item.to_string())
        .ok_or_else(|| FhirError::Invalid(format!("{field} is required")))
}

fn install_error(err: InstallError) -> FhirError {
    match err {
        InstallError::FhirVersion { existing, incoming } => FhirError::Invalid(format!(
            "package fhirVersion {incoming} does not match {existing}"
        )),
        InstallError::Expression { base, code } => FhirError::Invalid(format!(
            "search parameter {base}.{code} expression conflicts with an installed package"
        )),
        InstallError::Persist(err) => FhirError::Storage(err.to_string()),
    }
}
