//! Run a named operation with OCC retry.
//!
//! Convex `UDF_EXECUTOR_OCC_MAX_RETRIES` in `crates/common/src/knobs.rs` defaults
//! to 4. Handlers only touch `Transaction`, so retrying them is safe.

use std::collections::BTreeMap;

use refhir_database::{CommitError, Database, DocumentWrite, ReadSet};
use refhir_types::Timestamp;
use serde_json::Value;

use crate::bundle;
use crate::catalog::Catalog;
use crate::error::FhirError;
use crate::history;
use crate::ops;
use crate::search;

/// Retries after an OCC conflict. The first attempt is not counted.
pub const MAX_OCC_FAILURES: usize = 4;

#[derive(Debug)]
pub enum RunError {
    Fhir(FhirError),
    Commit(CommitError),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Fhir(err) => write!(f, "{err}"),
            RunError::Commit(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for RunError {}

#[derive(Debug)]
pub struct RunResult {
    pub value: Value,
    pub ts: Timestamp,
    pub reads: ReadSet,
    pub wrote: bool,
    /// Document creates, updates, and deletes from the committed transaction.
    pub writes: Vec<DocumentWrite>,
}

pub fn run(
    db: &Database,
    catalog: &Catalog,
    path: &str,
    args: &Value,
) -> Result<RunResult, RunError> {
    for attempt in 0..=MAX_OCC_FAILURES {
        let mut tx = db.begin();
        let mut value = match dispatch(&mut tx, catalog, path, args) {
            Ok(value) => value,
            Err(err) => return Err(RunError::Fhir(err)),
        };
        let reads = tx.read_set().clone();
        let written = tx.written_ids();
        let wrote = tx.has_writes();
        let writes = tx.document_writes();
        match db.commit_with(tx, stamp_stored_json) {
            Ok(ts) => {
                if wrote {
                    bundle::finalize(&mut value, &written, ts);
                }
                return Ok(RunResult {
                    value,
                    ts,
                    reads,
                    wrote,
                    writes,
                });
            }
            Err(CommitError::Occ(_)) if wrote && attempt < MAX_OCC_FAILURES => continue,
            Err(err) => return Err(RunError::Commit(err)),
        }
    }
    unreachable!("OCC retry loop returns on every attempt");
}

pub fn stamp_stored_json(json: &mut String, ts: Timestamp) {
    let Ok(mut value) = serde_json::from_str::<Value>(json) else {
        return;
    };
    if value.get("resourceType").is_none() {
        return;
    }
    ops::set_version(&mut value, ts);
    if let Ok(text) = serde_json::to_string(&value) {
        *json = text;
    }
}

pub fn dispatch(
    tx: &mut refhir_database::Transaction,
    catalog: &Catalog,
    path: &str,
    args: &Value,
) -> Result<Value, FhirError> {
    if path == "Bundle/transaction" {
        let bundle = args
            .get("resource")
            .cloned()
            .unwrap_or_else(|| args.clone());
        return bundle::transaction(tx, catalog, &bundle);
    }
    let (type_name, op) = path
        .split_once('/')
        .ok_or_else(|| FhirError::Invalid(format!("bad operation {path}")))?;
    if matches!(op, "$expand" | "$validate-code" | "$lookup" | "$translate") {
        return crate::terms::operation(catalog, type_name, op, args);
    }
    let model = catalog.get(type_name)?;
    match op {
        "create" => ops::create(tx, catalog, model, resource_arg(args)?),
        "read" => ops::read(tx, model, &id_arg(args)?),
        "update" => ops::update(
            tx,
            catalog,
            model,
            &id_arg(args)?,
            resource_arg(args)?,
            args.get("ifMatch").and_then(|value| value.as_str()),
        ),
        "delete" => ops::delete(tx, catalog, model, &id_arg(args)?),
        "vread" => {
            let version = args
                .get("versionId")
                .and_then(|value| value.as_str())
                .ok_or_else(|| FhirError::Invalid("versionId is required".into()))?;
            history::vread(tx, model, &id_arg(args)?, version)
        }
        "history" => history::history(tx, model, &id_arg(args)?),
        "search" => {
            let params = search_args(args)?;
            search::search(tx, catalog, model, &params)
        }
        "$validate" => Ok(ops::validation_report(tx, catalog, &resource_arg(args)?)),
        other => Err(FhirError::Unsupported(format!(
            "unsupported operation {type_name}/{other}"
        ))),
    }
}

fn resource_arg(args: &Value) -> Result<Value, FhirError> {
    args.get("resource")
        .cloned()
        .ok_or_else(|| FhirError::Invalid("resource is required".into()))
}

fn id_arg(args: &Value) -> Result<String, FhirError> {
    args.get("id")
        .and_then(|value| value.as_str())
        .map(|id| id.to_string())
        .ok_or_else(|| FhirError::Invalid("id is required".into()))
}

fn search_args(args: &Value) -> Result<BTreeMap<String, String>, FhirError> {
    let object = args
        .as_object()
        .ok_or_else(|| FhirError::Invalid("search args must be an object".into()))?;
    let mut params = BTreeMap::new();
    for (key, value) in object {
        let Some(text) = value.as_str() else {
            return Err(FhirError::Invalid(format!(
                "search parameter {key} must be a string"
            )));
        };
        params.insert(key.clone(), text.to_string());
    }
    Ok(params)
}
