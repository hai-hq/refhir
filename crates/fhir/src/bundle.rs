use std::collections::{HashMap, VecDeque};

use haifhir_database::Transaction;
use haifhir_types::Timestamp;
use serde_json::{json, Value};

use crate::catalog::{valid_id, Catalog};
use crate::error::FhirError;
use crate::ops;

/// One FHIR transaction Bundle is one Convex transaction: one begin timestamp, one commit.
///
/// An Observation may name a Patient that another entry in this bundle writes.
/// That Patient entry is applied first. The response entries stay in request order.
pub fn transaction(
    tx: &mut Transaction,
    catalog: &Catalog,
    bundle: &Value,
) -> Result<Value, FhirError> {
    if bundle.get("resourceType").and_then(|value| value.as_str()) != Some("Bundle") {
        return Err(FhirError::Invalid("resourceType Bundle is required".into()));
    }
    if bundle.get("type").and_then(|value| value.as_str()) != Some("transaction") {
        return Err(FhirError::Invalid("Bundle.type must be transaction".into()));
    }
    let entries = bundle
        .get("entry")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let mut response_entries = vec![Value::Null; entries.len()];
    for index in apply_order(catalog, &entries) {
        response_entries[index] =
            apply_entry(tx, catalog, &entries[index]).map_err(|err| annotate(index, err))?;
    }
    Ok(json!({
        "resourceType": "Bundle",
        "type": "transaction-response",
        "entry": response_entries
    }))
}

/// Writes come before entries that reference them. Every other entry keeps its place.
fn apply_order(catalog: &Catalog, entries: &[Value]) -> Vec<usize> {
    let writers = resource_writers(entries);
    let mut waiting = vec![0usize; entries.len()];
    let mut after: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
    for (index, entry) in entries.iter().enumerate() {
        for dependency in referenced_resources(catalog, entry) {
            let Some(&writer) = writers.get(&dependency) else {
                continue;
            };
            if writer == index {
                continue;
            }
            after[writer].push(index);
            waiting[index] += 1;
        }
    }
    let mut ready: VecDeque<usize> = (0..entries.len())
        .filter(|&index| waiting[index] == 0)
        .collect();
    let mut order = Vec::with_capacity(entries.len());
    while let Some(index) = ready.pop_front() {
        order.push(index);
        let mut unlocked = Vec::new();
        for &dependent in &after[index] {
            waiting[dependent] -= 1;
            if waiting[dependent] == 0 {
                unlocked.push(dependent);
            }
        }
        unlocked.sort_unstable();
        ready.extend(unlocked);
    }
    for index in 0..entries.len() {
        if !order.contains(&index) {
            order.push(index);
        }
    }
    order
}

fn resource_writers(entries: &[Value]) -> HashMap<(String, String), usize> {
    let mut writers = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        let Some(key) = written_resource(entry) else {
            continue;
        };
        writers.entry(key).or_insert(index);
    }
    writers
}

fn written_resource(entry: &Value) -> Option<(String, String)> {
    let method = request_method(entry)?;
    if method != "POST" && method != "PUT" {
        return None;
    }
    let (type_name, id) = parse_url(request_url(entry)?).ok()?;
    if let Some(id) = id {
        return Some((type_name, id));
    }
    let id = entry
        .pointer("/resource/id")
        .and_then(|value| value.as_str())?;
    valid_id(id).then(|| (type_name, id.to_string()))
}

fn referenced_resources(catalog: &Catalog, entry: &Value) -> Vec<(String, String)> {
    let mut keys = Vec::new();
    if request_method(entry) == Some("GET") {
        if let Some(url) = request_url(entry) {
            if let Ok((type_name, Some(id))) = parse_url(url) {
                keys.push((type_name, id));
            }
        }
        return keys;
    }
    let Some(resource) = entry.get("resource") else {
        return keys;
    };
    let Some(type_name) = resource
        .get("resourceType")
        .and_then(|value| value.as_str())
    else {
        return keys;
    };
    let Ok(model) = catalog.get(type_name) else {
        return keys;
    };
    for element in &model.references {
        let Some(node) = resource.get(&element.path) else {
            continue;
        };
        let values = match node {
            Value::Array(items) => items.iter().collect::<Vec<_>>(),
            other => vec![other],
        };
        for value in values {
            let Some(reference) = value.get("reference").and_then(|item| item.as_str()) else {
                continue;
            };
            let Some((target_type, id)) = reference.split_once('/') else {
                continue;
            };
            if reference.contains("://") || !valid_id(id) {
                continue;
            }
            keys.push((target_type.to_string(), id.to_string()));
        }
    }
    keys
}

fn request_method(entry: &Value) -> Option<&str> {
    entry
        .pointer("/request/method")
        .and_then(|value| value.as_str())
}

fn request_url(entry: &Value) -> Option<&str> {
    entry
        .pointer("/request/url")
        .and_then(|value| value.as_str())
}

fn apply_entry(tx: &mut Transaction, catalog: &Catalog, entry: &Value) -> Result<Value, FhirError> {
    let request = entry
        .get("request")
        .ok_or_else(|| FhirError::Invalid("entry.request is required".into()))?;
    let method = request
        .get("method")
        .and_then(|value| value.as_str())
        .ok_or_else(|| FhirError::Invalid("entry.request.method is required".into()))?;
    let url = request
        .get("url")
        .and_then(|value| value.as_str())
        .ok_or_else(|| FhirError::Invalid("entry.request.url is required".into()))?;
    if url.contains('?') {
        return Err(FhirError::Unsupported(
            "search inside a transaction bundle is not supported".into(),
        ));
    }
    let (type_name, id) = parse_url(url)?;
    let model = catalog.get(&type_name)?;
    match method {
        "POST" => {
            let mut resource = entry
                .get("resource")
                .cloned()
                .ok_or_else(|| FhirError::Invalid("POST entry requires a resource".into()))?;
            if let Some(id) = id {
                if let Some(body_id) = resource.get("id").and_then(|value| value.as_str()) {
                    if body_id != id {
                        return Err(FhirError::Invalid(
                            "entry resource id does not match request url".into(),
                        ));
                    }
                }
                resource["id"] = json!(id);
            }
            let created = ops::create(tx, catalog, model, resource)?;
            let id = created["id"].as_str().unwrap_or("").to_string();
            Ok(json!({
                "response": {
                    "status": "201 Created",
                    "location": format!("{}/{id}/_history/0", model.name),
                    "etag": "W/\"0\""
                },
                "resource": created
            }))
        }
        "PUT" => {
            let id = id.ok_or_else(|| FhirError::Invalid("PUT url requires an id".into()))?;
            let resource = entry
                .get("resource")
                .cloned()
                .ok_or_else(|| FhirError::Invalid("PUT entry requires a resource".into()))?;
            let updated = ops::update(tx, catalog, model, &id, resource, None)?;
            Ok(json!({
                "response": {
                    "status": "200 OK",
                    "location": format!("{}/{id}/_history/0", model.name),
                    "etag": "W/\"0\""
                },
                "resource": updated
            }))
        }
        "DELETE" => {
            let id = id.ok_or_else(|| FhirError::Invalid("DELETE url requires an id".into()))?;
            ops::delete(tx, catalog, model, &id)?;
            Ok(json!({
                "response": {"status": "204 No Content"}
            }))
        }
        "GET" => {
            let id = id.ok_or_else(|| FhirError::Invalid("GET url requires an id".into()))?;
            let resource = ops::read(tx, model, &id)?;
            let version = resource
                .pointer("/meta/versionId")
                .and_then(|value| value.as_str())
                .unwrap_or("0");
            Ok(json!({
                "response": {
                    "status": "200 OK",
                    "etag": format!("W/\"{version}\"")
                },
                "resource": resource
            }))
        }
        other => Err(FhirError::Unsupported(format!(
            "bundle method {other} is not supported"
        ))),
    }
}

fn parse_url(url: &str) -> Result<(String, Option<String>), FhirError> {
    let url = url.trim_start_matches('/');
    let mut parts = url.split('/');
    let type_name = parts
        .next()
        .filter(|part| !part.is_empty())
        .ok_or_else(|| FhirError::Invalid(format!("bad request url {url}")))?
        .to_string();
    let id = match parts.next() {
        Some(id) if !id.is_empty() => {
            if !valid_id(id) {
                return Err(FhirError::Invalid(format!("invalid id {id}")));
            }
            Some(id.to_string())
        }
        Some(_) => {
            return Err(FhirError::Invalid(format!("bad request url {url}")));
        }
        None => None,
    };
    if parts.next().is_some() {
        return Err(FhirError::Unsupported(format!(
            "unsupported request url {url}"
        )));
    }
    Ok((type_name, id))
}

pub fn finalize(value: &mut Value, written: &[haifhir_database::WrittenId], ts: Timestamp) {
    stamp_tree(value, written, ts);
    rewrite_placeholders(value, ts);
}

fn stamp_tree(value: &mut Value, written: &[haifhir_database::WrittenId], ts: Timestamp) {
    let resource_type = value
        .get("resourceType")
        .and_then(|item| item.as_str())
        .map(|item| item.to_string());
    let id = value
        .get("id")
        .and_then(|item| item.as_str())
        .map(|item| item.to_string());
    if let (Some(resource_type), Some(id)) = (resource_type, id) {
        if written
            .iter()
            .any(|item| item.tablet == resource_type && item.id == id)
        {
            ops::set_version(value, ts);
        }
    }
    if let Some(entries) = value
        .get_mut("entry")
        .and_then(|entry| entry.as_array_mut())
    {
        for entry in entries {
            if let Some(resource) = entry.get_mut("resource") {
                stamp_tree(resource, written, ts);
            }
        }
    }
}

fn rewrite_placeholders(value: &mut Value, ts: Timestamp) {
    match value {
        Value::Array(items) => {
            for item in items {
                rewrite_placeholders(item, ts);
            }
        }
        Value::Object(map) => {
            if let Some(Value::String(location)) = map.get_mut("location") {
                let placeholder = "/_history/0";
                if let Some(stripped) = location.strip_suffix(placeholder) {
                    *location = format!("{stripped}/_history/{ts}");
                }
            }
            if let Some(Value::String(etag)) = map.get_mut("etag") {
                if etag == "W/\"0\"" {
                    *etag = format!("W/\"{ts}\"");
                }
            }
            for child in map.values_mut() {
                rewrite_placeholders(child, ts);
            }
        }
        _ => {}
    }
}

fn annotate(index: usize, err: FhirError) -> FhirError {
    let message = format!("entry {index}: {err}");
    match err {
        FhirError::NotFound(_) => FhirError::NotFound(message),
        FhirError::Gone(_) => FhirError::Gone(message),
        FhirError::Conflict(_) => FhirError::Conflict(message),
        FhirError::Invalid(_) => FhirError::Invalid(message),
        FhirError::UnknownResource(_) => FhirError::UnknownResource(message),
        FhirError::Unsupported(_) => FhirError::Unsupported(message),
        FhirError::Storage(_) => FhirError::Storage(message),
    }
}
