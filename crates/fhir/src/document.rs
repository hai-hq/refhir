//! Document Bundles: what `$document` and `$summary` return.
//!
//! A document Bundle is more than the resources it holds. It has an `identifier` and a
//! `timestamp`, its Composition comes first, and each entry has a `fullUrl` that the references
//! inside the document resolve to. [`gather`] collects what a Composition reaches and [`package`]
//! puts it in that shape.

use std::collections::{HashMap, HashSet, VecDeque};

use refhir_database::Transaction;
use serde_json::{json, Value};

use crate::catalog::Catalog;
use crate::error::FhirError;
use crate::ops::decorate;

/// Most resources one document may hold. A Composition reaches what its resources reference in
/// turn, so a densely linked record would otherwise be read whole.
pub const MAX_RESOURCES: usize = 1000;

/// The type and id a reference names, whatever form it is written in (`Patient/1`, an absolute
/// URL, one with a version). A contained, `urn:` or search-style reference names none.
pub(crate) fn split_reference(reference: &str) -> Option<(String, String)> {
    if reference.starts_with('#') || reference.starts_with("urn:") || reference.contains('?') {
        return None;
    }
    let path = match reference.find("/_history/") {
        Some(at) => &reference[..at],
        None => reference,
    };
    let mut segments = path.rsplit('/');
    let id = segments.next()?;
    let type_name = segments.next()?;
    if id.is_empty() || type_name.is_empty() {
        return None;
    }
    Some((type_name.to_string(), id.to_string()))
}

fn identity(resource: &Value) -> Option<(String, String)> {
    let type_name = resource.get("resourceType")?.as_str()?;
    let id = resource.get("id")?.as_str()?;
    Some((type_name.to_string(), id.to_string()))
}

/// What a caller may read, by resource type: the types their token's scopes name (`_readScopes`,
/// which the server puts on the arguments), or every type where the arguments carry none.
pub(crate) fn may_read(args: &Value) -> impl Fn(&str) -> bool {
    let scopes: Option<Vec<String>> = args
        .get("_readScopes")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        });
    move |type_name| match &scopes {
        None => true,
        Some(scopes) => refhir_sdc::scope_allows(scopes, type_name),
    }
}

/// `start` and every stored resource they reference, and those reference, in the order they are
/// found. A resource that is not stored, that the caller may not read (`may_read`), or that
/// `keep` refuses once it is read, is left out, and what it references is not followed.
pub(crate) fn gather(
    tx: &mut Transaction,
    catalog: &Catalog,
    start: Vec<Value>,
    may_read: &dyn Fn(&str) -> bool,
    keep: &dyn Fn(&Value) -> bool,
) -> Result<Vec<Value>, FhirError> {
    if start.len() > MAX_RESOURCES {
        return Err(too_large());
    }
    let mut seen = HashSet::new();
    let mut found: Vec<Value> = Vec::new();
    let mut queue = VecDeque::new();
    for resource in start {
        if let Some(key) = identity(&resource) {
            seen.insert(key);
        }
        queue.push_back(found.len());
        found.push(resource);
    }
    while let Some(index) = queue.pop_front() {
        let mut references = Vec::new();
        collect_references(&found[index], &mut references);
        for reference in references {
            let Some((type_name, id)) = split_reference(&reference) else {
                continue;
            };
            if !seen.insert((type_name.clone(), id.clone())) {
                continue;
            }
            if catalog.deny_response_read(&type_name).is_err() || !may_read(&type_name) {
                continue;
            }
            let Ok(target) = catalog.get(&type_name) else {
                continue;
            };
            let Some(stored) = tx.get(&target.tablet(), &id, &target.by_id())? else {
                continue;
            };
            let resource = decorate(stored.json, &id, stored.ts);
            if !keep(&resource) {
                continue;
            }
            if found.len() >= MAX_RESOURCES {
                return Err(too_large());
            }
            queue.push_back(found.len());
            found.push(resource);
        }
    }
    Ok(found)
}

fn too_large() -> FhirError {
    FhirError::Unprocessable(json!({
        "resourceType": "OperationOutcome",
        "issue": [{
            "severity": "error",
            "code": "too-costly",
            "diagnostics": format!(
                "the document would hold more than {MAX_RESOURCES} resources"
            )
        }]
    }))
}

/// A document Bundle of `resources`, the Composition first. Each entry gets a `urn:uuid`
/// `fullUrl`, and a reference to a resource in the document is rewritten to it, so the document
/// stands on its own.
pub(crate) fn package(resources: Vec<Value>) -> Value {
    let urls: Vec<String> = resources
        .iter()
        .map(|_| format!("urn:uuid:{}", uuid::Uuid::new_v4()))
        .collect();
    let mut targets = HashMap::new();
    for (resource, url) in resources.iter().zip(&urls) {
        if let Some(key) = identity(resource) {
            targets.insert(key, url.clone());
        }
    }
    let entries: Vec<Value> = resources
        .into_iter()
        .zip(urls)
        .map(|(mut resource, url)| {
            point_at_entries(&mut resource, &targets);
            json!({"fullUrl": url, "resource": resource})
        })
        .collect();
    json!({
        "resourceType": "Bundle",
        "identifier": {
            "system": "urn:ietf:rfc:3986",
            "value": format!("urn:uuid:{}", uuid::Uuid::new_v4())
        },
        "type": "document",
        "timestamp": now(),
        "entry": entries
    })
}

fn point_at_entries(value: &mut Value, targets: &HashMap<(String, String), String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get_mut("reference") {
                if let Some(url) = split_reference(reference).and_then(|key| targets.get(&key)) {
                    *reference = url.clone();
                }
            }
            for item in map.values_mut() {
                point_at_entries(item, targets);
            }
        }
        Value::Array(items) => {
            for item in items {
                point_at_entries(item, targets);
            }
        }
        _ => {}
    }
}

pub(crate) fn collect_references(value: &Value, references: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(reference) = map.get("reference").and_then(|item| item.as_str()) {
                references.push(reference.to_string());
            }
            for item in map.values() {
                collect_references(item, references);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_references(item, references);
            }
        }
        _ => {}
    }
}

/// The current time as a FHIR instant, in UTC.
pub(crate) fn now() -> String {
    crate::instant::format(crate::instant::now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_names_a_type_and_id_in_any_form() {
        let key = |reference| split_reference(reference);
        let expected = Some(("Patient".to_string(), "p1".to_string()));
        assert_eq!(key("Patient/p1"), expected);
        assert_eq!(key("https://example.org/fhir/Patient/p1"), expected);
        assert_eq!(key("Patient/p1/_history/3"), expected);
        assert_eq!(key("#contained"), None);
        assert_eq!(key("urn:uuid:7b0c"), None);
        assert_eq!(key("Patient?identifier=a|b"), None);
        assert_eq!(key("p1"), None);
        assert_eq!(key("Patient/"), None);
    }

    #[test]
    fn instants_are_utc_with_a_zone() {
        let text = now();
        assert_eq!(text.len(), 24, "{text}");
        assert!(text.ends_with('Z'), "{text}");
        assert_eq!(&text[4..5], "-");
        assert_eq!(&text[10..11], "T");
    }
}
