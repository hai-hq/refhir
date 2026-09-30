use std::collections::BTreeMap;

use haifhir_database::Transaction;
use haifhir_types::{exact_key, string_interval, token_interval, Interval};
use serde_json::{json, Value};

use crate::catalog::{ResourceType, SearchKind};
use crate::error::FhirError;
use crate::ops::decorate;

const IGNORED: &[&str] = &["_format", "_pretty"];

pub fn search(
    tx: &mut Transaction,
    ty: ResourceType,
    params: &BTreeMap<String, String>,
) -> Result<Value, FhirError> {
    for name in params.keys() {
        if IGNORED.contains(&name.as_str()) {
            continue;
        }
        if ty.param(name).is_none() {
            return Err(FhirError::Invalid(format!(
                "unsupported search parameter {name} on {}",
                ty.as_str()
            )));
        }
    }
    let (index, interval) = choose_index(ty, params);
    let rows = tx.index_range(&index, interval)?;
    let mut entries = Vec::new();
    for row in rows {
        if !matches_params(ty, &row.json, params) {
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

fn choose_index(
    ty: ResourceType,
    params: &BTreeMap<String, String>,
) -> (haifhir_types::IndexId, Interval) {
    let order = [
        "_id",
        "identifier",
        "subject",
        "code",
        "active",
        "name",
        "date",
    ];
    for name in order {
        let Some(value) = params.get(name) else {
            continue;
        };
        let Some(param) = ty.param(name) else {
            continue;
        };
        if name == "_id" {
            return (ty.by_id(), Interval::point(exact_key(value)));
        }
        let interval = match param.kind {
            SearchKind::Token => token_interval(value),
            SearchKind::String => string_interval(value),
        };
        return (ty.search_index(name), interval);
    }
    (ty.by_id(), Interval::all())
}

fn matches_params(ty: ResourceType, resource: &Value, params: &BTreeMap<String, String>) -> bool {
    params.iter().all(|(name, value)| {
        if IGNORED.contains(&name.as_str()) {
            return true;
        }
        match (ty, name.as_str()) {
            (_, "_id") => resource.get("id").and_then(|item| item.as_str()) == Some(value),
            (ResourceType::Patient, "identifier") => identifier_matches(resource, value),
            (ResourceType::Patient, "name") => name_matches(resource, value),
            (ResourceType::Patient, "active") => active_matches(resource, value),
            (ResourceType::Observation, "subject") => {
                resource
                    .pointer("/subject/reference")
                    .and_then(|item| item.as_str())
                    == Some(value)
            }
            (ResourceType::Observation, "code") => code_matches(resource, value),
            (ResourceType::Observation, "date") => resource
                .get("effectiveDateTime")
                .and_then(|item| item.as_str())
                .map(|date| date.starts_with(value))
                .unwrap_or(false),
            _ => false,
        }
    })
}

fn identifier_matches(resource: &Value, expected: &str) -> bool {
    let Some(identifiers) = resource
        .get("identifier")
        .and_then(|value| value.as_array())
    else {
        return false;
    };
    identifiers.iter().any(|identifier| {
        let Some(value) = identifier.get("value").and_then(|item| item.as_str()) else {
            return false;
        };
        if value == expected {
            return true;
        }
        match identifier.get("system").and_then(|item| item.as_str()) {
            Some(system) => format!("{system}|{value}") == expected,
            None => false,
        }
    })
}

fn name_matches(resource: &Value, prefix: &str) -> bool {
    let prefix = prefix.to_lowercase();
    let Some(names) = resource.get("name").and_then(|value| value.as_array()) else {
        return false;
    };
    names.iter().any(|name| {
        let mut parts = Vec::new();
        if let Some(family) = name.get("family").and_then(|value| value.as_str()) {
            parts.push(family.to_string());
        }
        if let Some(text) = name.get("text").and_then(|value| value.as_str()) {
            parts.push(text.to_string());
        }
        if let Some(given) = name.get("given").and_then(|value| value.as_array()) {
            for item in given {
                if let Some(text) = item.as_str() {
                    parts.push(text.to_string());
                }
            }
        }
        parts
            .iter()
            .any(|part| part.to_lowercase().starts_with(&prefix))
    })
}

fn active_matches(resource: &Value, expected: &str) -> bool {
    let Some(active) = resource.get("active").and_then(|value| value.as_bool()) else {
        return false;
    };
    match expected {
        "true" => active,
        "false" => !active,
        _ => false,
    }
}

fn code_matches(resource: &Value, expected: &str) -> bool {
    let Some(codings) = resource
        .pointer("/code/coding")
        .and_then(|value| value.as_array())
    else {
        return false;
    };
    codings.iter().any(|coding| {
        let Some(code) = coding.get("code").and_then(|value| value.as_str()) else {
            return false;
        };
        if code == expected {
            return true;
        }
        match coding.get("system").and_then(|value| value.as_str()) {
            Some(system) => format!("{system}|{code}") == expected,
            None => false,
        }
    })
}
