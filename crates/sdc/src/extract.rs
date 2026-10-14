//! $extract returns a transaction Bundle and does not write it.

use std::collections::BTreeMap;

use refhir_map::apply;
use serde_json::{json, Value as Json};

use crate::chart::Chart;
use crate::evalexpr::{eval_text, find_canonical};
use crate::ext::{
    value_of, ALLOCATE_ID, DEFINITION_EXTRACT, DEFINITION_VALUE, IS_SUBJECT, OBS_CATEGORY,
    OBS_ENTRY, OBS_EXTRACT, TARGET_MAP, TEMPLATE_CONTEXT, TEMPLATE_EXTRACT, TEMPLATE_VALUE, UNIT,
};
use crate::items::{answer_values, answers, each_item};
use crate::populate::new_id;

pub fn extract(chart: &mut dyn Chart, response: &Json) -> Result<Json, String> {
    let questionnaire = questionnaire_for(chart, response)?;
    if crate::ext::first_ext(&questionnaire, TARGET_MAP).is_some() {
        return extract_map(chart, &questionnaire, response);
    }
    let mut entries = Vec::new();
    extract_observation(&questionnaire, response, &mut entries);
    extract_definition(chart, &questionnaire, response, &mut entries)?;
    extract_template(chart, &questionnaire, response, &mut entries)?;
    if entries.len() == 1 && entries[0].get("request").is_none() {
        if let Some(resource) = entries[0].get("resource") {
            return Ok(resource.clone());
        }
    }
    Ok(json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": entries
    }))
}

fn questionnaire_for(chart: &mut dyn Chart, response: &Json) -> Result<Json, String> {
    let canonical = response
        .get("questionnaire")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "QuestionnaireResponse.questionnaire is required".to_string())?;
    if let Some(id) = canonical.strip_prefix('#') {
        return response
            .get("contained")
            .and_then(|value| value.as_array())
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item.get("id").and_then(|value| value.as_str()) == Some(id))
            })
            .cloned()
            .ok_or_else(|| format!("contained questionnaire {canonical} was not found"));
    }
    find_canonical(chart, "Questionnaire", canonical)?
        .ok_or_else(|| format!("questionnaire {canonical} was not found"))
}

fn extract_map(
    chart: &mut dyn Chart,
    questionnaire: &Json,
    response: &Json,
) -> Result<Json, String> {
    let canonical = crate::ext::ext_text(questionnaire, TARGET_MAP).unwrap_or_default();
    let map = if let Some(id) = canonical.strip_prefix('#') {
        questionnaire
            .get("contained")
            .and_then(|value| value.as_array())
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item.get("id").and_then(|value| value.as_str()) == Some(id))
            })
            .cloned()
            .ok_or_else(|| format!("contained map {canonical} was not found"))?
    } else {
        find_canonical(chart, "StructureMap", &canonical)?
            .ok_or_else(|| format!("StructureMap {canonical} was not found"))?
    };
    let mut result = apply(&map, response, &mut |map_url, system, code| {
        chart.translate(map_url, system, code)
    })?;
    if result.get("resourceType").and_then(|value| value.as_str()) == Some("Bundle") {
        return Ok(result);
    }
    if result.get("resourceType").is_none() {
        result["resourceType"] = json!("Bundle");
        result["type"] = json!("transaction");
    }
    Ok(result)
}

fn extract_observation(questionnaire: &Json, response: &Json, entries: &mut Vec<Json>) {
    let root = crate::ext::ext_bool(questionnaire, OBS_EXTRACT) == Some(true)
        || crate::ext::ext_text(questionnaire, OBS_EXTRACT).is_some();
    let mut subject = response
        .pointer("/subject/reference")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string());
    let mut items = Vec::new();
    each_item(questionnaire, &mut |item| items.push(item.clone()));
    for item in items {
        if crate::ext::ext_bool(&item, IS_SUBJECT) == Some(true) {
            if let Some(link_id) = item.get("linkId").and_then(|value| value.as_str()) {
                if let Some(reference) = answer_values(response, link_id)
                    .first()
                    .and_then(|value| value.get("reference"))
                    .and_then(|value| value.as_str())
                {
                    subject = Some(reference.to_string());
                }
            }
        }
        let marked = crate::ext::ext_bool(&item, OBS_EXTRACT) == Some(true)
            || crate::ext::ext_text(&item, OBS_EXTRACT).is_some()
            || root;
        if !marked {
            continue;
        }
        let Some(link_id) = item.get("linkId").and_then(|value| value.as_str()) else {
            continue;
        };
        let values = answer_values(response, link_id);
        if values.is_empty() {
            continue;
        }
        let relationship = crate::ext::ext_text(&item, OBS_EXTRACT).unwrap_or_default();
        for value in values {
            let mut observation = json!({
                "resourceType": "Observation",
                "status": "final",
                "code": {"coding": item.get("code").cloned().unwrap_or(json!([]))},
                "derivedFrom": [{"reference": response_reference(response)}]
            });
            if let Some(subject) = &subject {
                observation["subject"] = json!({"reference": subject});
            }
            if let Some(encounter) = response.pointer("/encounter/reference") {
                observation["encounter"] = json!({"reference": encounter});
            }
            if let Some(authored) = response.get("authored") {
                observation["effectiveDateTime"] = authored.clone();
                observation["issued"] = authored.clone();
            }
            if let Some(author) = response.get("author") {
                observation["performer"] = json!([author]);
            }
            if let Some(based_on) = response.get("basedOn") {
                observation["basedOn"] = based_on.clone();
            }
            if let Some(part_of) = response.get("partOf") {
                observation["partOf"] = part_of.clone();
            }
            if let Some(category) = crate::ext::first_ext(&item, OBS_CATEGORY).and_then(value_of) {
                observation["category"] = json!([category]);
            }
            let item_type = item
                .get("type")
                .and_then(|kind| kind.as_str())
                .unwrap_or("string");
            observation[value_key(item_type, &item)] = quantity_or(item_type, &item, value);
            let mut entry = json!({"resource": observation, "request": {"method": "POST", "url": "Observation"}});
            if let Some(entry_ext) = crate::ext::first_ext(&item, OBS_ENTRY) {
                if let Some(full_url) = crate::ext::nested(entry_ext, "fullUrl").and_then(value_of)
                {
                    entry["fullUrl"] = full_url.clone();
                }
                if let Some(resource_id) = crate::ext::nested(entry_ext, "resourceId")
                    .and_then(value_of)
                    .and_then(|v| v.as_str())
                {
                    entry["resource"]["id"] = json!(resource_id);
                    entry["request"] =
                        json!({"method": "PUT", "url": format!("Observation/{resource_id}")});
                }
            }
            if relationship == "component" && attach_component(entries, &entry) {
                continue;
            }
            entries.push(entry);
            if relationship == "hasMember" {
                attach_member(entries);
            }
        }
    }
}

fn attach_component(entries: &mut [Json], entry: &Json) -> bool {
    let Some(parent) = entries.last_mut().and_then(|item| item.get_mut("resource")) else {
        return false;
    };
    let Some(resource) = entry.get("resource") else {
        return false;
    };
    let mut component = json!({"code": resource.get("code").cloned().unwrap_or(json!({}))});
    for (key, value) in resource.as_object().into_iter().flatten() {
        if key.starts_with("value") {
            component[key] = value.clone();
        }
    }
    if !parent
        .get("component")
        .map(|value| value.is_array())
        .unwrap_or(false)
    {
        parent["component"] = json!([]);
    }
    parent["component"].as_array_mut().unwrap().push(component);
    true
}

fn attach_member(entries: &mut [Json]) {
    if entries.len() < 2 {
        return;
    }
    let reference = entries
        .last()
        .and_then(|entry| entry.get("fullUrl").cloned())
        .or_else(|| {
            entries
                .last()
                .and_then(|entry| entry.pointer("/resource/id"))
                .and_then(|id| id.as_str())
                .map(|id| json!(format!("Observation/{id}")))
        });
    let Some(reference) = reference else {
        return;
    };
    let parent = entries.len() - 2;
    let resource = &mut entries[parent]["resource"];
    if !resource
        .get("hasMember")
        .map(|value| value.is_array())
        .unwrap_or(false)
    {
        resource["hasMember"] = json!([]);
    }
    resource["hasMember"]
        .as_array_mut()
        .unwrap()
        .push(json!({"reference": reference}));
}

fn value_key(item_type: &str, item: &Json) -> &'static str {
    if (item_type == "decimal" || item_type == "integer")
        && crate::ext::first_ext(item, UNIT).is_some()
    {
        return "valueQuantity";
    }
    match item_type {
        "boolean" => "valueBoolean",
        "decimal" => "valueDecimal",
        "integer" => "valueInteger",
        "date" => "valueDate",
        "dateTime" => "valueDateTime",
        "time" => "valueTime",
        "quantity" => "valueQuantity",
        "coding" | "choice" => "valueCodeableConcept",
        _ => "valueString",
    }
}

fn quantity_or(item_type: &str, item: &Json, value: Json) -> Json {
    if let Some(unit) = crate::ext::first_ext(item, UNIT).and_then(value_of) {
        if item_type == "decimal" || item_type == "integer" || item_type == "quantity" {
            return json!({
                "value": value.as_f64().or_else(|| value.as_i64().map(|n| n as f64)).unwrap_or(0.0),
                "unit": unit.get("display").or_else(|| unit.get("code")).cloned().unwrap_or(json!("")),
                "system": unit.get("system").cloned().unwrap_or(json!("http://unitsofmeasure.org")),
                "code": unit.get("code").cloned().unwrap_or(json!(""))
            });
        }
    }
    value
}

fn extract_definition(
    chart: &mut dyn Chart,
    questionnaire: &Json,
    response: &Json,
    entries: &mut Vec<Json>,
) -> Result<(), String> {
    let mut ids = BTreeMap::new();
    allocate(questionnaire, &mut ids);
    let mut items = vec![questionnaire.clone()];
    each_item(questionnaire, &mut |item| {
        allocate(item, &mut ids);
        items.push(item.clone());
    });
    for item in &items {
        for ext in crate::ext::extensions(item, DEFINITION_EXTRACT) {
            let canonical = crate::ext::nested(ext, "definition")
                .and_then(value_of)
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if canonical.is_empty() {
                continue;
            }
            let link_id = item.get("linkId").and_then(|value| value.as_str());
            if link_id.is_some()
                && answers(response, link_id.unwrap()).is_empty()
                && item.get("item").is_none()
            {
                continue;
            }
            let type_name = resource_type_from(canonical);
            let mut resource = json!({"resourceType": type_name});
            let profile = canonical.split('#').next().unwrap_or(canonical);
            if profile.contains("/StructureDefinition/")
                && !profile.ends_with(&format!("/{type_name}"))
            {
                resource["meta"] = json!({"profile": [profile]});
            }
            fill_definition(
                chart,
                questionnaire,
                response,
                &mut resource,
                canonical,
                &ids,
            )?;
            let id = resource
                .get("id")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string());
            let full_url = crate::ext::nested(ext, "fullUrl")
                .and_then(|node| crate::ext::expression_of(node))
                .and_then(|(language, expression)| {
                    eval_text(chart, language, expression, response, &Default::default()).ok()
                })
                .and_then(|values| values.first().cloned())
                .or_else(|| ids.values().next().cloned().map(|value| json!(value)));
            let mut entry = json!({
                "resource": resource,
                "request": {
                    "method": if id.is_some() { "PUT" } else { "POST" },
                    "url": if let Some(id) = &id { format!("{type_name}/{id}") } else { type_name.clone() }
                }
            });
            if let Some(full_url) = full_url {
                entry["fullUrl"] = full_url;
            }
            entries.push(entry);
        }
    }
    Ok(())
}

fn fill_definition(
    chart: &mut dyn Chart,
    questionnaire: &Json,
    response: &Json,
    resource: &mut Json,
    canonical: &str,
    ids: &BTreeMap<String, String>,
) -> Result<(), String> {
    let mut items = Vec::new();
    each_item(questionnaire, &mut |item| items.push(item.clone()));
    for item in items {
        if let Some(definition) = item.get("definition").and_then(|value| value.as_str()) {
            if let Some(path) = definition.split('#').nth(1) {
                if !same_profile(definition, canonical) {
                    continue;
                }
                let link_id = item
                    .get("linkId")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let values = answer_values(response, link_id);
                for value in values {
                    set_fhir_path(resource, path, value);
                }
            }
        }
        for ext in crate::ext::extensions(&item, DEFINITION_VALUE) {
            let Some((language, expression)) = crate::ext::expression_of(ext) else {
                continue;
            };
            let path = crate::ext::nested(ext, "definition")
                .and_then(value_of)
                .and_then(|value| value.as_str())
                .and_then(|value| value.split('#').nth(1))
                .unwrap_or("");
            if path.is_empty() {
                continue;
            }
            let mut vars = BTreeMap::new();
            for (name, id) in ids {
                vars.insert(name.clone(), vec![crate::evalexpr::json_value(&json!(id))]);
            }
            let values =
                eval_text(chart, language, expression, response, &vars).unwrap_or_default();
            for value in values {
                set_fhir_path(resource, path, value);
            }
        }
    }
    Ok(())
}

fn extract_template(
    chart: &mut dyn Chart,
    questionnaire: &Json,
    response: &Json,
    entries: &mut Vec<Json>,
) -> Result<(), String> {
    let mut nodes = vec![questionnaire.clone()];
    each_item(questionnaire, &mut |item| nodes.push(item.clone()));
    for node in nodes {
        for ext in crate::ext::extensions(&node, TEMPLATE_EXTRACT) {
            let reference = crate::ext::nested(ext, "template")
                .and_then(value_of)
                .and_then(|value| value.get("reference").or(Some(value)))
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let Some(id) = reference.strip_prefix('#') else {
                continue;
            };
            let Some(template) = questionnaire
                .get("contained")
                .and_then(|value| value.as_array())
                .and_then(|items| {
                    items
                        .iter()
                        .find(|item| item.get("id").and_then(|value| value.as_str()) == Some(id))
                })
                .cloned()
            else {
                continue;
            };
            let focus = node
                .get("linkId")
                .and_then(|value| value.as_str())
                .and_then(|link_id| crate::items::item_by_link(response, link_id))
                .cloned()
                .unwrap_or_else(|| response.clone());
            let mut resource = template;
            resource.as_object_mut().map(|object| object.remove("id"));
            apply_template(chart, &mut resource, &focus)?;
            let type_name = resource
                .get("resourceType")
                .and_then(|value| value.as_str())
                .unwrap_or("Resource");
            let id = resource
                .get("id")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string());
            entries.push(json!({
                "resource": resource,
                "request": {
                    "method": if id.is_some() { "PUT" } else { "POST" },
                    "url": if let Some(id) = &id { format!("{type_name}/{id}") } else { type_name.to_string() }
                }
            }));
        }
    }
    Ok(())
}

fn apply_template(chart: &mut dyn Chart, node: &mut Json, focus: &Json) -> Result<(), String> {
    if let Some(object) = node.as_object_mut() {
        let keys: Vec<String> = object.keys().cloned().collect();
        for key in keys {
            let child = object.get_mut(&key).unwrap();
            if key.starts_with('_') || child.is_object() || child.is_array() {
                apply_template(chart, child, focus)?;
            }
        }
    } else if let Some(items) = node.as_array_mut() {
        for item in items {
            apply_template(chart, item, focus)?;
        }
    }
    let contexts: Vec<Json> = crate::ext::extensions(node, TEMPLATE_CONTEXT)
        .into_iter()
        .cloned()
        .collect();
    let values: Vec<Json> = crate::ext::extensions(node, TEMPLATE_VALUE)
        .into_iter()
        .cloned()
        .collect();
    if contexts.is_empty() && values.is_empty() {
        return Ok(());
    }
    for ext in &contexts {
        if let Some((language, expression)) = crate::ext::expression_of(ext) {
            let result = eval_text(chart, language, expression, focus, &Default::default())
                .unwrap_or_default();
            if result.is_empty() {
                *node = Json::Null;
                return Ok(());
            }
        }
    }
    for ext in &values {
        if let Some((language, expression)) = crate::ext::expression_of(ext) {
            let result = eval_text(chart, language, expression, focus, &Default::default())
                .unwrap_or_default();
            if result.is_empty() {
                *node = Json::Null;
                return Ok(());
            }
            if let Some(value) = result.first() {
                if let Some(primitive) = node.as_object_mut() {
                    primitive.remove("extension");
                    let _ = primitive;
                }
                *node = value.clone();
            }
        }
    }
    Ok(())
}

fn allocate(node: &Json, ids: &mut BTreeMap<String, String>) {
    if let Some(name) = crate::ext::ext_text(node, ALLOCATE_ID) {
        let name = name.trim_start_matches('%').to_string();
        ids.entry(name).or_insert_with(new_id);
    }
}

fn resource_type_from(canonical: &str) -> String {
    let without_version = canonical.split('|').next().unwrap_or(canonical);
    let without_hash = without_version.split('#').next().unwrap_or(without_version);
    without_hash
        .rsplit('/')
        .next()
        .unwrap_or("Resource")
        .to_string()
}

fn same_profile(definition: &str, canonical: &str) -> bool {
    let left = definition.split('#').next().unwrap_or(definition);
    let right = canonical.split('#').next().unwrap_or(canonical);
    left == right || resource_type_from(left) == resource_type_from(right)
}

fn set_fhir_path(resource: &mut Json, path: &str, value: Json) {
    let type_name = resource
        .get("resourceType")
        .and_then(|item| item.as_str())
        .unwrap_or("");
    let relative = path.strip_prefix(&format!("{type_name}.")).unwrap_or(path);
    let segments: Vec<&str> = relative
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        return;
    }
    write_segments(resource, &segments, value);
}

fn write_segments(node: &mut Json, segments: &[&str], value: Json) {
    if !node.is_object() {
        *node = json!({});
    }
    let key = segments[0];
    if segments.len() == 1 {
        let object = node.as_object_mut().expect("object");
        match object.get_mut(key) {
            None => {
                object.insert(key.to_string(), value);
            }
            Some(Json::Array(items)) => items.push(value),
            Some(existing) => {
                let prior = existing.clone();
                *existing = json!([prior, value]);
            }
        }
        return;
    }
    let object = node.as_object_mut().expect("object");
    let child = object.entry(key.to_string()).or_insert_with(|| json!({}));
    if !child.is_object() && !child.is_array() {
        *child = json!({});
    }
    if child.is_array() {
        let list = child.as_array_mut().unwrap();
        if list.is_empty() || !list.last().unwrap().is_object() {
            list.push(json!({}));
        }
        write_segments(list.last_mut().unwrap(), &segments[1..], value);
    } else {
        write_segments(child, &segments[1..], value);
    }
}

fn response_reference(response: &Json) -> String {
    match response.get("id").and_then(|value| value.as_str()) {
        Some(id) => format!("QuestionnaireResponse/{id}"),
        None => "QuestionnaireResponse".into(),
    }
}
