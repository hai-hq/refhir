use refhir_database::{Revision, Transaction};
use refhir_types::{Interval, Timestamp};
use serde_json::{json, Value};

use crate::catalog::{Catalog, ResourceModel};
use crate::error::FhirError;
use crate::ops::decorate;

pub fn vread(
    tx: &Transaction,
    model: &ResourceModel,
    id: &str,
    version_id: &str,
) -> Result<Value, FhirError> {
    let ts = Timestamp::from_version_id(version_id)
        .map_err(|_| FhirError::Invalid(format!("invalid versionId {version_id}")))?;
    match tx.exact_revision(&model.tablet(), id, ts)? {
        None => Err(FhirError::NotFound(format!(
            "{}/{} version {version_id} not found",
            model.name, id
        ))),
        Some(rev) if rev.deleted => Err(FhirError::Gone(format!(
            "{}/{id} version {version_id} was deleted",
            model.name
        ))),
        Some(rev) => {
            let json = rev.json.unwrap_or(Value::Null);
            Ok(decorate(json, id, Some(rev.ts)))
        }
    }
}

pub fn history(tx: &mut Transaction, model: &ResourceModel, id: &str) -> Result<Value, FhirError> {
    let revisions = tx.history(&model.tablet(), id, &model.by_id())?;
    if revisions.is_empty() {
        return Err(FhirError::NotFound(format!(
            "{}/{} not found",
            model.name, id
        )));
    }
    let entries: Vec<Value> = revisions
        .into_iter()
        .map(|rev| history_entry(model, id, rev))
        .collect();
    Ok(json!({
        "resourceType": "Bundle",
        "type": "history",
        "total": entries.len(),
        "entry": entries
    }))
}

pub fn type_history(
    tx: &mut Transaction,
    model: &ResourceModel,
    since: Option<Timestamp>,
    count: Option<usize>,
    offset: usize,
) -> Result<Value, FhirError> {
    tx.record_span(model.by_id(), Interval::all());
    log_bundle(tx, Some(model), since, count, offset, None)
}

pub fn system_history(
    tx: &mut Transaction,
    catalog: &Catalog,
    since: Option<Timestamp>,
    count: Option<usize>,
    offset: usize,
) -> Result<Value, FhirError> {
    for model in catalog.resource_models() {
        tx.record_span(model.by_id(), Interval::all());
    }
    let hide = catalog.sdc_archiver().then_some("QuestionnaireResponse");
    log_bundle(tx, None, since, count, offset, hide)
}

fn log_bundle(
    tx: &Transaction,
    model: Option<&ResourceModel>,
    since: Option<Timestamp>,
    count: Option<usize>,
    offset: usize,
    hide: Option<&str>,
) -> Result<Value, FhirError> {
    let start = since.unwrap_or(Timestamp::ZERO);
    let rows = tx.log_since(start)?;
    let mut entries = Vec::new();
    for row in rows {
        let type_name = row.id.tablet.as_str();
        if model.is_some_and(|item| item.name != type_name) || hide == Some(type_name) {
            continue;
        }
        let Some(id) = row.id.id_str() else {
            continue;
        };
        let deleted = row.value.is_none();
        let rev = Revision {
            ts: row.ts,
            prev_ts: row.prev_ts,
            deleted,
            json: row
                .value
                .as_deref()
                .and_then(|text| serde_json::from_str(text).ok()),
        };
        let placeholder = ResourceModel {
            name: type_name.to_string(),
            references: Vec::new(),
            params: Vec::new(),
        };
        let model = model.unwrap_or(&placeholder);
        entries.push(history_entry(model, id, rev));
    }
    let total = entries.len();
    let page: Vec<_> = entries
        .into_iter()
        .skip(offset)
        .take(count.unwrap_or(usize::MAX))
        .collect();
    Ok(json!({
        "resourceType": "Bundle",
        "type": "history",
        "total": total,
        "entry": page
    }))
}

fn history_entry(model: &ResourceModel, id: &str, rev: Revision) -> Value {
    let (method, status) = if rev.deleted {
        ("DELETE", "204")
    } else if rev.prev_ts.is_none() {
        ("POST", "201")
    } else {
        ("PUT", "200")
    };
    let mut entry = json!({
        "request": {"method": method, "url": format!("{}/{id}", model.name)},
        "response": {
            "status": status,
            "etag": format!("W/\"{}\"", rev.ts),
            "location": format!("{}/{id}/_history/{}", model.name, rev.ts)
        }
    });
    if let Some(json) = rev.json {
        entry["resource"] = decorate(json, id, Some(rev.ts));
    }
    entry
}
