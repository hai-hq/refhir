//! A small FHIR GraphQL surface. Queries and mutations lower to `dispatch`.

use axum::http::StatusCode;
use axum::response::Response;
use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::http::{execute, fhir_json_public, AppState};

pub async fn handle(app: &AppState, body: &[u8]) -> Response {
    let document = match serde_json::from_slice::<Value>(body) {
        Ok(value) => value
            .get("query")
            .and_then(|item| item.as_str())
            .unwrap_or("")
            .to_string(),
        Err(_) => String::from_utf8_lossy(body).to_string(),
    };
    match execute_document(app, &document).await {
        Ok(data) => fhir_json_public(StatusCode::OK, json!({"data": data})),
        Err(message) => fhir_json_public(
            StatusCode::BAD_REQUEST,
            json!({"errors": [{"message": message}]}),
        ),
    }
}

async fn execute_document(app: &AppState, document: &str) -> Result<Value, String> {
    let document = document.trim();
    let mutation = document.starts_with("mutation");
    let body = document
        .find('{')
        .map(|index| &document[index + 1..])
        .ok_or_else(|| "query needs a selection".to_string())?;
    let body = body
        .rsplit_once('}')
        .map(|(head, _)| head)
        .unwrap_or(body);
    let field = parse_field(body).ok_or_else(|| "empty selection".to_string())?;
    let (type_name, list, action) = split_field(&field.name);
    if mutation || action.is_some() {
        return mutate(app, &type_name, action.unwrap_or("Create"), &field).await;
    }
    if list {
        let mut args = Map::new();
        for (key, value) in &field.args {
            if key != "_count" {
                args.insert(key.clone(), Value::String(value.clone()));
            } else {
                args.insert(key.clone(), Value::String(value.clone()));
            }
        }
        let result = execute(
            &app.db,
            &app.catalog,
            &app.store,
            &format!("{type_name}/search"),
            Value::Object(args),
        )
        .await
        .map_err(|err| err.to_string())?;
        Ok(json!({ field.name.clone(): project_list(&result.value, &field) }))
    } else {
        let id = field
            .args
            .get("id")
            .cloned()
            .ok_or_else(|| "id is required".to_string())?;
        let result = execute(
            &app.db,
            &app.catalog,
            &app.store,
            &format!("{type_name}/read"),
            json!({"id": id}),
        )
        .await
        .map_err(|err| err.to_string())?;
        Ok(json!({ field.name.clone(): project(&result.value, &field) }))
    }
}

async fn mutate(
    app: &AppState,
    type_name: &str,
    action: &str,
    field: &Field,
) -> Result<Value, String> {
    let (path, args) = match action {
        "Delete" => (
            format!("{type_name}/delete"),
            json!({"id": field.args.get("id").cloned().unwrap_or_default()}),
        ),
        "Update" => (
            format!("{type_name}/update"),
            json!({
                "id": field.args.get("id").cloned().unwrap_or_default(),
                "resource": parse_resource(field.args.get("resource"))?
            }),
        ),
        "Patch" => (
            format!("{type_name}/patch"),
            json!({
                "id": field.args.get("id").cloned().unwrap_or_default(),
                "patch": parse_resource(field.args.get("patch"))?
            }),
        ),
        _ => (
            format!("{type_name}/create"),
            json!({"resource": parse_resource(field.args.get("resource"))?}),
        ),
    };
    let result = execute(&app.db, &app.catalog, &app.store, &path, args)
        .await
        .map_err(|err| err.to_string())?;
    let value = if result.value.is_null() {
        json!({"id": field.args.get("id").cloned().unwrap_or_default()})
    } else {
        project(&result.value, field)
    };
    Ok(json!({ field.name.clone(): value }))
}

fn parse_resource(text: Option<&String>) -> Result<Value, String> {
    let Some(text) = text else {
        return Err("resource is required".into());
    };
    serde_json::from_str(text).map_err(|err| err.to_string())
}

fn split_field(name: &str) -> (String, bool, Option<&str>) {
    if let Some(type_name) = name.strip_suffix("List") {
        return (type_name.to_string(), true, None);
    }
    for action in ["Create", "Update", "Delete", "Patch"] {
        if let Some(type_name) = name.strip_suffix(action) {
            return (type_name.to_string(), false, Some(action));
        }
    }
    (name.to_string(), false, None)
}

struct Field {
    name: String,
    args: BTreeMap<String, String>,
    selection: Vec<String>,
}

fn parse_field(input: &str) -> Option<Field> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let name_end = input
        .find(|ch: char| ch == '(' || ch == '{' || ch.is_whitespace())
        .unwrap_or(input.len());
    let name = input[..name_end].trim().to_string();
    if name.is_empty() {
        return None;
    }
    let mut rest = input[name_end..].trim();
    let mut args = BTreeMap::new();
    if let Some(stripped) = rest.strip_prefix('(') {
        let end = stripped.find(')')?;
        args = parse_args(&stripped[..end]);
        rest = stripped[end + 1..].trim();
    }
    let mut selection = Vec::new();
    if let Some(stripped) = rest.strip_prefix('{') {
        let end = stripped.rfind('}')?;
        selection = stripped[..end]
            .split(|ch: char| ch == '{' || ch == '}' || ch.is_whitespace() || ch == ',')
            .filter(|item| !item.is_empty())
            .map(|item| item.to_string())
            .collect();
    }
    Some(Field {
        name,
        args,
        selection,
    })
}

fn parse_args(text: &str) -> BTreeMap<String, String> {
    let mut args = BTreeMap::new();
    for part in split_args(text) {
        let Some((key, value)) = part.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        args.insert(key.trim().to_string(), value.to_string());
    }
    args
}

fn split_args(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0;
    let mut quote = None;
    for (index, ch) in text.char_indices() {
        if let Some(mark) = quote {
            if ch == mark {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

fn project(resource: &Value, field: &Field) -> Value {
    if field.selection.is_empty() {
        return resource.clone();
    }
    let Some(object) = resource.as_object() else {
        return resource.clone();
    };
    let mut copy = Map::new();
    for name in &field.selection {
        if let Some(value) = object.get(name) {
            copy.insert(name.clone(), value.clone());
        }
    }
    Value::Object(copy)
}

fn project_list(bundle: &Value, field: &Field) -> Value {
    let entries = bundle
        .get("entry")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|entry| entry.get("resource"))
                .map(|resource| project(resource, field))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!(entries)
}
