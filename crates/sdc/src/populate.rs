//! $populate. StructureMap wins. Otherwise observation and expression population
//! run item by item and do not overwrite answers already on the response.

use std::collections::BTreeMap;

use refhir_map::apply;
use serde_json::{json, Value as Json};
use uuid::Uuid;

use crate::chart::Chart;
use crate::evalexpr::{bind_variables, eval_text, find_canonical, json_value};
use crate::ext::{
    value_of, INITIAL, IS_SUBJECT, LAUNCH, LINK_PERIOD, POPULATION, PROVENANCE, SOURCE_MAP,
    SOURCE_QUERIES, UNIT,
};
use crate::items::{answer_for, answers, each_item, set_answers};

pub fn populate(chart: &mut dyn Chart, questionnaire: &Json, args: &Json) -> Result<Json, String> {
    let subject = subject_of(args);
    let content = content_of(args);
    let local = args
        .get("local")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let mut response = args.get("response").cloned().unwrap_or_else(
        || json!({"resourceType": "QuestionnaireResponse", "status": "in-progress"}),
    );
    if response.get("resourceType").is_none() {
        response["resourceType"] = json!("QuestionnaireResponse");
    }
    if let Some(url) = questionnaire.get("url").and_then(|value| value.as_str()) {
        response["questionnaire"] = json!(url);
    }
    if let Some(subject) = &subject {
        response["subject"] = json!({"reference": subject});
    }
    if crate::ext::first_ext(questionnaire, SOURCE_MAP).is_some() {
        return populate_map(chart, questionnaire, &response, &content, local);
    }
    let mut sources = Vec::new();
    populate_observation(
        chart,
        questionnaire,
        &mut response,
        subject.as_deref(),
        &content,
        local,
        &mut sources,
    )?;
    populate_expressions(chart, questionnaire, &mut response, &content, local)?;
    if !sources.is_empty() {
        attach_provenance(&mut response, &sources);
    }
    Ok(response)
}

fn populate_map(
    chart: &mut dyn Chart,
    questionnaire: &Json,
    response: &Json,
    content: &[Json],
    local: bool,
) -> Result<Json, String> {
    let canonical = crate::ext::ext_text(questionnaire, SOURCE_MAP)
        .ok_or_else(|| "sourceStructureMap is missing".to_string())?;
    let map = resolve_ref(chart, questionnaire, "StructureMap", &canonical)?;
    let bundle = source_bundle(chart, questionnaire, content, local)?;
    let mut translated = apply(&map, &bundle, &mut |map_url, system, code| {
        chart.translate(map_url, system, code)
    })?;
    if translated.get("resourceType").is_none() {
        translated["resourceType"] = json!("QuestionnaireResponse");
    }
    if translated.get("status").is_none() {
        translated["status"] = json!("in-progress");
    }
    if translated.get("questionnaire").is_none() {
        if let Some(url) = questionnaire.get("url").and_then(|value| value.as_str()) {
            translated["questionnaire"] = json!(url);
        }
    }
    if response.get("subject").is_some() && translated.get("subject").is_none() {
        translated["subject"] = response["subject"].clone();
    }
    let _ = content;
    Ok(translated)
}

fn source_bundle(
    chart: &mut dyn Chart,
    questionnaire: &Json,
    content: &[Json],
    local: bool,
) -> Result<Json, String> {
    let Some(reference) = crate::ext::ext_text(questionnaire, SOURCE_QUERIES) else {
        return Ok(
            json!({"resourceType": "Bundle", "type": "collection", "entry": content.iter().map(|resource| json!({"resource": resource})).collect::<Vec<_>>() }),
        );
    };
    let batch = resolve_ref(chart, questionnaire, "Bundle", &reference)?;
    let mut entries = Vec::new();
    for entry in batch
        .get("entry")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
    {
        let url = entry
            .pointer("/request/url")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let rows = if local {
            content.to_vec()
        } else if let Some((type_name, query)) = url.split_once('?') {
            let params = query_params(query);
            let owned: Vec<(&str, String)> = params
                .iter()
                .map(|(name, value)| (*name, value.clone()))
                .collect();
            chart.search_type(type_name, &owned)?
        } else if let Some((type_name, id)) = url.split_once('/') {
            chart.read_type(type_name, id)?.into_iter().collect()
        } else {
            Vec::new()
        };
        entries.push(json!({
            "resource": {
                "resourceType": "Bundle",
                "type": "searchset",
                "entry": rows.into_iter().map(|resource| json!({"resource": resource})).collect::<Vec<_>>()
            }
        }));
    }
    Ok(json!({"resourceType": "Bundle", "type": "collection", "entry": entries}))
}

fn populate_observation(
    chart: &mut dyn Chart,
    questionnaire: &Json,
    response: &mut Json,
    subject: Option<&str>,
    content: &[Json],
    local: bool,
    sources: &mut Vec<String>,
) -> Result<(), String> {
    let inherited =
        period_seconds(crate::ext::first_ext(questionnaire, LINK_PERIOD).and_then(value_of));
    walk_observation(
        chart,
        questionnaire,
        response,
        subject.map(|value| value.to_string()),
        content,
        local,
        sources,
        inherited,
    )
}

fn walk_observation(
    chart: &mut dyn Chart,
    node: &Json,
    response: &mut Json,
    mut subject: Option<String>,
    content: &[Json],
    local: bool,
    sources: &mut Vec<String>,
    inherited: Option<i64>,
) -> Result<(), String> {
    if crate::ext::ext_bool(node, IS_SUBJECT) == Some(true) {
        if let Some(link_id) = node.get("linkId").and_then(|value| value.as_str()) {
            if let Some(reference) = crate::items::answer_values(response, link_id)
                .first()
                .and_then(|value| value.get("reference"))
                .and_then(|value| value.as_str())
            {
                subject = Some(reference.to_string());
            }
        }
    }
    let period =
        period_seconds(crate::ext::first_ext(node, LINK_PERIOD).and_then(value_of)).or(inherited);
    if let Some(link_id) = node.get("linkId").and_then(|value| value.as_str()) {
        let observes = period.is_some() && !crate::ext::codes_of(node).is_empty();
        if observes && crate::ext::first_ext(node, INITIAL).is_some() {
            return Err(format!(
                "item {link_id} cannot use both observation-based and expression-based population"
            ));
        }
        if observes && answers(response, link_id).is_empty() {
            let codes = crate::ext::codes_of(node);
            if !codes.is_empty() {
                if let Some(observation) =
                    newest_observation(chart, subject.as_deref(), &codes, period, content, local)?
                {
                    if let Some(reference) = observation_reference(&observation) {
                        sources.push(reference);
                    }
                    let item_type = node
                        .get("type")
                        .and_then(|value| value.as_str())
                        .unwrap_or("string");
                    if let Some(value) = observation_value(&observation, node, item_type) {
                        set_answers(response, link_id, &[answer_for(item_type, &value)]);
                    }
                }
            }
        }
    }
    if let Some(items) = node.get("item").and_then(|value| value.as_array()) {
        for item in items {
            walk_observation(
                chart,
                item,
                response,
                subject.clone(),
                content,
                local,
                sources,
                period,
            )?;
        }
    }
    Ok(())
}

fn newest_observation(
    chart: &mut dyn Chart,
    subject: Option<&str>,
    codes: &[(String, String)],
    period: Option<i64>,
    content: &[Json],
    local: bool,
) -> Result<Option<Json>, String> {
    let mut rows = content
        .iter()
        .filter(|row| {
            row.get("resourceType").and_then(|value| value.as_str()) == Some("Observation")
        })
        .cloned()
        .collect::<Vec<_>>();
    if !local && chart.allows("Observation") {
        let mut params = Vec::new();
        if let Some(subject) = subject {
            params.push(("subject", subject.to_string()));
        }
        if let Some((_, code)) = codes.first() {
            params.push(("code", code.clone()));
        }
        let owned: Vec<(&str, String)> = params
            .iter()
            .map(|(name, value)| (*name, value.clone()))
            .collect();
        rows.extend(chart.search_type("Observation", &owned)?);
    }
    let now = now_secs();
    let mut matched: Vec<Json> = rows
        .into_iter()
        .filter(|row| observation_matches(row, subject, codes))
        .filter(|row| within_period(row, period, now))
        .filter(|row| row.pointer("/valueQuantity/comparator").is_none())
        .collect();
    matched.sort_by(|left, right| observation_time(right).cmp(&observation_time(left)));
    Ok(matched.into_iter().next())
}

fn observation_matches(row: &Json, subject: Option<&str>, codes: &[(String, String)]) -> bool {
    if let Some(subject) = subject {
        let reference = row
            .pointer("/subject/reference")
            .and_then(|value| value.as_str());
        if reference != Some(subject) {
            return false;
        }
    }
    let present = crate::ext::codes_of(row.get("code").unwrap_or(&json!({})));
    codes
        .iter()
        .any(|(_, code)| present.iter().any(|(_, item)| item == code))
}

fn within_period(row: &Json, period: Option<i64>, now: i64) -> bool {
    let Some(period) = period else {
        return false;
    };
    let Some(when) = observation_time(row) else {
        return true;
    };
    now.saturating_sub(when) <= period
}

fn observation_time(row: &Json) -> Option<i64> {
    let text = row
        .get("effectiveDateTime")
        .or_else(|| row.get("issued"))
        .and_then(|value| value.as_str())?;
    parse_instant(text)
}

fn observation_value(observation: &Json, item: &Json, item_type: &str) -> Option<Json> {
    let value = observation
        .as_object()?
        .iter()
        .find(|(key, _)| key.starts_with("value"))
        .map(|(_, value)| value.clone())?;
    if let Some(unit) = crate::ext::first_ext(item, UNIT).and_then(value_of) {
        let wanted = unit.get("code").and_then(|value| value.as_str());
        let actual = value.get("code").and_then(|code| code.as_str());
        if let (Some(wanted), Some(actual)) = (wanted, actual) {
            if wanted != actual && item_type != "quantity" {
                return None;
            }
        }
    }
    if item_type == "decimal" || item_type == "integer" {
        return value.get("value").cloned().or(Some(value));
    }
    Some(value)
}

fn observation_reference(observation: &Json) -> Option<String> {
    crate::ext::reference_of(observation).filter(|reference| reference.starts_with("Observation/"))
}

fn populate_expressions(
    chart: &mut dyn Chart,
    questionnaire: &Json,
    response: &mut Json,
    content: &[Json],
    local: bool,
) -> Result<(), String> {
    let mut vars = BTreeMap::new();
    vars.insert("questionnaire".into(), vec![json_value(questionnaire)]);
    vars.insert("resource".into(), vec![json_value(response)]);
    for resource in content {
        if let Some(type_name) = resource
            .get("resourceType")
            .and_then(|value| value.as_str())
        {
            vars.entry(type_name.to_ascii_lowercase())
                .or_insert_with(Vec::new)
                .push(json_value(resource));
        }
    }
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
        if let (Some(name), Some(type_name)) = (name, type_name) {
            if let Some(resource) = content.iter().find(|row| {
                row.get("resourceType").and_then(|value| value.as_str()) == Some(type_name)
            }) {
                vars.insert(name, vec![json_value(resource)]);
            } else if !local {
                if let Some(reference) = response
                    .pointer("/subject/reference")
                    .and_then(|value| value.as_str())
                {
                    if let Some((found, id)) = reference.split_once('/') {
                        if found == type_name {
                            if let Some(resource) = chart.read_type(found, id)? {
                                vars.insert(name, vec![json_value(&resource)]);
                            }
                        }
                    }
                }
            }
        }
    }
    bind_variables(chart, questionnaire, response, &mut vars)?;
    let mut items = Vec::new();
    each_item(questionnaire, &mut |item| items.push(item.clone()));
    for item in items {
        let Some(link_id) = item.get("linkId").and_then(|value| value.as_str()) else {
            continue;
        };
        if !answers(response, link_id).is_empty() {
            continue;
        }
        bind_variables(chart, &item, response, &mut vars)?;
        if let Some(ext) = crate::ext::first_ext(&item, POPULATION) {
            if let Some((language, expression)) = crate::ext::expression_of(ext) {
                let values = eval_text(chart, language, expression, response, &vars)?;
                if let Some(name) = crate::ext::expression_name(ext) {
                    if let Some(first) = values.first() {
                        vars.insert(name, vec![json_value(first)]);
                    }
                }
            }
        }
        let Some(ext) = crate::ext::first_ext(&item, INITIAL) else {
            continue;
        };
        let Some((language, expression)) = crate::ext::expression_of(ext) else {
            continue;
        };
        let values = eval_text(chart, language, expression, response, &vars).unwrap_or_default();
        let item_type = item
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("string");
        let stored: Vec<Json> = values
            .iter()
            .map(|value| answer_for(item_type, value))
            .collect();
        set_answers(response, link_id, &stored);
        vars.insert("resource".into(), vec![json_value(response)]);
    }
    Ok(())
}

fn attach_provenance(response: &mut Json, sources: &[String]) {
    let id = "populate-provenance";
    let provenance = json!({
        "resourceType": "Provenance",
        "id": id,
        "target": [{"reference": "QuestionnaireResponse"}],
        "recorded": "1970-01-01T00:00:00Z",
        "agent": [{"who": {"display": "refhir-sdc"}}],
        "entity": sources.iter().map(|reference| json!({
            "role": "source",
            "what": {"reference": reference}
        })).collect::<Vec<_>>()
    });
    let contained = response
        .as_object_mut()
        .expect("response")
        .entry("contained")
        .or_insert_with(|| json!([]));
    contained
        .as_array_mut()
        .expect("contained")
        .push(provenance);
    crate::ext::push_extension(
        response,
        json!({"url": PROVENANCE, "valueReference": {"reference": format!("#{id}")}}),
    );
}

fn subject_of(args: &Json) -> Option<String> {
    args.get("subject")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .or_else(|| {
            args.pointer("/subject/reference")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        })
}

fn content_of(args: &Json) -> Vec<Json> {
    let mut rows = Vec::new();
    if let Some(items) = args.get("content").and_then(|value| value.as_array()) {
        for item in items {
            if item.get("resourceType").and_then(|value| value.as_str()) == Some("Bundle") {
                for entry in item
                    .get("entry")
                    .and_then(|value| value.as_array())
                    .into_iter()
                    .flatten()
                {
                    if let Some(resource) = entry.get("resource") {
                        rows.push(resource.clone());
                    }
                }
            } else {
                rows.push(item.clone());
            }
        }
    }
    rows
}

fn resolve_ref(
    chart: &mut dyn Chart,
    owner: &Json,
    type_name: &str,
    reference: &str,
) -> Result<Json, String> {
    if let Some(id) = reference.strip_prefix('#') {
        return owner
            .get("contained")
            .and_then(|value| value.as_array())
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item.get("id").and_then(|value| value.as_str()) == Some(id))
            })
            .cloned()
            .ok_or_else(|| format!("contained {reference} was not found"));
    }
    find_canonical(chart, type_name, reference)?
        .ok_or_else(|| format!("{type_name} {reference} was not found"))
}

fn query_params(query: &str) -> Vec<(&str, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (name, value.to_string())
        })
        .collect()
}

fn period_seconds(value: Option<&Json>) -> Option<i64> {
    let value = value?;
    let amount = value
        .get("value")
        .and_then(|item| item.as_f64())
        .unwrap_or(0.0);
    let code = value
        .get("code")
        .and_then(|item| item.as_str())
        .unwrap_or("d");
    let seconds = match code {
        "a" | "year" | "yr" => amount * 365.0 * 86_400.0,
        "mo" => amount * 30.0 * 86_400.0,
        "wk" => amount * 7.0 * 86_400.0,
        "d" => amount * 86_400.0,
        "h" => amount * 3_600.0,
        "min" => amount * 60.0,
        "s" => amount,
        _ => amount * 86_400.0,
    };
    Some(seconds as i64)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn parse_instant(text: &str) -> Option<i64> {
    let date = text.get(0..10)?;
    let year: i32 = date.get(0..4)?.parse().ok()?;
    let month: u8 = date.get(5..7)?.parse().ok()?;
    let day: u8 = date.get(8..10)?.parse().ok()?;
    let days = civil_days(year, month, day);
    Some(days * 86_400)
}

fn civil_days(year: i32, month: u8, day: u8) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = (year - era * 400) as u64;
    let mp = if month > 2 {
        month as u64 - 3
    } else {
        month as u64 + 9
    };
    let doy = (153 * mp + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era as i64 * 146_097 + doe as i64 - 719_468
}

pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}
