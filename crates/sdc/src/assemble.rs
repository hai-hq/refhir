//! Modular questionnaire assembly.

use serde_json::{json, Value as Json};

use crate::chart::Chart;
use crate::evalexpr::find_canonical;
use crate::ext::{
    push_extension, ADAPTIVE, ASSEMBLED_FROM, ASSEMBLE_EXPECTATION, CRITICAL, ENDPOINT, ENTRY_MODE,
    LAUNCH, LIBRARY, LINK_PREFIX, PERFORMER, SUB_QUESTIONNAIRE, VARIABLE,
};
use crate::items::{item_by_link, prefix_link_ids};

const ROOT_BLOCKED: &[&str] = &[
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-definitionExtract",
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-itemExtractionContext",
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-itemPopulationContext",
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-observationExtract",
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-observation-extract-category",
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-observationLinkPeriod",
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-preferredTerminologyServer",
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-templateExtract",
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-templateExtractContext",
];

pub fn assemble(chart: &mut dyn Chart, questionnaire: &Json) -> Result<Json, String> {
    let mut seen = Vec::new();
    let mut assembled = questionnaire.clone();
    assemble_in(chart, &mut assembled, &mut seen)?;
    Ok(assembled)
}

fn assemble_in(
    chart: &mut dyn Chart,
    questionnaire: &mut Json,
    seen: &mut Vec<String>,
) -> Result<(), String> {
    let url = questionnaire
        .get("url")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    if !url.is_empty() {
        if seen.iter().any(|item| item == &url) {
            return Err(format!("questionnaire assembly cycle at {url}"));
        }
        seen.push(url.clone());
    }
    if let Some(code) = crate::ext::ext_text(questionnaire, ASSEMBLE_EXPECTATION) {
        if code == "assemble-child" || code == "independent-child" {
            return Err("questionnaire cannot be assembled as a root form".into());
        }
    }
    expand_definitions(chart, questionnaire)?;
    replace_subquestionnaires(chart, questionnaire, seen)?;
    if !url.is_empty() {
        seen.pop();
    }
    Ok(())
}

fn replace_subquestionnaires(
    chart: &mut dyn Chart,
    questionnaire: &mut Json,
    seen: &mut Vec<String>,
) -> Result<(), String> {
    let items = questionnaire
        .get("item")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let mut next = Vec::new();
    for item in items {
        if item.get("type").and_then(|value| value.as_str()) == Some("display") {
            if let Some(canonical) = crate::ext::ext_text(&item, SUB_QUESTIONNAIRE) {
                let child = load_child(chart, questionnaire, &canonical)?;
                if crate::ext::ext_bool(&child, ADAPTIVE) == Some(true) {
                    return Err("a sub-questionnaire cannot be adaptive".into());
                }
                align(questionnaire, &child)?;
                let mut child = child;
                assemble_in(chart, &mut child, seen)?;
                propagate(questionnaire, &item, &child)?;
                let prefix = crate::ext::ext_text(&item, LINK_PREFIX).unwrap_or_default();
                let mut imported = child
                    .get("item")
                    .and_then(|value| value.as_array())
                    .cloned()
                    .unwrap_or_default();
                if !prefix.is_empty() {
                    for imported in &mut imported {
                        if let Some(link_id) =
                            imported.get("linkId").and_then(|value| value.as_str())
                        {
                            imported["linkId"] = json!(format!("{prefix}{link_id}"));
                        }
                        prefix_link_ids(imported, &prefix);
                    }
                }
                remember_source(questionnaire, &canonical);
                next.extend(imported);
                continue;
            }
        }
        let mut item = item;
        replace_subquestionnaires(chart, &mut item, seen)?;
        next.push(item);
    }
    if questionnaire.get("item").is_some() || !next.is_empty() {
        questionnaire["item"] = json!(next);
    }
    Ok(())
}

fn load_child(chart: &mut dyn Chart, parent: &Json, canonical: &str) -> Result<Json, String> {
    if let Some(id) = canonical.strip_prefix('#') {
        return parent
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

fn align(parent: &Json, child: &Json) -> Result<(), String> {
    for field in ["language", "implicitRules"] {
        if let Some(child_value) = child.get(field) {
            if parent.get(field).is_some() && parent.get(field) != Some(child_value) {
                return Err(format!(
                    "sub-questionnaire {field} does not match the parent"
                ));
            }
        }
    }
    for url in [ENDPOINT, ENTRY_MODE, PERFORMER, CRITICAL] {
        let child_values = crate::ext::extensions(child, url);
        if child_values.is_empty() {
            continue;
        }
        let parent_values = crate::ext::extensions(parent, url);
        if parent_values.is_empty() {
            return Err(format!(
                "sub-questionnaire extension {url} is not declared on the parent"
            ));
        }
    }
    Ok(())
}

fn propagate(parent: &mut Json, display: &Json, child: &Json) -> Result<(), String> {
    for url in ROOT_BLOCKED {
        if crate::ext::first_ext(child, url).is_some() {
            return Err(format!(
                "sub-questionnaire must not carry {url} at the root"
            ));
        }
    }
    for ext in crate::ext::extensions(child, LIBRARY) {
        let canonical = crate::ext::value_of(ext).and_then(|value| value.as_str());
        let already = crate::ext::extensions(parent, LIBRARY)
            .iter()
            .any(|item| crate::ext::value_of(item).and_then(|value| value.as_str()) == canonical);
        if !already {
            push_extension(parent, (*ext).clone());
        }
    }
    for ext in crate::ext::extensions(child, LAUNCH) {
        push_extension(parent, (*ext).clone());
    }
    if display.get("linkId").is_some() {
        for ext in crate::ext::extensions(child, VARIABLE) {
            push_extension(parent, (*ext).clone());
        }
    }
    if let Some(contained) = child.get("contained").and_then(|value| value.as_array()) {
        let list = parent
            .as_object_mut()
            .expect("questionnaire")
            .entry("contained")
            .or_insert_with(|| json!([]));
        let list = list.as_array_mut().expect("contained");
        for resource in contained {
            let id = resource.get("id").and_then(|value| value.as_str());
            let present = list
                .iter()
                .any(|item| item.get("id").and_then(|value| value.as_str()) == id);
            if !present {
                list.push(resource.clone());
            }
        }
    }
    Ok(())
}

fn remember_source(questionnaire: &mut Json, canonical: &str) {
    let already = crate::ext::extensions(questionnaire, ASSEMBLED_FROM)
        .iter()
        .any(|ext| crate::ext::value_of(ext).and_then(|value| value.as_str()) == Some(canonical));
    if already {
        return;
    }
    push_extension(
        questionnaire,
        json!({
            "url": ASSEMBLED_FROM,
            "valueCanonical": canonical
        }),
    );
}

fn expand_definitions(chart: &mut dyn Chart, questionnaire: &mut Json) -> Result<(), String> {
    let items = questionnaire
        .get("item")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    if items.is_empty() {
        return Ok(());
    }
    let mut next = Vec::new();
    for item in items {
        let mut item = item;
        if let Some(definition) = item.get("definition").and_then(|value| value.as_str()) {
            if let Some((canonical, link_id)) = definition.split_once('#') {
                if canonical.contains("Questionnaire")
                    || chart
                        .search_type("Questionnaire", &[("url", canonical.to_string())])
                        .ok()
                        .and_then(|rows| rows.into_iter().next())
                        .is_some()
                {
                    if let Some(source) = find_canonical(chart, "Questionnaire", canonical)? {
                        if let Some(imported) = item_by_link(&source, link_id) {
                            item = imported.clone();
                        }
                    }
                } else if item.get("text").is_none() {
                    if let Some(definition_resource) =
                        find_canonical(chart, "StructureDefinition", canonical)?
                    {
                        if let Some(text) = element_text(&definition_resource, link_id) {
                            item["text"] = json!(text);
                        }
                    }
                }
            }
        }
        expand_definitions(chart, &mut item)?;
        next.push(item);
    }
    questionnaire["item"] = json!(next);
    Ok(())
}

fn element_text(definition: &Json, id: &str) -> Option<String> {
    definition
        .pointer("/snapshot/element")
        .and_then(|value| value.as_array())
        .and_then(|items| {
            items.iter().find(|element| {
                element.get("id").and_then(|value| value.as_str()) == Some(id)
                    || element.get("path").and_then(|value| value.as_str()) == Some(id)
            })
        })
        .and_then(|element| {
            element
                .get("short")
                .or_else(|| element.get("label"))
                .or_else(|| element.get("definition"))
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        })
}
