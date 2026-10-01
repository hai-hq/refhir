use std::collections::BTreeMap;

use haifhir_database::Transaction;
use haifhir_types::{exact_key, string_interval, token_interval, IndexId, Interval};
use serde_json::{json, Value};

use crate::catalog::{Catalog, ResourceModel};
use crate::error::FhirError;
use crate::extract::SearchKind;
use crate::ops::decorate;
use crate::resolve::TxHost;

const IGNORED: &[&str] = &["_format", "_pretty"];

pub fn search(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    params: &BTreeMap<String, String>,
) -> Result<Value, FhirError> {
    for name in params.keys() {
        if IGNORED.contains(&name.as_str()) || name == "_id" {
            continue;
        }
        if model.ready_param(name).is_none() {
            return Err(FhirError::Invalid(format!(
                "unsupported search parameter {name} on {}",
                model.name
            )));
        }
    }
    let (index, interval) = choose_index(model, params);
    let rows = tx.index_range(&index, interval)?;
    let host = TxHost::new(tx, catalog);
    let mut entries = Vec::new();
    for row in rows {
        if !matches_params(catalog, model, &row.json, params, &host) {
            continue;
        }
        let id = row.id.id_str().unwrap_or("").to_string();
        entries.push(json!({
            "resource": decorate(row.json, &id, row.ts)
        }));
    }
    Ok(json!({
        "resourceType": "Bundle",
        "type": "searchset",
        "total": entries.len(),
        "entry": entries
    }))
}

fn choose_index(model: &ResourceModel, params: &BTreeMap<String, String>) -> (IndexId, Interval) {
    if let Some(id) = params.get("_id") {
        return (model.by_id(), Interval::point(exact_key(id)));
    }
    let mut chosen: Option<(u8, &str, &str, SearchKind)> = None;
    for (name, value) in params {
        if IGNORED.contains(&name.as_str()) {
            continue;
        }
        let Some(param) = model.ready_param(name) else {
            continue;
        };
        let rank = match param.kind {
            SearchKind::Token | SearchKind::Reference => 0,
            SearchKind::String | SearchKind::Date => 1,
        };
        let replace = chosen.map(|(prev, _, _, _)| rank < prev).unwrap_or(true);
        if replace {
            chosen = Some((rank, name, value, param.kind));
        }
    }
    if let Some((_, name, value, kind)) = chosen {
        let interval = match kind {
            SearchKind::Token | SearchKind::Reference => token_interval(value),
            SearchKind::String | SearchKind::Date => string_interval(value),
        };
        return (model.search_index(name), interval);
    }
    (model.by_id(), Interval::all())
}

fn matches_params(
    catalog: &Catalog,
    model: &ResourceModel,
    resource: &Value,
    params: &BTreeMap<String, String>,
    host: &TxHost<'_>,
) -> bool {
    params.iter().all(|(name, value)| {
        if IGNORED.contains(&name.as_str()) {
            return true;
        }
        if name == "_id" {
            return resource.get("id").and_then(|item| item.as_str()) == Some(value);
        }
        model
            .ready_param(name)
            .map(|param| param.extractor.matches(resource, value, catalog.path_model(), host))
            .unwrap_or(false)
    })
}
