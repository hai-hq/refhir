//! Terminology operations read the in-memory index. They do not write the document log.

use serde_json::{json, Value};

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
            Ok(terms.expand_page(url, filter, offset, count).unwrap_or_else(|| {
                outcome(
                    "warning",
                    "not-supported",
                    &format!("value set {url} expansion is not supported"),
                )
            }))
        }
        ("ValueSet", "$validate-code") => {
            let url = required(args, &["url"])?;
            let (system, code, display) = code_parts(args)?;
            Ok(terms.validate_code(Some(url), system.as_deref(), &code, display.as_deref()))
        }
        ("CodeSystem", "$validate-code") => {
            let (system, code, display) = code_parts(args)?;
            let system = optional(args, &["system"])
                .or_else(|| optional(args, &["url"]))
                .map(str::to_string)
                .or(system)
                .ok_or_else(|| FhirError::Invalid("system is required".into()))?;
            Ok(terms.validate_code(None, Some(&system), &code, display.as_deref()))
        }
        ("CodeSystem", "$lookup") => {
            let (system, code, _) = code_parts(args)?;
            let system = optional(args, &["system"])
                .or_else(|| optional(args, &["url"]))
                .map(str::to_string)
                .or(system)
                .ok_or_else(|| FhirError::Invalid("system is required".into()))?;
            Ok(terms.lookup(&system, &code).unwrap_or_else(|| {
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
            let outcome = if terms.subsumes(system, code_a, code_b) && terms.subsumes(system, code_b, code_a)
            {
                "equivalent"
            } else if terms.subsumes(system, code_a, code_b) {
                "subsumes"
            } else if terms.subsumes(system, code_b, code_a) {
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
        (_, "$expand" | "$validate-code" | "$lookup" | "$translate" | "$subsumes") => Err(
            FhirError::Unsupported(format!("unsupported operation {type_name}/{op}")),
        ),
        _ => Err(FhirError::Unsupported(format!(
            "unsupported operation {type_name}/{op}"
        ))),
    }
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
