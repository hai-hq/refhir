use std::collections::BTreeMap;

use refhir_database::Transaction;
use refhir_types::{exact_key, string_interval, token_interval, IndexId, Interval};
use serde_json::{json, Value};

use crate::catalog::{Catalog, ResourceModel};
use crate::error::FhirError;
use crate::extract::Probe;
use crate::ops::decorate;
use crate::resolve::TxHost;

const IGNORED: &[&str] = &["_format", "_pretty"];

pub fn search(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    params: &BTreeMap<String, String>,
) -> Result<Value, FhirError> {
    for name in params.keys() {
        if IGNORED.contains(&name.as_str()) || name == "_id" {
            continue;
        }
        if model.ready_param(name).is_none() {
            return Err(FhirError::Invalid(format!(
                "unsupported search parameter {name} on {}",
                model.name
            )));
        }
    }
    if let Some(query) = params.get("_query") {
        return Err(FhirError::Invalid(format!("unknown query {query}")));
    }
    if let Some(filter) = params.get("_filter") {
        parse_filter(filter).map_err(FhirError::Invalid)?;
    }
    let (index, interval) = choose_index(model, params);
    let rows = tx.index_range(&index, interval)?;
    let host = TxHost::new(tx, catalog);
    let mut entries = Vec::new();
    for row in rows {
        if !matches_params(catalog, model, &row.json, params, &host) {
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

fn choose_index(model: &ResourceModel, params: &BTreeMap<String, String>) -> (IndexId, Interval) {
    if let Some(id) = params.get("_id") {
        return (model.by_id(), Interval::point(exact_key(id)));
    }
    let mut chosen: Option<(u8, &str, Probe<'_>)> = None;
    for (name, value) in params {
        if IGNORED.contains(&name.as_str()) || name == "_filter" || name == "_query" {
            continue;
        }
        let Some(param) = model.ready_param(name) else {
            continue;
        };
        let probe = param.extractor.probe(value);
        let rank = match probe {
            Probe::Token(_) => 0,
            Probe::Prefix(_) => 1,
            Probe::Scan => continue,
        };
        let replace = chosen.map(|(prev, _, _)| rank < prev).unwrap_or(true);
        if replace {
            chosen = Some((rank, name, probe));
        }
    }
    if let Some((_, name, probe)) = chosen {
        let interval = match probe {
            Probe::Token(text) => token_interval(text),
            Probe::Prefix(text) => string_interval(text),
            Probe::Scan => Interval::all(),
        };
        return (model.search_index(name), interval);
    }
    (model.by_id(), Interval::all())
}

fn matches_params(
    catalog: &Catalog,
    model: &ResourceModel,
    resource: &Value,
    params: &BTreeMap<String, String>,
    host: &TxHost<'_>,
) -> bool {
    params.iter().all(|(name, value)| {
        if IGNORED.contains(&name.as_str()) {
            return true;
        }
        if name == "_id" {
            return resource.get("id").and_then(|item| item.as_str()) == Some(value);
        }
        if name == "_query" {
            return false;
        }
        if name == "_filter" {
            return eval_filter(value, catalog, model, resource, host);
        }
        model
            .ready_param(name)
            .map(|param| {
                param
                    .extractor
                    .matches(resource, value, catalog.path_model(), host)
            })
            .unwrap_or(false)
    })
}

#[derive(Debug)]
enum Filter {
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Cmp {
        name: String,
        op: String,
        value: String,
    },
}

fn parse_filter(input: &str) -> Result<Filter, String> {
    let tokens = filter_tokens(input)?;
    let mut parser = FilterParser { tokens, pos: 0 };
    let filter = parser.parse_or()?;
    if parser.pos != parser.tokens.len() {
        return Err("invalid _filter".into());
    }
    Ok(filter)
}

struct FilterParser {
    tokens: Vec<String>,
    pos: usize,
}

impl FilterParser {
    fn parse_or(&mut self) -> Result<Filter, String> {
        let mut parts = vec![self.parse_and()?];
        while self.eat("or") {
            parts.push(self.parse_and()?);
        }
        if parts.len() == 1 {
            Ok(parts.pop().expect("part"))
        } else {
            Ok(Filter::Or(parts))
        }
    }

    fn parse_and(&mut self) -> Result<Filter, String> {
        let mut parts = vec![self.parse_primary()?];
        while self.eat("and") {
            parts.push(self.parse_primary()?);
        }
        if parts.len() == 1 {
            Ok(parts.pop().expect("part"))
        } else {
            Ok(Filter::And(parts))
        }
    }

    fn parse_primary(&mut self) -> Result<Filter, String> {
        if self.eat("(") {
            let inner = self.parse_or()?;
            if !self.eat(")") {
                return Err("invalid _filter".into());
            }
            return Ok(inner);
        }
        let name = self.next().ok_or_else(|| "invalid _filter".to_string())?;
        let op = self.next().ok_or_else(|| "invalid _filter".to_string())?;
        if !is_filter_op(&op) {
            return Err("invalid _filter".into());
        }
        let value = self.next().ok_or_else(|| "invalid _filter".to_string())?;
        Ok(Filter::Cmp { name, op, value })
    }

    fn eat(&mut self, word: &str) -> bool {
        if self.tokens.get(self.pos).map(String::as_str) == Some(word) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn next(&mut self) -> Option<String> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }
}

fn is_filter_op(op: &str) -> bool {
    matches!(
        op,
        "eq" | "ne" | "gt" | "lt" | "ge" | "le" | "sa" | "eb" | "ap" | "co" | "sw" | "ew" | "pr"
    )
}

fn filter_tokens(input: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '(' || ch == ')' {
            tokens.push(ch.to_string());
            continue;
        }
        if ch == '"' {
            let mut text = String::new();
            let mut closed = false;
            for item in chars.by_ref() {
                if item == '"' {
                    closed = true;
                    break;
                }
                text.push(item);
            }
            if !closed {
                return Err("invalid _filter".into());
            }
            tokens.push(text);
            continue;
        }
        let mut text = String::from(ch);
        while let Some(item) = chars.peek() {
            if item.is_whitespace() || *item == '(' || *item == ')' {
                break;
            }
            text.push(chars.next().expect("peeked"));
        }
        tokens.push(text);
    }
    Ok(tokens)
}

fn eval_filter(
    input: &str,
    catalog: &Catalog,
    model: &ResourceModel,
    resource: &Value,
    host: &TxHost<'_>,
) -> bool {
    let Ok(filter) = parse_filter(input) else {
        return false;
    };
    eval_filter_expr(&filter, catalog, model, resource, host)
}

fn eval_filter_expr(
    filter: &Filter,
    catalog: &Catalog,
    model: &ResourceModel,
    resource: &Value,
    host: &TxHost<'_>,
) -> bool {
    match filter {
        Filter::And(parts) => parts
            .iter()
            .all(|part| eval_filter_expr(part, catalog, model, resource, host)),
        Filter::Or(parts) => parts
            .iter()
            .any(|part| eval_filter_expr(part, catalog, model, resource, host)),
        Filter::Cmp { name, op, value } => model
            .ready_param(name)
            .map(|param| {
                param
                    .extractor
                    .matches_op(resource, op, value, catalog.path_model(), host)
            })
            .unwrap_or(false),
    }
}
