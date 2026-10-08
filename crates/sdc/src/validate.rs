//! QuestionnaireResponse validation against its Questionnaire.

use serde_json::{json, Value as Json};

use crate::behavior::{apply_behavior, enabled_map};
use crate::chart::Chart;
use crate::evalexpr::eval_text;
use crate::ext::{value_of, CONSTRAINT, MAX_VALUE, MIN_VALUE, REGEX};
use crate::items::{answer_values, answers, each_item, item_by_link};

pub fn validate_response(
    chart: &mut dyn Chart,
    questionnaire: &Json,
    response: &Json,
) -> Result<Json, String> {
    let view = apply_behavior(chart, questionnaire, response)?;
    let enabled = enabled_map(&view);
    let mut issues = Vec::new();
    let mut seen = Vec::new();
    each_item(response, &mut |item| {
        let Some(link_id) = item.get("linkId").and_then(|value| value.as_str()) else {
            issues.push(issue("error", "item needs a linkId", ""));
            return;
        };
        seen.push(link_id.to_string());
        let Some(definition) = item_by_link(questionnaire, link_id) else {
            issues.push(issue("error", "unknown linkId", link_id));
            return;
        };
        if enabled.get(link_id) == Some(&false) && item.get("answer").is_some() {
            issues.push(issue(
                "error",
                "disabled item must be omitted on submit",
                link_id,
            ));
        }
        check_item(chart, definition, item, response, &mut issues);
    });
    if response.get("status").and_then(|value| value.as_str()) == Some("completed") {
        each_item(questionnaire, &mut |item| {
            let Some(link_id) = item.get("linkId").and_then(|value| value.as_str()) else {
                return;
            };
            if item.get("type").and_then(|value| value.as_str()) == Some("display") {
                return;
            }
            if item.get("required").and_then(|value| value.as_bool()) != Some(true) {
                return;
            }
            if enabled.get(link_id) == Some(&false) {
                return;
            }
            if answers(response, link_id).is_empty() {
                issues.push(issue(
                    "error",
                    "required item is missing an answer",
                    link_id,
                ));
            }
        });
    }
    for ext in crate::ext::extensions(questionnaire, CONSTRAINT) {
        let expression = crate::ext::nested(ext, "expression")
            .and_then(value_of)
            .and_then(|value| value.as_str())
            .or_else(|| {
                crate::ext::nested(ext, "expression")
                    .and_then(|item| item.get("valueString"))
                    .and_then(|value| value.as_str())
            });
        let Some(expression) = expression else {
            continue;
        };
        let values = eval_text(
            chart,
            "text/fhirpath",
            expression,
            response,
            &Default::default(),
        )?;
        let ok = values
            .first()
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if !ok {
            let text = crate::ext::nested(ext, "human")
                .and_then(value_of)
                .and_then(|value| value.as_str())
                .unwrap_or("questionnaire constraint failed");
            let severity = crate::ext::nested(ext, "severity")
                .and_then(value_of)
                .and_then(|value| value.as_str())
                .unwrap_or("error");
            issues.push(issue(severity, text, ""));
        }
    }
    Ok(outcome(issues))
}

pub fn has_errors(outcome: &Json) -> bool {
    outcome
        .get("issue")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .any(|item| item.get("severity").and_then(|value| value.as_str()) == Some("error"))
        })
        .unwrap_or(false)
}

fn check_item(
    chart: &mut dyn Chart,
    definition: &Json,
    item: &Json,
    response: &Json,
    issues: &mut Vec<Json>,
) {
    let link_id = item
        .get("linkId")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let item_type = definition
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("group");
    let answer_list = item
        .get("answer")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    if definition.get("repeats").and_then(|value| value.as_bool()) != Some(true)
        && answer_list.len() > 1
    {
        issues.push(issue("error", "item does not repeat", link_id));
    }
    if let Some(max) = definition.get("maxLength").and_then(|value| value.as_u64()) {
        for value in answer_values(response, link_id) {
            if value.as_str().map(|text| text.len() as u64).unwrap_or(0) > max {
                issues.push(issue("error", "answer exceeds maxLength", link_id));
            }
        }
    }
    for answer in &answer_list {
        if let Some(value) = value_of(answer) {
            if !type_matches(item_type, answer) {
                issues.push(issue(
                    "error",
                    "answer type does not match the item",
                    link_id,
                ));
            }
            if let Some(options) = definition
                .get("answerOption")
                .and_then(|item| item.as_array())
            {
                if !options.is_empty()
                    && !options
                        .iter()
                        .any(|option| value_of(option) == Some(value) || option == answer)
                {
                    issues.push(issue("error", "answer is not an allowed option", link_id));
                }
            }
            if let Some(pattern) = crate::ext::ext_text(definition, REGEX) {
                if let Ok(regex) = regex::Regex::new(&pattern) {
                    if value.as_str().is_some_and(|text| !regex.is_match(text)) {
                        issues.push(issue(
                            "error",
                            "answer does not match the required pattern",
                            link_id,
                        ));
                    }
                }
            }
            check_bound(definition, link_id, value, issues);
            if let Some(url) = definition
                .get("answerValueSet")
                .and_then(|item| item.as_str())
            {
                if let Some(code) = value.get("code").and_then(|item| item.as_str()) {
                    let system = value.get("system").and_then(|item| item.as_str());
                    if chart.in_valueset(system, code, url) == Some(false) {
                        issues.push(issue("error", "answer is not in the value set", link_id));
                    }
                }
            }
        }
    }
}

fn check_bound(definition: &Json, link_id: &str, value: &Json, issues: &mut Vec<Json>) {
    if let Some(min) = crate::ext::first_ext(definition, MIN_VALUE).and_then(value_of) {
        if let (Some(value), Some(min)) = (as_number(value), as_number(min)) {
            if value < min {
                issues.push(issue("error", "answer is below the minimum", link_id));
            }
        }
    }
    if let Some(max) = crate::ext::first_ext(definition, MAX_VALUE).and_then(value_of) {
        if let (Some(value), Some(max)) = (as_number(value), as_number(max)) {
            if value > max {
                issues.push(issue("error", "answer is above the maximum", link_id));
            }
        }
    }
}

fn type_matches(item_type: &str, answer: &Json) -> bool {
    let keys: Vec<&str> = answer
        .as_object()
        .map(|object| {
            object
                .keys()
                .filter(|key| key.starts_with("value"))
                .map(|key| key.as_str())
                .collect()
        })
        .unwrap_or_default();
    if keys.is_empty() {
        return item_type == "group" || item_type == "display";
    }
    let expected = match item_type {
        "boolean" => "valueBoolean",
        "decimal" => "valueDecimal",
        "integer" => "valueInteger",
        "date" => "valueDate",
        "dateTime" => "valueDateTime",
        "time" => "valueTime",
        "url" | "uri" | "canonical" => "valueUri",
        "quantity" => "valueQuantity",
        "reference" => "valueReference",
        "attachment" => "valueAttachment",
        "coding" | "choice" => "valueCoding",
        "string" | "text" => "valueString",
        _ => return true,
    };
    keys.contains(&expected)
        || (item_type == "choice"
            && (keys.contains(&"valueString") || keys.contains(&"valueCoding")))
        || (item_type == "url" && keys.contains(&"valueUrl"))
}

fn as_number(value: &Json) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|item| item as f64))
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

fn issue(severity: &str, diagnostics: &str, link_id: &str) -> Json {
    let mut item = json!({
        "severity": severity,
        "code": "invariant",
        "diagnostics": diagnostics
    });
    if !link_id.is_empty() {
        item["expression"] = json!([format!("item.where(linkId='{link_id}')")]);
    }
    item
}

fn outcome(issues: Vec<Json>) -> Json {
    if issues.is_empty() {
        json!({
            "resourceType": "OperationOutcome",
            "issue": [{
                "severity": "information",
                "code": "informational",
                "diagnostics": "questionnaire response is valid"
            }]
        })
    } else {
        json!({"resourceType": "OperationOutcome", "issue": issues})
    }
}
