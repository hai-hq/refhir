use haifhir_database::{Delete, Put, Stored, Transaction};
use haifhir_types::Timestamp;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::catalog::{index_keys, valid_id, ResourceType};
use crate::error::FhirError;

pub fn create(
    tx: &mut Transaction,
    ty: ResourceType,
    mut resource: Value,
) -> Result<Value, FhirError> {
    expect_type(ty, &resource)?;
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
    if tx.get(&ty.tablet(), &id, &ty.by_id())?.is_some() {
        return Err(FhirError::Conflict(format!(
            "{}/{} already exists",
            ty.as_str(),
            id
        )));
    }
    if ty == ResourceType::Observation {
        require_patient_subject(tx, &resource)?;
    }
    let keys = index_keys(ty, &resource)?;
    tx.stage_put(Put {
        tablet: ty.tablet(),
        id: id.clone(),
        json: resource.clone(),
        new_keys: keys,
        old_keys: Vec::new(),
        prev_ts: None,
    })?;
    Ok(resource)
}

pub fn read(tx: &mut Transaction, ty: ResourceType, id: &str) -> Result<Value, FhirError> {
    let stored = load(tx, ty, id)?;
    Ok(decorate(stored.json, id, stored.ts))
}

pub fn update(
    tx: &mut Transaction,
    ty: ResourceType,
    id: &str,
    mut resource: Value,
    if_match: Option<&str>,
) -> Result<Value, FhirError> {
    if !valid_id(id) {
        return Err(FhirError::Invalid(format!("invalid id {id}")));
    }
    expect_type(ty, &resource)?;
    if let Some(body_id) = resource.get("id").and_then(|value| value.as_str()) {
        if body_id != id {
            return Err(FhirError::Invalid(format!(
                "resource id {body_id} does not match URL {id}"
            )));
        }
    }
    resource["id"] = json!(id);
    let current = load(tx, ty, id)?;
    check_version(current.ts, if_match, &resource)?;
    if ty == ResourceType::Observation {
        require_patient_subject(tx, &resource)?;
    }
    let old_keys = index_keys(ty, &current.json)?;
    let new_keys = index_keys(ty, &resource)?;
    tx.stage_put(Put {
        tablet: ty.tablet(),
        id: id.to_string(),
        json: resource.clone(),
        new_keys,
        old_keys,
        prev_ts: current.ts,
    })?;
    Ok(resource)
}

pub fn delete(tx: &mut Transaction, ty: ResourceType, id: &str) -> Result<Value, FhirError> {
    let current = load(tx, ty, id)?;
    let old_keys = index_keys(ty, &current.json)?;
    let prev_ts = current.ts.unwrap_or(Timestamp::ZERO);
    tx.stage_delete(Delete {
        tablet: ty.tablet(),
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

fn load(tx: &mut Transaction, ty: ResourceType, id: &str) -> Result<Stored, FhirError> {
    tx.get(&ty.tablet(), id, &ty.by_id())?
        .ok_or_else(|| FhirError::NotFound(format!("{}/{} not found", ty.as_str(), id)))
}

fn expect_type(ty: ResourceType, resource: &Value) -> Result<(), FhirError> {
    match resource
        .get("resourceType")
        .and_then(|value| value.as_str())
    {
        Some(name) if name == ty.as_str() => Ok(()),
        Some(name) => Err(FhirError::Invalid(format!(
            "resourceType {name} does not match {}",
            ty.as_str()
        ))),
        None => Err(FhirError::Invalid("resourceType is required".into())),
    }
}

/// Observation.subject must name a Patient visible in this snapshot.
pub fn require_patient_subject(tx: &mut Transaction, resource: &Value) -> Result<(), FhirError> {
    let reference = resource
        .pointer("/subject/reference")
        .and_then(|value| value.as_str())
        .ok_or_else(|| FhirError::Invalid("Observation.subject.reference is required".into()))?;
    let Some(id) = reference.strip_prefix("Patient/") else {
        return Err(FhirError::Invalid(
            "Observation.subject must be a relative reference Patient/{id}".into(),
        ));
    };
    if reference.contains("://") || !valid_id(id) {
        return Err(FhirError::Invalid(
            "Observation.subject must be a relative reference Patient/{id}".into(),
        ));
    }
    if tx
        .get(
            &ResourceType::Patient.tablet(),
            id,
            &ResourceType::Patient.by_id(),
        )?
        .is_none()
    {
        return Err(FhirError::Invalid(format!(
            "subject Patient/{id} does not exist"
        )));
    }
    Ok(())
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
