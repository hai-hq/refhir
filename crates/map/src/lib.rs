//! FHIR mapping language used by SDC StructureMap population and extraction.
//!
//! The executor runs a StructureMap resource. `parse` turns a mapping-language
//! script into that resource so both forms share one implementation.

use std::collections::BTreeMap;

use refhir_fhirpath::evaluate_owned;
use serde_json::{json, Map, Value as Json};
use uuid::Uuid;

/// Transform `source` with `map`. `translate` resolves ConceptMap lookups.
pub fn apply(
    map: &Json,
    source: &Json,
    translate: &mut dyn FnMut(&str, Option<&str>, &str) -> Option<Json>,
) -> Result<Json, String> {
    let map = if map.get("group").is_some() {
        map.clone()
    } else if let Some(text) = map.get("text").and_then(|value| value.as_str()) {
        parse(text)?
    } else {
        return Err("StructureMap has no group".into());
    };
    let groups = map
        .get("group")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let group = groups
        .first()
        .cloned()
        .ok_or_else(|| "StructureMap has no group".to_string())?;
    let inputs = group
        .get("input")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let source_name = inputs
        .iter()
        .find(|input| input.get("mode").and_then(|value| value.as_str()) != Some("target"))
        .and_then(|input| input.get("name"))
        .and_then(|value| value.as_str())
        .unwrap_or("src");
    let target = inputs
        .iter()
        .find(|input| input.get("mode").and_then(|value| value.as_str()) == Some("target"))
        .cloned()
        .unwrap_or_else(
            || json!({"name": "out", "type": "QuestionnaireResponse", "mode": "target"}),
        );
    let target_name = target
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("out");
    let target_type = target
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("QuestionnaireResponse");
    let mut vars = BTreeMap::new();
    vars.insert(source_name.to_string(), source.clone());
    vars.insert(
        target_name.to_string(),
        json!({"resourceType": target_type}),
    );
    let rules = group
        .get("rule")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    apply_rules(&rules, &mut vars, translate)?;
    vars.get(target_name)
        .cloned()
        .ok_or_else(|| "mapping produced no target".to_string())
}

/// Parse a mapping-language group into a StructureMap resource.
pub fn parse(text: &str) -> Result<Json, String> {
    let mut name = "main".to_string();
    let mut source_name = "src".to_string();
    let mut source_type = "Resource".to_string();
    let mut target_name = "out".to_string();
    let mut target_type = "Resource".to_string();
    let mut rules = Vec::new();
    for raw in text.lines() {
        let line = raw.trim().trim_end_matches(';').trim();
        if line.is_empty() || line.starts_with("map ") || line == "{" || line == "}" {
            continue;
        }
        if let Some(rest) = line.strip_prefix("group ") {
            let rest = rest.trim_end_matches('{').trim();
            let (group_name, signature) = rest.split_once('(').unwrap_or((rest, ""));
            name = group_name.trim().to_string();
            let signature = signature.trim_end_matches(')');
            for part in signature.split(',') {
                let part = part.trim();
                let (head, type_name) = part.split_once(':').unwrap_or((part, "Resource"));
                let mut words = head.split_whitespace();
                let mode = words.next().unwrap_or("");
                let ident = words.next().unwrap_or("");
                let type_name = type_name.trim().to_string();
                if mode == "source" {
                    source_name = ident.to_string();
                    source_type = type_name;
                } else if mode == "target" {
                    target_name = ident.to_string();
                    target_type = type_name;
                }
            }
            continue;
        }
        if let Some((left, right)) = line.split_once("->") {
            rules.push(parse_rule(left.trim(), right.trim())?);
        }
    }
    Ok(json!({
        "resourceType": "StructureMap",
        "url": "urn:uuid:parsed",
        "status": "active",
        "group": [{
            "name": name,
            "typeMode": "none",
            "input": [
                {"name": source_name, "type": source_type, "mode": "source"},
                {"name": target_name, "type": target_type, "mode": "target"}
            ],
            "rule": rules
        }]
    }))
}

fn parse_rule(left: &str, right: &str) -> Result<Json, String> {
    let mut source_text = left;
    let mut variable = None;
    if let Some((path, alias)) = left.split_once(" as ") {
        source_text = path.trim();
        variable = Some(alias.trim().to_string());
    }
    let (context, element) = split_path(source_text);
    let mut source = json!({"context": context, "element": element});
    if let Some(variable) = &variable {
        source["variable"] = json!(variable);
    }
    let right = right.trim().trim_end_matches(';').trim();
    let (target_text, transform) = if let Some((path, transform)) = right.split_once('=') {
        (path.trim(), transform.trim())
    } else {
        (right, "copy()")
    };
    let (target_context, target_element) = split_path(target_text);
    let mut target = json!({"context": target_context, "element": target_element});
    if transform.starts_with("copy") {
        target["transform"] = json!("copy");
        if let Some(variable) = &variable {
            target["parameter"] = json!([{"valueId": variable}]);
        }
    } else if transform.starts_with("create") {
        target["transform"] = json!("create");
        if let Some(type_name) = transform
            .strip_prefix("create(")
            .and_then(|text| text.strip_suffix(')'))
        {
            let type_name = type_name.trim().trim_matches('\'').trim_matches('"');
            if !type_name.is_empty() {
                target["parameter"] = json!([{"valueString": type_name}]);
            }
        }
    } else if transform.starts_with("truncate") {
        target["transform"] = json!("truncate");
    } else if transform.starts_with("append") {
        target["transform"] = json!("append");
    } else if transform.starts_with("reference") {
        target["transform"] = json!("reference");
    } else if transform.starts_with("translate") {
        target["transform"] = json!("translate");
    } else if transform.starts_with("uuid") {
        target["transform"] = json!("uuid");
    } else if transform.starts_with("evaluate") {
        target["transform"] = json!("evaluate");
        if let Some(body) = transform
            .strip_prefix("evaluate(")
            .and_then(|text| text.strip_suffix(')'))
        {
            target["parameter"] = json!([{"valueString": body.trim().trim_matches('\'')}]);
        }
    } else {
        target["transform"] = json!("copy");
        target["parameter"] =
            json!([{"valueString": transform.trim_matches('\'').trim_matches('"')}]);
    }
    Ok(json!({"name": element, "source": [source], "target": [target]}))
}

fn split_path(path: &str) -> (String, String) {
    let path = path.trim();
    match path.split_once('.') {
        Some((context, element)) => (context.to_string(), element.to_string()),
        None => (path.to_string(), String::new()),
    }
}

fn apply_rules(
    rules: &[Json],
    vars: &mut BTreeMap<String, Json>,
    translate: &mut dyn FnMut(&str, Option<&str>, &str) -> Option<Json>,
) -> Result<(), String> {
    for rule in rules {
        apply_rule(rule, vars, translate)?;
    }
    Ok(())
}

fn apply_rule(
    rule: &Json,
    vars: &mut BTreeMap<String, Json>,
    translate: &mut dyn FnMut(&str, Option<&str>, &str) -> Option<Json>,
) -> Result<(), String> {
    let sources = rule
        .get("source")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let mut rows = vec![BTreeMap::new()];
    for source in &sources {
        let context = source
            .get("context")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let element = source
            .get("element")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let variable = source
            .get("variable")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let condition = source
            .get("condition")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let base = vars.get(context).cloned().unwrap_or(Json::Null);
        let values = if element.is_empty() {
            vec![base.clone()]
        } else {
            read_path(&base, element)
        };
        let mut next = Vec::new();
        for row in &rows {
            for value in &values {
                if !condition.is_empty() {
                    let mut focus = base.clone();
                    if let Some(object) = focus.as_object_mut() {
                        object.insert(variable.to_string(), value.clone());
                    }
                    let answer = evaluate_owned(condition, &focus).unwrap_or_default();
                    let ok = answer.iter().any(|item| item.render() == "true");
                    if !ok {
                        continue;
                    }
                }
                let mut copy = row.clone();
                if !variable.is_empty() {
                    copy.insert(variable.to_string(), value.clone());
                }
                next.push(copy);
            }
        }
        rows = next;
    }
    if rows.is_empty() {
        rows.push(BTreeMap::new());
    }
    let targets = rule
        .get("target")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let nested = rule
        .get("rule")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    for row in rows {
        for (key, value) in &row {
            vars.insert(key.clone(), value.clone());
        }
        for target in &targets {
            apply_target(target, vars, &row, translate)?;
        }
        if !nested.is_empty() {
            apply_rules(&nested, vars, translate)?;
        }
    }
    Ok(())
}

fn apply_target(
    target: &Json,
    vars: &mut BTreeMap<String, Json>,
    row: &BTreeMap<String, Json>,
    translate: &mut dyn FnMut(&str, Option<&str>, &str) -> Option<Json>,
) -> Result<(), String> {
    let context = target
        .get("context")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let element = target
        .get("element")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let transform = target
        .get("transform")
        .and_then(|value| value.as_str())
        .unwrap_or("copy");
    let produced = match transform {
        "create" => {
            let type_name = param_string(target).unwrap_or_default();
            if type_name.is_empty() {
                json!({})
            } else {
                json!({"resourceType": type_name})
            }
        }
        "uuid" => json!(format!("urn:uuid:{}", Uuid::new_v4())),
        "truncate" => json!(param_value(target, row)
            .as_str()
            .unwrap_or("")
            .chars()
            .take(32)
            .collect::<String>()),
        "append" => {
            let current = vars.get(context).cloned().unwrap_or(Json::Null);
            let prior = read_path(&current, element)
                .first()
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            let extra = param_value(target, row);
            json!(format!("{prior}{}", extra.as_str().unwrap_or("")))
        }
        "reference" => {
            let value = param_value(target, row);
            let reference = value
                .get("reference")
                .and_then(|item| item.as_str())
                .map(|item| item.to_string())
                .or_else(|| {
                    let type_name = value.get("resourceType").and_then(|item| item.as_str())?;
                    let id = value.get("id").and_then(|item| item.as_str())?;
                    Some(format!("{type_name}/{id}"))
                })
                .unwrap_or_default();
            json!({"reference": reference})
        }
        "translate" => {
            let value = param_value(target, row);
            let system = value.get("system").and_then(|item| item.as_str());
            let code = value
                .get("code")
                .and_then(|item| item.as_str())
                .or_else(|| value.as_str())
                .unwrap_or("");
            let map = param_string(target).unwrap_or("");
            translate(map, system, code).unwrap_or(value)
        }
        "evaluate" => {
            let expression = param_string(target).unwrap_or("");
            let focus = vars.get(context).cloned().unwrap_or(Json::Null);
            let answer = evaluate_owned(expression, &focus).unwrap_or_default();
            answer
                .first()
                .map(|item| json!(item.render()))
                .unwrap_or(Json::Null)
        }
        _ => param_value(target, row),
    };
    if let Some(variable) = target.get("variable").and_then(|value| value.as_str()) {
        vars.insert(variable.to_string(), produced.clone());
    }
    if element.is_empty() {
        vars.insert(context.to_string(), produced);
        return Ok(());
    }
    let current = vars.entry(context.to_string()).or_insert_with(|| json!({}));
    write_path(current, element, produced, transform == "create");
    Ok(())
}

fn param_string(target: &Json) -> Option<&str> {
    target
        .get("parameter")
        .and_then(|value| value.as_array())
        .and_then(|items| {
            items
                .iter()
                .find_map(|item| item.get("valueString").and_then(|value| value.as_str()))
        })
}

fn param_value(target: &Json, row: &BTreeMap<String, Json>) -> Json {
    if let Some(id) = target
        .pointer("/parameter/0/valueId")
        .and_then(|value| value.as_str())
    {
        return row.get(id).cloned().unwrap_or(Json::Null);
    }
    if let Some(text) = param_string(target) {
        return json!(text);
    }
    if let Some(source) = target
        .pointer("/parameter/0/valueId")
        .and_then(|value| value.as_str())
    {
        return row.get(source).cloned().unwrap_or(Json::Null);
    }
    Json::Null
}

fn read_path(value: &Json, path: &str) -> Vec<Json> {
    let mut current = vec![value.clone()];
    for segment in path.split('.').filter(|segment| !segment.is_empty()) {
        let mut next = Vec::new();
        for item in current {
            match item.get(segment) {
                Some(Json::Array(items)) => next.extend(items.clone()),
                Some(child) => next.push(child.clone()),
                None => {}
            }
        }
        current = next;
    }
    current
}

fn write_path(root: &mut Json, path: &str, value: Json, append: bool) {
    let segments: Vec<&str> = path
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        *root = value;
        return;
    }
    if !root.is_object() {
        *root = json!({});
    }
    write_segments(root, &segments, value, append);
}

fn write_segments(node: &mut Json, segments: &[&str], value: Json, append: bool) {
    let key = segments[0];
    if segments.len() == 1 {
        let object = node.as_object_mut().expect("object");
        if append {
            push_value(object, key, value);
        } else {
            object.insert(key.to_string(), value);
        }
        return;
    }
    let object = node.as_object_mut().expect("object");
    let child = object.entry(key).or_insert_with(|| json!({}));
    if child.is_array() {
        if child.as_array().unwrap().is_empty() {
            child.as_array_mut().unwrap().push(json!({}));
        }
        let last = child.as_array_mut().unwrap().last_mut().unwrap();
        write_segments(last, &segments[1..], value, append);
    } else {
        if !child.is_object() {
            *child = json!({});
        }
        write_segments(child, &segments[1..], value, append);
    }
}

fn push_value(object: &mut Map<String, Json>, key: &str, value: Json) {
    match object.get_mut(key) {
        None => {
            object.insert(key.to_string(), json!([value]));
        }
        Some(Json::Array(items)) => items.push(value),
        Some(existing) => {
            let prior = existing.clone();
            *existing = json!([prior, value]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn copies_a_field_from_a_structured_map() {
        let map = json!({
            "resourceType": "StructureMap",
            "status": "active",
            "group": [{
                "name": "main",
                "input": [
                    {"name": "src", "type": "Patient", "mode": "source"},
                    {"name": "out", "type": "QuestionnaireResponse", "mode": "target"}
                ],
                "rule": [{
                    "name": "gender",
                    "source": [{"context": "src", "element": "gender", "variable": "g"}],
                    "target": [{
                        "context": "out",
                        "element": "status",
                        "transform": "copy",
                        "parameter": [{"valueId": "g"}]
                    }]
                }]
            }]
        });
        let got = apply(
            &map,
            &json!({"resourceType": "Patient", "gender": "female"}),
            &mut |_map, _system, _code| None,
        )
        .unwrap();
        assert_eq!(got["resourceType"], "QuestionnaireResponse");
        assert_eq!(got["status"], "female");
    }

    #[test]
    fn parses_a_mapping_script() {
        let script = r#"
            group main(source src : Patient, target out : QuestionnaireResponse) {
              src.birthDate as birth -> out.authored = copy();
            }
        "#;
        let map = parse(script).unwrap();
        let got = apply(
            &map,
            &json!({"resourceType": "Patient", "birthDate": "1980-01-02"}),
            &mut |_map, _system, _code| None,
        )
        .unwrap();
        assert_eq!(got["authored"], "1980-01-02");
    }

    #[test]
    fn translates_a_code() {
        let map = json!({
            "resourceType": "StructureMap",
            "status": "active",
            "group": [{
                "name": "main",
                "input": [
                    {"name": "src", "mode": "source"},
                    {"name": "out", "type": "Observation", "mode": "target"}
                ],
                "rule": [{
                    "name": "code",
                    "source": [{"context": "src", "element": "code", "variable": "c"}],
                    "target": [{
                        "context": "out",
                        "element": "code",
                        "transform": "translate",
                        "parameter": [{"valueId": "c"}, {"valueString": "http://example.org/ConceptMap/gender"}]
                    }]
                }]
            }]
        });
        let got = apply(&map, &json!({"code": "M"}), &mut |map, system, code| {
            assert_eq!(map, "http://example.org/ConceptMap/gender");
            assert_eq!(system, None);
            assert_eq!(code, "M");
            Some(json!({"coding": [{"code": "male"}]}))
        })
        .unwrap();
        assert_eq!(got["code"]["coding"][0]["code"], "male");
    }
}
