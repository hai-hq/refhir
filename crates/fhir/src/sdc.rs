//! SDC operations on the open transaction.
//!
//! [`Chart`] is the boundary so the questionnaire engine never opens its own
//! database. Reads and searches use the caller's scopes: population cannot
//! return an Observation the caller could not search.

use std::collections::BTreeMap;

use refhir_database::Transaction;
use refhir_fhirpath::Model;
use refhir_sdc::{behavior_document, scope_allows, Chart};
use serde_json::{json, Value};

use crate::catalog::{Catalog, ResourceModel};
use crate::error::FhirError;
use crate::ops;
use crate::search;

struct Bridge<'a> {
    tx: &'a mut Transaction,
    catalog: &'a Catalog,
    scopes: Option<Vec<String>>,
}

impl Chart for Bridge<'_> {
    fn read_type(&mut self, type_name: &str, id: &str) -> Result<Option<Value>, String> {
        if !self.allows(type_name) || self.catalog.deny_response_read(type_name).is_err() {
            return Ok(None);
        }
        let Ok(model) = self.catalog.get(type_name) else {
            return Ok(None);
        };
        match ops::read(self.tx, model, id) {
            Ok(value) => Ok(Some(value)),
            Err(FhirError::NotFound(_)) => Ok(None),
            Err(err) => Err(err.to_string()),
        }
    }

    fn search_type(
        &mut self,
        type_name: &str,
        params: &[(&str, String)],
    ) -> Result<Vec<Value>, String> {
        if !self.allows(type_name) || self.catalog.deny_response_read(type_name).is_err() {
            return Ok(Vec::new());
        }
        let Ok(model) = self.catalog.get(type_name) else {
            return Ok(Vec::new());
        };
        let (indexed, rest) = split_params(model, params);
        let bundle = search::search(self.tx, self.catalog, model, &indexed).map_err(|err| err.to_string())?;
        let mut rows = bundle_resources(&bundle);
        rows.retain(|row| rest.iter().all(|(name, value)| field_matches(row, name, value)));
        Ok(rows)
    }

    fn allows(&self, type_name: &str) -> bool {
        match &self.scopes {
            None => true,
            Some(scopes) => scope_allows(scopes, type_name),
        }
    }

    fn path_model(&self) -> Model {
        self.catalog.path_model().clone()
    }

    fn translate(&mut self, map_url: &str, system: Option<&str>, code: &str) -> Option<Value> {
        self.catalog
            .terms()
            .translate_coding(map_url, system, code)
    }

    fn in_valueset(&mut self, system: Option<&str>, code: &str, value_set: &str) -> Option<bool> {
        let result = self
            .catalog
            .terms()
            .validate_code(Some(value_set), system, code, None);
        if result.get("resourceType").and_then(|value| value.as_str()) == Some("OperationOutcome") {
            return None;
        }
        result
            .get("parameter")
            .and_then(|value| value.as_array())
            .and_then(|items| {
                items.iter().find_map(|parameter| {
                    if parameter.get("name").and_then(|value| value.as_str()) == Some("result") {
                        parameter.get("valueBoolean").and_then(|value| value.as_bool())
                    } else {
                        None
                    }
                })
            })
    }
}

pub fn operation(
    tx: &mut Transaction,
    catalog: &Catalog,
    type_name: &str,
    op: &str,
    args: &Value,
) -> Result<Value, FhirError> {
    let mut chart = Bridge {
        tx,
        catalog,
        scopes: scopes_of(args),
    };
    let result = match op {
        "$populate" => {
            expect(type_name, "Questionnaire")?;
            let questionnaire = load_questionnaire(&mut chart, args).map_err(FhirError::Invalid)?;
            let args = normalize_populate(args);
            refhir_sdc::populate(&mut chart, &questionnaire, &args)
        }
        "$assemble" => {
            expect(type_name, "Questionnaire")?;
            let questionnaire = load_questionnaire(&mut chart, args).map_err(FhirError::Invalid)?;
            refhir_sdc::assemble(&mut chart, &questionnaire)
        }
        "$next-question" => {
            expect(type_name, "Questionnaire")?;
            let response = load_response(args).map_err(FhirError::Invalid)?;
            refhir_sdc::next_question(&mut chart, &response)
        }
        "$behavior" => {
            expect(type_name, "Questionnaire")?;
            let (questionnaire, response) = load_behavior(&mut chart, args).map_err(FhirError::Invalid)?;
            behavior_document(&mut chart, &questionnaire, &response)
        }
        "$extract" => {
            expect(type_name, "QuestionnaireResponse")?;
            let response = load_response(args).map_err(FhirError::Invalid)?;
            refhir_sdc::extract(&mut chart, &response)
        }
        _ => return Err(FhirError::Unsupported(format!("unknown operation {op}"))),
    };
    result.map_err(FhirError::Invalid)
}

/// Validate a completed QuestionnaireResponse before it is stored.
/// An archiver stores the body unchanged.
pub fn prepare_store(
    tx: &mut Transaction,
    catalog: &Catalog,
    args: &Value,
    resource: Value,
) -> Result<Value, FhirError> {
    if resource.get("resourceType").and_then(|value| value.as_str()) != Some("QuestionnaireResponse")
    {
        return Ok(resource);
    }
    if catalog.sdc_archiver() {
        return Ok(resource);
    }
    let mut chart = Bridge {
        tx,
        catalog,
        scopes: scopes_of(args),
    };
    match refhir_sdc::prepare_completed(&mut chart, &resource) {
        Ok(value) => Ok(value),
        Err(outcome) => Err(FhirError::Outcome(outcome)),
    }
}

fn expect(actual: &str, wanted: &str) -> Result<(), FhirError> {
    if actual == wanted {
        Ok(())
    } else {
        Err(FhirError::Invalid(format!(
            "{wanted} operation was addressed to {actual}"
        )))
    }
}

fn scopes_of(args: &Value) -> Option<Vec<String>> {
    args.get("_readScopes").and_then(|value| value.as_array()).map(|items| {
        items
            .iter()
            .filter_map(|value| value.as_str().map(|item| item.to_string()))
            .collect()
    })
}

fn load_questionnaire(chart: &mut Bridge<'_>, args: &Value) -> Result<Value, String> {
    if let Some(resource) = parameter_resource(args, "questionnaire") {
        return Ok(resource);
    }
    if let Some(resource) = args.get("resource") {
        if resource.get("resourceType").and_then(|value| value.as_str()) == Some("Questionnaire") {
            return Ok(resource.clone());
        }
    }
    if let Some(id) = args.get("id").and_then(|value| value.as_str()) {
        if let Some(found) = chart.read_type("Questionnaire", id)? {
            return Ok(found);
        }
    }
    let canonical = parameter_canonical(args, "questionnaire")
        .or_else(|| parameter_reference(args, "questionnaireRef"))
        .or_else(|| args.get("questionnaire").and_then(|value| value.as_str()).map(|value| value.to_string()));
    if let Some(canonical) = canonical {
        if let Some(id) = canonical.strip_prefix("Questionnaire/") {
            if let Some(found) = chart.read_type("Questionnaire", id)? {
                return Ok(found);
            }
        }
        if let Some(found) = refhir_sdc::find_canonical(chart, "Questionnaire", &canonical)? {
            return Ok(found);
        }
    }
    if let Some(identifier) = parameter_canonical(args, "identifier") {
        let rows = chart.search_type("Questionnaire", &[("identifier", identifier)])?;
        if let Some(found) = rows.into_iter().next() {
            return Ok(found);
        }
    }
    Err("questionnaire was not found".into())
}

fn load_response(args: &Value) -> Result<Value, String> {
    if let Some(resource) = parameter_resource(args, "questionnaireResponse")
        .or_else(|| parameter_resource(args, "response"))
    {
        return Ok(resource);
    }
    let resource = args
        .get("resource")
        .cloned()
        .or_else(|| args.get("response").cloned())
        .ok_or_else(|| "QuestionnaireResponse is required".to_string())?;
    if resource.get("resourceType").and_then(|value| value.as_str()) == Some("Parameters") {
        return Err("QuestionnaireResponse is required".into());
    }
    Ok(resource)
}

fn load_behavior(chart: &mut Bridge<'_>, args: &Value) -> Result<(Value, Value), String> {
    if let Some(resource) = args.get("resource") {
        if let (Some(questionnaire), Some(response)) = (
            resource.get("questionnaire").filter(|value| value.is_object()),
            resource.get("response").filter(|value| value.is_object()),
        ) {
            return Ok((questionnaire.clone(), response.clone()));
        }
    }
    if let (Some(questionnaire), Some(response)) = (
        args.get("questionnaire").filter(|value| value.is_object()).cloned(),
        args.get("response").filter(|value| value.is_object()).cloned(),
    ) {
        return Ok((questionnaire, response));
    }
    if let Some(questionnaire) = parameter_resource(args, "questionnaire") {
        let response = parameter_resource(args, "response").unwrap_or_else(|| {
            json!({"resourceType": "QuestionnaireResponse", "status": "in-progress"})
        });
        return Ok((questionnaire, response));
    }
    let response = load_response(args)?;
    if response.get("resourceType").and_then(|value| value.as_str()) == Some("Questionnaire") {
        return Ok((
            response,
            json!({"resourceType": "QuestionnaireResponse", "status": "in-progress"}),
        ));
    }
    let questionnaire = load_questionnaire(chart, &json!({
        "questionnaire": response.get("questionnaire").cloned().unwrap_or(Value::Null),
        "resource": response.get("contained").and_then(|items| items.as_array()).and_then(|items| {
            items.iter().find(|item| item.get("resourceType").and_then(|value| value.as_str()) == Some("Questionnaire")).cloned()
        }).unwrap_or(Value::Null)
    }))?;
    Ok((questionnaire, response))
}

fn normalize_populate(args: &Value) -> Value {
    let mut out = args.clone();
    if out.get("subject").and_then(|value| value.as_str()).is_none() {
        if let Some(subject) = parameter_reference(args, "subject")
            .or_else(|| parameter_canonical(args, "subject"))
        {
            out["subject"] = json!(subject);
        }
    }
    if out.get("content").is_none() {
        let content = parameter_resources(args, "content");
        if !content.is_empty() {
            out["content"] = json!(content);
        }
    }
    if let Some(local) = parameter_bool(args, "local") {
        out["local"] = json!(local);
    }
    out
}

fn parameters(args: &Value) -> Option<&Vec<Value>> {
    let resource = args.get("resource")?;
    if resource.get("resourceType").and_then(|value| value.as_str()) != Some("Parameters") {
        return None;
    }
    resource.get("parameter")?.as_array()
}

fn named<'a>(args: &'a Value, name: &str) -> Vec<&'a Value> {
    parameters(args)
        .into_iter()
        .flatten()
        .filter(|parameter| parameter.get("name").and_then(|value| value.as_str()) == Some(name))
        .collect()
}

fn parameter_resource(args: &Value, name: &str) -> Option<Value> {
    named(args, name).into_iter().find_map(|parameter| {
        parameter
            .get("resource")
            .cloned()
            .or_else(|| parameter.pointer("/part").and_then(part_resource))
    })
}

fn parameter_resources(args: &Value, name: &str) -> Vec<Value> {
    named(args, name)
        .into_iter()
        .filter_map(|parameter| parameter.get("resource").cloned())
        .collect()
}

fn part_resource(parts: &Value) -> Option<Value> {
    parts.as_array()?.iter().find_map(|part| part.get("resource").cloned())
}

fn parameter_canonical(args: &Value, name: &str) -> Option<String> {
    named(args, name).into_iter().find_map(|parameter| {
        parameter
            .get("valueCanonical")
            .or_else(|| parameter.get("valueUri"))
            .or_else(|| parameter.get("valueString"))
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
    })
}

fn parameter_reference(args: &Value, name: &str) -> Option<String> {
    named(args, name).into_iter().find_map(|parameter| {
        parameter
            .pointer("/valueReference/reference")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
    })
}

fn parameter_bool(args: &Value, name: &str) -> Option<bool> {
    named(args, name)
        .into_iter()
        .find_map(|parameter| parameter.get("valueBoolean").and_then(|value| value.as_bool()))
}

fn split_params(
    model: &ResourceModel,
    params: &[(&str, String)],
) -> (BTreeMap<String, String>, Vec<(String, String)>) {
    let mut indexed = BTreeMap::new();
    let mut rest = Vec::new();
    for (name, value) in params {
        let base = name.split(':').next().unwrap_or(name);
        if base.starts_with('_') || model.ready_param(base).is_some() {
            indexed.insert((*name).to_string(), value.clone());
        } else {
            rest.push(((*name).to_string(), value.clone()));
        }
    }
    (indexed, rest)
}

fn bundle_resources(bundle: &Value) -> Vec<Value> {
    bundle
        .get("entry")
        .and_then(|value| value.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("resource").cloned())
                .collect()
        })
        .unwrap_or_default()
}

fn field_matches(row: &Value, name: &str, value: &str) -> bool {
    let base = name.split(':').next().unwrap_or(name);
    match base {
        "_id" => row.get("id").and_then(|item| item.as_str()) == Some(value),
        "url" => url_matches(row, value),
        "status" | "title" | "gender" | "name" => {
            row.get(base).and_then(|item| item.as_str()) == Some(value)
        }
        "subject" | "patient" | "questionnaire" | "author" => {
            let field = if base == "patient" { "subject" } else { base };
            reference_matches(row.get(field), value)
        }
        "identifier" => identifier_matches(row.get("identifier"), value),
        "code" => code_matches(row, value),
        // An unindexed parameter must not widen into a full type scan.
        _ => false,
    }
}

fn url_matches(row: &Value, value: &str) -> bool {
    let url = row.get("url").and_then(|item| item.as_str()).unwrap_or("");
    if let Some((canonical, version)) = value.split_once('|') {
        url == canonical && row.get("version").and_then(|item| item.as_str()) == Some(version)
    } else {
        url == value
    }
}

fn reference_matches(node: Option<&Value>, value: &str) -> bool {
    let Some(node) = node else {
        return false;
    };
    node.get("reference").and_then(|item| item.as_str()) == Some(value) || node.as_str() == Some(value)
}

fn identifier_matches(node: Option<&Value>, value: &str) -> bool {
    let Some(node) = node else {
        return false;
    };
    let items = if node.is_array() {
        node.as_array().cloned().unwrap_or_default()
    } else {
        vec![node.clone()]
    };
    items.iter().any(|item| {
        let found = item.get("value").and_then(|part| part.as_str()).unwrap_or("");
        if let Some((system, code)) = value.split_once('|') {
            found == code && item.get("system").and_then(|part| part.as_str()) == Some(system)
        } else {
            found == value
        }
    })
}

fn code_matches(row: &Value, value: &str) -> bool {
    let wanted = value.rsplit('|').next().unwrap_or(value);
    let mut codes = Vec::new();
    collect_codes(row.get("code"), &mut codes);
    collect_codes(row.get("item"), &mut codes);
    codes.iter().any(|code| code == wanted)
}

fn collect_codes(node: Option<&Value>, codes: &mut Vec<String>) {
    let Some(node) = node else {
        return;
    };
    if let Some(code) = node.get("code").and_then(|value| value.as_str()) {
        codes.push(code.to_string());
    }
    if let Some(items) = node.as_array() {
        for item in items {
            collect_codes(Some(item), codes);
        }
    } else if let Some(object) = node.as_object() {
        for child in object.values() {
            if child.is_array() || child.is_object() {
                collect_codes(Some(child), codes);
            }
        }
    }
}
