//! CQL for SDC: compile a questionnaire subset to ELM JSON, then evaluate it.
//!
//! `retrieve` goes through [`DataSource`], which the caller binds to the open
//! transaction and the caller's scopes. Inline `text/cql-expression` compiles
//! here. `text/cql-identifier` evaluates a named statement in a Library's ELM.

mod compile;
mod elm;

use std::collections::BTreeMap;

use serde_json::Value as Json;

pub use compile::compile;
pub use elm::evaluate;

/// FHIR data the evaluator may read. Callers enforce scopes before returning rows.
pub trait DataSource {
    fn retrieve(&mut self, type_name: &str, code: Option<&str>) -> Result<Vec<Json>, String>;
}

/// Compile `source` when it is CQL text, or accept ELM JSON, then evaluate `name`.
/// An empty name runs the inline statement `expr` or the first statement.
pub fn run(
    source: &str,
    name: &str,
    context: &Json,
    params: &BTreeMap<String, Json>,
    data: &mut dyn DataSource,
) -> Result<Vec<Json>, String> {
    let trimmed = source.trim();
    let elm = if trimmed.starts_with('{') {
        serde_json::from_str(trimmed).map_err(|err| err.to_string())?
    } else {
        compile(trimmed)?
    };
    let name = if name.is_empty() {
        default_name(&elm)
    } else {
        name.to_string()
    };
    evaluate(&elm, &name, context, params, data)
}

fn default_name(elm: &Json) -> String {
    elm.pointer("/library/statements/def")
        .and_then(|value| value.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.get("name"))
        .and_then(|value| value.as_str())
        .unwrap_or("expr")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Rows(Vec<Json>);

    impl DataSource for Rows {
        fn retrieve(&mut self, type_name: &str, code: Option<&str>) -> Result<Vec<Json>, String> {
            Ok(self
                .0
                .iter()
                .filter(|row| row["resourceType"] == type_name)
                .filter(|row| match code {
                    None => true,
                    Some(code) => codes(row).iter().any(|item| item == code),
                })
                .cloned()
                .collect())
        }
    }

    fn codes(row: &Json) -> Vec<String> {
        let mut found = Vec::new();
        if let Some(items) = row
            .pointer("/code/coding")
            .and_then(|value| value.as_array())
        {
            for item in items {
                if let Some(code) = item.get("code").and_then(|value| value.as_str()) {
                    found.push(code.to_string());
                }
            }
        }
        found
    }

    #[test]
    fn compiles_arithmetic_and_logic() {
        let mut data = Rows(vec![]);
        let got = run("1 + 2", "", &json!({}), &BTreeMap::new(), &mut data).unwrap();
        assert_eq!(got, vec![json!(3)]);
        let got = run(
            "true and not false",
            "",
            &json!({}),
            &BTreeMap::new(),
            &mut data,
        )
        .unwrap();
        assert_eq!(got, vec![json!(true)]);
    }

    #[test]
    fn reads_context_and_retrieves_by_code() {
        let patient = json!({"resourceType": "Patient", "birthDate": "1980-01-02"});
        let mut data = Rows(vec![
            json!({"resourceType": "Observation", "code": {"coding": [{"code": "29463-7"}]}, "valueQuantity": {"value": 70}}),
            json!({"resourceType": "Observation", "code": {"coding": [{"code": "other"}]}, "valueQuantity": {"value": 1}}),
        ]);
        let birth = run("birthDate", "", &patient, &BTreeMap::new(), &mut data).unwrap();
        assert_eq!(birth, vec![json!("1980-01-02")]);
        let weights = run(
            "[Observation: \"29463-7\"].valueQuantity.value",
            "",
            &patient,
            &BTreeMap::new(),
            &mut data,
        )
        .unwrap();
        assert_eq!(weights, vec![json!(70)]);
        let missing = run("notAField", "", &patient, &BTreeMap::new(), &mut data).unwrap();
        assert!(missing.is_empty());
    }

    #[test]
    fn named_define_in_a_library() {
        let source = r#"
            library Demo version '1.0'
            context Patient
            define "Birth": birthDate
        "#;
        let mut data = Rows(vec![]);
        let got = run(
            source,
            "Birth",
            &json!({"resourceType": "Patient", "birthDate": "2001-02-03"}),
            &BTreeMap::new(),
            &mut data,
        )
        .unwrap();
        assert_eq!(got, vec![json!("2001-02-03")]);
    }
}
