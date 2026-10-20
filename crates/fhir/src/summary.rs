//! `Patient/$summary`: an International Patient Summary (IPS) of one patient, as a document
//! Bundle built from what the server holds.
//!
//! The summary is a Composition whose sections are the IPS ones this server fills: allergies,
//! problems and medications, which the profile requires, and immunizations, results and
//! procedures. A section holds the patient's resources of the kinds it takes, picked by the rules
//! in [`keep`], and a table that says in words what they are. A required section with nothing in
//! it says so (`emptyReason`) rather than being left out, and an optional one is left out.
//!
//! Only the patient's own resources go in a section, found through the Patient compartment. What
//! those resources point at (the practitioner, the organization, a medication) comes with them, but
//! another patient's record never does.

use std::collections::BTreeMap;

use refhir_database::Transaction;
use serde_json::{json, Value};

use crate::catalog::{Catalog, ResourceModel};
use crate::document;
use crate::error::FhirError;
use crate::ops;
use crate::search;

const COMPOSITION_PROFILE: &str =
    "http://hl7.org/fhir/uv/ips/StructureDefinition/Composition-uv-ips";
const BUNDLE_PROFILE: &str = "http://hl7.org/fhir/uv/ips/StructureDefinition/Bundle-uv-ips";
const LOINC: &str = "http://loinc.org";
const EMPTY_REASON: &str = "http://terminology.hl7.org/CodeSystem/list-empty-reason";
const XHTML: &str = "http://www.w3.org/1999/xhtml";

/// What a resource a patient summary points at may be, besides the patient's own resources of the
/// kinds the sections take: things shared between patients, which carry no one's record.
const SHARED: &[&str] = &[
    "Practitioner",
    "PractitionerRole",
    "Organization",
    "Location",
    "Medication",
    "Device",
    "Specimen",
];

/// The most resources of one kind read to fill a patient's sections or cards: the most recently
/// written. A record with more is read to this many, so a request costs what it can hold and not
/// what the record does.
pub(crate) const READ_LIMIT: usize = 2_000;

/// The most resources one section holds: the most recently written of those its rules keep.
const SECTION_LIMIT: usize = 100;

struct Section {
    title: &'static str,
    code: &'static str,
    display: &'static str,
    /// What the section says when the profile requires it and there is nothing to show.
    none: Option<&'static str>,
    types: &'static [&'static str],
    headers: &'static [&'static str],
}

const SECTIONS: &[Section] = &[
    Section {
        title: "Allergies and Intolerances",
        code: "48765-2",
        display: "Allergies and adverse reactions Document",
        none: Some("There is no information available regarding the subject's allergies."),
        types: &["AllergyIntolerance"],
        headers: &["Allergen", "Status", "Criticality", "Reaction"],
    },
    Section {
        title: "Problem List",
        code: "11450-4",
        display: "Problem list - Reported",
        none: Some("There is no information available about the subject's health problems or disabilities."),
        types: &["Condition"],
        headers: &["Problem", "Status", "Onset"],
    },
    Section {
        title: "Medication Summary",
        code: "10160-0",
        display: "History of Medication use Narrative",
        none: Some("There is no information available about the subject's medication use or administration."),
        types: &["MedicationStatement", "MedicationRequest"],
        headers: &["Medication", "Status", "Dosage", "Since"],
    },
    Section {
        title: "Immunizations",
        code: "11369-6",
        display: "History of Immunization note",
        none: None,
        types: &["Immunization"],
        headers: &["Vaccine", "Date", "Status"],
    },
    Section {
        title: "Results",
        code: "30954-2",
        display: "Relevant diagnostic tests/laboratory data note",
        none: None,
        types: &["Observation", "DiagnosticReport"],
        headers: &["Result", "Value", "Date"],
    },
    Section {
        title: "History of Procedures",
        code: "47519-4",
        display: "History of Procedures Document",
        none: None,
        types: &["Procedure"],
        headers: &["Procedure", "Status", "Date"],
    },
];

pub fn summary(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    args: &Value,
) -> Result<Value, FhirError> {
    if model.name != "Patient" {
        return Err(FhirError::Unsupported(
            "the summary operation is only supported on Patient".into(),
        ));
    }
    let mut patient = find_patient(tx, catalog, model, args)?;
    // A link to another patient's record is not part of this patient's summary.
    if let Some(object) = patient.as_object_mut() {
        object.remove("link");
    }
    let patient_id = patient
        .get("id")
        .and_then(|id| id.as_str())
        .ok_or_else(|| FhirError::Invalid("the patient has no id".into()))?
        .to_string();
    let may_read = document::may_read(args);

    // What goes in each section: the patient's resources of the kinds it takes that its rules keep.
    let mut matched: Vec<Vec<Value>> = Vec::new();
    for section in SECTIONS {
        let mut found = Vec::new();
        for type_name in section.types {
            for resource in
                compartment_resources(tx, catalog, &patient_id, type_name, &may_read, READ_LIMIT)?
            {
                if keep(type_name, &resource) {
                    found.push(resource);
                }
            }
        }
        // A section holds the latest of what there is, so a long record is a summary and not an error.
        if found.len() > SECTION_LIMIT {
            found.sort_by(|left, right| written(right).cmp(written(left)));
            found.truncate(SECTION_LIMIT);
        }
        matched.push(found);
    }

    // The patient, those resources, and what they point at, so every reference in the summary
    // resolves inside it.
    let mut start = vec![patient.clone()];
    start.extend(matched.iter().flatten().cloned());
    let own = format!("Patient/{patient_id}");
    let found = document::gather(tx, catalog, start, &may_read, &|resource| {
        shared_or_the_patients(resource, &own)
    })?;
    let index: BTreeMap<String, &Value> = found
        .iter()
        .filter_map(|resource| Some((identity(resource)?, resource)))
        .collect();

    let mut sections = Vec::new();
    for (section, resources) in SECTIONS.iter().zip(&matched) {
        if let Some(built) = build_section(section, resources, &index) {
            sections.push(built);
        }
    }
    let now = document::now();
    let composition = json!({
        "resourceType": "Composition",
        "meta": {"profile": [COMPOSITION_PROFILE]},
        "text": {
            "status": "generated",
            "div": format!("<div xmlns=\"{XHTML}\"><p>International Patient Summary</p></div>")
        },
        "identifier": {
            "system": "urn:ietf:rfc:3986",
            "value": format!("urn:uuid:{}", uuid::Uuid::new_v4())
        },
        "status": "final",
        "type": {"coding": [{
            "system": LOINC,
            "code": "60591-5",
            "display": "Patient summary Document"
        }]},
        "subject": {"reference": own},
        "date": now,
        "author": [author_of(&patient)],
        "title": format!("Patient Summary as of {}", &now[..10]),
        "section": sections
    });
    let mut resources = vec![composition];
    resources.extend(found);
    let mut bundle = document::package(resources);
    bundle["meta"] = json!({"profile": [BUNDLE_PROFILE]});
    Ok(bundle)
}

/// The patient the request names: by id on a patient, or by `identifier` (`system|value`) on the
/// type. The identifier can come as a query parameter or in a Parameters body.
fn find_patient(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    args: &Value,
) -> Result<Value, FhirError> {
    if let Some(id) = args
        .get("id")
        .and_then(|id| id.as_str())
        .filter(|id| !id.is_empty())
    {
        return ops::read(tx, model, id);
    }
    let Some(identifier) = requested_identifier(args) else {
        return Err(FhirError::Invalid(
            "name the patient: an id in the path, or the identifier parameter".into(),
        ));
    };
    let mut query = BTreeMap::new();
    query.insert("identifier".to_string(), identifier);
    let found = search::search(tx, catalog, model, &query)?;
    let mut matches = found
        .get("entry")
        .and_then(|entries| entries.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("resource").cloned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    match matches.len() {
        0 => Err(FhirError::NotFound("no patient has that identifier".into())),
        1 => Ok(matches.remove(0)),
        _ => Err(FhirError::Precondition(
            "more than one patient has that identifier".into(),
        )),
    }
}

fn requested_identifier(args: &Value) -> Option<String> {
    let from = |value: &Value| -> Option<String> {
        match value {
            Value::String(text) if !text.is_empty() => Some(text.clone()),
            Value::Object(object) => {
                let value = object.get("value")?.as_str()?;
                match object.get("system").and_then(|system| system.as_str()) {
                    Some(system) => Some(format!("{system}|{value}")),
                    None => Some(value.to_string()),
                }
            }
            _ => None,
        }
    };
    if let Some(found) = args.get("identifier").and_then(from) {
        return Some(found);
    }
    args.pointer("/resource/parameter")?
        .as_array()?
        .iter()
        .filter(|part| part.get("name").and_then(|name| name.as_str()) == Some("identifier"))
        .find_map(|part| {
            part.get("valueIdentifier")
                .or_else(|| part.get("valueString"))
                .and_then(from)
        })
}

/// When a resource was written, as the text FHIR gives it, which sorts in time order. A resource
/// from before the server kept times has none, and sorts first.
fn written(resource: &Value) -> &str {
    resource
        .pointer("/meta/lastUpdated")
        .and_then(|value| value.as_str())
        .unwrap_or("")
}

/// The patient's resources of one type, through the compartment link the Patient compartment
/// names for it: the most recently written `limit` of them. A type this server does not hold, or
/// that the caller's token may not read, has none.
pub(crate) fn compartment_resources(
    tx: &mut Transaction,
    catalog: &Catalog,
    patient_id: &str,
    type_name: &str,
    may_read: &dyn Fn(&str) -> bool,
    limit: usize,
) -> Result<Vec<Value>, FhirError> {
    if !may_read(type_name) || catalog.deny_response_read(type_name).is_err() {
        return Ok(Vec::new());
    }
    let Ok(model) = catalog.get(type_name) else {
        return Ok(Vec::new());
    };
    let Some(param) = catalog
        .compartment("Patient")
        .and_then(|compartment| compartment.params_for(type_name))
        .and_then(|params| params.first())
    else {
        return Ok(Vec::new());
    };
    let mut query = BTreeMap::new();
    query.insert(param.clone(), format!("Patient/{patient_id}"));
    // Newest first, and no more than `limit`: a sorted search keeps only the page it is asked for.
    // A type whose catalog has no `_lastUpdated` parameter cannot be sorted by it, and a search
    // refuses a sort it cannot apply.
    if model.ready_param("_lastUpdated").is_some() {
        query.insert("_sort".to_string(), "-_lastUpdated".to_string());
    }
    query.insert("_count".to_string(), limit.to_string());
    let found = search::search(tx, catalog, model, &query)?;
    Ok(found
        .get("entry")
        .and_then(|entries| entries.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("resource").cloned())
                .collect()
        })
        .unwrap_or_default())
}

/// What a resource reached through a reference has to be to go in the summary: a shared kind of
/// thing, or an Observation about this patient (a report's results).
fn shared_or_the_patients(resource: &Value, patient: &str) -> bool {
    let type_name = resource
        .get("resourceType")
        .and_then(|name| name.as_str())
        .unwrap_or("");
    if SHARED.contains(&type_name) {
        return true;
    }
    type_name == "Observation"
        && resource
            .pointer("/subject/reference")
            .and_then(|reference| reference.as_str())
            .and_then(document::split_reference)
            .is_some_and(|(kind, id)| format!("{kind}/{id}") == patient)
}

pub(crate) fn identity(resource: &Value) -> Option<String> {
    Some(format!(
        "{}/{}",
        resource.get("resourceType")?.as_str()?,
        resource.get("id")?.as_str()?
    ))
}

/// Whether a section takes a resource. The rules keep what is current and believed: not
/// entered in error, not refuted, not over.
fn keep(type_name: &str, resource: &Value) -> bool {
    let status = resource.get("status").and_then(|status| status.as_str());
    let coded = |key: &str, wanted: &[&str]| {
        codes(resource.get(key))
            .iter()
            .any(|code| wanted.contains(&code.as_str()))
    };
    match type_name {
        "MedicationStatement" => matches!(status, Some("active" | "intended" | "on-hold")),
        "MedicationRequest" => matches!(status, Some("active" | "on-hold")),
        "AllergyIntolerance" => {
            !coded("verificationStatus", &["entered-in-error", "refuted"])
                && !coded("clinicalStatus", &["inactive", "resolved"])
        }
        "Condition" => {
            !coded("verificationStatus", &["entered-in-error", "refuted"])
                && !coded("clinicalStatus", &["inactive", "resolved"])
        }
        "Immunization" => status == Some("completed"),
        "Observation" => {
            matches!(status, Some("final" | "amended" | "corrected"))
                && category_has(resource, "laboratory")
        }
        "DiagnosticReport" => {
            matches!(status, Some("final" | "amended" | "corrected" | "appended"))
                && category_has(resource, "LAB")
        }
        "Procedure" => !matches!(status, Some("entered-in-error" | "not-done")),
        _ => false,
    }
}

pub(crate) fn category_has(resource: &Value, wanted: &str) -> bool {
    let categories = match resource.get("category") {
        Some(Value::Array(items)) => items.iter().collect::<Vec<_>>(),
        Some(single) => vec![single],
        None => Vec::new(),
    };
    categories
        .into_iter()
        .any(|category| codes(Some(category)).iter().any(|code| code == wanted))
}

/// Every code a CodeableConcept carries.
pub(crate) fn codes(concept: Option<&Value>) -> Vec<String> {
    concept
        .and_then(|concept| concept.get("coding"))
        .and_then(|coding| coding.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("code").and_then(|code| code.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// How a CodeableConcept reads to a person: its text, else the display of its first coding with
/// one, else its first code.
fn words(concept: Option<&Value>) -> String {
    let Some(concept) = concept else {
        return String::new();
    };
    if let Some(text) = concept.get("text").and_then(|text| text.as_str()) {
        return text.to_string();
    }
    let codings = concept
        .get("coding")
        .and_then(|coding| coding.as_array())
        .cloned()
        .unwrap_or_default();
    codings
        .iter()
        .find_map(|coding| coding.get("display").and_then(|display| display.as_str()))
        .or_else(|| {
            codings
                .iter()
                .find_map(|coding| coding.get("code").and_then(|code| code.as_str()))
        })
        .unwrap_or("")
        .to_string()
}

fn first_of(resource: &Value, keys: &[&str]) -> String {
    for key in keys {
        match resource.get(*key) {
            Some(Value::String(text)) => return text.clone(),
            Some(period @ Value::Object(_)) => {
                if let Some(start) = period.get("start").and_then(|start| start.as_str()) {
                    return start.to_string();
                }
            }
            _ => {}
        }
    }
    String::new()
}

fn status_words(resource: &Value, key: &str) -> String {
    match resource.get(key) {
        Some(Value::String(text)) => text.clone(),
        other => codes(other).into_iter().next().unwrap_or_default(),
    }
}

/// The medication a statement or request names, in words: the concept it holds, or the code of the
/// Medication it points at.
fn medication_words(resource: &Value, index: &BTreeMap<String, &Value>) -> String {
    if let Some(concept) = resource.get("medicationCodeableConcept") {
        return words(Some(concept));
    }
    let reference = resource.get("medicationReference");
    if let Some(medication) = reference
        .and_then(|reference| reference.get("reference"))
        .and_then(|reference| reference.as_str())
        .and_then(document::split_reference)
        .and_then(|(kind, id)| index.get(&format!("{kind}/{id}")))
    {
        let found = words(medication.get("code"));
        if !found.is_empty() {
            return found;
        }
    }
    reference
        .and_then(|reference| reference.get("display"))
        .and_then(|display| display.as_str())
        .unwrap_or("")
        .to_string()
}

fn value_words(observation: &Value) -> String {
    if let Some(quantity) = observation.get("valueQuantity") {
        let number = quantity.get("value").map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        });
        let unit = quantity
            .get("unit")
            .or_else(|| quantity.get("code"))
            .and_then(|unit| unit.as_str());
        return match (number, unit) {
            (Some(number), Some(unit)) => format!("{number} {unit}"),
            (Some(number), None) => number,
            _ => String::new(),
        };
    }
    if let Some(text) = observation
        .get("valueString")
        .and_then(|text| text.as_str())
    {
        return text.to_string();
    }
    if let Some(concept) = observation.get("valueCodeableConcept") {
        return words(Some(concept));
    }
    String::new()
}

/// One row of a section's table, in the order of the section's headers.
fn row(type_name: &str, resource: &Value, index: &BTreeMap<String, &Value>) -> Vec<String> {
    let first_text = |pointer: &str| {
        resource
            .pointer(pointer)
            .and_then(|text| text.as_str())
            .unwrap_or("")
            .to_string()
    };
    match type_name {
        "MedicationStatement" => vec![
            medication_words(resource, index),
            status_words(resource, "status"),
            first_text("/dosage/0/text"),
            first_of(
                resource,
                &["effectiveDateTime", "effectivePeriod", "dateAsserted"],
            ),
        ],
        "MedicationRequest" => vec![
            medication_words(resource, index),
            status_words(resource, "status"),
            first_text("/dosageInstruction/0/text"),
            first_of(resource, &["authoredOn"]),
        ],
        "AllergyIntolerance" => vec![
            words(resource.get("code")),
            status_words(resource, "clinicalStatus"),
            status_words(resource, "criticality"),
            words(resource.pointer("/reaction/0/manifestation/0")),
        ],
        "Condition" => vec![
            words(resource.get("code")),
            status_words(resource, "clinicalStatus"),
            first_of(resource, &["onsetDateTime", "onsetPeriod", "onsetString"]),
        ],
        "Immunization" => vec![
            words(resource.get("vaccineCode")),
            first_of(resource, &["occurrenceDateTime", "occurrenceString"]),
            status_words(resource, "status"),
        ],
        "Observation" => vec![
            words(resource.get("code")),
            value_words(resource),
            first_of(
                resource,
                &["effectiveDateTime", "effectivePeriod", "issued"],
            ),
        ],
        "DiagnosticReport" => vec![
            words(resource.get("code")),
            first_text("/conclusion"),
            first_of(
                resource,
                &["effectiveDateTime", "effectivePeriod", "issued"],
            ),
        ],
        "Procedure" => vec![
            words(resource.get("code")),
            status_words(resource, "status"),
            first_of(
                resource,
                &["performedDateTime", "performedPeriod", "performedString"],
            ),
        ],
        _ => Vec::new(),
    }
}

/// One section of the Composition, or `None` for an optional section with nothing in it.
fn build_section(
    section: &Section,
    resources: &[Value],
    index: &BTreeMap<String, &Value>,
) -> Option<Value> {
    let code = json!({"coding": [{
        "system": LOINC,
        "code": section.code,
        "display": section.display
    }]});
    if resources.is_empty() {
        let none = section.none?;
        return Some(json!({
            "title": section.title,
            "code": code,
            "text": {
                "status": "generated",
                "div": format!("<div xmlns=\"{XHTML}\">{}</div>", escape(none))
            },
            "emptyReason": {
                "coding": [{"system": EMPTY_REASON, "code": "unavailable", "display": "Unavailable"}],
                "text": "No information available"
            }
        }));
    }
    let rows: Vec<Vec<String>> = resources
        .iter()
        .map(|resource| {
            let type_name = resource
                .get("resourceType")
                .and_then(|name| name.as_str())
                .unwrap_or("");
            row(type_name, resource, index)
        })
        .collect();
    let entries: Vec<Value> = resources
        .iter()
        .filter_map(|resource| identity(resource).map(|reference| json!({"reference": reference})))
        .collect();
    Some(json!({
        "title": section.title,
        "code": code,
        "text": {"status": "generated", "div": table(section.headers, &rows)},
        "entry": entries
    }))
}

/// The summary's author: the organization that looks after the patient, where there is one, and
/// otherwise this server by name.
fn author_of(patient: &Value) -> Value {
    match patient
        .pointer("/managingOrganization/reference")
        .and_then(|reference| reference.as_str())
    {
        Some(reference) => json!({"reference": reference}),
        None => json!({"display": "ReFHIR"}),
    }
}

/// An XHTML table, with every cell escaped.
fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut out = format!("<div xmlns=\"{XHTML}\"><table><thead><tr>");
    for header in headers {
        out.push_str(&format!("<th>{}</th>", escape(header)));
    }
    out.push_str("</tr></thead><tbody>");
    for row in rows {
        out.push_str("<tr>");
        for cell in row {
            out.push_str(&format!("<td>{}</td>", escape(cell)));
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table></div>");
    out
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_narrative_cell_cannot_carry_markup() {
        let rows = vec![vec!["<script>x</script> & \"y\"".to_string()]];
        let text = table(&["A"], &rows);
        assert!(!text.contains("<script>"), "{text}");
        assert!(text.contains("&lt;script&gt;x&lt;/script&gt; &amp; &quot;y&quot;"));
        assert!(refhir_fhirpath::html_checks(&text));
    }

    #[test]
    fn a_concept_reads_as_its_text_then_a_display_then_a_code() {
        assert_eq!(
            words(Some(&json!({"text": "Asthma", "coding": [{"code": "x"}]}))),
            "Asthma"
        );
        assert_eq!(
            words(Some(
                &json!({"coding": [{"code": "x"}, {"code": "y", "display": "Why"}]})
            )),
            "Why"
        );
        assert_eq!(words(Some(&json!({"coding": [{"code": "x"}]}))), "x");
        assert_eq!(words(None), "");
    }

    #[test]
    fn what_is_current_and_believed_is_kept() {
        let active = json!({"clinicalStatus": {"coding": [{"code": "active"}]}});
        let resolved = json!({"clinicalStatus": {"coding": [{"code": "resolved"}]}});
        let refuted = json!({"verificationStatus": {"coding": [{"code": "refuted"}]}});
        assert!(keep("Condition", &active));
        assert!(keep("Condition", &json!({})));
        assert!(!keep("Condition", &resolved));
        assert!(!keep("Condition", &refuted));
        assert!(keep("MedicationStatement", &json!({"status": "active"})));
        assert!(!keep("MedicationStatement", &json!({"status": "stopped"})));
        assert!(!keep("MedicationRequest", &json!({"status": "completed"})));
        assert!(keep("Immunization", &json!({"status": "completed"})));
        assert!(!keep("Immunization", &json!({"status": "not-done"})));
        let lab = json!({"status": "final", "category": [{"coding": [{"code": "laboratory"}]}]});
        let vitals =
            json!({"status": "final", "category": [{"coding": [{"code": "vital-signs"}]}]});
        assert!(keep("Observation", &lab));
        assert!(!keep("Observation", &vitals));
        assert!(!keep("Procedure", &json!({"status": "entered-in-error"})));
    }

    #[test]
    fn an_identifier_comes_as_a_parameter_a_string_or_an_identifier() {
        assert_eq!(
            requested_identifier(&json!({"identifier": "http://h|mrn-1"})).as_deref(),
            Some("http://h|mrn-1")
        );
        assert_eq!(
            requested_identifier(&json!({"resource": {"resourceType": "Parameters", "parameter": [
                {"name": "identifier", "valueIdentifier": {"system": "http://h", "value": "mrn-1"}}
            ]}}))
            .as_deref(),
            Some("http://h|mrn-1")
        );
        assert_eq!(
            requested_identifier(
                &json!({"resource": {"resourceType": "Parameters", "parameter": [
                    {"name": "identifier", "valueString": "mrn-2"}
                ]}})
            )
            .as_deref(),
            Some("mrn-2")
        );
        assert_eq!(requested_identifier(&json!({})), None);
    }
}
