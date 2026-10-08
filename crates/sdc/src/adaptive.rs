//! Adaptive $next-question.
//!
//! The item bank lives on the questionnaire named by `derivedFrom`. A Library
//! expression named `NextQuestion` may return the next linkId. Otherwise the
//! next enabled item that is not yet on the contained questionnaire is added.
//! The response is completed when the bank has nothing left to ask.

use serde_json::{json, Value as Json};

use crate::behavior::apply_behavior;
use crate::chart::Chart;
use crate::evalexpr::library_expression;
use crate::ext::ADAPTIVE;
use crate::items::item_by_link;

pub fn next_question(chart: &mut dyn Chart, response: &Json) -> Result<Json, String> {
    let mut response = response.clone();
    if response.get("resourceType").is_none() {
        response["resourceType"] = json!("QuestionnaireResponse");
    }
    let contained = contained_questionnaire(&response);
    let bank = bank_for(chart, &response, contained.as_ref())?;
    if crate::ext::ext_bool(&bank, ADAPTIVE) == Some(false) {
        return Err("questionnaire is not adaptive".into());
    }
    let mut shown = contained
        .as_ref()
        .and_then(|item| item.get("item"))
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let next = next_link(chart, &bank, &response)?;
    if let Some(link_id) = next {
        if shown.iter().all(|item| {
            item.get("linkId").and_then(|value| value.as_str()) != Some(link_id.as_str())
        }) {
            if let Some(item) = item_by_link(&bank, &link_id) {
                shown.push(item.clone());
            }
        }
        response["status"] = json!("in-progress");
    } else {
        response["status"] = json!("completed");
    }
    let mut form = contained.unwrap_or_else(|| {
        json!({
            "resourceType": "Questionnaire",
            "id": "adaptive",
            "status": "active"
        })
    });
    form["item"] = json!(shown);
    if form.get("derivedFrom").is_none() {
        if let Some(url) = bank.get("url") {
            form["derivedFrom"] = json!([url]);
        }
    }
    let id = form
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("adaptive")
        .to_string();
    response["contained"] = json!([form]);
    response["questionnaire"] = json!(format!("#{id}"));
    Ok(response)
}

fn contained_questionnaire(response: &Json) -> Option<Json> {
    let canonical = response
        .get("questionnaire")
        .and_then(|value| value.as_str())?;
    let id = canonical.strip_prefix('#')?;
    response
        .get("contained")
        .and_then(|value| value.as_array())
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.get("id").and_then(|value| value.as_str()) == Some(id))
        })
        .cloned()
}

fn bank_for(
    chart: &mut dyn Chart,
    response: &Json,
    contained: Option<&Json>,
) -> Result<Json, String> {
    if let Some(contained) = contained {
        if let Some(url) = contained
            .get("derivedFrom")
            .and_then(|value| value.as_array())
            .and_then(|items| items.first())
            .and_then(|value| value.as_str())
        {
            if let Some(bank) = crate::evalexpr::find_canonical(chart, "Questionnaire", url)? {
                return Ok(bank);
            }
        }
    }
    if let Some(url) = response
        .get("questionnaire")
        .and_then(|value| value.as_str())
    {
        if !url.starts_with('#') {
            if let Some(bank) = crate::evalexpr::find_canonical(chart, "Questionnaire", url)? {
                return Ok(bank);
            }
        }
    }
    contained
        .cloned()
        .ok_or_else(|| "adaptive questionnaire was not found".to_string())
}

fn next_link(
    chart: &mut dyn Chart,
    bank: &Json,
    response: &Json,
) -> Result<Option<String>, String> {
    if let Some(values) =
        library_expression(chart, bank, "NextQuestion", response, &Default::default())?
    {
        if let Some(link_id) = values.first().and_then(|value| value.as_str()) {
            return Ok(Some(link_id.to_string()));
        }
    }
    let view = apply_behavior(chart, bank, response)?;
    let enabled = crate::behavior::enabled_map(&view);
    let mut link_id = None;
    crate::items::each_item(bank, &mut |item| {
        if link_id.is_some() {
            return;
        }
        let Some(id) = item.get("linkId").and_then(|value| value.as_str()) else {
            return;
        };
        if item.get("type").and_then(|value| value.as_str()) == Some("display") {
            return;
        }
        if enabled.get(id) == Some(&false) {
            return;
        }
        if crate::items::answers(response, id).is_empty() {
            link_id = Some(id.to_string());
        }
    });
    Ok(link_id)
}
