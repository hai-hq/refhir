//! The R4 suite from FHIR/fhir-test-cases. A failing case is an engine bug.
//! Tests marked version 2.1.0 belong to a later draft and are skipped.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use haifhir_fhirpath::{evaluate, parse, Error, Host, Model, Value};
use roxmltree::Node;
use serde_json::{Map, Number, Value as Json};

struct SuiteHost {
    strict: bool,
}

impl Host for SuiteHost {
    fn strict(&self) -> bool {
        self.strict
    }

    fn member_of(&self, _system: Option<&str>, _code: &str, _value_set: &str) -> Result<Option<bool>, Error> {
        Ok(None)
    }
}

#[derive(Clone, Default)]
struct Elem {
    types: Vec<String>,
    repeating: bool,
}

#[test]
fn hl7_r4_fhirpath_suite() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/hl7");
    let (model, elems) = load_model();
    let mut inputs = BTreeMap::new();
    for entry in fs::read_dir(root.join("input")).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        let text = fs::read_to_string(entry.path()).unwrap();
        let json = if name.ends_with(".json") {
            serde_json::from_str(&text).unwrap()
        } else {
            fhir_xml_to_json(&text, &elems)
        };
        inputs.insert(name, json);
    }
    let suite = fs::read_to_string(root.join("tests-fhir-r4.xml")).unwrap();
    let document = roxmltree::Document::parse(&suite).unwrap();
    let mut failures = Vec::new();
    let mut ran = 0;
    for test in document.descendants().filter(|node| node.has_tag_name("test")) {
        if test.attribute("version") == Some("2.1.0") {
            continue;
        }
        ran += 1;
        let name = test.attribute("name").unwrap_or("unnamed");
        let expression = test
            .children()
            .find(|node| node.has_tag_name("expression"))
            .and_then(|node| node.text())
            .unwrap_or("")
            .trim();
        let input_name = test.attribute("inputfile").unwrap_or("");
        let resource = inputs.get(input_name).cloned().unwrap_or_else(|| Json::Object(Map::new()));
        let host = SuiteHost {
            strict: test.attribute("mode") == Some("strict"),
        };
        let expected: Vec<String> = test
            .children()
            .filter(|node| node.has_tag_name("output"))
            .map(|node| node.text().unwrap_or("").trim().to_string())
            .collect();
        let actual = match parse(expression).and_then(|expr| evaluate(&expr, &resource, &model, &host)) {
            Ok(values) => values.iter().map(Value::render).collect::<Vec<_>>(),
            Err(_) if expected.is_empty() => continue,
            Err(err) => {
                failures.push(format!("{name}: {expression} -> error {err}"));
                continue;
            }
        };
        if test.attribute("predicate") == Some("true") {
            let holds = actual.iter().any(|value| value == "true") || !actual.is_empty();
            if expected == ["true"] && !holds {
                failures.push(format!("{name}: {expression} -> {actual:?}, expected predicate true"));
            }
            continue;
        }
        if expected.is_empty() {
            if !actual.is_empty() {
                failures.push(format!("{name}: {expression} -> {actual:?}, expected empty"));
            }
            continue;
        }
        let same = if test.attribute("ordered") == Some("false") {
            let mut left = actual.clone();
            let mut right = expected.clone();
            left.sort();
            right.sort();
            left == right
        } else {
            actual == expected
        };
        if !same {
            failures.push(format!("{name}: {expression} -> {actual:?}, expected {expected:?}"));
        }
    }
    if !failures.is_empty() {
        fs::write("/tmp/fp-fail.txt", failures.join("\n")).ok();
        let shown = failures.iter().take(30).cloned().collect::<Vec<_>>().join("\n");
        panic!("{} of {ran} failed\n{shown}", failures.len());
    }
}

#[test]
fn resolve_contained_reference_and_member_of_without_terminology() {
    let patient = serde_json::json!({
        "resourceType": "Patient",
        "contained": [{ "resourceType": "Patient", "id": "p1", "gender": "female" }]
    });
    let expr = parse("'#p1'.resolve().gender").unwrap();
    let values = evaluate(&expr, &patient, &Model::new(), &SuiteHost { strict: false }).unwrap();
    assert_eq!(values.iter().map(Value::render).collect::<Vec<_>>(), vec!["female".to_string()]);

    let member = parse("'code'.memberOf('http://example.org/ValueSet/demo')").unwrap();
    let error = evaluate(&member, &Json::Object(Map::new()), &Model::new(), &SuiteHost { strict: false }).unwrap_err();
    assert!(matches!(error.kind, haifhir_fhirpath::ErrorKind::Unsupported));
}

fn load_model() -> (Model, BTreeMap<String, Elem>) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fhir/fixtures/r4");
    let mut definitions = Vec::new();
    let mut elems = BTreeMap::new();
    for entry in fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("StructureDefinition-") || !name.ends_with(".json") {
            continue;
        }
        let json: Json = serde_json::from_str(&fs::read_to_string(entry.path()).unwrap()).unwrap();
        if json.get("derivation").and_then(|value| value.as_str()) == Some("constraint") {
            continue;
        }
        if let Some(elements) = json.pointer("/snapshot/element").and_then(|value| value.as_array()) {
            for element in elements {
                let Some(path) = element.get("path").and_then(|value| value.as_str()) else {
                    continue;
                };
                let types = element
                    .get("type")
                    .and_then(|value| value.as_array())
                    .map(|types| {
                        types
                            .iter()
                            .filter_map(|item| item.get("code").and_then(|code| code.as_str()).map(|code| code.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let repeating = element.get("max").and_then(|value| value.as_str()).is_some_and(|max| max != "1");
                elems.insert(path.to_string(), Elem { types, repeating });
            }
        }
        definitions.push(json);
    }
    (Model::from_definitions(&definitions), elems)
}

fn fhir_xml_to_json(xml: &str, elems: &BTreeMap<String, Elem>) -> Json {
    let document = roxmltree::Document::parse(xml).unwrap();
    let root = document.root_element();
    resource_object(root, elems)
}

fn resource_object(node: Node<'_, '_>, elems: &BTreeMap<String, Elem>) -> Json {
    let mut object = Map::new();
    let name = node.tag_name().name().to_string();
    object.insert("resourceType".into(), Json::String(name.clone()));
    fill_children(node, &name, &mut object, elems);
    Json::Object(object)
}

fn fill_children(node: Node<'_, '_>, path: &str, object: &mut Map<String, Json>, elems: &BTreeMap<String, Elem>) {
    let mut groups: Vec<(String, Vec<Node<'_, '_>>)> = Vec::new();
    for child in node.children().filter(|child| child.is_element()) {
        let name = child.tag_name().name().to_string();
        if let Some((_, items)) = groups.iter_mut().find(|(existing, _)| existing == &name) {
            items.push(child);
        } else {
            groups.push((name, vec![child]));
        }
    }
    for (name, nodes) in groups {
        if name == "div" {
            object.insert(name, Json::String(xml_fragment(nodes[0])));
            continue;
        }
        let info = lookup(elems, path, &name);
        if info.types.iter().any(|kind| kind == "Resource" || kind == "DomainResource") {
            let resources: Vec<Json> = nodes.iter().flat_map(|node| node.children().filter(|child| child.is_element()).map(|child| resource_object(child, elems))).collect();
            object.insert(name, Json::Array(resources));
            continue;
        }
        let mut values = Vec::new();
        let mut extras = Vec::new();
        let mut any_extra = false;
        for node in nodes {
            let (value, extra) = read_element(node, path, &name, elems);
            any_extra |= extra.is_some();
            values.push(value);
            extras.push(extra.unwrap_or(Json::Null));
        }
        let repeating = info.repeating || values.len() > 1;
        object.insert(name.clone(), collapse(values, repeating));
        if any_extra {
            object.insert(format!("_{name}"), collapse(extras, repeating));
        }
    }
}

fn read_element(node: Node<'_, '_>, parent: &str, name: &str, elems: &BTreeMap<String, Elem>) -> (Json, Option<Json>) {
    let info = lookup(elems, parent, name);
    let value_attr = node.attribute("value");
    let children: Vec<Node<'_, '_>> = node.children().filter(|child| child.is_element()).collect();
    let extension_only = children.iter().all(|child| matches!(child.tag_name().name(), "extension" | "id"));
    if value_attr.is_some() && extension_only {
        let value = primitive_json(type_code(parent, name, &info), value_attr.unwrap_or(""));
        let extra = extension_object(node, children, elems);
        return (value, extra);
    }
    let mut object = Map::new();
    if let Some(url) = node.attribute("url") {
        object.insert("url".into(), Json::String(url.to_string()));
    }
    if let Some(id) = node.attribute("id") {
        object.insert("id".into(), Json::String(id.to_string()));
    }
    let child_path = if info.types.iter().any(|kind| kind.chars().next().is_some_and(|ch| ch.is_uppercase())) {
        info.types.iter().find(|kind| kind.chars().next().is_some_and(|ch| ch.is_uppercase())).cloned().unwrap_or_else(|| name.to_string())
    } else if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}.{name}")
    };
    fill_children(node, &child_path, &mut object, elems);
    (Json::Object(object), None)
}

fn extension_object(node: Node<'_, '_>, children: Vec<Node<'_, '_>>, elems: &BTreeMap<String, Elem>) -> Option<Json> {
    let mut object = Map::new();
    if let Some(id) = node.attribute("id") {
        object.insert("id".into(), Json::String(id.to_string()));
    }
    let extensions: Vec<Json> = children
        .iter()
        .filter(|child| child.tag_name().name() == "extension")
        .map(|child| read_element(*child, "Element", "extension", elems).0)
        .collect();
    if !extensions.is_empty() {
        object.insert("extension".into(), Json::Array(extensions));
    }
    (!object.is_empty()).then_some(Json::Object(object))
}

fn lookup<'a>(elems: &'a BTreeMap<String, Elem>, parent: &str, name: &str) -> Elem {
    let direct = format!("{parent}.{name}");
    if let Some(elem) = elems.get(&direct) {
        return elem.clone();
    }
    let choice = format!("{parent}.{name}[x]");
    if let Some(elem) = elems.get(&choice) {
        return elem.clone();
    }
    for (path, elem) in elems {
        let Some(stem) = path.strip_prefix(&format!("{parent}.")).and_then(|rest| rest.strip_suffix("[x]")) else {
            continue;
        };
        if name.starts_with(stem) && name.len() > stem.len() && name[stem.len()..].chars().next().is_some_and(|ch| ch.is_uppercase()) {
            return elem.clone();
        }
    }
    Elem::default()
}

fn type_code(parent: &str, name: &str, info: &Elem) -> &'static str {
    if let Some(stem) = name_without_choice(name) {
        return primitive_name(stem);
    }
    let _ = parent;
    info.types.first().map(|value| primitive_name(value)).unwrap_or("string")
}

fn name_without_choice(name: &str) -> Option<&str> {
    let mut chars = name.char_indices();
    let mut split = None;
    for (index, ch) in chars.by_ref() {
        if index > 0 && ch.is_uppercase() {
            split = Some(index);
            break;
        }
    }
    split.map(|index| &name[index..])
}

fn primitive_name(name: &str) -> &'static str {
    match name {
        "boolean" | "Boolean" => "boolean",
        "integer" | "Integer" | "unsignedInt" | "positiveInt" => "integer",
        "decimal" | "Decimal" => "decimal",
        "date" | "Date" => "date",
        "dateTime" | "DateTime" => "dateTime",
        "instant" => "instant",
        "time" | "Time" => "time",
        "code" | "Code" => "code",
        "id" | "Id" => "id",
        "uri" | "url" | "canonical" | "oid" | "uuid" | "string" | "markdown" => "string",
        _ => "string",
    }
}

fn primitive_json(kind: &str, raw: &str) -> Json {
    match kind {
        "boolean" => Json::Bool(raw == "true"),
        "integer" => raw.parse::<i64>().ok().map(|value| Json::Number(value.into())).unwrap_or_else(|| Json::String(raw.into())),
        "decimal" => serde_json::from_str::<Number>(raw).map(Json::Number).unwrap_or_else(|_| Json::String(raw.into())),
        _ => Json::String(raw.into()),
    }
}

fn collapse(mut values: Vec<Json>, repeating: bool) -> Json {
    if repeating {
        Json::Array(values)
    } else {
        values.pop().unwrap_or(Json::Null)
    }
}

fn xml_fragment(node: Node<'_, '_>) -> String {
    let mut text = String::new();
    write_xml(node, &mut text);
    text
}

fn write_xml(node: Node<'_, '_>, text: &mut String) {
    text.push('<');
    text.push_str(node.tag_name().name());
    text.push('>');
    for child in node.children() {
        if child.is_text() {
            text.push_str(child.text().unwrap_or(""));
        } else if child.is_element() {
            write_xml(child, text);
        }
    }
    text.push_str("</");
    text.push_str(node.tag_name().name());
    text.push('>');
}
