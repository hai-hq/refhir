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
            let offset = optional_usize(args, &["offset"]);
            let count = optional_usize(args, &["count"]);
            let active_only = optional_bool(args, &["activeOnly"]);
            let mut expanded = terms
                .expand_page_with(
                    url,
                    filter,
                    active_only.unwrap_or(false),
                    offset.unwrap_or(0),
                    count,
                )
                .ok_or_else(|| not_expandable(terms, url))?;
            if let Some(expansion) = expanded
                .get_mut("expansion")
                .and_then(|value| value.as_object_mut())
            {
                // R4 requires a timestamp: the time the expanding system produced the expansion,
                // which is when this one was read out of what the server holds and filtered and
                // paged for the caller. The identifier names this expansion, as R4 has it, and
                // is another for each, as the timestamp is.
                expansion.insert("timestamp".into(), json!(rfc3339_now()));
                expansion.insert(
                    "identifier".into(),
                    json!(format!("urn:uuid:{}", uuid::Uuid::new_v4())),
                );
                // What the caller asked that shaped it, which R4 says an expansion should list
                // (and must, for the offset and count of a page).
                let mut parameters = Vec::new();
                if let Some(filter) = filter {
                    parameters.push(json!({"name": "filter", "valueString": filter}));
                }
                if let Some(active_only) = active_only {
                    parameters.push(json!({"name": "activeOnly", "valueBoolean": active_only}));
                }
                if let Some(count) = count {
                    parameters.push(json!({"name": "count", "valueInteger": count}));
                }
                if let Some(offset) = offset {
                    parameters.push(json!({"name": "offset", "valueInteger": offset}));
                }
                if !parameters.is_empty() {
                    if let Some(Value::Array(own)) = expansion.remove("parameter") {
                        parameters.extend(own);
                    }
                    expansion.insert("parameter".into(), Value::Array(parameters));
                }
            }
            Ok(expanded)
        }
        ("ValueSet", "$expandability") => Ok(expandability(terms, &all_urls(args))),
        ("ValueSet", "$validate-code") => {
            let url = required(args, &["url"])?;
            let (system, code, display) = code_parts(args)?;
            let system_version = optional(args, &["systemVersion", "system-version"]);
            Ok(terms.validate_code_as(
                Some(url),
                system.as_deref(),
                system_version,
                &code,
                display.as_deref(),
                optional_bool(args, &["abstract"]),
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

/// The key a terminology operation sent to an instance (`ValueSet/[id]/$expand`) carries the id
/// under. It is not `id`, which a Parameters resource has of its own.
pub const INSTANCE: &str = "_instance";

/// Whether R4 defines `op` on `type_name` at the instance level: `CodeSystem/$lookup` is only
/// defined on the type.
fn at_instance(type_name: &str, op: &str) -> bool {
    matches!(
        (type_name, op),
        ("ValueSet", "$expand" | "$validate-code")
            | ("CodeSystem", "$validate-code" | "$subsumes")
            | ("ConceptMap", "$translate")
    )
}

/// The arguments of an operation sent to an instance, as the operation takes them at the type
/// level: the instance is a resource the server stores, and what is asked of it is asked of the
/// value set, code system or concept map that its `url` names (and, for a code system, its
/// `version`). Arguments that are not for an instance are returned as they are.
pub fn resolve_instance(
    tx: &mut refhir_database::Transaction,
    catalog: &Catalog,
    type_name: &str,
    op: &str,
    args: &Value,
) -> Result<Value, FhirError> {
    let Some(id) = args.get(INSTANCE).and_then(|value| value.as_str()) else {
        return Ok(args.clone());
    };
    if !at_instance(type_name, op) {
        return Err(FhirError::Unsupported(format!(
            "{op} is not an operation of {type_name}/[id]"
        )));
    }
    catalog.deny_response_read(type_name)?;
    let resource = crate::ops::read(tx, catalog.get(type_name)?, id)?;
    let url = resource
        .get("url")
        .and_then(|value| value.as_str())
        .filter(|url| !url.is_empty())
        .ok_or_else(|| {
            FhirError::Invalid(format!(
                "{type_name}/{id} has no url, which is what its terminology is known by"
            ))
        })?;
    let mut resolved = args.clone();
    if let Some(object) = resolved.as_object_mut() {
        object.remove(INSTANCE);
        object.insert("url".into(), json!(url));
        // A code system is kept in each of its versions, and this is the one asked of. Its
        // operations name it as `system` before `url`, so a `system` the caller sent is replaced.
        if type_name == "CodeSystem" {
            object.insert("system".into(), json!(url));
            if let Some(version) = resource.get("version").and_then(|value| value.as_str()) {
                object.insert("version".into(), json!(version));
            }
        }
    }
    Ok(resolved)
}

/// The current time as an RFC 3339 UTC instant, to the millisecond.
fn rfc3339_now() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    rfc3339(ms)
}

/// Unix milliseconds as an RFC 3339 UTC instant.
pub(crate) fn rfc3339(ms: u64) -> String {
    let secs = ms / 1000;
    let (year, month, day) = civil_from_days(secs / 86_400);
    let rest = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        ms % 1000
    )
}

/// Days since 1970-01-01 as a civil date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
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
    let parameters = parameters(args)?;
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

/// A boolean argument: a JSON boolean, `true` or `false` as text (a query string), or a
/// `valueBoolean` parameter.
fn optional_bool(args: &Value, names: &[&str]) -> Option<bool> {
    let read = |value: &Value| match value {
        Value::Bool(flag) => Some(*flag),
        Value::String(text) => match text.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    };
    for name in names {
        if let Some(flag) = args.get(*name).and_then(read) {
            return Some(flag);
        }
    }
    let parameters = parameters(args)?;
    for name in names {
        for parameter in parameters {
            if parameter.get("name").and_then(|value| value.as_str()) != Some(*name) {
                continue;
            }
            if let Some(flag) = parameter.get("valueBoolean").and_then(read) {
                return Some(flag);
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

#[cfg(test)]
mod tests {
    use super::rfc3339;

    #[test]
    fn instants_are_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(rfc3339(4_102_444_799_999), "2099-12-31T23:59:59.999Z");
    }
}
