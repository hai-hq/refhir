use refhir_database::{Delete, Put, Stored, Transaction};
use refhir_types::Timestamp;
use serde_json::{json, Value};
use uuid::Uuid;

use refhir_validate::validate;

use crate::catalog::{index_keys, valid_id, Catalog, ReferencePolicy, ResourceModel};
use crate::error::FhirError;
use crate::resolve::TxHost;

/// What `create` does with an `id` in the request body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdPolicy {
    /// Ignore it and assign a new one. This is the FHIR create interaction: a client cannot
    /// choose the id of a resource it creates (R4: "the server SHALL ignore it").
    Assign,
    /// The id was chosen by the server already: a transaction assigns ids up front so that its
    /// entries can refer to each other, and fixtures need stable ones. It is kept.
    Keep,
}

/// The FHIR create interaction. The server assigns the id and the version, whatever the body
/// says.
pub fn create(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    resource: Value,
) -> Result<Value, FhirError> {
    create_with(tx, catalog, model, resource, IdPolicy::Assign)
}

pub(crate) fn create_with(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    mut resource: Value,
    policy: IdPolicy,
) -> Result<Value, FhirError> {
    expect_type(model, &resource)?;
    strip_server_meta(&mut resource);
    let id = match (policy, resource.get("id").and_then(|value| value.as_str())) {
        (IdPolicy::Keep, Some(id)) => {
            if !valid_id(id) {
                return Err(FhirError::Invalid(format!("invalid id {id}")));
            }
            id.to_string()
        }
        _ => {
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
    check_instance(tx, catalog, &resource)?;
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

/// `meta.versionId` and `meta.lastUpdated` belong to the server; a create ignores the client's.
fn strip_server_meta(resource: &mut Value) {
    let Some(meta) = resource
        .get_mut("meta")
        .and_then(|value| value.as_object_mut())
    else {
        return;
    };
    meta.remove("versionId");
    meta.remove("lastUpdated");
    if meta.is_empty() {
        if let Some(object) = resource.as_object_mut() {
            object.remove("meta");
        }
    }
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
    resource: Value,
    if_match: Option<&str>,
) -> Result<Value, FhirError> {
    update_status(tx, catalog, model, id, resource, if_match).map(|(value, _)| value)
}

/// The update interaction and the HTTP status it ends in: 200, or 201 when the deployment allows
/// update-as-create and `id` was not in use.
pub fn update_status(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    id: &str,
    mut resource: Value,
    if_match: Option<&str>,
) -> Result<(Value, u16), FhirError> {
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
    let current = match load(tx, model, id) {
        Err(FhirError::NotFound(_)) if catalog.update_create() => {
            if if_match.is_some() {
                return Err(FhirError::Precondition(format!(
                    "If-Match was given but {}/{id} does not exist",
                    model.name
                )));
            }
            let created = create_with(tx, catalog, model, resource, IdPolicy::Keep)?;
            return Ok((created, 201));
        }
        Err(FhirError::NotFound(_)) => return Err(update_as_create(model, id)),
        other => other?,
    };
    check_version(current.ts, if_match, &resource)?;
    require_references(tx, catalog, model, &resource)?;
    check_instance(tx, catalog, &resource)?;
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
    Ok((resource, 200))
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

fn check_instance(
    tx: &mut Transaction,
    catalog: &Catalog,
    resource: &Value,
) -> Result<(), FhirError> {
    let host = TxHost::new(tx, catalog);
    let outcome = validate(resource, &[], catalog.schema(), catalog.terms(), &host);
    if outcome.has_errors() {
        return Err(FhirError::Outcome(outcome.operation_outcome()));
    }
    Ok(())
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
    if reference.starts_with('#') {
        return match catalog.reference_policy() {
            ReferencePolicy::Fhir => Ok(()),
            ReferencePolicy::Strict => Err(FhirError::Invalid(relative)),
        };
    }
    if reference.contains("://") {
        return match catalog.reference_policy() {
            ReferencePolicy::Strict => Err(FhirError::Invalid(relative)),
            ReferencePolicy::Fhir => {
                if let Some(type_name) = absolute_type(reference) {
                    reject_target(element, type_name, &relative)?;
                }
                Ok(())
            }
        };
    }
    let Some((type_name, id)) = reference.split_once('/') else {
        return Err(FhirError::Invalid(relative));
    };
    if type_name.is_empty() || reference.matches('/').count() != 1 || !valid_id(id) {
        return Err(FhirError::Invalid(relative));
    }
    reject_target(element, type_name, &relative)?;
    let target = catalog.get(type_name)?;
    if tx.get(&target.tablet(), id, &target.by_id())?.is_none() {
        if catalog.reference_policy() == ReferencePolicy::Fhir {
            return Ok(());
        }
        return Err(FhirError::Invalid(format!(
            "{} {type_name}/{id} does not exist",
            element.path
        )));
    }
    Ok(())
}

fn reject_target(
    element: &crate::catalog::ReferenceElement,
    type_name: &str,
    relative: &str,
) -> Result<(), FhirError> {
    if !element.targets.is_empty()
        && !element
            .targets
            .iter()
            .any(|target| target == "Resource" || target == type_name)
    {
        return Err(FhirError::Invalid(relative.to_string()));
    }
    Ok(())
}

fn absolute_type(reference: &str) -> Option<&str> {
    let path = reference.split(['?', '#']).next().unwrap_or(reference);
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let type_name = segments.get(segments.len().checked_sub(2)?)?;
    if type_name.chars().next()?.is_ascii_uppercase() {
        Some(*type_name)
    } else {
        None
    }
}

pub(crate) fn reference_strings(resource: &Value, path: &str) -> Vec<String> {
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

/// Update-as-create is the one optional part of `PUT` this server leaves out: a client cannot
/// pick the id of a resource that does not exist yet. FHIR R4 gives the answer to a server in
/// that position: 405. The capability statement says `updateCreate: false`.
fn update_as_create(model: &ResourceModel, id: &str) -> FhirError {
    FhirError::MethodNotAllowed(format!(
        "{}/{id} does not exist and this server does not support update-as-create \
         (client-defined ids); POST to {} to create a resource, and the server assigns its id",
        model.name, model.name
    ))
}

pub fn conditional_create(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    resource: Value,
    if_none_exist: &str,
) -> Result<(Value, u16), FhirError> {
    conditional_create_with(
        tx,
        catalog,
        model,
        resource,
        if_none_exist,
        IdPolicy::Assign,
    )
}

pub(crate) fn conditional_create_with(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    resource: Value,
    if_none_exist: &str,
    policy: IdPolicy,
) -> Result<(Value, u16), FhirError> {
    let found = search_ids(tx, catalog, model, if_none_exist)?;
    match found.len() {
        0 => Ok((create_with(tx, catalog, model, resource, policy)?, 201)),
        1 => {
            let existing = read(tx, model, &found[0])?;
            Ok((existing, 200))
        }
        _ => Err(FhirError::Precondition(format!(
            "{} conditional create matched {} resources",
            model.name,
            found.len()
        ))),
    }
}

pub fn conditional_update(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    resource: Value,
    query: &str,
) -> Result<(Value, u16), FhirError> {
    conditional_update_with(tx, catalog, model, resource, query, None)
}

/// With no match this creates the resource, as the create interaction does. A body that names an
/// id is asking for update-as-create instead, which is not supported. `assigned` is the id a
/// transaction already gave the entry, so that other entries can refer to it.
pub(crate) fn conditional_update_with(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    mut resource: Value,
    query: &str,
    assigned: Option<&str>,
) -> Result<(Value, u16), FhirError> {
    let found = search_ids(tx, catalog, model, query)?;
    match found.len() {
        0 => {
            if let Some(id) = resource.get("id").and_then(|value| value.as_str()) {
                if !catalog.update_create() {
                    return Err(update_as_create(model, id));
                }
                // The id is the client's to name, but only if nothing is using it: a resource
                // with that id that the query did not match is someone else's.
                let id = id.to_string();
                if tx.get(&model.tablet(), &id, &model.by_id())?.is_some() {
                    return Err(FhirError::Conflict(format!(
                        "{}/{id} exists but does not match the conditional update query",
                        model.name
                    )));
                }
                return update_status(tx, catalog, model, &id, resource, None);
            }
            match assigned {
                Some(id) => {
                    resource["id"] = json!(id);
                    Ok((
                        create_with(tx, catalog, model, resource, IdPolicy::Keep)?,
                        201,
                    ))
                }
                None => Ok((create(tx, catalog, model, resource)?, 201)),
            }
        }
        1 => Ok((update(tx, catalog, model, &found[0], resource, None)?, 200)),
        _ => Err(FhirError::Precondition(format!(
            "{} conditional update matched {} resources",
            model.name,
            found.len()
        ))),
    }
}

pub fn conditional_delete(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    query: &str,
) -> Result<Value, FhirError> {
    let found = search_ids(tx, catalog, model, query)?;
    match found.len() {
        0 => Err(FhirError::NotFound(format!(
            "{} conditional delete matched nothing",
            model.name
        ))),
        1 => delete(tx, catalog, model, &found[0]),
        _ => Err(FhirError::Precondition(format!(
            "{} conditional delete matched {} resources",
            model.name,
            found.len()
        ))),
    }
}

fn search_ids(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    query: &str,
) -> Result<Vec<String>, FhirError> {
    let params = crate::patch::parse_query(query);
    let bundle = crate::search::search(tx, catalog, model, &params)?;
    let mut ids = Vec::new();
    for entry in bundle
        .get("entry")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
    {
        if entry
            .pointer("/search/mode")
            .and_then(|value| value.as_str())
            == Some("include")
        {
            continue;
        }
        if let Some(id) = entry
            .pointer("/resource/id")
            .and_then(|value| value.as_str())
        {
            ids.push(id.to_string());
        }
    }
    Ok(ids)
}

fn parse_if_match(tag: &str) -> Result<Timestamp, FhirError> {
    let trimmed = tag.trim();
    let trimmed = trimmed.strip_prefix("W/").unwrap_or(trimmed).trim();
    let trimmed = trimmed.trim_matches('"');
    Timestamp::from_version_id(trimmed)
        .map_err(|_| FhirError::Invalid(format!("invalid If-Match {tag}")))
}

#[cfg(test)]
mod reference_target_tests {
    use super::reject_target;
    use crate::catalog::ReferenceElement;

    #[test]
    fn generic_resource_target_accepts_concrete_fhir_types() {
        let generic = ReferenceElement {
            path: "subject".into(),
            min: 0,
            targets: vec!["Resource".into()],
        };
        assert!(reject_target(&generic, "Organization", "invalid reference").is_ok());
        assert!(reject_target(&generic, "Appointment", "invalid reference").is_ok());
        let patient = ReferenceElement {
            path: "subject".into(),
            min: 0,
            targets: vec!["Patient".into()],
        };
        assert!(reject_target(&patient, "Organization", "invalid reference").is_err());
    }
}
