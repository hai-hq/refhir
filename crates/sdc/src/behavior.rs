//! Enablement, calculation, and initial values.

use std::collections::BTreeMap;

use refhir_fhirpath::Value;
use serde_json::{json, Value as Json};

use crate::chart::Chart;
use crate::evalexpr::{bind_variables, eval_text, json_value};
use crate::ext::{
    value_of, ANSWER_EXPR, CALCULATED, CANDIDATE, CONTEXT_EXPR, ENABLE_EXPR, INITIAL, LAUNCH,
};
use crate::items::{answer_for, answer_values, answers, each_item, set_answers};

pub fn apply_behavior(
    chart: &mut dyn Chart,
    questionnaire: &Json,
    response: &Json,
) -> Result<Json, String> {
    let mut response = response.clone();
    if response.get("resourceType").is_none() {
        response["resourceType"] = json!("QuestionnaireResponse");
    }
    if response.get("status").is_none() {
        response["status"] = json!("in-progress");
    }
    let mut vars = BTreeMap::new();
    vars.insert("questionnaire".into(), vec![json_value(questionnaire)]);
    vars.insert("resource".into(), vec![json_value(&response)]);
    bind_launch(chart, questionnaire, &response, &mut vars)?;
    bind_variables(chart, questionnaire, &response, &mut vars)?;
    let mut enabled = BTreeMap::new();
    let mut candidates = serde_json::Map::new();
    let mut contexts = serde_json::Map::new();
    let items = collect_items(questionnaire);
    for item in &items {
        let link_id = item
            .get("linkId")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        if link_id.is_empty() {
            continue;
        }
        bind_variables(chart, item, &response, &mut vars)?;
        let on = item_enabled(chart, item, &response, &vars)?;
        enabled.insert(link_id.clone(), on);
        if !on {
            continue;
        }
        if answers(&response, &link_id).is_empty() {
            if let Some(initial) = item.get("initial").and_then(|value| value.as_array()) {
                if !initial.is_empty() {
                    set_answers(&mut response, &link_id, initial);
                }
            }
            if let Some(ext) = crate::ext::first_ext(item, INITIAL) {
                if let Some((language, expression)) = crate::ext::expression_of(ext) {
                    let values = eval_text(chart, language, expression, &response, &vars)?;
                    let item_type = item
                        .get("type")
                        .and_then(|value| value.as_str())
                        .unwrap_or("string");
                    let answers: Vec<Json> = values
                        .iter()
                        .map(|value| answer_for(item_type, value))
                        .collect();
                    set_answers(&mut response, &link_id, &answers);
                }
            }
        }
        if let Some(ext) = crate::ext::first_ext(item, CALCULATED) {
            if let Some((language, expression)) = crate::ext::expression_of(ext) {
                let values = eval_text(chart, language, expression, &response, &vars)?;
                let item_type = item
                    .get("type")
                    .and_then(|value| value.as_str())
                    .unwrap_or("string");
                let answers: Vec<Json> = values
                    .iter()
                    .map(|value| answer_for(item_type, value))
                    .collect();
                if !answers.is_empty() {
                    set_answers(&mut response, &link_id, &answers);
                }
            }
        }
        if let Some(ext) = crate::ext::first_ext(item, ANSWER_EXPR) {
            if let Some((language, expression)) = crate::ext::expression_of(ext) {
                let _ = eval_text(chart, language, expression, &response, &vars);
            }
        }
        if let Some(ext) = crate::ext::first_ext(item, CANDIDATE) {
            if let Some((language, expression)) = crate::ext::expression_of(ext) {
                let values = eval_text(chart, language, expression, &response, &vars)?;
                candidates.insert(link_id.clone(), json!(values));
            }
        }
        if let Some(ext) = crate::ext::first_ext(item, CONTEXT_EXPR) {
            if let Some((language, expression)) = crate::ext::expression_of(ext) {
                let values = eval_text(chart, language, expression, &response, &vars)?;
                contexts.insert(link_id.clone(), json!(values));
            }
        }
        vars.insert("resource".into(), vec![json_value(&response)]);
    }
    Ok(json!({
        "enabled": enabled,
        "response": response,
        "candidates": candidates,
        "contexts": contexts
    }))
}

pub fn enabled_map(view: &Json) -> BTreeMap<String, bool> {
    view.get("enabled")
        .and_then(|value| value.as_object())
        .map(|object| {
            object
                .iter()
                .map(|(key, value)| (key.clone(), value.as_bool().unwrap_or(true)))
                .collect()
        })
        .unwrap_or_default()
}

fn collect_items(questionnaire: &Json) -> Vec<Json> {
    let mut items = Vec::new();
    each_item(questionnaire, &mut |item| items.push(item.clone()));
    items
}

fn bind_launch(
    chart: &mut dyn Chart,
    questionnaire: &Json,
    response: &Json,
    vars: &mut BTreeMap<String, Vec<Value>>,
) -> Result<(), String> {
    for ext in crate::ext::extensions(questionnaire, LAUNCH) {
        let name = crate::ext::nested(ext, "name")
            .and_then(value_of)
            .and_then(|value| {
                value.as_str().map(|text| text.to_string()).or_else(|| {
                    value
                        .get("code")
                        .and_then(|code| code.as_str())
                        .map(|code| code.to_string())
                })
            });
        let type_name = crate::ext::nested(ext, "type")
            .and_then(value_of)
            .and_then(|value| value.as_str());
        let Some(name) = name else {
            continue;
        };
        if let Some(reference) = response
            .get("subject")
            .and_then(|value| value.get("reference"))
            .and_then(|value| value.as_str())
        {
            if type_name.is_none()
                || reference.starts_with(&format!("{}/", type_name.unwrap_or("")))
            {
                if let Some((type_name, id)) = reference.split_once('/') {
                    if let Some(resource) = chart.read_type(type_name, id)? {
                        vars.insert(name, vec![json_value(&resource)]);
                    }
                }
            }
        }
    }
    Ok(())
}

fn item_enabled(
    chart: &mut dyn Chart,
    item: &Json,
    response: &Json,
    vars: &BTreeMap<String, Vec<Value>>,
) -> Result<bool, String> {
    if let Some(ext) = crate::ext::first_ext(item, ENABLE_EXPR) {
        if let Some((language, expression)) = crate::ext::expression_of(ext) {
            let values = eval_text(chart, language, expression, response, vars)?;
            return Ok(values
                .first()
                .and_then(|value| value.as_bool())
                .unwrap_or(false));
        }
    }
    let conditions = item
        .get("enableWhen")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    if conditions.is_empty() {
        return Ok(true);
    }
    let all = item.get("enableBehavior").and_then(|value| value.as_str()) != Some("any");
    let mut matches = Vec::new();
    for condition in conditions {
        matches.push(condition_matches(&condition, response));
    }
    Ok(if all {
        matches.iter().all(|item| *item)
    } else {
        matches.iter().any(|item| *item)
    })
}

fn condition_matches(condition: &Json, response: &Json) -> bool {
    let link_id = condition
        .get("question")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let operator = condition
        .get("operator")
        .and_then(|value| value.as_str())
        .unwrap_or("=");
    let values = answer_values(response, link_id);
    if operator == "exists" {
        let wanted = condition
            .get("answerBoolean")
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        return values.is_empty() != wanted;
    }
    let expected = condition.as_object().and_then(|object| {
        object
            .iter()
            .find(|(key, _)| key.starts_with("answer"))
            .map(|(_, value)| value)
    });
    values
        .iter()
        .any(|value| compare(operator, value, expected))
}

fn compare(operator: &str, left: &Json, right: Option<&Json>) -> bool {
    let Some(right) = right else {
        return false;
    };
    if left.get("code").is_some() || right.get("code").is_some() {
        let same = left.get("code") == right.get("code")
            && left.get("system").unwrap_or(&json!(""))
                == right.get("system").unwrap_or(&json!(""));
        return match operator {
            "!=" => !same,
            _ => same,
        };
    }
    if let (Some(left_n), Some(right_n)) = (as_number(left), as_number(right)) {
        return match operator {
            "=" => left_n == right_n,
            "!=" => left_n != right_n,
            ">" => left_n > right_n,
            "<" => left_n < right_n,
            ">=" => left_n >= right_n,
            "<=" => left_n <= right_n,
            _ => false,
        };
    }
    let left = left
        .as_str()
        .map(|value| value.to_string())
        .unwrap_or_else(|| left.to_string());
    let right = right
        .as_str()
        .map(|value| value.to_string())
        .unwrap_or_else(|| right.to_string());
    match operator {
        "=" => left == right,
        "!=" => left != right,
        ">" => left > right,
        "<" => left < right,
        ">=" => left >= right,
        "<=" => left <= right,
        _ => false,
    }
}

fn as_number(value: &Json) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|item| item as f64))
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}
