//! Extension URLs and small JSON helpers shared by the SDC operations.

use serde_json::Value as Json;

pub const LAUNCH: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-launchContext";
pub const INITIAL: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-initialExpression";
pub const CALCULATED: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-calculatedExpression";
pub const ENABLE_EXPR: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-enableWhenExpression";
pub const CANDIDATE: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-candidateExpression";
pub const CONTEXT_EXPR: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-contextExpression";
pub const ANSWER_EXPR: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-answerExpression";
pub const VARIABLE: &str = "http://hl7.org/fhir/StructureDefinition/variable";
pub const POPULATION: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-itemPopulationContext";
pub const LINK_PERIOD: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-observationLinkPeriod";
pub const OBS_EXTRACT: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-observationExtract";
pub const OBS_ENTRY: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-observationExtractEntry";
pub const OBS_CATEGORY: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-observation-extract-category";
pub const DEFINITION_EXTRACT: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-definitionExtract";
pub const DEFINITION_VALUE: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-definitionExtractValue";
pub const ALLOCATE_ID: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-extractAllocateId";
pub const TEMPLATE_EXTRACT: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-templateExtract";
pub const TEMPLATE_BUNDLE: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-templateExtractBundle";
pub const TEMPLATE_VALUE: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-templateExtractValue";
pub const TEMPLATE_CONTEXT: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-templateExtractContext";
pub const SOURCE_MAP: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-sourceStructureMap";
pub const SOURCE_QUERIES: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-sourceQueries";
pub const TARGET_MAP: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-targetStructureMap";
pub const SUB_QUESTIONNAIRE: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-subQuestionnaire";
pub const ASSEMBLE_EXPECTATION: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-assemble-expectation";
pub const ASSEMBLED_FROM: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-assembledFrom";
pub const ADAPTIVE: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-questionnaireAdaptive";
pub const IS_SUBJECT: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-isSubject";
pub const LIBRARY: &str = "http://hl7.org/fhir/StructureDefinition/cqf-library";
pub const UNIT: &str = "http://hl7.org/fhir/StructureDefinition/questionnaire-unit";
pub const ITEM_CONTROL: &str = "http://hl7.org/fhir/StructureDefinition/questionnaire-itemControl";
pub const HIDDEN: &str = "http://hl7.org/fhir/StructureDefinition/questionnaire-hidden";
pub const CONSTRAINT: &str = "http://hl7.org/fhir/StructureDefinition/questionnaire-constraint";
pub const MIN_VALUE: &str = "http://hl7.org/fhir/StructureDefinition/minValue";
pub const MAX_VALUE: &str = "http://hl7.org/fhir/StructureDefinition/maxValue";
pub const REGEX: &str = "http://hl7.org/fhir/StructureDefinition/regex";
pub const LINK_PREFIX: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-linkIdPrefix";
pub const ENDPOINT: &str = "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-endpoint";
pub const ENTRY_MODE: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-entryMode";
pub const PERFORMER: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaire-performerType";
pub const CRITICAL: &str =
    "http://hl7.org/fhir/StructureDefinition/rendering-criticalExtension";
pub const PROVENANCE: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-questionnaireresponse-provenance";
pub const TASK_ENDPOINT: &str =
    "http://hl7.org/fhir/uv/sdc/StructureDefinition/sdc-task-responseEndpoint";

pub fn client_extensions() -> &'static [&'static str] {
    &[ITEM_CONTROL, HIDDEN, TASK_ENDPOINT, TEMPLATE_BUNDLE]
}

pub fn extensions<'a>(node: &'a Json, url: &str) -> Vec<&'a Json> {
    node.get("extension")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("url").and_then(|value| value.as_str()) == Some(url))
                .collect()
        })
        .unwrap_or_default()
}

pub fn first_ext<'a>(node: &'a Json, url: &str) -> Option<&'a Json> {
    extensions(node, url).into_iter().next()
}

pub fn nested<'a>(ext: &'a Json, url: &str) -> Option<&'a Json> {
    ext.get("extension")
        .and_then(|value| value.as_array())
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.get("url").and_then(|value| value.as_str()) == Some(url))
        })
}

pub fn value_of(node: &Json) -> Option<&Json> {
    node.as_object()?
        .iter()
        .find(|(key, _)| key.starts_with("value") && *key != "value")
        .map(|(_, value)| value)
}

pub fn ext_bool(node: &Json, url: &str) -> Option<bool> {
    first_ext(node, url)
        .and_then(value_of)
        .and_then(|value| value.as_bool())
}

pub fn ext_text(node: &Json, url: &str) -> Option<String> {
    let ext = first_ext(node, url)?;
    if let Some(text) = value_of(ext).and_then(|value| value.as_str()) {
        return Some(text.to_string());
    }
    value_of(ext)
        .and_then(|value| value.get("reference"))
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .or_else(|| {
            value_of(ext)
                .and_then(|value| value.get("code"))
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        })
}

pub fn expression_of(ext: &Json) -> Option<(&str, &str)> {
    let expr = value_of(ext)
        .filter(|value| value.get("expression").is_some())
        .or_else(|| nested(ext, "expression").and_then(value_of))?;
    let text = expr.get("expression").and_then(|value| value.as_str())?;
    let language = expr
        .get("language")
        .and_then(|value| value.as_str())
        .unwrap_or("text/fhirpath");
    Some((language, text))
}

pub fn expression_name(ext: &Json) -> Option<String> {
    nested(ext, "name")
        .and_then(value_of)
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .or_else(|| {
            value_of(ext)
                .and_then(|value| value.get("name"))
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        })
}

pub fn push_extension(node: &mut Json, extension: Json) {
    let list = node
        .as_object_mut()
        .expect("object")
        .entry("extension")
        .or_insert_with(|| Json::Array(Vec::new()));
    list.as_array_mut().expect("array").push(extension);
}

pub fn reference_of(node: &Json) -> Option<String> {
    node.get("reference")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .or_else(|| {
            let type_name = node.get("resourceType").and_then(|value| value.as_str())?;
            let id = node.get("id").and_then(|value| value.as_str())?;
            Some(format!("{type_name}/{id}"))
        })
}

pub fn codes_of(node: &Json) -> Vec<(String, String)> {
    let mut found = Vec::new();
    push_codings(node.get("coding"), &mut found);
    match node.get("code") {
        Some(Json::Array(items)) => {
            for item in items {
                if item.get("code").is_some() {
                    push_codings(Some(item), &mut found);
                } else {
                    push_codings(item.get("coding"), &mut found);
                }
            }
        }
        Some(code) => push_codings(code.get("coding"), &mut found),
        None => {}
    }
    if let Some(items) = node.get("item").and_then(|value| value.as_array()) {
        for item in items {
            found.extend(codes_of(item));
        }
    }
    found
}

fn push_codings(node: Option<&Json>, found: &mut Vec<(String, String)>) {
    let Some(node) = node else {
        return;
    };
    let items = if node.is_array() {
        node.as_array().cloned().unwrap_or_default()
    } else {
        vec![node.clone()]
    };
    for coding in items {
        if let Some(code) = coding.get("code").and_then(|value| value.as_str()) {
            let system = coding
                .get("system")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            found.push((system, code.to_string()));
        }
    }
}
