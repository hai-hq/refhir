use std::collections::{HashMap, VecDeque};

use refhir_database::Transaction;
use refhir_types::Timestamp;
use serde_json::{json, Value};

use crate::catalog::{valid_id, Catalog};
use crate::error::{outcome_code, FhirError};
use crate::ops;

/// One FHIR transaction Bundle is one Convex transaction: one begin timestamp, one commit.
///
/// An Observation may name a Patient that another entry in this bundle writes.
/// That Patient entry is applied first. The response entries stay in request order.
pub fn transaction(
    tx: &mut Transaction,
    catalog: &Catalog,
    bundle: &Value,
    args: &Value,
) -> Result<Value, FhirError> {
    if bundle.get("resourceType").and_then(|value| value.as_str()) != Some("Bundle") {
        return Err(FhirError::Invalid("resourceType Bundle is required".into()));
    }
    if bundle.get("type").and_then(|value| value.as_str()) != Some("transaction") {
        return Err(FhirError::Invalid("Bundle.type must be transaction".into()));
    }
    let mut entries = bundle
        .get("entry")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    rewrite_urns(&mut entries);
    let mut response_entries = vec![Value::Null; entries.len()];
    for index in apply_order(catalog, &entries) {
        response_entries[index] =
            apply_entry(tx, catalog, args, &entries[index]).map_err(|err| annotate(index, err))?;
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

fn apply_entry(
    tx: &mut Transaction,
    catalog: &Catalog,
    args: &Value,
    entry: &Value,
) -> Result<Value, FhirError> {
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
    if let Some((path, query)) = url.split_once('?') {
        return conditional_entry(tx, catalog, args, entry, method, path, query);
    }
    let (type_name, id) = parse_url(url)?;
    let model = catalog.get(&type_name)?;
    match method {
        "POST" => {
            if let Some(query) = request.get("ifNoneExist").and_then(|value| value.as_str()) {
                let resource = entry.get("resource").cloned().unwrap_or(Value::Null);
                let resource = crate::sdc::prepare_store(tx, catalog, args, resource)?;
                let (created, status) =
                    ops::conditional_create(tx, catalog, model, resource, query)?;
                let code = if status == 201 {
                    "201 Created"
                } else {
                    "200 OK"
                };
                let id = created["id"].as_str().unwrap_or("");
                return Ok(json!({
                    "response": {
                        "status": code,
                        "location": format!("{}/{id}/_history/0", model.name),
                        "etag": "W/\"0\""
                    },
                    "resource": created
                }));
            }
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
            let resource = crate::sdc::prepare_store(tx, catalog, args, resource)?;
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
            let resource = crate::sdc::prepare_store(tx, catalog, args, resource)?;
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
            catalog.deny_response_read(&model.name)?;
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

fn conditional_entry(
    tx: &mut Transaction,
    catalog: &Catalog,
    args: &Value,
    entry: &Value,
    method: &str,
    path: &str,
    query: &str,
) -> Result<Value, FhirError> {
    let (type_name, _) = parse_url(path)?;
    let model = catalog.get(&type_name)?;
    let resource = entry.get("resource").cloned().unwrap_or(Value::Null);
    match method {
        "POST" | "PUT" => {
            let resource = crate::sdc::prepare_store(tx, catalog, args, resource)?;
            let (updated, status) = ops::conditional_update(tx, catalog, model, resource, query)?;
            let code = if status == 201 {
                "201 Created"
            } else {
                "200 OK"
            };
            let id = updated["id"].as_str().unwrap_or("");
            Ok(json!({
                "response": {
                    "status": code,
                    "location": format!("{type_name}/{id}/_history/0"),
                    "etag": "W/\"0\""
                },
                "resource": updated
            }))
        }
        "DELETE" => {
            ops::conditional_delete(tx, catalog, model, query)?;
            Ok(json!({"response": {"status": "204 No Content"}}))
        }
        "GET" => {
            let bundle =
                crate::search::search(tx, catalog, model, &crate::patch::parse_query(query))?;
            Ok(json!({
                "response": {"status": "200 OK"},
                "resource": bundle
            }))
        }
        other => Err(FhirError::Unsupported(format!(
            "bundle method {other} is not supported"
        ))),
    }
}

fn rewrite_urns(entries: &mut [Value]) {
    let mut assigned = std::collections::BTreeMap::new();
    for entry in entries.iter_mut() {
        let Some(full) = entry
            .get("fullUrl")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
        else {
            continue;
        };
        if !full.starts_with("urn:uuid:") {
            continue;
        }
        let type_name = entry
            .pointer("/request/url")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .split(['/', '?'])
            .next()
            .unwrap_or("")
            .trim_start_matches('/')
            .to_string();
        if type_name.is_empty() {
            continue;
        }
        let id = entry
            .pointer("/resource/id")
            .and_then(|value| value.as_str())
            .filter(|id| valid_id(id))
            .map(|id| id.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let resolved = format!("{type_name}/{id}");
        if let Some(resource) = entry.get_mut("resource") {
            resource["id"] = json!(id);
            if resource.get("resourceType").is_none() {
                resource["resourceType"] = json!(type_name);
            }
        }
        assigned.insert(full, resolved);
    }
    if assigned.is_empty() {
        return;
    }
    for entry in entries.iter_mut() {
        replace_urns(entry, &assigned);
    }
}

fn replace_urns(value: &mut Value, assigned: &std::collections::BTreeMap<String, String>) {
    match value {
        Value::String(text) => {
            if let Some(resolved) = assigned.get(text) {
                *text = resolved.clone();
            }
        }
        Value::Array(items) => {
            for item in items {
                replace_urns(item, assigned);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                replace_urns(item, assigned);
            }
        }
        _ => {}
    }
}

pub fn batch(
    db: &refhir_database::Database,
    catalog: &Catalog,
    args: &Value,
) -> Result<crate::run::RunResult, crate::run::RunError> {
    let bundle = args
        .get("resource")
        .cloned()
        .unwrap_or_else(|| args.clone());
    if bundle.get("type").and_then(|value| value.as_str()) != Some("batch") {
        return Err(crate::run::RunError::Fhir(FhirError::Invalid(
            "Bundle.type must be batch".into(),
        )));
    }
    let entries = bundle
        .get("entry")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let mut response = Vec::new();
    let mut last = db.latest_ts();
    let mut wrote = false;
    let mut writes = Vec::new();
    for entry in entries {
        match batch_entry(db, catalog, args, &entry) {
            Ok(result) => {
                wrote |= result.wrote;
                last = result.ts;
                writes.extend(result.writes);
                response.push(result.value);
            }
            Err(err) => response.push(batch_failure(&err)),
        }
    }
    Ok(crate::run::RunResult {
        value: json!({
            "resourceType": "Bundle",
            "type": "batch-response",
            "entry": response
        }),
        ts: last,
        reads: refhir_database::ReadSet::default(),
        wrote,
        writes,
        status: 200,
    })
}

fn batch_entry(
    db: &refhir_database::Database,
    catalog: &Catalog,
    controls: &Value,
    entry: &Value,
) -> Result<crate::run::RunResult, crate::run::RunError> {
    let method = request_method(entry).unwrap_or("");
    let url = request_url(entry).unwrap_or("");
    let (path, mut args) = batch_call(method, url, entry).map_err(crate::run::RunError::Fhir)?;
    if let Some(scopes) = controls.get("_readScopes") {
        if let Some(object) = args.as_object_mut() {
            object.insert("_readScopes".into(), scopes.clone());
        }
    }
    let result = crate::run::run(db, catalog, &path, &args)?;
    let value = bundle_response(&result);
    Ok(crate::run::RunResult { value, ..result })
}

fn batch_failure(err: &crate::run::RunError) -> Value {
    let (status, outcome) = match err {
        crate::run::RunError::Fhir(FhirError::Outcome(value)) => ("400 Bad Request", value.clone()),
        crate::run::RunError::Fhir(fhir) => (
            entry_status(fhir),
            operation_outcome(outcome_code(fhir), &fhir.to_string()),
        ),
        crate::run::RunError::Commit(refhir_database::CommitError::Occ(_)) => (
            "409 Conflict",
            operation_outcome("conflict", "OCC retries exhausted"),
        ),
        crate::run::RunError::Commit(other) => (
            "500 Internal Server Error",
            operation_outcome("exception", &other.to_string()),
        ),
    };
    json!({
        "response": {
            "status": status,
            "outcome": outcome
        }
    })
}

fn entry_status(err: &FhirError) -> &'static str {
    match err {
        FhirError::NotFound(_) | FhirError::UnknownResource(_) => "404 Not Found",
        FhirError::Gone(_) => "410 Gone",
        FhirError::Conflict(_) => "409 Conflict",
        FhirError::Precondition(_) => "412 Precondition Failed",
        FhirError::Storage(_) => "500 Internal Server Error",
        FhirError::Forbidden(_) => "403 Forbidden",
        FhirError::Invalid(_) | FhirError::Outcome(_) | FhirError::Unsupported(_) => {
            "400 Bad Request"
        }
    }
}

fn operation_outcome(code: &str, diagnostics: &str) -> Value {
    json!({
        "resourceType": "OperationOutcome",
        "issue": [{
            "severity": "error",
            "code": code,
            "diagnostics": diagnostics
        }]
    })
}

fn bundle_response(result: &crate::run::RunResult) -> Value {
    let status = if result.status == 201 {
        "201 Created"
    } else if result.status == 204 || result.value.is_null() {
        "204 No Content"
    } else {
        "200 OK"
    };
    let mut entry = json!({"response": {"status": status}});
    if !result.value.is_null() {
        entry["resource"] = result.value.clone();
    }
    entry
}

fn batch_call(method: &str, url: &str, entry: &Value) -> Result<(String, Value), FhirError> {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let (type_name, id) = parse_url(path)?;
    let resource = entry.get("resource").cloned();
    match method {
        "POST" if !query.is_empty() || entry.pointer("/request/ifNoneExist").is_some() => {
            let query = if query.is_empty() {
                entry
                    .pointer("/request/ifNoneExist")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
            } else {
                query
            };
            Ok((
                format!("{type_name}/conditional-create"),
                json!({
                    "resource": resource.unwrap_or(Value::Null),
                    "ifNoneExist": query
                }),
            ))
        }
        "POST" => Ok((
            format!("{type_name}/create"),
            json!({"resource": resource.unwrap_or(Value::Null)}),
        )),
        "PUT" if !query.is_empty() => Ok((
            format!("{type_name}/conditional-update"),
            json!({"resource": resource.unwrap_or(Value::Null), "query": query}),
        )),
        "PUT" => Ok((
            format!("{type_name}/update"),
            json!({"id": id.unwrap_or_default(), "resource": resource.unwrap_or(Value::Null)}),
        )),
        "DELETE" if !query.is_empty() => Ok((
            format!("{type_name}/conditional-delete"),
            json!({"query": query}),
        )),
        "DELETE" => Ok((
            format!("{type_name}/delete"),
            json!({"id": id.unwrap_or_default()}),
        )),
        "GET" if id.is_some() => Ok((
            format!("{type_name}/read"),
            json!({"id": id.unwrap_or_default()}),
        )),
        "GET" => {
            let mut args = serde_json::Map::new();
            for (key, value) in crate::patch::parse_query(query) {
                args.insert(key, Value::String(value));
            }
            Ok((format!("{type_name}/search"), Value::Object(args)))
        }
        "PATCH" => Ok((
            format!("{type_name}/patch"),
            json!({
                "id": id.unwrap_or_default(),
                "patch": resource.unwrap_or(Value::Null)
            }),
        )),
        other => Err(FhirError::Unsupported(format!(
            "bundle method {other} is not supported"
        ))),
    }
}

pub fn finalize(value: &mut Value, written: &[refhir_database::WrittenId], ts: Timestamp) {
    stamp_tree(value, written, ts);
    rewrite_placeholders(value, ts);
}

fn stamp_tree(value: &mut Value, written: &[refhir_database::WrittenId], ts: Timestamp) {
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

fn prefix_outcome(index: usize, mut value: Value) -> Value {
    let Some(issues) = value.get_mut("issue").and_then(|item| item.as_array_mut()) else {
        return value;
    };
    for issue in issues {
        let Some(object) = issue.as_object_mut() else {
            continue;
        };
        let diagnostics = object
            .get("diagnostics")
            .and_then(|item| item.as_str())
            .unwrap_or("validation failed")
            .to_string();
        object.insert(
            "diagnostics".to_string(),
            Value::String(format!("entry {index}: {diagnostics}")),
        );
    }
    value
}

fn annotate(index: usize, err: FhirError) -> FhirError {
    let message = format!("entry {index}: {err}");
    match err {
        FhirError::NotFound(_) => FhirError::NotFound(message),
        FhirError::Gone(_) => FhirError::Gone(message),
        FhirError::Conflict(_) => FhirError::Conflict(message),
        FhirError::Precondition(_) => FhirError::Precondition(message),
        FhirError::Invalid(_) => FhirError::Invalid(message),
        FhirError::Outcome(value) => FhirError::Outcome(prefix_outcome(index, value)),
        FhirError::UnknownResource(_) => FhirError::UnknownResource(message),
        FhirError::Unsupported(_) => FhirError::Unsupported(message),
        FhirError::Forbidden(_) => FhirError::Forbidden(message),
        FhirError::Storage(_) => FhirError::Storage(message),
    }
}
