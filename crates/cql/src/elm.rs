//! ELM JSON evaluator for the expressions the compiler emits and for Library content.

use std::collections::BTreeMap;

use serde_json::{json, Value as Json};

use crate::DataSource;

pub fn evaluate(
    elm: &Json,
    name: &str,
    context: &Json,
    params: &BTreeMap<String, Json>,
    data: &mut dyn DataSource,
) -> Result<Vec<Json>, String> {
    let statements = elm
        .pointer("/library/statements/def")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let expression = statements
        .iter()
        .find(|item| item.get("name").and_then(|value| value.as_str()) == Some(name))
        .or_else(|| statements.first())
        .and_then(|item| item.get("expression"))
        .cloned()
        .ok_or_else(|| format!("cql statement {name} was not found"))?;
    let mut env = Env {
        context,
        params,
        statements,
        data,
        locals: BTreeMap::new(),
    };
    env.eval(&expression)
}

struct Env<'a> {
    context: &'a Json,
    params: &'a BTreeMap<String, Json>,
    statements: Vec<Json>,
    data: &'a mut dyn DataSource,
    locals: BTreeMap<String, Vec<Json>>,
}

impl Env<'_> {
    fn eval(&mut self, expr: &Json) -> Result<Vec<Json>, String> {
        let kind = expr
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        match kind {
            "Literal" => Ok(vec![literal(expr)]),
            "Null" => Ok(vec![]),
            "Identifier" | "AliasRef" | "OperandRef" => self.identifier(expr),
            "ParameterRef" => {
                let name = expr.get("name").and_then(|value| value.as_str()).unwrap_or("");
                Ok(self
                    .params
                    .get(name)
                    .cloned()
                    .map(|value| vec![value])
                    .unwrap_or_default())
            }
            "ExpressionRef" => self.expression_ref(expr),
            "Property" => self.property(expr),
            "Retrieve" => self.retrieve(expr),
            "Query" => self.query(expr),
            "If" => self.if_expr(expr),
            "And" | "Or" | "Not" | "Equal" | "NotEqual" | "Greater" | "Less"
            | "GreaterOrEqual" | "LessOrEqual" | "Add" | "Subtract" | "Multiply" | "Divide"
            | "Concatenate" => self.operator(kind, expr),
            "Exists" | "Count" | "First" | "Last" | "ToString" | "ToInteger" | "ToDecimal"
            | "ToBoolean" | "Today" | "Now" | "exists" | "count" | "first" | "last"
            | "toString" | "toInteger" | "toDecimal" => self.function(kind, expr),
            "List" => {
                let mut items = Vec::new();
                for item in expr.get("element").and_then(|value| value.as_array()).into_iter().flatten()
                {
                    items.extend(self.eval(item)?);
                }
                Ok(items)
            }
            other => Err(format!("unsupported ELM expression {other}")),
        }
    }

    fn identifier(&self, expr: &Json) -> Result<Vec<Json>, String> {
        let name = expr.get("name").and_then(|value| value.as_str()).unwrap_or("");
        let name = name.trim_start_matches('%');
        if let Some(value) = self.locals.get(name) {
            return Ok(value.clone());
        }
        if let Some(value) = self.params.get(name) {
            return Ok(vec![value.clone()]);
        }
        if self.context.get("resourceType").and_then(|value| value.as_str()) == Some(name) {
            return Ok(vec![self.context.clone()]);
        }
        if let Some(value) = self.context.get(name) {
            return Ok(list_of(value));
        }
        Ok(vec![])
    }

    fn expression_ref(&mut self, expr: &Json) -> Result<Vec<Json>, String> {
        let name = expr.get("name").and_then(|value| value.as_str()).unwrap_or("");
        let expression = self
            .statements
            .iter()
            .find(|item| item.get("name").and_then(|value| value.as_str()) == Some(name))
            .and_then(|item| item.get("expression"))
            .cloned()
            .ok_or_else(|| format!("unknown CQL expression {name}"))?;
        self.eval(&expression)
    }

    fn property(&mut self, expr: &Json) -> Result<Vec<Json>, String> {
        let path = expr.get("path").and_then(|value| value.as_str()).unwrap_or("");
        let source = if let Some(source) = expr.get("source") {
            self.eval(source)?
        } else if let Some(scope) = expr.get("scope").and_then(|value| value.as_str()) {
            self.identifier(&json!({"name": scope}))?
        } else {
            vec![self.context.clone()]
        };
        let mut found = Vec::new();
        for item in source {
            found.extend(read_path(&item, path));
        }
        Ok(found)
    }

    fn retrieve(&mut self, expr: &Json) -> Result<Vec<Json>, String> {
        let data_type = expr
            .get("dataType")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let type_name = data_type
            .rsplit(['}', '.'])
            .next()
            .unwrap_or(data_type)
            .to_string();
        let code = expr
            .get("code")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string());
        self.data.retrieve(&type_name, code.as_deref())
    }

    fn query(&mut self, expr: &Json) -> Result<Vec<Json>, String> {
        let sources = expr
            .get("source")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        let mut rows = vec![json!({})];
        for source in &sources {
            let alias = source
                .get("alias")
                .and_then(|value| value.as_str())
                .unwrap_or("row");
            let expression = source.get("expression").cloned().unwrap_or(json!({}));
            let values = self.eval(&expression)?;
            let mut next = Vec::new();
            for row in &rows {
                for value in &values {
                    let mut copy = row.clone();
                    copy[alias] = value.clone();
                    next.push(copy);
                }
            }
            rows = next;
        }
        if let Some(filter) = expr.get("where") {
            let mut kept = Vec::new();
            for row in rows {
                self.locals.clear();
                if let Some(object) = row.as_object() {
                    for (key, value) in object {
                        self.locals.insert(key.clone(), vec![value.clone()]);
                    }
                }
                let answer = self.eval(filter)?;
                if truth(&answer) == Some(true) {
                    kept.push(row);
                }
            }
            rows = kept;
            self.locals.clear();
        }
        let mut result = Vec::new();
        let ret = expr.get("return");
        for row in rows {
            self.locals.clear();
            if let Some(object) = row.as_object() {
                for (key, value) in object {
                    self.locals.insert(key.clone(), vec![value.clone()]);
                }
            }
            if let Some(ret) = ret {
                result.extend(self.eval(ret)?);
            } else if let Some(object) = row.as_object() {
                if let Some((_, value)) = object.iter().next() {
                    result.push(value.clone());
                }
            }
        }
        self.locals.clear();
        Ok(result)
    }

    fn if_expr(&mut self, expr: &Json) -> Result<Vec<Json>, String> {
        let condition = self.eval(expr.get("condition").unwrap_or(&json!({})))?;
        if truth(&condition) == Some(true) {
            self.eval(expr.get("then").unwrap_or(&json!({})))
        } else {
            self.eval(expr.get("else").unwrap_or(&json!({})))
        }
    }

    fn operator(&mut self, kind: &str, expr: &Json) -> Result<Vec<Json>, String> {
        if kind == "Not" {
            let operand = self.eval(expr.get("operand").unwrap_or(&json!({})))?;
            return Ok(match truth(&operand) {
                Some(value) => vec![json!(!value)],
                None => vec![],
            });
        }
        let operands = expr
            .get("operand")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        if kind == "And" || kind == "Or" {
            let mut acc = if kind == "And" { Some(true) } else { Some(false) };
            for operand in &operands {
                let value = self.eval(operand)?;
                let bit = truth(&value);
                acc = if kind == "And" {
                    match (acc, bit) {
                        (Some(false), _) | (_, Some(false)) => Some(false),
                        (None, _) | (_, None) => None,
                        _ => Some(true),
                    }
                } else {
                    match (acc, bit) {
                        (Some(true), _) | (_, Some(true)) => Some(true),
                        (None, _) | (_, None) => None,
                        _ => Some(false),
                    }
                };
            }
            return Ok(acc.map(|value| vec![json!(value)]).unwrap_or_default());
        }
        let mut values = Vec::new();
        for operand in &operands {
            values.push(self.eval(operand)?);
        }
        let left = values.first().cloned().unwrap_or_default();
        let right = values.get(1).cloned().unwrap_or_default();
        match kind {
            "Equal" | "NotEqual" | "Greater" | "Less" | "GreaterOrEqual" | "LessOrEqual" => {
                Ok(compare(kind, &left, &right))
            }
            "Add" | "Subtract" | "Multiply" | "Divide" => Ok(math(kind, &left, &right)),
            "Concatenate" => {
                let text: String = left
                    .iter()
                    .chain(right.iter())
                    .filter_map(as_text)
                    .collect();
                Ok(vec![json!(text)])
            }
            _ => Err(format!("unsupported operator {kind}")),
        }
    }

    fn function(&mut self, kind: &str, expr: &Json) -> Result<Vec<Json>, String> {
        let kind = match kind {
            "exists" => "Exists",
            "count" => "Count",
            "first" => "First",
            "last" => "Last",
            "toString" => "ToString",
            "toInteger" => "ToInteger",
            "toDecimal" => "ToDecimal",
            other => other,
        };
        if kind == "Today" || kind == "Now" {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let days = (stamp / 86_400) as i64;
            let (year, month, day) = civil(days);
            let date = format!("{year:04}-{month:02}-{day:02}");
            return Ok(vec![json!(date)]);
        }
        let mut operand = if let Some(source) = expr.get("source") {
            self.eval(source)?
        } else {
            vec![]
        };
        if let Some(items) = expr.get("operand").and_then(|value| value.as_array()) {
            for item in items {
                operand.extend(self.eval(item)?);
            }
        }
        match kind {
            "Exists" => Ok(vec![json!(!operand.is_empty())]),
            "Count" => Ok(vec![json!(operand.len() as i64)]),
            "First" => Ok(operand.into_iter().next().into_iter().collect()),
            "Last" => Ok(operand.into_iter().next_back().into_iter().collect()),
            "ToString" => Ok(operand.first().and_then(as_text).map(|text| json!(text)).into_iter().collect()),
            "ToInteger" => Ok(operand
                .first()
                .and_then(as_number)
                .map(|value| json!(value as i64))
                .into_iter()
                .collect()),
            "ToDecimal" | "ToBoolean" => Ok(operand),
            _ => Err(format!("unsupported function {kind}")),
        }
    }
}

fn literal(expr: &Json) -> Json {
    if let Some(value) = expr.get("value") {
        if expr.get("valueType").and_then(|value| value.as_str()) == Some("Decimal") {
            if let Some(text) = value.as_str() {
                if let Ok(number) = text.parse::<f64>() {
                    return json!(number);
                }
            }
        }
        return value.clone();
    }
    json!(null)
}

fn read_path(value: &Json, path: &str) -> Vec<Json> {
    if path.is_empty() {
        return vec![value.clone()];
    }
    let mut current = vec![value.clone()];
    for segment in path.split('.') {
        let mut next = Vec::new();
        for item in current {
            match item.get(segment) {
                Some(Json::Array(items)) => next.extend(items.clone()),
                Some(child) => next.push(child.clone()),
                None => {}
            }
        }
        current = next;
    }
    current
}

fn list_of(value: &Json) -> Vec<Json> {
    match value {
        Json::Array(items) => items.clone(),
        other => vec![other.clone()],
    }
}

fn truth(values: &[Json]) -> Option<bool> {
    match values {
        [] => None,
        [Json::Bool(value), ..] => Some(*value),
        [other, ..] if other.is_null() => None,
        _ => Some(true),
    }
}

fn as_number(value: &Json) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|value| value as f64))
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

fn as_text(value: &Json) -> Option<String> {
    value
        .as_str()
        .map(|text| text.to_string())
        .or_else(|| value.as_i64().map(|value| value.to_string()))
        .or_else(|| value.as_bool().map(|value| value.to_string()))
        .or_else(|| value.as_f64().map(|value| value.to_string()))
}

fn compare(kind: &str, left: &[Json], right: &[Json]) -> Vec<Json> {
    let Some(left) = left.first() else {
        return vec![];
    };
    let Some(right) = right.first() else {
        return vec![];
    };
    let answer = if let (Some(left), Some(right)) = (as_number(left), as_number(right)) {
        match kind {
            "Equal" => left == right,
            "NotEqual" => left != right,
            "Greater" => left > right,
            "Less" => left < right,
            "GreaterOrEqual" => left >= right,
            "LessOrEqual" => left <= right,
            _ => false,
        }
    } else {
        let left = left.to_string();
        let right = right.to_string();
        match kind {
            "Equal" => left == right,
            "NotEqual" => left != right,
            "Greater" => left > right,
            "Less" => left < right,
            "GreaterOrEqual" => left >= right,
            "LessOrEqual" => left <= right,
            _ => false,
        }
    };
    vec![json!(answer)]
}

fn math(kind: &str, left: &[Json], right: &[Json]) -> Vec<Json> {
    let Some(left) = left.first().and_then(as_number) else {
        return vec![];
    };
    let Some(right) = right.first().and_then(as_number) else {
        return vec![];
    };
    let value = match kind {
        "Add" => left + right,
        "Subtract" => left - right,
        "Multiply" => left * right,
        "Divide" if right != 0.0 => left / right,
        _ => return vec![],
    };
    if value.fract() == 0.0 {
        vec![json!(value as i64)]
    } else {
        vec![json!(value)]
    }
}

fn civil(days: i64) -> (i32, u8, u8) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u8;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}
