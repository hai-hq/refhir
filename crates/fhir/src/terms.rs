//! Terminology operations read the in-memory index. They do not write the document log.

use serde_json::{json, Value};

use refhir_validate::{ExpansionFailure, MemoryTerminology};

use crate::catalog::Catalog;
use crate::error::FhirError;

pub fn operation(
    catalog: &Catalog,
    type_name: &str,
    op: &str,
    args: &Value,
) -> Result<Value, FhirError> {
    let terms = catalog.terms();
    match (type_name, op) {
        ("ValueSet", "$expand") => {
            let url = required(args, &["url"])?;
            let filter = optional(args, &["filter"]);
            let offset = optional_usize(args, &["offset"]).unwrap_or(0);
            let count = optional_usize(args, &["count"]);
            terms
                .expand_page(url, filter, offset, count)
                .ok_or_else(|| not_expandable(terms, url))
        }
        ("ValueSet", "$expandability") => Ok(expandability(terms, &all_urls(args))),
        ("ValueSet", "$validate-code") => {
            let url = required(args, &["url"])?;
            let (system, code, display) = code_parts(args)?;
            let system_version = optional(args, &["systemVersion", "system-version"]);
            Ok(terms.validate_code(
                Some(url),
                system.as_deref(),
                system_version,
                &code,
                display.as_deref(),
            ))
        }
        ("CodeSystem", "$validate-code") => {
            let (system, code, display) = code_parts(args)?;
            let system = optional(args, &["system"])
                .or_else(|| optional(args, &["url"]))
                .map(str::to_string)
                .or(system)
                .ok_or_else(|| FhirError::Invalid("system is required".into()))?;
            // `version` is the version of the code system, which this server keeps side by side.
            let version = optional(args, &["version"]);
            Ok(terms.validate_code(None, Some(&system), version, &code, display.as_deref()))
        }
        ("CodeSystem", "$lookup") => {
            let (system, code, _) = code_parts(args)?;
            let system = optional(args, &["system"])
                .or_else(|| optional(args, &["url"]))
                .map(str::to_string)
                .or(system)
                .ok_or_else(|| FhirError::Invalid("system is required".into()))?;
            let version = optional(args, &["version"]);
            Ok(terms.lookup(&system, version, &code).unwrap_or_else(|| {
                outcome(
                    "error",
                    "not-found",
                    &format!("code {code} is not in {system}"),
                )
            }))
        }
        ("CodeSystem", "$subsumes") => {
            let system = optional(args, &["system", "url"])
                .ok_or_else(|| FhirError::Invalid("system is required".into()))?;
            let code_a = required(args, &["codeA", "code"])?;
            let code_b = required(args, &["codeB"])?;
            let version = optional(args, &["version"]);
            let outcome = if terms.subsumes(system, version, code_a, code_b)
                && terms.subsumes(system, version, code_b, code_a)
            {
                "equivalent"
            } else if terms.subsumes(system, version, code_a, code_b) {
                "subsumes"
            } else if terms.subsumes(system, version, code_b, code_a) {
                "subsumed-by"
            } else {
                "not-subsumed"
            };
            Ok(json!({
                "resourceType": "Parameters",
                "parameter": [{"name": "outcome", "valueCode": outcome}]
            }))
        }
        ("ConceptMap", "$translate") => {
            let (system, code, _) = code_parts(args)?;
            let system = system.ok_or_else(|| FhirError::Invalid("system is required".into()))?;
            Ok(terms.translate(
                optional(args, &["url"]),
                &system,
                &code,
                optional(args, &["targetsystem", "targetSystem"]),
            ))
        }
        (
            _,
            "$expand" | "$expandability" | "$validate-code" | "$lookup" | "$translate"
            | "$subsumes",
        ) => Err(FhirError::Unsupported(format!(
            "unsupported operation {type_name}/{op}"
        ))),
        _ => Err(FhirError::Unsupported(format!(
            "unsupported operation {type_name}/{op}"
        ))),
    }
}

/// A value set that cannot be expanded is a failed request, not an empty answer: HTTP 422, with
/// one issue per cause and the part of the value set each one comes from. Returning only the
/// codes that could be expanded would look like a complete answer.
fn not_expandable(terms: &MemoryTerminology, url: &str) -> FhirError {
    let failures = terms.why_not_expanded(url).unwrap_or_else(|| {
        vec![ExpansionFailure::NotExpanded {
            value_set: url.to_string(),
        }]
    });
    let headline = format!("value set {url} expansion is not supported");
    let issues: Vec<Value> = failures
        .iter()
        .enumerate()
        .map(|(position, failure)| {
            let diagnostics = if position == 0 {
                format!("{headline}: {}", failure.message())
            } else {
                failure.message()
            };
            let mut issue = json!({
                "severity": "error",
                "code": "not-supported",
                "diagnostics": diagnostics
            });
            if let Some(expression) = failure.expression() {
                issue["expression"] = json!([expression]);
            }
            issue
        })
        .collect();
    FhirError::Unprocessable(json!({
        "resourceType": "OperationOutcome",
        "issue": issues
    }))
}

/// A report, not an expansion: for each value set asked about (or, with none named, every loaded
/// value set that cannot be expanded), whether it expands and, when it does not, why. It lets a
/// deployment check the value sets a UI depends on before the UI asks for them.
fn expandability(terms: &MemoryTerminology, urls: &[String]) -> Value {
    let entries: Vec<(String, Option<Vec<ExpansionFailure>>)> = if urls.is_empty() {
        terms
            .unexpandable()
            .into_iter()
            .map(|(url, failures)| (url, Some(failures)))
            .collect()
    } else {
        urls.iter()
            .map(|url| (url.clone(), terms.why_not_expanded(url)))
            .collect()
    };
    let parameters: Vec<Value> = entries
        .into_iter()
        .map(|(url, failures)| {
            let mut part = vec![
                json!({"name": "url", "valueUri": url}),
                json!({"name": "expandable", "valueBoolean": failures.is_none()}),
            ];
            for failure in failures.iter().flatten() {
                part.push(json!({"name": "reason", "valueCode": failure.reason()}));
                part.push(json!({"name": "diagnostics", "valueString": failure.message()}));
            }
            json!({"name": "valueSet", "part": part})
        })
        .collect();
    json!({"resourceType": "Parameters", "parameter": parameters})
}

/// Every `url` the request names: a string or array in a plain body, or each `url` parameter of
/// a Parameters resource.
fn all_urls(args: &Value) -> Vec<String> {
    let mut urls = Vec::new();
    match args.get("url") {
        Some(Value::String(url)) if !url.is_empty() => urls.push(url.clone()),
        Some(Value::Array(items)) => {
            urls.extend(
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(str::to_string),
            );
        }
        _ => {}
    }
    for parameter in parameters(args).into_iter().flatten() {
        if parameter.get("name").and_then(|value| value.as_str()) != Some("url") {
            continue;
        }
        for key in ["valueUri", "valueUrl", "valueCanonical", "valueString"] {
            if let Some(url) = parameter.get(key).and_then(|value| value.as_str()) {
                urls.push(url.to_string());
            }
        }
    }
    urls
}

fn code_parts(args: &Value) -> Result<(Option<String>, String, Option<String>), FhirError> {
    let (coding_system, coding_code, coding_display) = coding_param(args);
    let system = optional(args, &["system"])
        .map(str::to_string)
        .or(coding_system);
    let code = optional(args, &["code"])
        .map(str::to_string)
        .or(coding_code)
        .ok_or_else(|| FhirError::Invalid("code is required".into()))?;
    let display = optional(args, &["display"])
        .map(str::to_string)
        .or(coding_display);
    Ok((system, code, display))
}

fn coding_param(args: &Value) -> (Option<String>, Option<String>, Option<String>) {
    let coding = args.get("coding").or_else(|| {
        parameters(args)
            .and_then(|items| {
                items.iter().find(|parameter| {
                    parameter.get("name").and_then(|value| value.as_str()) == Some("coding")
                })
            })
            .and_then(|parameter| parameter.get("valueCoding"))
    });
    let Some(coding) = coding else {
        return (None, None, None);
    };
    (
        text(coding, "system"),
        text(coding, "code"),
        text(coding, "display"),
    )
}

fn text(value: &Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(|item| item.as_str())
        .filter(|item| !item.is_empty())
        .map(str::to_string)
}

fn optional<'a>(args: &'a Value, names: &[&str]) -> Option<&'a str> {
    for name in names {
        if let Some(text) = args.get(*name).and_then(|value| value.as_str()) {
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    let Some(parameters) = parameters(args) else {
        return None;
    };
    for name in names {
        for parameter in parameters {
            if parameter.get("name").and_then(|value| value.as_str()) != Some(*name) {
                continue;
            }
            for key in [
                "valueUri",
                "valueUrl",
                "valueCanonical",
                "valueString",
                "valueCode",
            ] {
                if let Some(text) = parameter.get(key).and_then(|value| value.as_str()) {
                    if !text.is_empty() {
                        return Some(text);
                    }
                }
            }
        }
    }
    None
}

fn parameters(args: &Value) -> Option<&Vec<Value>> {
    args.get("parameter")
        .and_then(|value| value.as_array())
        .or_else(|| {
            args.pointer("/resource/parameter")
                .and_then(|value| value.as_array())
        })
}

fn optional_usize(args: &Value, names: &[&str]) -> Option<usize> {
    for name in names {
        if let Some(value) = args.get(*name).and_then(|item| item.as_u64()) {
            return Some(value as usize);
        }
        if let Some(text) = args.get(*name).and_then(|item| item.as_str()) {
            if let Ok(value) = text.parse::<usize>() {
                return Some(value);
            }
        }
    }
    let parameters = parameters(args)?;
    for name in names {
        for parameter in parameters {
            if parameter.get("name").and_then(|value| value.as_str()) != Some(*name) {
                continue;
            }
            if let Some(value) = parameter.get("valueInteger").and_then(|item| item.as_u64()) {
                return Some(value as usize);
            }
        }
    }
    None
}

fn required<'a>(args: &'a Value, names: &[&str]) -> Result<&'a str, FhirError> {
    optional(args, names).ok_or_else(|| FhirError::Invalid(format!("{} is required", names[0])))
}

fn outcome(severity: &str, code: &str, diagnostics: &str) -> Value {
    json!({
        "resourceType": "OperationOutcome",
        "issue": [{
            "severity": severity,
            "code": code,
            "diagnostics": diagnostics
        }]
    })
}
