//! JSON Patch (RFC 6902) and a FHIRPath Patch subset.
//! Both produce one updated resource that `update` then indexes.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::error::FhirError;

pub fn apply(resource: &mut Value, patch: &Value) -> Result<(), FhirError> {
    if patch.get("resourceType").and_then(|value| value.as_str()) == Some("Parameters") {
        return apply_fhirpath(resource, patch);
    }
    apply_json_patch(resource, patch)
}

fn apply_json_patch(resource: &mut Value, patch: &Value) -> Result<(), FhirError> {
    let ops = patch
        .as_array()
        .ok_or_else(|| FhirError::Invalid("JSON Patch document must be an array".into()))?;
    for op in ops {
        let kind = op
            .get("op")
            .and_then(|value| value.as_str())
            .ok_or_else(|| FhirError::Invalid("patch op is required".into()))?;
        let path = op
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        match kind {
            "add" => {
                let value = op
                    .get("value")
                    .cloned()
                    .ok_or_else(|| FhirError::Invalid("add requires value".into()))?;
                pointer_add(resource, path, value)?;
            }
            "remove" => {
                pointer_remove(resource, path)?;
            }
            "replace" => {
                let value = op
                    .get("value")
                    .cloned()
                    .ok_or_else(|| FhirError::Invalid("replace requires value".into()))?;
                pointer_remove(resource, path)?;
                pointer_add(resource, path, value)?;
            }
            "test" => {
                let value = op.get("value").cloned().unwrap_or(Value::Null);
                let current = pointer_get(resource, path)?;
                if current != &value {
                    return Err(FhirError::Conflict("patch test failed".into()));
                }
            }
            "move" | "copy" => {
                let from = op
                    .get("from")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| FhirError::Invalid("move requires from".into()))?;
                let value = pointer_get(resource, from)?.clone();
                if kind == "move" {
                    pointer_remove(resource, from)?;
                }
                pointer_add(resource, path, value)?;
            }
            other => {
                return Err(FhirError::Unsupported(format!("patch op {other}")));
            }
        }
    }
    Ok(())
}

fn apply_fhirpath(resource: &mut Value, parameters: &Value) -> Result<(), FhirError> {
    let ops = parameters
        .get("parameter")
        .and_then(|value| value.as_array())
        .ok_or_else(|| FhirError::Invalid("FHIRPath patch requires parameter".into()))?;
    for op in ops {
        if op.get("name").and_then(|value| value.as_str()) != Some("operation") {
            continue;
        }
        let parts = op
            .get("part")
            .and_then(|value| value.as_array())
            .ok_or_else(|| FhirError::Invalid("operation requires part".into()))?;
        let kind = part_text(parts, "type").unwrap_or("");
        let path = part_text(parts, "path").unwrap_or("");
        let name = part_text(parts, "name").unwrap_or("");
        let value = part_value(parts);
        match kind {
            "add" | "insert" | "replace" => {
                let target = fhir_path(resource, path)?;
                write_child(target, name, value.unwrap_or(Value::Null), kind == "insert")?;
            }
            "delete" => {
                if name.is_empty() {
                    pointer_remove(resource, &fhir_pointer(path))?;
                } else {
                    let target = fhir_path(resource, path)?;
                    if let Some(object) = target.as_object_mut() {
                        object.remove(name);
                    }
                }
            }
            other => {
                return Err(FhirError::Unsupported(format!(
                    "FHIRPath patch type {other}"
                )));
            }
        }
    }
    Ok(())
}

fn part_text<'a>(parts: &'a [Value], name: &str) -> Option<&'a str> {
    parts.iter().find_map(|part| {
        if part.get("name").and_then(|value| value.as_str()) != Some(name) {
            return None;
        }
        part.get("valueString")
            .or_else(|| part.get("valueCode"))
            .and_then(|value| value.as_str())
    })
}

fn part_value(parts: &[Value]) -> Option<Value> {
    for part in parts {
        if part.get("name").and_then(|value| value.as_str()) != Some("value") {
            continue;
        }
        for (key, value) in part.as_object()? {
            if key.starts_with("value") {
                return Some(primitive_or_value(key, value));
            }
        }
    }
    None
}

fn primitive_or_value(key: &str, value: &Value) -> Value {
    if value.is_object() || value.is_array() {
        return value.clone();
    }
    if key == "valueString"
        || key == "valueCode"
        || key == "valueUri"
        || key == "valueDate"
        || key == "valueDateTime"
        || key == "valueId"
    {
        return value.clone();
    }
    value.clone()
}

fn fhir_path<'a>(resource: &'a mut Value, path: &str) -> Result<&'a mut Value, FhirError> {
    let pointer = fhir_pointer(path);
    pointer_get_mut(resource, &pointer)
}

fn fhir_pointer(path: &str) -> String {
    let mut out = String::new();
    for segment in path.split('.') {
        if segment.is_empty() {
            continue;
        }
        let (name, index) = segment
            .split_once('[')
            .map(|(name, rest)| (name, rest.trim_end_matches(']')))
            .unwrap_or((segment, ""));
        if name
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase())
            && out.is_empty()
        {
            continue;
        }
        out.push('/');
        out.push_str(&name.replace('~', "~0").replace('/', "~1"));
        if !index.is_empty() {
            out.push('/');
            out.push_str(index);
        }
    }
    out
}

fn write_child(
    target: &mut Value,
    name: &str,
    value: Value,
    insert: bool,
) -> Result<(), FhirError> {
    if name.is_empty() {
        *target = value;
        return Ok(());
    }
    let object = target
        .as_object_mut()
        .ok_or_else(|| FhirError::Invalid("patch path is not an object".into()))?;
    if insert {
        let slot = object.entry(name).or_insert_with(|| json!([]));
        let array = slot
            .as_array_mut()
            .ok_or_else(|| FhirError::Invalid("insert requires an array".into()))?;
        array.insert(0, value);
        return Ok(());
    }
    object.insert(name.to_string(), value);
    Ok(())
}

fn pointer_get<'a>(resource: &'a Value, pointer: &str) -> Result<&'a Value, FhirError> {
    if pointer.is_empty() {
        return Ok(resource);
    }
    resource
        .pointer(&unescape(pointer))
        .ok_or_else(|| FhirError::Invalid(format!("patch path {pointer} was not found")))
}

fn pointer_get_mut<'a>(resource: &'a mut Value, pointer: &str) -> Result<&'a mut Value, FhirError> {
    if pointer.is_empty() {
        return Ok(resource);
    }
    resource
        .pointer_mut(&unescape(pointer))
        .ok_or_else(|| FhirError::Invalid(format!("patch path {pointer} was not found")))
}

fn pointer_add(resource: &mut Value, pointer: &str, value: Value) -> Result<(), FhirError> {
    if pointer.is_empty() {
        *resource = value;
        return Ok(());
    }
    let pointer = unescape(pointer);
    let (parent, token) = split_pointer(&pointer)?;
    let parent = parent_value(resource, parent, &pointer)?;
    match parent {
        Value::Object(map) => {
            map.insert(token.to_string(), value);
            Ok(())
        }
        Value::Array(items) => {
            if token == "-" {
                items.push(value);
                return Ok(());
            }
            let index: usize = token
                .parse()
                .map_err(|_| FhirError::Invalid(format!("bad array index {token}")))?;
            if index > items.len() {
                return Err(FhirError::Invalid(format!(
                    "array index {index} is past the end"
                )));
            }
            items.insert(index, value);
            Ok(())
        }
        _ => Err(FhirError::Invalid("patch path is not a container".into())),
    }
}

fn pointer_remove(resource: &mut Value, pointer: &str) -> Result<(), FhirError> {
    if pointer.is_empty() {
        *resource = Value::Null;
        return Ok(());
    }
    let pointer = unescape(pointer);
    let (parent, token) = split_pointer(&pointer)?;
    let parent = parent_value(resource, parent, &pointer)?;
    match parent {
        Value::Object(map) => {
            map.remove(token);
            Ok(())
        }
        Value::Array(items) => {
            let index: usize = token
                .parse()
                .map_err(|_| FhirError::Invalid(format!("bad array index {token}")))?;
            if index >= items.len() {
                return Err(FhirError::Invalid(format!(
                    "array index {index} is past the end"
                )));
            }
            items.remove(index);
            Ok(())
        }
        _ => Err(FhirError::Invalid("patch path is not a container".into())),
    }
}

fn parent_value<'a>(
    resource: &'a mut Value,
    parent: &str,
    pointer: &str,
) -> Result<&'a mut Value, FhirError> {
    if parent.is_empty() {
        return Ok(resource);
    }
    let parent_pointer = if parent.starts_with('/') {
        parent.to_string()
    } else {
        format!("/{parent}")
    };
    resource
        .pointer_mut(&parent_pointer)
        .ok_or_else(|| FhirError::Invalid(format!("patch path {pointer} was not found")))
}

fn split_pointer(pointer: &str) -> Result<(&str, &str), FhirError> {
    let pointer = pointer.trim_start_matches('/');
    match pointer.rfind('/') {
        Some(index) => Ok((&pointer[..index], &pointer[index + 1..])),
        None => Ok(("", pointer)),
    }
}

fn unescape(pointer: &str) -> String {
    if pointer.is_empty() {
        return String::new();
    }
    let pointer = if pointer.starts_with('/') {
        pointer.to_string()
    } else {
        format!("/{pointer}")
    };
    pointer.replace("~1", "/").replace("~0", "~")
}

pub fn parse_query(query: &str) -> BTreeMap<String, String> {
    let mut params = BTreeMap::new();
    for pair in query.trim_start_matches('?').split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(decode(key), decode(value));
    }
    params
}

fn decode(text: &str) -> String {
    let text = text.replace('+', " ");
    let mut out = String::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("00"),
                16,
            ) {
                out.push(byte as char);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    out
}
