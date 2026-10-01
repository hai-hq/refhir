use refhir_database::{Delete, Put, Stored, Transaction};
use refhir_types::Timestamp;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::catalog::{index_keys, valid_id, Catalog, ResourceModel};
use crate::error::FhirError;
use crate::resolve::TxHost;

pub fn create(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    mut resource: Value,
) -> Result<Value, FhirError> {
    expect_type(model, &resource)?;
    let id = match resource.get("id").and_then(|value| value.as_str()) {
        Some(id) => {
            if !valid_id(id) {
                return Err(FhirError::Invalid(format!("invalid id {id}")));
            }
            id.to_string()
        }
        None => {
            let id = Uuid::new_v4().to_string();
            resource["id"] = json!(id);
            id
        }
    };
    if tx.get(&model.tablet(), &id, &model.by_id())?.is_some() {
        return Err(FhirError::Conflict(format!(
            "{}/{} already exists",
            model.name, id
        )));
    }
    require_references(tx, catalog, model, &resource)?;
    let keys = {
        let host = TxHost::new(tx, catalog);
        index_keys(catalog.path_model(), model, &resource, &host)?
    };
    tx.stage_put(Put {
        tablet: model.tablet(),
        id: id.clone(),
        json: resource.clone(),
        new_keys: keys,
        old_keys: Vec::new(),
        prev_ts: None,
    })?;
    Ok(resource)
}

pub fn read(tx: &mut Transaction, model: &ResourceModel, id: &str) -> Result<Value, FhirError> {
    let stored = load(tx, model, id)?;
    Ok(decorate(stored.json, id, stored.ts))
}

pub fn update(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    id: &str,
    mut resource: Value,
    if_match: Option<&str>,
) -> Result<Value, FhirError> {
    if !valid_id(id) {
        return Err(FhirError::Invalid(format!("invalid id {id}")));
    }
    expect_type(model, &resource)?;
    if let Some(body_id) = resource.get("id").and_then(|value| value.as_str()) {
        if body_id != id {
            return Err(FhirError::Invalid(format!(
                "resource id {body_id} does not match URL {id}"
            )));
        }
    }
    resource["id"] = json!(id);
    let current = load(tx, model, id)?;
    check_version(current.ts, if_match, &resource)?;
    require_references(tx, catalog, model, &resource)?;
    let (old_keys, new_keys) = {
        let host = TxHost::new(tx, catalog);
        (
            index_keys(catalog.path_model(), model, &current.json, &host)?,
            index_keys(catalog.path_model(), model, &resource, &host)?,
        )
    };
    tx.stage_put(Put {
        tablet: model.tablet(),
        id: id.to_string(),
        json: resource.clone(),
        new_keys,
        old_keys,
        prev_ts: current.ts,
    })?;
    Ok(resource)
}

pub fn delete(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    id: &str,
) -> Result<Value, FhirError> {
    let current = load(tx, model, id)?;
    let old_keys = {
        let host = TxHost::new(tx, catalog);
        index_keys(catalog.path_model(), model, &current.json, &host)?
    };
    let prev_ts = current.ts.unwrap_or(Timestamp::ZERO);
    tx.stage_delete(Delete {
        tablet: model.tablet(),
        id: id.to_string(),
        old_keys,
        prev_ts,
    })?;
    Ok(Value::Null)
}

pub fn decorate(mut json: Value, id: &str, ts: Option<Timestamp>) -> Value {
    let Some(object) = json.as_object_mut() else {
        return json;
    };
    object.insert("id".into(), json!(id));
    if let Some(ts) = ts {
        set_version_id(object, ts);
    }
    json
}

pub fn set_version(value: &mut Value, ts: Timestamp) {
    if let Some(object) = value.as_object_mut() {
        set_version_id(object, ts);
    }
}

fn set_version_id(object: &mut serde_json::Map<String, Value>, ts: Timestamp) {
    let meta = object.entry("meta").or_insert_with(|| json!({}));
    if let Some(meta) = meta.as_object_mut() {
        meta.insert("versionId".into(), json!(ts.to_string()));
    }
}

fn load(tx: &mut Transaction, model: &ResourceModel, id: &str) -> Result<Stored, FhirError> {
    tx.get(&model.tablet(), id, &model.by_id())?
        .ok_or_else(|| FhirError::NotFound(format!("{}/{} not found", model.name, id)))
}

fn expect_type(model: &ResourceModel, resource: &Value) -> Result<(), FhirError> {
    match resource
        .get("resourceType")
        .and_then(|value| value.as_str())
    {
        Some(name) if name == model.name => Ok(()),
        Some(name) => Err(FhirError::Invalid(format!(
            "resourceType {name} does not match {}",
            model.name
        ))),
        None => Err(FhirError::Invalid("resourceType is required".into())),
    }
}

pub fn require_references(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    resource: &Value,
) -> Result<(), FhirError> {
    for element in &model.references {
        let references = reference_strings(resource, &element.path);
        if element.min >= 1 && references.is_empty() {
            return Err(FhirError::Invalid(format!(
                "{}.{}.reference is required",
                model.name, element.path
            )));
        }
        for reference in references {
            check_reference(tx, catalog, model, element, &reference)?;
        }
    }
    Ok(())
}

fn check_reference(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    element: &crate::catalog::ReferenceElement,
    reference: &str,
) -> Result<(), FhirError> {
    let relative = format!(
        "{}.{} must be a relative reference {}/{{id}}",
        model.name,
        element.path,
        element
            .targets
            .first()
            .map(String::as_str)
            .unwrap_or("Resource")
    );
    if reference.contains("://") {
        return Err(FhirError::Invalid(relative));
    }
    let Some((type_name, id)) = reference.split_once('/') else {
        return Err(FhirError::Invalid(relative));
    };
    if type_name.is_empty() || reference.matches('/').count() != 1 || !valid_id(id) {
        return Err(FhirError::Invalid(relative));
    }
    if !element.targets.is_empty() && !element.targets.iter().any(|target| target == type_name) {
        return Err(FhirError::Invalid(relative));
    }
    let target = catalog.get(type_name)?;
    if tx.get(&target.tablet(), id, &target.by_id())?.is_none() {
        return Err(FhirError::Invalid(format!(
            "{} {type_name}/{id} does not exist",
            element.path
        )));
    }
    Ok(())
}

fn reference_strings(resource: &Value, path: &str) -> Vec<String> {
    let mut current = vec![resource];
    for segment in path.split('.') {
        let mut next = Vec::new();
        for value in current {
            match value {
                Value::Array(items) => {
                    for item in items {
                        if let Some(child) = item.get(segment) {
                            next.push(child);
                        }
                    }
                }
                other => {
                    if let Some(child) = other.get(segment) {
                        next.push(child);
                    }
                }
            }
        }
        current = next;
    }
    let mut references = Vec::new();
    for value in current {
        match value {
            Value::Array(items) => {
                for item in items {
                    push_reference(&mut references, item);
                }
            }
            other => push_reference(&mut references, other),
        }
    }
    references
}

fn push_reference(references: &mut Vec<String>, value: &Value) {
    if let Some(reference) = value.get("reference").and_then(|item| item.as_str()) {
        if !reference.is_empty() {
            references.push(reference.to_string());
        }
    }
}

fn check_version(
    current: Option<Timestamp>,
    if_match: Option<&str>,
    body: &Value,
) -> Result<(), FhirError> {
    let body_version = body
        .pointer("/meta/versionId")
        .and_then(|value| value.as_str());
    let Some(current) = current else {
        if if_match.is_some() || body_version.is_some() {
            return Err(FhirError::Invalid(
                "cannot check a version on a resource staged in this transaction".into(),
            ));
        }
        return Ok(());
    };
    if let Some(tag) = if_match {
        let expected = parse_if_match(tag)?;
        if expected != current {
            return Err(FhirError::Conflict(format!(
                "If-Match {tag} does not match version {current}"
            )));
        }
    }
    if let Some(version) = body_version {
        let expected = Timestamp::from_version_id(version)
            .map_err(|_| FhirError::Invalid(format!("invalid meta.versionId {version}")))?;
        if expected != current {
            return Err(FhirError::Conflict(format!(
                "meta.versionId {version} does not match version {current}"
            )));
        }
    }
    Ok(())
}

fn parse_if_match(tag: &str) -> Result<Timestamp, FhirError> {
    let trimmed = tag.trim();
    let trimmed = trimmed.strip_prefix("W/").unwrap_or(trimmed).trim();
    let trimmed = trimmed.trim_matches('"');
    Timestamp::from_version_id(trimmed)
        .map_err(|_| FhirError::Invalid(format!("invalid If-Match {tag}")))
}
