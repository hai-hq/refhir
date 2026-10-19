//! `Patient/$health-cards-issue`, the half that needs the record: what a SMART Health Card holds,
//! and holding it in as little space as the card format asks.
//!
//! A card is signed by the issuer and carried in a QR code, so it holds a small FHIR Bundle: the
//! patient's identity, and the resources asked for. Signing needs the issuer's key, which the
//! server has and this crate does not, so this returns each credential unsigned (its Bundle, the
//! types it carries, and where each resource in it came from) and the server signs it.
//!
//! Nothing here goes into a card unless the caller asked for it: the kinds of resource named by
//! `credentialType`, the identity claims named by `includeIdentityClaim`, and, if `_since` is
//! given, only what was written at or after it.

use std::collections::{BTreeSet, HashMap};

use refhir_database::Transaction;
use serde_json::{json, Map, Value};

use crate::catalog::{Catalog, ResourceModel};
use crate::document;
use crate::error::FhirError;
use crate::ops;
use crate::summary;

/// The legacy credential types some wallets still ask for. The operation's own definition says a
/// server should read `#immunization` as `Immunization` and `#laboratory` as `Observation`.
const IMMUNIZATION: &str = "https://smarthealth.cards#immunization";
const LABORATORY: &str = "https://smarthealth.cards#laboratory";

/// The Patient elements a card may carry to say who it is about.
const IDENTITY_CLAIMS: &[&str] = &[
    "name",
    "birthDate",
    "gender",
    "identifier",
    "telecom",
    "address",
];

/// What a card says of the patient when the request names no claim: enough to tell a person.
const DEFAULT_CLAIMS: &[&str] = &["name", "birthDate"];

/// Most resources one card may hold. A card is read off a screen or a printout.
const MAX_RESOURCES: usize = 500;

/// One kind of resource a credential is asked to hold.
#[derive(PartialEq, Eq, Hash, Clone)]
struct Want {
    resource_type: String,
    /// The `#laboratory` type: only the Observations that are laboratory results.
    laboratory_only: bool,
}

pub fn issue(
    tx: &mut Transaction,
    catalog: &Catalog,
    model: &ResourceModel,
    args: &Value,
) -> Result<Value, FhirError> {
    if model.name != "Patient" {
        return Err(FhirError::Unsupported(
            "the health-cards-issue operation is only supported on Patient".into(),
        ));
    }
    let id = args
        .get("id")
        .and_then(|id| id.as_str())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| FhirError::Invalid("the patient's id is required".into()))?;
    let since = since_of(args)?;
    let wanted = credential_types(catalog, args)?;
    let claims = identity_claims(args)?;
    let patient = ops::read(tx, model, id)?;

    // Every kind asked for has to be there: the types are "and", a card of what is left would say
    // less than was asked.
    let may_read = document::may_read(args);
    let mut found: Vec<Value> = Vec::new();
    let mut seen = BTreeSet::new();
    for want in &wanted {
        let mut matching = Vec::new();
        for resource in
            summary::compartment_resources(tx, catalog, id, &want.resource_type, &may_read)?
        {
            if believed(&resource)
                && is_new(&resource, since)
                && (!want.laboratory_only || summary::category_has(&resource, "laboratory"))
            {
                matching.push(resource);
            }
        }
        if matching.is_empty() {
            return Ok(json!({"credentials": []}));
        }
        for resource in matching {
            if let Some(key) = summary::identity(&resource) {
                if seen.insert(key) {
                    found.push(resource);
                }
            }
        }
    }
    if found.len() > MAX_RESOURCES {
        return Err(FhirError::Unprocessable(json!({
            "resourceType": "OperationOutcome",
            "issue": [{
                "severity": "error",
                "code": "too-costly",
                "diagnostics": format!("a health card would hold more than {MAX_RESOURCES} resources")
            }]
        })));
    }

    let mut types = BTreeSet::new();
    for want in &wanted {
        if want.laboratory_only {
            types.insert(LABORATORY);
        } else if want.resource_type == "Immunization" {
            types.insert(IMMUNIZATION);
        }
    }
    let (bundle, links) = minimized(id, identity_of(&patient, &claims), found);
    Ok(json!({
        "fhirVersion": catalog.fhir_version(),
        "credentials": [{
            "types": types.into_iter().collect::<Vec<_>>(),
            "bundle": bundle,
            "links": links
        }]
    }))
}

/// The values of a Parameters parameter, whatever type they were given as.
fn parameter_values(args: &Value, name: &str) -> Vec<String> {
    let Some(parameters) = args
        .pointer("/resource/parameter")
        .and_then(|parameters| parameters.as_array())
    else {
        return Vec::new();
    };
    parameters
        .iter()
        .filter(|part| part.get("name").and_then(|part| part.as_str()) == Some(name))
        .filter_map(|part| {
            part.as_object()?
                .iter()
                .find(|(key, _)| key.starts_with("value"))
                .and_then(|(_, value)| value.as_str())
                .map(str::to_string)
        })
        .collect()
}

/// The moment `_since` names, if the request has one: only what was written at or after it goes in
/// the card.
fn since_of(args: &Value) -> Result<Option<i64>, FhirError> {
    match parameter_values(args, "_since").as_slice() {
        [] => Ok(None),
        [text] => crate::instant::parse(text).map(Some).ok_or_else(|| {
            FhirError::Invalid(format!("_since is not a date or a date and time: {text}"))
        }),
        _ => Err(FhirError::Invalid("_since is given once".into())),
    }
}

/// Whether a resource was written at or after the moment (or there is none). A resource with no
/// time of its own, one from before the server kept times, is not: a card for what is new holds
/// only what is known to be.
fn is_new(resource: &Value, since: Option<i64>) -> bool {
    let Some(since) = since else {
        return true;
    };
    resource
        .pointer("/meta/lastUpdated")
        .and_then(|value| value.as_str())
        .and_then(crate::instant::parse)
        .is_some_and(|at| at >= since)
}

/// The kinds of resource the request names. A type is a FHIR resource type of the patient's
/// compartment, or one of the two legacy URIs.
fn credential_types(catalog: &Catalog, args: &Value) -> Result<Vec<Want>, FhirError> {
    let mut wanted: Vec<Want> = Vec::new();
    for name in parameter_values(args, "credentialType") {
        let want = match name.as_str() {
            IMMUNIZATION => Want {
                resource_type: "Immunization".into(),
                laboratory_only: false,
            },
            LABORATORY => Want {
                resource_type: "Observation".into(),
                laboratory_only: true,
            },
            other => {
                let held = catalog
                    .compartment("Patient")
                    .and_then(|compartment| compartment.params_for(other))
                    .is_some()
                    && catalog.get(other).is_ok();
                if !held {
                    return Err(FhirError::Invalid(format!(
                        "credentialType {other} is not supported: name a resource type of a \
                         patient's record, such as Immunization or Observation"
                    )));
                }
                Want {
                    resource_type: other.to_string(),
                    laboratory_only: false,
                }
            }
        };
        if !wanted.contains(&want) {
            wanted.push(want);
        }
    }
    if wanted.is_empty() {
        return Err(FhirError::Invalid(
            "credentialType is required: the kind of resource the card is to hold".into(),
        ));
    }
    Ok(wanted)
}

/// The Patient elements the card says of the patient: those the request names, or a name and a
/// birth date.
fn identity_claims(args: &Value) -> Result<Vec<String>, FhirError> {
    let named = parameter_values(args, "includeIdentityClaim");
    if named.is_empty() {
        return Ok(DEFAULT_CLAIMS
            .iter()
            .map(|claim| claim.to_string())
            .collect());
    }
    let mut claims = Vec::new();
    for claim in named {
        let element = claim.strip_prefix("Patient.").unwrap_or(&claim);
        if !IDENTITY_CLAIMS.contains(&element) {
            return Err(FhirError::Invalid(format!(
                "includeIdentityClaim {claim} is not supported: use Patient.{}",
                IDENTITY_CLAIMS.join(", Patient.")
            )));
        }
        if !claims.iter().any(|known| known == element) {
            claims.push(element.to_string());
        }
    }
    Ok(claims)
}

/// A Patient that holds only the identity claims.
fn identity_of(patient: &Value, claims: &[String]) -> Value {
    let mut out = Map::new();
    out.insert("resourceType".into(), json!("Patient"));
    for claim in claims {
        let Some(value) = patient.get(claim) else {
            continue;
        };
        let value = if claim == "name" {
            names_only(value)
        } else {
            value.clone()
        };
        out.insert(claim.clone(), value);
    }
    Value::Object(out)
}

/// The family and given names of a patient's names: what tells people apart, and no more.
fn names_only(names: &Value) -> Value {
    let Some(names) = names.as_array() else {
        return names.clone();
    };
    Value::Array(
        names
            .iter()
            .map(|name| {
                let mut kept = Map::new();
                for key in ["family", "given"] {
                    if let Some(value) = name.get(key) {
                        kept.insert(key.to_string(), value.clone());
                    }
                }
                Value::Object(kept)
            })
            .collect(),
    )
}

/// Whether a record is one to stand behind: not entered in error, not refuted.
fn believed(resource: &Value) -> bool {
    if resource.get("status").and_then(|status| status.as_str()) == Some("entered-in-error") {
        return false;
    }
    !summary::codes(resource.get("verificationStatus"))
        .iter()
        .any(|code| matches!(code.as_str(), "entered-in-error" | "refuted"))
}

/// The card's Bundle, and where each resource in it came from. The patient is `resource:0`, and
/// every resource is as small as the card format asks: no `id`, no `meta` but `meta.security`, no
/// narrative, no `CodeableConcept.text`, no `Coding.display`, and references that point at
/// `resource:N`. A reference to something the card does not hold keeps what it says in words, if
/// anything, and otherwise goes.
fn minimized(patient_id: &str, patient: Value, resources: Vec<Value>) -> (Value, Vec<Value>) {
    let mut keys = vec![format!("Patient/{patient_id}")];
    let mut entries = vec![patient];
    for resource in resources {
        keys.push(summary::identity(&resource).unwrap_or_default());
        entries.push(resource);
    }
    let index: HashMap<String, usize> = keys
        .iter()
        .enumerate()
        .map(|(position, key)| (key.clone(), position))
        .collect();
    for (position, entry) in entries.iter_mut().enumerate() {
        strip(entry, true);
        if position > 0 {
            retarget(entry, &index);
        }
    }
    let links: Vec<Value> = keys
        .iter()
        .enumerate()
        .map(|(position, key)| json!({"bundledResource": format!("resource:{position}"), "hostedResource": key}))
        .collect();
    let bundle = json!({
        "resourceType": "Bundle",
        "type": "collection",
        "entry": entries
            .into_iter()
            .enumerate()
            .map(|(position, resource)| json!({"fullUrl": format!("resource:{position}"), "resource": resource}))
            .collect::<Vec<_>>()
    });
    (bundle, links)
}

fn strip(value: &mut Value, root: bool) {
    match value {
        Value::Object(map) => {
            if root {
                map.remove("id");
                map.remove("text");
                if let Some(meta) = map.remove("meta") {
                    if let Some(security) = meta.get("security") {
                        map.insert("meta".into(), json!({"security": security}));
                    }
                }
            }
            // A concept's words go where a coding says the same, and stay where one does not:
            // without them such a concept would say nothing at all.
            if map
                .get("coding")
                .and_then(|coding| coding.as_array())
                .is_some_and(|codings| !codings.is_empty())
            {
                map.remove("text");
            }
            if let Some(Value::Array(codings)) = map.get_mut("coding") {
                for coding in codings {
                    if let Some(coding) = coding.as_object_mut() {
                        coding.remove("display");
                    }
                }
            }
            for (key, child) in map.iter_mut() {
                if key == "contained" {
                    // A contained resource is named by its id, which its references use, so the
                    // id stays; what the format asks to leave out still goes.
                    if let Value::Array(items) = child {
                        for item in items {
                            if let Some(item) = item.as_object_mut() {
                                item.remove("text");
                                item.remove("meta");
                            }
                            strip(item, false);
                        }
                    }
                } else {
                    strip(child, false);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                strip(item, false);
            }
        }
        _ => {}
    }
}

/// Point the references in `value` at the resources of the card. Returns whether `value` is a
/// reference with nothing left in it, for its parent to drop.
fn retarget(value: &mut Value, index: &HashMap<String, usize>) -> bool {
    match value {
        Value::Object(map) => {
            if let Some(reference) = map
                .get("reference")
                .and_then(|reference| reference.as_str())
                .map(str::to_string)
            {
                if reference.starts_with('#') {
                    return false;
                }
                let held = document::split_reference(&reference)
                    .and_then(|(kind, id)| index.get(&format!("{kind}/{id}")).copied());
                match held {
                    Some(position) => {
                        map.insert("reference".into(), json!(format!("resource:{position}")));
                    }
                    None => {
                        map.remove("reference");
                        return !map.contains_key("display") && !map.contains_key("identifier");
                    }
                }
                return false;
            }
            let dropped: Vec<String> = map
                .iter_mut()
                .filter_map(|(key, child)| retarget(child, index).then(|| key.clone()))
                .collect();
            let removed = !dropped.is_empty();
            for key in dropped {
                map.remove(&key);
            }
            // Whatever held only the reference that went has nothing left either.
            removed && map.is_empty()
        }
        Value::Array(items) => {
            items.retain_mut(|item| !retarget(item, index));
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_of(keys: &[&str]) -> HashMap<String, usize> {
        keys.iter()
            .enumerate()
            .map(|(position, key)| (key.to_string(), position))
            .collect()
    }

    #[test]
    fn a_resource_is_cut_to_what_the_card_format_asks() {
        let mut resource = json!({
            "resourceType": "Immunization",
            "id": "i1",
            "meta": {"versionId": "7", "security": [{"code": "R"}]},
            "text": {"status": "generated", "div": "<div/>"},
            "status": "completed",
            "vaccineCode": {"text": "Flu", "coding": [{"system": "http://hl7.org/fhir/sid/cvx", "code": "140", "display": "Influenza"}]},
            "note": [{"text": "kept: this is a note"}],
            "occurrenceDateTime": "2023-10-01"
        });
        strip(&mut resource, true);
        assert!(resource.get("id").is_none() && resource.get("text").is_none());
        assert_eq!(resource["meta"], json!({"security": [{"code": "R"}]}));
        assert!(resource["vaccineCode"].get("text").is_none());
        assert!(resource["vaccineCode"]["coding"][0]
            .get("display")
            .is_none());
        assert_eq!(resource["vaccineCode"]["coding"][0]["code"], "140");
        assert_eq!(resource["note"][0]["text"], "kept: this is a note");
        assert_eq!(resource["status"], "completed");

        let mut words_only = json!({"vaccineCode": {"text": "Flu"}});
        strip(&mut words_only, false);
        assert_eq!(words_only["vaccineCode"]["text"], "Flu");

        let mut without_security = json!({"resourceType": "Patient", "meta": {"versionId": "1"}});
        strip(&mut without_security, true);
        assert!(without_security.get("meta").is_none());
    }

    #[test]
    fn a_reference_points_into_the_card_or_keeps_its_words_or_goes() {
        let index = index_of(&["Patient/p1", "Practitioner/x"]);
        let mut resource = json!({
            "patient": {"reference": "Patient/p1"},
            "performer": [
                {"actor": {"reference": "Practitioner/x", "display": "Dr X"}},
                {"actor": {"reference": "Practitioner/elsewhere", "display": "Dr Elsewhere"}},
                {"actor": {"reference": "Practitioner/gone"}}
            ],
            "location": {"reference": "Location/l1"},
            "contained": [{"id": "m", "resourceType": "Medication"}],
            "medication": {"reference": "#m"}
        });
        retarget(&mut resource, &index);
        assert_eq!(resource["patient"]["reference"], "resource:0");
        assert_eq!(resource["performer"][0]["actor"]["reference"], "resource:1");
        assert_eq!(
            resource["performer"][1]["actor"],
            json!({"display": "Dr Elsewhere"})
        );
        assert_eq!(
            resource["performer"].as_array().unwrap().len(),
            2,
            "{resource}"
        );
        assert!(resource.get("location").is_none());
        assert_eq!(resource["medication"]["reference"], "#m");
    }

    #[test]
    fn a_name_is_family_and_given_and_nothing_else() {
        let names = json!([{"use": "official", "text": "Ada Lovelace", "family": "Lovelace", "given": ["Ada"], "prefix": ["Countess"]}]);
        assert_eq!(
            names_only(&names),
            json!([{"family": "Lovelace", "given": ["Ada"]}])
        );
    }

    #[test]
    fn identity_claims_default_to_a_name_and_a_birth_date_and_refuse_what_is_unknown() {
        assert_eq!(identity_claims(&json!({})).unwrap(), ["name", "birthDate"]);
        let asked = json!({"resource": {"parameter": [
            {"name": "includeIdentityClaim", "valueString": "Patient.gender"},
            {"name": "includeIdentityClaim", "valueString": "name"},
            {"name": "includeIdentityClaim", "valueString": "Patient.gender"}
        ]}});
        assert_eq!(identity_claims(&asked).unwrap(), ["gender", "name"]);
        let unknown = json!({"resource": {"parameter": [
            {"name": "includeIdentityClaim", "valueString": "Patient.photo"}
        ]}});
        assert!(matches!(
            identity_claims(&unknown),
            Err(FhirError::Invalid(_))
        ));
    }

    #[test]
    fn a_resource_with_no_time_is_not_new_when_a_moment_is_asked_for() {
        let dated = json!({"meta": {"lastUpdated": "2026-10-18T04:45:36.123Z"}});
        let at = crate::instant::parse("2026-10-18T04:45:36.123Z");
        assert!(is_new(&dated, at), "at the moment itself counts");
        assert!(!is_new(&dated, at.map(|ms| ms + 1)));
        assert!(is_new(&dated, None), "no moment asked for, everything is");
        assert!(is_new(&json!({}), None));
        assert!(
            !is_new(&json!({}), at),
            "with no time of its own it is not known to be new"
        );
        assert!(!is_new(&json!({"meta": {"lastUpdated": "soon"}}), at));
    }

    #[test]
    fn what_is_not_to_be_stood_behind_is_not_believed() {
        assert!(believed(&json!({"status": "completed"})));
        assert!(!believed(&json!({"status": "entered-in-error"})));
        assert!(!believed(
            &json!({"verificationStatus": {"coding": [{"code": "refuted"}]}})
        ));
        assert!(believed(
            &json!({"verificationStatus": {"coding": [{"code": "confirmed"}]}})
        ));
    }
}
