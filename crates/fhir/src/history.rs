use haifhir_database::{Revision, Transaction};
use haifhir_types::Timestamp;
use serde_json::{json, Value};

use crate::catalog::ResourceModel;
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
