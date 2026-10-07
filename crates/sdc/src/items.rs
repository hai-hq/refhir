//! Questionnaire and QuestionnaireResponse item walks.

use serde_json::{json, Value as Json};

use crate::ext::value_of;

pub fn each_item(node: &Json, visit: &mut dyn FnMut(&Json)) {
    let Some(items) = node.get("item").and_then(|value| value.as_array()) else {
        return;
    };
    for item in items {
        visit(item);
        each_item(item, visit);
    }
}

pub fn item_by_link<'a>(node: &'a Json, link_id: &str) -> Option<&'a Json> {
    let items = node.get("item").and_then(|value| value.as_array())?;
    for item in items {
        if item.get("linkId").and_then(|value| value.as_str()) == Some(link_id) {
            return Some(item);
        }
        if let Some(found) = item_by_link(item, link_id) {
            return Some(found);
        }
    }
    None
}

pub fn answers(response: &Json, link_id: &str) -> Vec<Json> {
    let mut found = Vec::new();
    each_item(response, &mut |item| {
        if item.get("linkId").and_then(|value| value.as_str()) != Some(link_id) {
            return;
        }
        if let Some(answers) = item.get("answer").and_then(|value| value.as_array()) {
            found.extend(answers.iter().cloned());
        }
    });
    found
}

pub fn answer_values(response: &Json, link_id: &str) -> Vec<Json> {
    answers(response, link_id)
        .iter()
        .filter_map(value_of)
        .cloned()
        .collect()
}

pub fn set_answers(response: &mut Json, link_id: &str, values: &[Json]) {
    if values.is_empty() {
        return;
    }
    if !replace_answers(response, link_id, values) {
        let item = json!({
            "linkId": link_id,
            "answer": values
        });
        let list = response
            .as_object_mut()
            .expect("response")
            .entry("item")
            .or_insert_with(|| json!([]));
        list.as_array_mut().expect("items").push(item);
    }
}

fn replace_answers(node: &mut Json, link_id: &str, values: &[Json]) -> bool {
    let Some(items) = node.get_mut("item").and_then(|value| value.as_array_mut()) else {
        return false;
    };
    for item in items {
        if item.get("linkId").and_then(|value| value.as_str()) == Some(link_id) {
            item["answer"] = json!(values);
            return true;
        }
        if replace_answers(item, link_id, values) {
            return true;
        }
    }
    false
}

pub fn answer_for(item_type: &str, value: &Json) -> Json {
    if value.get("resourceType").is_some() && item_type == "reference" {
        let reference = match (
            value.get("resourceType").and_then(|item| item.as_str()),
            value.get("id").and_then(|item| item.as_str()),
        ) {
            (Some(type_name), Some(id)) => format!("{type_name}/{id}"),
            _ => String::new(),
        };
        return json!({"valueReference": {"reference": reference}});
    }
    let key = match item_type {
        "boolean" => "valueBoolean",
        "decimal" => "valueDecimal",
        "integer" => "valueInteger",
        "date" => "valueDate",
        "dateTime" => "valueDateTime",
        "time" => "valueTime",
        "url" | "uri" | "canonical" => "valueUri",
        "coding" | "choice" if value.get("code").is_some() || value.get("coding").is_some() => {
            "valueCoding"
        }
        "quantity" => "valueQuantity",
        "reference" => "valueReference",
        "attachment" => "valueAttachment",
        _ => "valueString",
    };
    let stored = if key == "valueString" && !value.is_string() {
        json!(value.to_string())
    } else if key == "valueInteger" {
        json!(value.as_i64().or_else(|| value.as_f64().map(|number| number as i64)).unwrap_or(0))
    } else if key == "valueDecimal" {
        json!(value.as_f64().or_else(|| value.as_i64().map(|number| number as f64)).unwrap_or(0.0))
    } else if key == "valueBoolean" {
        json!(value.as_bool().unwrap_or(false))
    } else {
        value.clone()
    };
    json!({key: stored})
}

pub fn strip_link(node: &mut Json, link_id: &str) {
    let Some(items) = node.get_mut("item").and_then(|value| value.as_array_mut()) else {
        return;
    };
    items.retain(|item| item.get("linkId").and_then(|value| value.as_str()) != Some(link_id));
    for item in items {
        strip_link(item, link_id);
    }
}

pub fn prefix_link_ids(node: &mut Json, prefix: &str) {
    let Some(items) = node.get_mut("item").and_then(|value| value.as_array_mut()) else {
        return;
    };
    for item in items {
        if let Some(link_id) = item.get("linkId").and_then(|value| value.as_str()) {
            item["linkId"] = json!(format!("{prefix}{link_id}"));
        }
        prefix_link_ids(item, prefix);
    }
}
