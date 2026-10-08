//! Read and search surface used by population, extraction, and assembly.
//!
//! The FHIR server implements this over the open `Transaction`. Tests use
//! [`MemoryChart`]. An empty chart backs the WebAssembly behavior export.

use refhir_fhirpath::Model;
use serde_json::Value as Json;

pub trait Chart {
    fn read_type(&mut self, type_name: &str, id: &str) -> Result<Option<Json>, String>;
    fn search_type(
        &mut self,
        type_name: &str,
        params: &[(&str, String)],
    ) -> Result<Vec<Json>, String>;
    fn allows(&self, type_name: &str) -> bool;
    fn path_model(&self) -> Model;
    fn in_valueset(&mut self, system: Option<&str>, code: &str, value_set: &str) -> Option<bool> {
        let _ = (system, code, value_set);
        None
    }

    /// ConceptMap translation for a StructureMap `translate` transform.
    fn translate(&mut self, map_url: &str, system: Option<&str>, code: &str) -> Option<Json> {
        let _ = (map_url, system, code);
        None
    }
}

pub struct NullChart;

impl Chart for NullChart {
    fn read_type(&mut self, _type_name: &str, _id: &str) -> Result<Option<Json>, String> {
        Ok(None)
    }

    fn search_type(
        &mut self,
        _type_name: &str,
        _params: &[(&str, String)],
    ) -> Result<Vec<Json>, String> {
        Ok(Vec::new())
    }

    fn allows(&self, _type_name: &str) -> bool {
        true
    }

    fn path_model(&self) -> Model {
        Model::new()
    }
}

pub struct MemoryChart {
    pub rows: Vec<Json>,
    pub scopes: Option<Vec<String>>,
    model: Model,
    translations: Vec<(String, Option<String>, String, Json)>,
}

impl MemoryChart {
    pub fn new(rows: Vec<Json>) -> Self {
        Self {
            rows,
            scopes: None,
            model: Model::new(),
            translations: Vec::new(),
        }
    }

    pub fn translate_as(
        mut self,
        map_url: impl Into<String>,
        code: impl Into<String>,
        value: Json,
    ) -> Self {
        self.translations
            .push((map_url.into(), None, code.into(), value));
        self
    }
}

impl Chart for MemoryChart {
    fn read_type(&mut self, type_name: &str, id: &str) -> Result<Option<Json>, String> {
        if !self.allows(type_name) {
            return Ok(None);
        }
        Ok(self
            .rows
            .iter()
            .find(|row| {
                row.get("resourceType").and_then(|value| value.as_str()) == Some(type_name)
                    && row.get("id").and_then(|value| value.as_str()) == Some(id)
            })
            .cloned())
    }

    fn search_type(
        &mut self,
        type_name: &str,
        params: &[(&str, String)],
    ) -> Result<Vec<Json>, String> {
        if !self.allows(type_name) {
            return Ok(Vec::new());
        }
        Ok(self
            .rows
            .iter()
            .filter(|row| {
                row.get("resourceType").and_then(|value| value.as_str()) == Some(type_name)
            })
            .filter(|row| {
                params
                    .iter()
                    .all(|(name, value)| row_matches(row, name, value))
            })
            .cloned()
            .collect())
    }

    fn allows(&self, type_name: &str) -> bool {
        match &self.scopes {
            None => true,
            Some(scopes) => scope_allows(scopes, type_name),
        }
    }

    fn path_model(&self) -> Model {
        self.model.clone()
    }

    fn translate(&mut self, map_url: &str, system: Option<&str>, code: &str) -> Option<Json> {
        self.translations
            .iter()
            .find(|(map, stored_system, stored, _)| {
                map == map_url
                    && stored == code
                    && stored_system
                        .as_deref()
                        .is_none_or(|item| system == Some(item))
            })
            .map(|(_, _, _, value)| value.clone())
    }
}

pub fn scope_allows(scopes: &[String], type_name: &str) -> bool {
    scopes
        .iter()
        .any(|scope| scope_allows_type(scope, type_name))
}

fn scope_allows_type(scope: &str, type_name: &str) -> bool {
    let scope = scope.split_once('?').map(|(head, _)| head).unwrap_or(scope);
    let mut parts = scope.split('/');
    let context = parts.next().unwrap_or("");
    if !matches!(context, "patient" | "user" | "system") {
        return false;
    }
    let Some(rest) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    let (name, permission) = rest.split_once('.').unwrap_or((rest, "*"));
    if name != "*" && name != type_name {
        return false;
    }
    let letters = v2_letters(permission);
    letters.contains('r') || letters.contains('s')
}

fn v2_letters(permission: &str) -> String {
    match permission {
        "read" => "rs".into(),
        "write" => "cud".into(),
        "*" => "cruds".into(),
        letters if is_cruds(letters) => letters.to_string(),
        _ => String::new(),
    }
}

fn is_cruds(letters: &str) -> bool {
    if letters.is_empty() {
        return false;
    }
    let mut last = 0usize;
    for ch in letters.chars() {
        let Some(index) = "cruds".find(ch) else {
            return false;
        };
        if index < last {
            return false;
        }
        last = index + 1;
    }
    true
}

fn row_matches(row: &Json, name: &str, value: &str) -> bool {
    let (name, _) = name.split_once(':').unwrap_or((name, ""));
    match name {
        "_id" => row.get("id").and_then(|item| item.as_str()) == Some(value),
        "url" => {
            let (url, version) = value.split_once('|').unwrap_or((value, ""));
            row.get("url").and_then(|item| item.as_str()) == Some(url)
                && (version.is_empty()
                    || row.get("version").and_then(|item| item.as_str()) == Some(version))
        }
        "status" | "title" | "gender" => {
            row.get(name).and_then(|item| item.as_str()) == Some(value)
        }
        "subject" | "patient" | "questionnaire" => {
            let reference = row
                .get(if name == "patient" { "subject" } else { name })
                .and_then(|item| item.get("reference"))
                .and_then(|item| item.as_str());
            let canonical = row.get(name).and_then(|item| item.as_str());
            reference == Some(value) || canonical == Some(value)
        }
        "code" => code_matches(row, value),
        "identifier" => identifier_matches(row, value),
        _ => false,
    }
}

fn code_matches(row: &Json, value: &str) -> bool {
    let wanted = value.rsplit('|').next().unwrap_or(value);
    row.pointer("/code/coding")
        .and_then(|item| item.as_array())
        .map(|items| {
            items
                .iter()
                .any(|coding| coding.get("code").and_then(|item| item.as_str()) == Some(wanted))
        })
        .unwrap_or(false)
        || row
            .get("item")
            .and_then(|item| item.as_array())
            .map(|items| items.iter().any(|item| code_matches(item, value)))
            .unwrap_or(false)
}

fn identifier_matches(row: &Json, value: &str) -> bool {
    let (system, id) = value.split_once('|').unwrap_or(("", value));
    row.get("identifier")
        .and_then(|item| item.as_array())
        .map(|items| {
            items.iter().any(|identifier| {
                identifier.get("value").and_then(|item| item.as_str()) == Some(id)
                    && (system.is_empty()
                        || identifier.get("system").and_then(|item| item.as_str()) == Some(system))
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_read_search_letters_count_as_read_access() {
        assert!(scope_allows(
            &["patient/Observation.rs".into()],
            "Observation"
        ));
        assert!(scope_allows(
            &["patient/Observation.rs?category=vital-signs".into()],
            "Observation"
        ));
        assert!(scope_allows(&["user/*.cruds".into()], "Observation"));
        assert!(scope_allows(
            &["user/Observation.read".into()],
            "Observation"
        ));
        assert!(!scope_allows(&["user/Patient.read".into()], "Observation"));
        assert!(!scope_allows(
            &["user/Observation.write".into()],
            "Observation"
        ));
        assert!(!scope_allows(
            &["user/Observation.cud".into()],
            "Observation"
        ));
        assert!(!scope_allows(&["launch/patient".into()], "Observation"));
    }
}
