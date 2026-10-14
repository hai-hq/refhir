//! FHIR JSON at the edges of the document log. The engine stays JSON.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

pub fn to_json(text: &str) -> Result<Value, String> {
    let text = text.trim();
    let text = text
        .strip_prefix("<?xml")
        .and_then(|rest| rest.split_once("?>").map(|(_, rest)| rest))
        .unwrap_or(text)
        .trim();
    let (value, rest) = parse_element(text)?;
    if !rest.trim().is_empty() {
        return Err("trailing xml".into());
    }
    Ok(value)
}

pub fn from_json(value: &Value) -> String {
    let Some(object) = value.as_object() else {
        return String::new();
    };
    let name = object
        .get("resourceType")
        .and_then(|item| item.as_str())
        .unwrap_or("Resource");
    let mut out = format!("<{name} xmlns=\"http://hl7.org/fhir\">");
    for (key, child) in object {
        if key == "resourceType" {
            continue;
        }
        write_child(&mut out, key, child);
    }
    out.push_str(&format!("</{name}>"));
    out
}

fn write_child(out: &mut String, name: &str, value: &Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                write_child(out, name, item);
            }
        }
        Value::Object(map) => {
            if let Some(div) = map.get("div").and_then(|item| item.as_str()) {
                out.push_str(&format!(
                    "<{name}><div xmlns=\"http://www.w3.org/1999/xhtml\">{div}</div></{name}>"
                ));
                return;
            }
            out.push('<');
            out.push_str(name);
            out.push('>');
            for (key, child) in map {
                write_child(out, key, child);
            }
            out.push_str("</");
            out.push_str(name);
            out.push('>');
        }
        Value::String(text) => {
            out.push_str(&format!("<{name} value=\"{}\"/>", escape(text)));
        }
        Value::Bool(value) => {
            out.push_str(&format!("<{name} value=\"{value}\"/>"));
        }
        Value::Number(value) => {
            out.push_str(&format!("<{name} value=\"{value}\"/>"));
        }
        Value::Null => {}
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

fn parse_element(input: &str) -> Result<(Value, &str), String> {
    let input = input.trim_start();
    let input = input
        .strip_prefix('<')
        .ok_or_else(|| "expected an element".to_string())?;
    if input.starts_with('/') || input.starts_with('!') || input.starts_with('?') {
        return Err("unexpected element".into());
    }
    let (name, input) = take_name(input)?;
    let (attrs, input) = take_attrs(input)?;
    if let Some(rest) = input.strip_prefix("/>") {
        return Ok((element_value(name, &attrs, &Map::new()), rest));
    }
    let input = input
        .strip_prefix('>')
        .ok_or_else(|| "expected >".to_string())?;
    let mut children: Map<String, Value> = Map::new();
    let mut input = input;
    loop {
        let trimmed = input.trim_start();
        if let Some(closing) = trimmed.strip_prefix("</") {
            let trimmed = closing.trim_start();
            let (end, rest) = take_name(trimmed)?;
            if end != name {
                return Err(format!("expected </{name}>"));
            }
            let rest = rest.trim_start().strip_prefix('>').unwrap_or(rest);
            return Ok((element_value(name, &attrs, &children), rest));
        }
        if trimmed.starts_with('<') {
            let (child, rest) = parse_element(trimmed)?;
            let child_name = child
                .get("resourceType")
                .and_then(|value| value.as_str())
                .unwrap_or("child")
                .to_string();
            let primitive = child.get("value").is_some()
                && child
                    .as_object()
                    .is_some_and(|map| map.len() == 2 && map.contains_key("resourceType"));
            let stored = if primitive {
                child.get("value").cloned().unwrap_or(child)
            } else {
                child
            };
            push_child(&mut children, &child_name, stored);
            input = rest;
            continue;
        }
        return Err("expected a child or a close tag".into());
    }
}

fn element_value(
    name: &str,
    attrs: &BTreeMap<String, String>,
    children: &Map<String, Value>,
) -> Value {
    if let Some(value) = attrs.get("value") {
        let parsed = match value.as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            other => Value::String(other.to_string()),
        };
        if name
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase())
            && children.is_empty()
        {
            return json!({"resourceType": name, "value": parsed});
        }
        return json!({"resourceType": name, "value": parsed});
    }
    if name
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
    {
        let mut object = Map::new();
        object.insert("resourceType".into(), Value::String(name.to_string()));
        for (key, value) in children {
            object.insert(key.clone(), value.clone());
        }
        return Value::Object(object);
    }
    if children.is_empty() {
        return Value::Object(Map::new());
    }
    Value::Object(children.clone())
}

fn push_child(children: &mut Map<String, Value>, name: &str, value: Value) {
    let name = if name == "child" {
        value
            .get("resourceType")
            .and_then(|item| item.as_str())
            .unwrap_or(name)
            .to_string()
    } else {
        name.to_string()
    };
    match children.get_mut(&name) {
        Some(Value::Array(items)) => items.push(value),
        Some(existing) => {
            let previous = existing.clone();
            *existing = json!([previous, value]);
        }
        None => {
            children.insert(name, value);
        }
    }
}

fn take_name(input: &str) -> Result<(&str, &str), String> {
    let end = input
        .find(|ch: char| ch.is_whitespace() || ch == '>' || ch == '/' || ch == '=')
        .ok_or_else(|| "bad element name".to_string())?;
    Ok((&input[..end], &input[end..]))
}

fn take_attrs(input: &str) -> Result<(BTreeMap<String, String>, &str), String> {
    let mut attrs = BTreeMap::new();
    let mut input = input;
    loop {
        input = input.trim_start();
        if input.starts_with('>') || input.starts_with("/>") || input.is_empty() {
            return Ok((attrs, input));
        }
        let (name, rest) = take_name(input)?;
        let rest = rest.trim_start();
        let rest = rest
            .strip_prefix('=')
            .ok_or_else(|| "expected =".to_string())?;
        let rest = rest.trim_start();
        let quote = rest
            .chars()
            .next()
            .ok_or_else(|| "expected a quoted attribute".to_string())?;
        if quote != '"' && quote != '\'' {
            return Err("expected a quoted attribute".into());
        }
        let rest = &rest[1..];
        let end = rest
            .find(quote)
            .ok_or_else(|| "unterminated attribute".to_string())?;
        attrs.insert(name.to_string(), rest[..end].to_string());
        input = &rest[end + 1..];
    }
}
