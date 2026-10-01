//! Compile a SearchParameter expression into index keys.
//!
//! The expression is FHIRPath. `memberOf()` stays unsupported until a
//! terminology service is configured. The indexed bytes are still the token,
//! string, reference, and date keys this engine already stores.

use haifhir_fhirpath::{Expr, Host, Model};
use haifhir_types::{string_key, token_key, IndexId};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchKind {
    Token,
    String,
    Reference,
    Date,
}

#[derive(Clone, Debug)]
pub struct Extractor {
    expr: Expr,
    kind: SearchKind,
}

impl Extractor {
    pub fn kind(&self) -> SearchKind {
        self.kind
    }

    pub fn keys(
        &self,
        resource: &Value,
        id: &str,
        index: &IndexId,
        path_model: &Model,
        host: &dyn Host,
    ) -> Vec<(IndexId, Vec<u8>)> {
        let values = self.values(resource, path_model, host);
        match self.kind {
            SearchKind::Token | SearchKind::Reference => token_rows(index, id, &values),
            SearchKind::String | SearchKind::Date => string_rows(index, id, &values),
        }
    }

    pub fn matches(
        &self,
        resource: &Value,
        expected: &str,
        path_model: &Model,
        host: &dyn Host,
    ) -> bool {
        let values = self.values(resource, path_model, host);
        match self.kind {
            SearchKind::Token | SearchKind::Reference => values.iter().any(|token| token == expected),
            SearchKind::String => {
                let prefix = expected.to_lowercase();
                values
                    .iter()
                    .any(|text| text.to_lowercase().starts_with(&prefix))
            }
            SearchKind::Date => values.iter().any(|text| text.starts_with(expected)),
        }
    }

    fn values(&self, resource: &Value, path_model: &Model, host: &dyn Host) -> Vec<String> {
        let Ok(items) = haifhir_fhirpath::evaluate(&self.expr, resource, path_model, host) else {
            return Vec::new();
        };
        let mut values = Vec::new();
        for item in items {
            values.extend(encode_value(self.kind, &item));
        }
        values
    }
}

pub fn compile(param_type: &str, expression: Option<&str>) -> Option<Extractor> {
    let kind = search_kind(param_type)?;
    let expression = expression?.trim();
    let expr = haifhir_fhirpath::parse(expression).ok()?;
    if expr.calls("memberOf") {
        return None;
    }
    Some(Extractor { expr, kind })
}

fn search_kind(param_type: &str) -> Option<SearchKind> {
    Some(match param_type {
        "token" => SearchKind::Token,
        "string" => SearchKind::String,
        "reference" => SearchKind::Reference,
        "date" => SearchKind::Date,
        _ => return None,
    })
}

fn encode_value(kind: SearchKind, item: &haifhir_fhirpath::Value) -> Vec<String> {
    let json = match item {
        haifhir_fhirpath::Value::Node { json, .. } => json,
        haifhir_fhirpath::Value::String(text) => return vec![text.clone()],
        haifhir_fhirpath::Value::Boolean { value, .. } => return vec![value.to_string()],
        haifhir_fhirpath::Value::Date(_) | haifhir_fhirpath::Value::DateTime(_) => {
            return vec![item.render().trim_start_matches('@').to_string()];
        }
        _ => return Vec::new(),
    };
    match kind {
        SearchKind::Token => token_json(json),
        SearchKind::String => string_json(json),
        SearchKind::Reference => reference_tokens(json, &[]),
        SearchKind::Date => date_json(json),
    }
}

fn token_json(json: &Value) -> Vec<String> {
    if json.as_bool().is_some() {
        return boolean_tokens(json, &[]);
    }
    if json.get("value").and_then(|item| item.as_str()).is_some() {
        return identifier_tokens(json, &[]);
    }
    if json.get("coding").is_some() || json.get("code").is_some() {
        return coding_tokens(json, &[]);
    }
    string_tokens(json, &[])
}

fn string_json(json: &Value) -> Vec<String> {
    if json.get("family").is_some() || json.get("given").is_some() || json.get("text").is_some() {
        let names = name_strings(json, &[]);
        if !names.is_empty() {
            return names;
        }
    }
    string_tokens(json, &[])
}

fn date_json(json: &Value) -> Vec<String> {
    if let Some(text) = json.as_str() {
        return vec![text.to_string()];
    }
    let mut values = Vec::new();
    if let Some(start) = json.get("start").and_then(|item| item.as_str()) {
        values.push(start.to_string());
    }
    if let Some(end) = json.get("end").and_then(|item| item.as_str()) {
        values.push(end.to_string());
    }
    values
}

fn token_rows(index: &IndexId, id: &str, tokens: &[String]) -> Vec<(IndexId, Vec<u8>)> {
    tokens
        .iter()
        .map(|token| (index.clone(), token_key(token, id)))
        .collect()
}

fn string_rows(index: &IndexId, id: &str, values: &[String]) -> Vec<(IndexId, Vec<u8>)> {
    values
        .iter()
        .map(|value| (index.clone(), string_key(value, id)))
        .collect()
}

fn identifier_tokens(resource: &Value, path: &[String]) -> Vec<String> {
    let mut tokens = Vec::new();
    for node in nodes_at(resource, path) {
        let Some(value) = node.get("value").and_then(|item| item.as_str()) else {
            continue;
        };
        tokens.push(value.to_string());
        if let Some(system) = node.get("system").and_then(|item| item.as_str()) {
            tokens.push(format!("{system}|{value}"));
        }
    }
    tokens
}

fn coding_tokens(resource: &Value, path: &[String]) -> Vec<String> {
    let mut tokens = Vec::new();
    for node in nodes_at(resource, path) {
        if let Some(codings) = node.get("coding").and_then(|item| item.as_array()) {
            for coding in codings {
                push_coding(&mut tokens, coding);
            }
            continue;
        }
        push_coding(&mut tokens, node);
    }
    tokens
}

fn push_coding(tokens: &mut Vec<String>, node: &Value) {
    let Some(code) = node.get("code").and_then(|item| item.as_str()) else {
        return;
    };
    tokens.push(code.to_string());
    if let Some(system) = node.get("system").and_then(|item| item.as_str()) {
        tokens.push(format!("{system}|{code}"));
    }
}

fn boolean_tokens(resource: &Value, path: &[String]) -> Vec<String> {
    nodes_at(resource, path)
        .into_iter()
        .filter_map(|node| node.as_bool())
        .map(|value| {
            if value {
                "true".to_string()
            } else {
                "false".to_string()
            }
        })
        .collect()
}

fn string_tokens(resource: &Value, path: &[String]) -> Vec<String> {
    nodes_at(resource, path)
        .into_iter()
        .filter_map(|node| node.as_str().map(|text| text.to_string()))
        .collect()
}

fn name_strings(resource: &Value, path: &[String]) -> Vec<String> {
    let mut parts = Vec::new();
    for node in nodes_at(resource, path) {
        let Some(name) = node.as_object() else {
            continue;
        };
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
    }
    parts
}

fn reference_tokens(resource: &Value, path: &[String]) -> Vec<String> {
    nodes_at(resource, path)
        .into_iter()
        .filter_map(|node| {
            node.get("reference")
                .and_then(|item| item.as_str())
                .or_else(|| node.as_str())
                .map(|text| text.to_string())
        })
        .collect()
}

fn nodes_at<'a>(root: &'a Value, path: &[String]) -> Vec<&'a Value> {
    let mut current = vec![root];
    for segment in path {
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
    let mut flat = Vec::new();
    for value in current {
        if let Value::Array(items) = value {
            flat.extend(items.iter());
        } else {
            flat.push(value);
        }
    }
    flat
}

