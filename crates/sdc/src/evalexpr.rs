//! FHIRPath and CQL evaluation in a questionnaire variable scope.

use std::cell::RefCell;
use std::collections::BTreeMap;

use refhir_cql::{run, DataSource};
use refhir_fhirpath::{evaluate, parse, EmptyHost, Error, Host, Value};
use serde_json::{json, Value as Json};

use crate::chart::Chart;
use crate::ext::{expression_name, expression_of, value_of, LIBRARY};

pub struct ChartHost<'a> {
    pub chart: RefCell<&'a mut dyn Chart>,
    pub vars: BTreeMap<String, Vec<Value>>,
}

impl Host for ChartHost<'_> {
    fn resolve(&self, reference: &str) -> Result<Option<Json>, Error> {
        let Some((type_name, id)) = reference.rsplit_once('/') else {
            return Ok(None);
        };
        if type_name.is_empty()
            || id.is_empty()
            || !type_name.chars().next().unwrap().is_ascii_uppercase()
        {
            return Ok(None);
        }
        self.chart
            .borrow_mut()
            .read_type(type_name, id)
            .map_err(Error::eval)
    }

    fn lookup_variable(&self, name: &str) -> Result<Option<Vec<Value>>, Error> {
        Ok(self.vars.get(name).cloned())
    }

    fn now(&self) -> refhir_fhirpath::Temporal {
        EmptyHost.now()
    }
}

struct ChartSource<'a> {
    chart: &'a RefCell<&'a mut dyn Chart>,
}

impl DataSource for ChartSource<'_> {
    fn retrieve(&mut self, type_name: &str, code: Option<&str>) -> Result<Vec<Json>, String> {
        let params = match code {
            Some(code) => vec![("code", code.to_string())],
            None => Vec::new(),
        };
        self.chart.borrow_mut().search_type(type_name, &params)
    }
}

pub fn json_value(value: &Json) -> Value {
    Value::Node {
        json: value.clone(),
        fhir_type: value
            .get("resourceType")
            .and_then(|item| item.as_str())
            .map(|item| item.to_string()),
        path: String::new(),
        extensions: Vec::new(),
    }
}

pub fn value_json(value: &Value) -> Json {
    match value {
        Value::Boolean { value, .. } => json!(value),
        Value::Integer(value) => json!(value),
        Value::Decimal { value, .. } => json!(value.to_string().parse::<f64>().unwrap_or(0.0)),
        Value::String(value) => json!(value),
        Value::Date(_) | Value::DateTime(_) | Value::Time(_) => {
            json!(value
                .render()
                .trim_start_matches('@')
                .trim_start_matches('T'))
        }
        Value::Quantity { value, unit, .. } => json!({
            "value": value.to_string().parse::<f64>().unwrap_or(0.0),
            "unit": unit,
            "system": "http://unitsofmeasure.org",
            "code": unit
        }),
        Value::Node { json, .. } => json.clone(),
        Value::TypeInfo { name, .. } => json!(name),
    }
}

pub fn eval_text(
    chart: &mut dyn Chart,
    language: &str,
    expression: &str,
    root: &Json,
    vars: &BTreeMap<String, Vec<Value>>,
) -> Result<Vec<Json>, String> {
    let expression = substitute(chart, expression, root, vars)?;
    if is_query(&expression) {
        return run_query(chart, &expression);
    }
    if language.contains("cql") {
        let mut params = BTreeMap::new();
        for (name, values) in vars {
            if let Some(first) = values.first() {
                params.insert(name.clone(), value_json(first));
            }
        }
        let mut source = ChartSource {
            chart: &RefCell::new(chart),
        };
        // The RefCell above borrows chart for the whole call. `run` only uses data.
        return run(&expression, "", root, &params, &mut source);
    }
    let model = chart.path_model();
    let host = ChartHost {
        chart: RefCell::new(chart),
        vars: vars.clone(),
    };
    let expr = parse(&expression).map_err(|err| err.to_string())?;
    let values = evaluate(&expr, root, &model, &host).map_err(|err| err.to_string())?;
    Ok(values.iter().map(value_json).collect())
}

pub fn bind_variables(
    chart: &mut dyn Chart,
    node: &Json,
    root: &Json,
    vars: &mut BTreeMap<String, Vec<Value>>,
) -> Result<(), String> {
    for ext in crate::ext::extensions(node, crate::ext::VARIABLE) {
        let Some(name) = expression_name(ext).or_else(|| {
            crate::ext::nested(ext, "name")
                .and_then(value_of)
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        }) else {
            continue;
        };
        let Some((language, expression)) = expression_of(ext) else {
            continue;
        };
        let values = eval_text(chart, language, expression, root, vars)?;
        vars.insert(name, values.iter().map(json_value).collect());
    }
    Ok(())
}

pub fn library_expression(
    chart: &mut dyn Chart,
    questionnaire: &Json,
    name: &str,
    root: &Json,
    vars: &BTreeMap<String, Vec<Value>>,
) -> Result<Option<Vec<Json>>, String> {
    for ext in crate::ext::extensions(questionnaire, LIBRARY) {
        let Some(canonical) = value_of(ext).and_then(|value| value.as_str()) else {
            continue;
        };
        let Some(library) = find_canonical(chart, "Library", canonical)? else {
            continue;
        };
        for content in library
            .get("content")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
        {
            let mime = content
                .get("contentType")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if !mime.contains("cql") && !mime.contains("elm") {
                continue;
            }
            let Some(text) = decode_attachment(content) else {
                continue;
            };
            let mut params = BTreeMap::new();
            for (key, values) in vars {
                if let Some(first) = values.first() {
                    params.insert(key.clone(), value_json(first));
                }
            }
            let mut source = ChartSource {
                chart: &RefCell::new(chart),
            };
            let values = run(&text, name, root, &params, &mut source)?;
            if !values.is_empty() {
                return Ok(Some(values));
            }
        }
    }
    Ok(None)
}

pub fn find_canonical(
    chart: &mut dyn Chart,
    type_name: &str,
    canonical: &str,
) -> Result<Option<Json>, String> {
    if let Some(_id) = canonical.strip_prefix('#') {
        return Ok(None);
    }
    if let Some((type_name, id)) = canonical.split_once('/') {
        if !canonical.contains(':')
            && type_name
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_uppercase())
        {
            return chart.read_type(type_name, id);
        }
    }
    let rows = chart.search_type(type_name, &[("url", canonical.to_string())])?;
    Ok(rows.into_iter().next())
}

fn substitute(
    chart: &mut dyn Chart,
    expression: &str,
    root: &Json,
    vars: &BTreeMap<String, Vec<Value>>,
) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = expression;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start + 2..].find("}}") else {
            return Err("unclosed {{ in expression".into());
        };
        let inner = &rest[start + 2..start + 2 + end];
        let model = chart.path_model();
        let host = ChartHost {
            chart: RefCell::new(chart),
            vars: vars.clone(),
        };
        let expr = parse(inner).map_err(|err| err.to_string())?;
        let values = evaluate(&expr, root, &model, &host).unwrap_or_default();
        if let Some(first) = values.first() {
            out.push_str(
                &value_json(first)
                    .as_str()
                    .unwrap_or(&first.render())
                    .replace('"', ""),
            );
        }
        rest = &rest[start + 2 + end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn is_query(expression: &str) -> bool {
    let expression = expression.trim();
    let Some((type_name, rest)) = expression.split_once('?') else {
        return false;
    };
    !type_name.is_empty()
        && !type_name.contains(' ')
        && type_name
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase())
        && !rest.contains('\n')
}

fn run_query(chart: &mut dyn Chart, expression: &str) -> Result<Vec<Json>, String> {
    let (type_name, query) = expression.split_once('?').unwrap();
    let mut params = Vec::new();
    if !query.is_empty() {
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            params.push((name, value.to_string()));
        }
    }
    let owned: Vec<(&str, String)> = params
        .iter()
        .map(|(name, value)| (*name, value.clone()))
        .collect();
    chart.search_type(type_name, &owned)
}

fn decode_attachment(content: &Json) -> Option<String> {
    let data = content.get("data").and_then(|value| value.as_str())?;
    if data.trim_start().starts_with('{') || data.contains("define") || data.contains(' ') {
        return Some(data.to_string());
    }
    let bytes = base64_decode(data).ok()?;
    String::from_utf8(bytes).ok()
}

fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let text: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in bytes {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return Err("bad base64".into()),
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Ok(out)
}
