//! Instance operations that read or update `meta`, plus compartment and document bundles.

use std::collections::BTreeMap;

use refhir_database::Transaction;
use serde_json::{json, Value};

use crate::catalog::{Catalog, ResourceModel};
use crate::document;
use crate::error::FhirError;
use crate::ops;
use crate::search;

pub fn get(tx: &mut Transaction, model: &ResourceModel, id: &str) -> Result<Value, FhirError> {
    let resource = ops::read(tx, model, id)?;
    let meta = resource.get("meta").cloned().unwrap_or_else(|| json!({}));
    Ok(parameters(meta))
}

pub fn add(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    id: &str,
    body: &Value,
) -> Result<Value, FhirError> {
    merge(tx, catalog, model, id, body, true)
}

pub fn delete(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    id: &str,
    body: &Value,
) -> Result<Value, FhirError> {
    merge(tx, catalog, model, id, body, false)
}

fn merge(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    id: &str,
    body: &Value,
    add: bool,
) -> Result<Value, FhirError> {
    let mut resource = ops::read(tx, model, id)?;
    let incoming = meta_of(body);
    let meta = resource
        .as_object_mut()
        .ok_or_else(|| FhirError::Invalid("resource must be an object".into()))?
        .entry("meta")
        .or_insert_with(|| json!({}));
    for name in ["tag", "security", "profile"] {
        let current = meta
            .get(name)
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        let extra = incoming
            .get(name)
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        let next = if add {
            union_meta(&current, &extra)
        } else {
            current
                .into_iter()
                .filter(|item| !extra.iter().any(|other| meta_same(item, other)))
                .collect()
        };
        if let Some(object) = meta.as_object_mut() {
            if next.is_empty() {
                object.remove(name);
            } else {
                object.insert(name.to_string(), Value::Array(next));
            }
        }
    }
    let updated = ops::update(tx, catalog, model, id, resource, None)?;
    Ok(parameters(
        updated.get("meta").cloned().unwrap_or_else(|| json!({})),
    ))
}

pub fn everything(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    id: &str,
    args: &Value,
) -> Result<Value, FhirError> {
    catalog.deny_response_read(&model.name)?;
    let may_read = document::may_read(args);
    let focus = ops::read(tx, model, id)?;
    let mut entries = vec![json!({"resource": focus, "search": {"mode": "match"}})];
    let Some(compartment) = catalog.compartment(&model.name) else {
        return Ok(bundle(entries));
    };
    for link in &compartment.resources {
        let Ok(other) = catalog.get(&link.code) else {
            continue;
        };
        if link.params.is_empty()
            || catalog.deny_response_read(&link.code).is_err()
            || !may_read(&link.code)
        {
            continue;
        }
        let mut params = BTreeMap::new();
        params.insert(link.params[0].clone(), format!("{}/{}", model.name, id));
        let found = search::search(tx, catalog, other, &params)?;
        if let Some(items) = found.get("entry").and_then(|value| value.as_array()) {
            entries.extend(items.iter().cloned());
        }
    }
    Ok(bundle(entries))
}

pub fn document(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    id: &str,
    args: &Value,
) -> Result<Value, FhirError> {
    if model.name != "Composition" {
        return Err(FhirError::Unsupported(
            "document operation is only supported on Composition".into(),
        ));
    }
    let composition = ops::read(tx, model, id)?;
    // A caller limited to one patient gets that patient's document: what the Composition reaches
    // that is another patient's stays out, and so does what it would reach through that.
    let own = document::limited_to(args);
    let resources = document::gather(
        tx,
        catalog,
        vec![composition],
        &document::may_read(args),
        &|resource| own.is_none_or(|patient| document::within_patient(resource, patient)),
    )?;
    Ok(document::package(resources))
}

fn bundle(entries: Vec<Value>) -> Value {
    json!({
        "resourceType": "Bundle",
        "type": "searchset",
        "total": entries.len(),
        "entry": entries
    })
}

fn parameters(meta: Value) -> Value {
    json!({
        "resourceType": "Parameters",
        "parameter": [{"name": "return", "valueMeta": meta}]
    })
}

fn meta_of(body: &Value) -> Value {
    if body.get("resourceType").and_then(|value| value.as_str()) == Some("Parameters") {
        return body
            .get("parameter")
            .and_then(|value| value.as_array())
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item.get("name").and_then(|value| value.as_str()) == Some("meta"))
            })
            .and_then(|item| item.get("valueMeta"))
            .cloned()
            .unwrap_or_else(|| json!({}));
    }
    body.get("meta").cloned().unwrap_or_else(|| body.clone())
}

fn union_meta(current: &[Value], extra: &[Value]) -> Vec<Value> {
    let mut next = current.to_vec();
    for item in extra {
        if !next.iter().any(|existing| meta_same(existing, item)) {
            next.push(item.clone());
        }
    }
    next
}

fn meta_same(left: &Value, right: &Value) -> bool {
    if left == right {
        return true;
    }
    let code = |value: &Value| {
        (
            value
                .get("system")
                .and_then(|item| item.as_str())
                .map(|item| item.to_string()),
            value
                .get("code")
                .and_then(|item| item.as_str())
                .map(|item| item.to_string()),
        )
    };
    let left_code = code(left);
    left_code.0.is_some() && left_code == code(right)
        || left.as_str().is_some() && left.as_str() == right.as_str()
}
