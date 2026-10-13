//! The audit trail: who did what to which resource, and what was refused.
//!
//! Every way into the data (REST, GraphQL and `/sync`) reaches the engine through
//! `http::execute`, so that is where an operation is recorded, with what the engine says it
//! did and not what the request claimed. A transaction of fifty entries leaves fifty rows,
//! one per document written, read from the committed `RunResult` rather than guessed from the
//! bundle. A request that never reached the engine (no token, a scope too narrow) is recorded
//! at the edge instead, by `Governance::finish`.
//!
//! **What is recorded.** The caller, the interaction, the resource type and id, the patient
//! whose record it touches where that can be told, the outcome, and for a search its
//! parameters. **What is not:** request or response bodies, and the text of errors, which can
//! repeat what a patient record says. A failure is described by its FHIR issue code.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;
use refhir_fhir::{outcome_code, RunError, RunResult};
use refhir_persistence::AuditRow;
use serde_json::{json, Map, Value};

use super::civil;
use super::scope::{current, RequestScope};

/// The longest search string kept.
const MAX_QUERY: usize = 1_000;
/// The longest resource type, id, patient id or operation name kept: a FHIR id is at most 64.
/// What reaches the engine is whatever the caller wrote, however long.
const MAX_NAME: usize = 64;
/// The longest reason a refusal keeps.
const MAX_DETAIL: usize = 256;

/// What an operation was asked to do, read off its name and arguments before it runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
    /// `read`, `search`, `create`, `$everything`, …
    pub op: String,
    pub query: Option<String>,
    pub patient: Option<String>,
}

impl Facts {
    pub fn from_call(path: &str, args: &Value, include_query: bool) -> Facts {
        let (type_name, op) = path.split_once('/').unwrap_or((path, ""));
        let resource_type = match type_name {
            "" | "system" | "Bundle" => None,
            other => Some(truncate(other, MAX_NAME)),
        };
        let resource_id = args
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(|id| truncate(id, MAX_NAME));
        let mut patient = ["_compartment", "patient", "subject"]
            .into_iter()
            .find_map(|name| args.get(name).and_then(Value::as_str))
            .and_then(patient_id_of_reference)
            .or_else(|| args.get("resource").and_then(patient_of_resource));
        if patient.is_none() && resource_type.as_deref() == Some("Patient") {
            patient = resource_id.clone();
        }
        let query = if include_query {
            match op {
                "search" | "history" => render_query(args),
                "conditional-create" => text_arg(args, "ifNoneExist"),
                "conditional-update" | "conditional-delete" => text_arg(args, "query"),
                _ => None,
            }
        } else {
            None
        };
        Facts {
            resource_type,
            resource_id,
            op: truncate(op, MAX_NAME),
            query,
            patient,
        }
    }
}

fn text_arg(args: &Value, name: &str) -> Option<String> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(|text| truncate(text, MAX_QUERY))
}

/// `text` cut to at most `max` characters.
pub(super) fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((index, _)) => text[..index].to_string(),
        None => text.to_string(),
    }
}

/// A FHIR resource type name as the path would carry it: letters and digits, at most 64. Anything
/// else in that position is not one, and is not recorded as one.
pub(super) fn resource_type_name(text: &str) -> Option<&str> {
    (!text.is_empty()
        && text.len() <= 64
        && text.starts_with(|c: char| c.is_ascii_uppercase())
        && text.bytes().all(|byte| byte.is_ascii_alphanumeric()))
    .then_some(text)
}

/// A FHIR logical id: letters, digits, `-` and `.`, at most 64.
pub(super) fn resource_id_text(text: &str) -> Option<&str> {
    (!text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.'))
    .then_some(text)
}

/// The search arguments as `name=value&name=value`, without what the server added itself.
fn render_query(args: &Value) -> Option<String> {
    let object = args.as_object()?;
    let parts: Vec<String> = object
        .iter()
        .filter(|(name, _)| {
            !matches!(
                name.as_str(),
                "_readScopes" | "_sdcArchiver" | "_compartment" | "access_token"
            )
        })
        .filter_map(|(name, value)| value.as_str().map(|value| format!("{name}={value}")))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(truncate(&parts.join("&"), MAX_QUERY))
    }
}

/// `p1` from `Patient/p1`, `Patient/p1/_history/3`, `http://host/fhir/Patient/p1`, or a bare id.
fn patient_id_of_reference(reference: &str) -> Option<String> {
    let reference = reference.split(['?', '#']).next().unwrap_or(reference);
    let segments: Vec<&str> = reference.split('/').collect();
    // The id is the segment after the last `Patient`.
    if let Some(index) = segments.iter().rposition(|segment| *segment == "Patient") {
        return segments
            .get(index + 1)
            .filter(|id| !id.is_empty())
            .map(|id| truncate(id, MAX_NAME));
    }
    // A search written `patient=p1` names the id alone.
    (segments.len() == 1 && !reference.is_empty()).then(|| truncate(reference, MAX_NAME))
}

/// The patient whose record a resource belongs to, as far as it says: a `Patient` is its own,
/// and most clinical resources name theirs in `subject` or `patient`.
pub fn patient_of_resource(resource: &Value) -> Option<String> {
    patient_from_parts(
        resource.get("resourceType").and_then(Value::as_str)?,
        resource.get("id").and_then(Value::as_str),
        [resource.get("subject"), resource.get("patient")],
    )
}

/// [`patient_of_resource`] for a document as stored. A transaction can write thousands, and only
/// a few fields of each are wanted, so they are read without building the whole tree.
fn patient_of_text(text: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Fields {
        #[serde(rename = "resourceType")]
        kind: Option<String>,
        id: Option<String>,
        subject: Option<Value>,
        patient: Option<Value>,
    }
    let fields: Fields = serde_json::from_str(text).ok()?;
    patient_from_parts(
        fields.kind.as_deref()?,
        fields.id.as_deref(),
        [fields.subject.as_ref(), fields.patient.as_ref()],
    )
}

fn patient_from_parts(kind: &str, id: Option<&str>, links: [Option<&Value>; 2]) -> Option<String> {
    if kind == "Patient" {
        return id.map(|id| truncate(id, MAX_NAME));
    }
    links.into_iter().flatten().find_map(|link| {
        let reference = link.get("reference")?.as_str()?;
        // A subject that is a Group or a Device is not a patient.
        if reference.contains("Patient/") {
            patient_id_of_reference(reference)
        } else {
            None
        }
    })
}

/// The FHIR audit action and restful-interaction code for an engine operation.
pub fn interaction_of(
    op: &str,
    resource_type: Option<&str>,
    has_id: bool,
) -> (&'static str, String) {
    let (action, subtype) = match op {
        "create" | "conditional-create" => ("C", "create"),
        "read" => ("R", "read"),
        "vread" => ("R", "vread"),
        "history" if has_id => ("R", "history-instance"),
        "history" if resource_type.is_some() => ("R", "history-type"),
        "history" => ("R", "history-system"),
        "search" => ("E", "search-type"),
        "update" | "conditional-update" => ("U", "update"),
        "patch" => ("U", "patch"),
        "$meta-add" | "$meta-delete" => ("U", "operation"),
        "delete" | "conditional-delete" => ("D", "delete"),
        "transaction" => ("E", "transaction"),
        "batch" => ("E", "batch"),
        _ => ("E", "operation"),
    };
    (action, subtype.to_string())
}

fn is_write(op: &str) -> bool {
    matches!(
        op,
        "create"
            | "conditional-create"
            | "update"
            | "conditional-update"
            | "patch"
            | "delete"
            | "conditional-delete"
            | "transaction"
            | "batch"
            // These change what is stored too, though they are not named as writes.
            | "$meta-add"
            | "$meta-delete"
            | "$install"
    )
}

/// Operations that read terminology and nothing a patient is in: no record is touched, and an
/// interface that calls them on every keystroke would otherwise be most of the audit trail.
fn is_terminology(op: &str) -> bool {
    matches!(
        op,
        "$expand" | "$expandability" | "$validate-code" | "$lookup" | "$translate" | "$subsumes"
    )
}

/// 0 for success, 4 for a refusal or a bad request, 8 for the server failing.
pub fn outcome_of(status: u16) -> u8 {
    match status {
        0..=399 => 0,
        400..=499 => 4,
        _ => 8,
    }
}

/// An operation that is under way, to be finished with what became of it.
pub struct Pending {
    scope: Arc<RequestScope>,
    facts: Facts,
}

/// Start auditing an operation, if there is a request to attribute it to and the audit is on.
pub fn begin(path: &str, args: &Value) -> Option<Pending> {
    let scope = current()?;
    let config = &scope.governance().config.audit;
    if !config.enabled {
        return None;
    }
    let facts = Facts::from_call(path, args, config.include_query);
    if is_terminology(&facts.op) {
        // Not audited, and not to be picked up again at the edge as an unaccounted request.
        scope.note_audited(1);
        return None;
    }
    Some(Pending { scope, facts })
}

impl Pending {
    pub fn finish(self, outcome: &Result<RunResult, RunError>) {
        // A replica that cannot commit sends the operation to the primary, which records it.
        // Nothing was done here, and what was refused, if it is not sent on, is recorded at the
        // edge. Leaving this request unaccounted for is what makes that so.
        if matches!(
            outcome,
            Err(RunError::Commit(refhir_database::CommitError::ReadOnly))
        ) {
            return;
        }
        let config = &self.scope.governance().config.audit;
        let rows = rows_for(&self.scope, &self.facts, outcome, super::now_ms());
        // Whatever is recorded, this request has been accounted for.
        self.scope.note_audited(1);
        let failed = outcome.is_err();
        let read_only = !is_write(&self.facts.op);
        if !config.reads && read_only && !failed {
            return;
        }
        self.scope.governance().push_audit(rows);
    }
}

/// Record an operation that was refused before the engine ran it: a `/sync` message whose
/// scope does not allow it.
pub fn denied(scope: &RequestScope, path: &str, args: &Value, status: u16, reason: &str) {
    let governance = scope.governance();
    if !governance.config.audit.enabled {
        return;
    }
    let facts = Facts::from_call(path, args, governance.config.audit.include_query);
    let (action, subtype) = interaction_of(
        facts.op.as_str(),
        facts.resource_type.as_deref(),
        facts.resource_id.is_some(),
    );
    let mut row = base_row(scope, super::now_ms(), action, &subtype, status);
    fill_from_facts(&mut row, &facts);
    // The reason can repeat the operation the caller named, which is as long as they made it.
    row.detail = Some(truncate(reason, MAX_DETAIL));
    scope.note_audited(1);
    governance.push_audit(vec![row]);
}

fn base_row(
    scope: &RequestScope,
    ts_ms: u64,
    action: &str,
    subtype: &str,
    status: u16,
) -> AuditRow {
    let identity = scope.identity();
    AuditRow {
        ts_ms,
        request_id: scope.request_id.clone(),
        source: scope.source.name().to_string(),
        action: action.to_string(),
        subtype: subtype.to_string(),
        outcome: outcome_of(status),
        status,
        principal: scope.principal(),
        client_id: identity.as_ref().map(|identity| identity.client_id.clone()),
        user: identity.as_ref().and_then(|identity| identity.user.clone()),
        fhir_user: identity.and_then(|identity| identity.fhir_user),
        remote_ip: scope.remote_ip.map(|ip| ip.to_string()),
        ..AuditRow::default()
    }
}

fn fill_from_facts(row: &mut AuditRow, facts: &Facts) {
    row.resource_type = facts.resource_type.clone();
    row.resource_id = facts.resource_id.clone();
    row.patient = facts.patient.clone();
    row.query = facts.query.clone();
}

/// The rows an operation leaves, given what became of it.
pub fn rows_for(
    scope: &RequestScope,
    facts: &Facts,
    outcome: &Result<RunResult, RunError>,
    ts_ms: u64,
) -> Vec<AuditRow> {
    let (action, subtype) = interaction_of(
        facts.op.as_str(),
        facts.resource_type.as_deref(),
        facts.resource_id.is_some(),
    );
    let operation_name = facts.op.starts_with('$').then(|| facts.op.clone());
    let result = match outcome {
        Ok(result) => result,
        Err(err) => {
            let status = crate::http::run_error_status(err).as_u16();
            let code = match err {
                RunError::Fhir(fhir) => outcome_code(fhir),
                RunError::Commit(refhir_database::CommitError::Occ(_)) => "conflict",
                RunError::Commit(_) => "exception",
            };
            let mut row = base_row(scope, ts_ms, action, &subtype, status);
            fill_from_facts(&mut row, facts);
            row.detail = Some(match &operation_name {
                Some(name) => format!("{name}: {code}"),
                None => code.to_string(),
            });
            return vec![row];
        }
    };
    let status = match (result.status, facts.op.as_str()) {
        (0, "create") => 201,
        (0, _) => 200,
        (status, _) => status,
    };
    if is_write(&facts.op) && !result.writes.is_empty() {
        let created = created_in_response(&result.value);
        return result
            .writes
            .iter()
            .map(|write| {
                let write_action = if write.json.is_none() {
                    "D"
                } else {
                    match facts.op.as_str() {
                        "create" | "conditional-create" => "C",
                        "update" | "conditional-update" if status == 201 => "C",
                        "transaction" | "batch"
                            if created.get(&(write.tablet.clone(), write.id.clone()))
                                == Some(&true) =>
                        {
                            "C"
                        }
                        _ => "U",
                    }
                };
                let mut row = base_row(scope, ts_ms, write_action, &subtype, status);
                row.resource_type = Some(write.tablet.clone());
                row.resource_id = Some(write.id.clone());
                row.patient = write
                    .json
                    .as_deref()
                    .and_then(patient_of_text)
                    .or_else(|| (write.tablet == "Patient").then(|| write.id.clone()))
                    .or_else(|| facts.patient.clone());
                row.query = facts.query.clone();
                row.detail = operation_name.clone();
                row
            })
            .collect();
    }
    let mut row = base_row(scope, ts_ms, action, &subtype, status);
    fill_from_facts(&mut row, facts);
    row.detail = operation_name;
    // A read names its patient in the resource it returned, where the arguments did not.
    if row.patient.is_none() && matches!(facts.op.as_str(), "read" | "vread") {
        row.patient = patient_of_resource(&result.value);
    }
    vec![row]
}

/// Which `(type, id)` a transaction response says it created: `201` on the entry.
fn created_in_response(value: &Value) -> HashMap<(String, String), bool> {
    let mut created = HashMap::new();
    let Some(entries) = value.get("entry").and_then(Value::as_array) else {
        return created;
    };
    for entry in entries {
        let Some(response) = entry.get("response") else {
            continue;
        };
        let Some(location) = response.get("location").and_then(Value::as_str) else {
            continue;
        };
        let mut parts = location.trim_start_matches('/').split('/');
        let (Some(kind), Some(id)) = (parts.next(), parts.next()) else {
            continue;
        };
        let was_created = response
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status.starts_with("201"));
        created.insert((kind.to_string(), id.to_string()), was_created);
    }
    created
}

const SUBTYPE_SYSTEM: &str = "http://hl7.org/fhir/restful-interaction";
const OTHER_SUBTYPE_SYSTEM: &str = "urn:refhir:audit-event-subtype";

/// A stored row as the FHIR `AuditEvent` it describes.
pub fn audit_event(row: &AuditRow) -> Value {
    let standard = matches!(
        row.subtype.as_str(),
        "read"
            | "vread"
            | "update"
            | "patch"
            | "delete"
            | "create"
            | "search-type"
            | "history-instance"
            | "history-type"
            | "history-system"
            | "transaction"
            | "batch"
    );
    let mut agent = Map::new();
    let mut who = Map::new();
    if let Some(fhir_user) = &row.fhir_user {
        who.insert("reference".into(), json!(fhir_user));
    }
    if let Some(client) = &row.client_id {
        who.insert("identifier".into(), json!({"value": client}));
    }
    if who.is_empty() {
        who.insert("display".into(), json!("anonymous"));
    }
    agent.insert("who".into(), Value::Object(who));
    if let Some(user) = &row.user {
        agent.insert("name".into(), json!(user));
    }
    agent.insert("requestor".into(), json!(true));
    if let Some(address) = &row.remote_ip {
        agent.insert("network".into(), json!({"address": address, "type": "2"}));
    }
    let mut entities = Vec::new();
    if let (Some(kind), Some(id)) = (&row.resource_type, &row.resource_id) {
        let mut entity = Map::new();
        entity.insert("what".into(), json!({"reference": format!("{kind}/{id}")}));
        if let Some(query) = &row.query {
            entity.insert("query".into(), json!(base64_text(query)));
        }
        entities.push(Value::Object(entity));
    } else if let Some(kind) = &row.resource_type {
        // A search names no one resource: the type it ran against and what it asked for.
        let mut entity = Map::new();
        entity.insert("description".into(), json!(kind));
        if let Some(query) = &row.query {
            entity.insert("query".into(), json!(base64_text(query)));
        }
        entities.push(Value::Object(entity));
    } else if let Some(query) = &row.query {
        entities.push(json!({"query": base64_text(query)}));
    }
    if let Some(patient) = &row.patient {
        if row.resource_type.as_deref() != Some("Patient")
            || row.resource_id.as_deref() != Some(patient)
        {
            entities.push(json!({
                "what": {"reference": format!("Patient/{patient}")},
                "role": {
                    "system": "http://terminology.hl7.org/CodeSystem/object-role",
                    "code": "1",
                    "display": "Patient"
                }
            }));
        }
    }
    let mut event = Map::new();
    event.insert("resourceType".into(), json!("AuditEvent"));
    event.insert("id".into(), json!(row.id.to_string()));
    event.insert(
        "extension".into(),
        json!([
            {"url": "urn:refhir:request-id", "valueString": row.request_id},
            {"url": "urn:refhir:source", "valueString": row.source},
            {"url": "urn:refhir:http-status", "valueInteger": row.status}
        ]),
    );
    event.insert(
        "type".into(),
        json!({
            "system": "http://terminology.hl7.org/CodeSystem/audit-event-type",
            "code": "rest",
            "display": "Restful Operation"
        }),
    );
    event.insert(
        "subtype".into(),
        json!([{
            "system": if standard { SUBTYPE_SYSTEM } else { OTHER_SUBTYPE_SYSTEM },
            "code": row.subtype
        }]),
    );
    event.insert("action".into(), json!(row.action));
    event.insert("recorded".into(), json!(civil::instant_string(row.ts_ms)));
    event.insert("outcome".into(), json!(row.outcome.to_string()));
    if let Some(detail) = &row.detail {
        event.insert("outcomeDesc".into(), json!(detail));
    }
    event.insert("agent".into(), json!([Value::Object(agent)]));
    event.insert(
        "source".into(),
        json!({
            "observer": {"display": "refhir"},
            "type": [{
                "system": "http://terminology.hl7.org/CodeSystem/security-source-type",
                "code": "4",
                "display": "Application Server"
            }]
        }),
    );
    if !entities.is_empty() {
        event.insert("entity".into(), Value::Array(entities));
    }
    Value::Object(event)
}

fn base64_text(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use refhir_database::DocumentWrite;
    use refhir_types::Timestamp;

    fn write(kind: &str, id: &str, json: Option<Value>) -> DocumentWrite {
        DocumentWrite {
            tablet: kind.to_string(),
            id: id.to_string(),
            json: json.map(|value| value.to_string()),
        }
    }

    #[test]
    fn a_reference_gives_up_its_patient_id() {
        for (reference, expected) in [
            ("Patient/p1", Some("p1")),
            ("Patient/p1/_history/3", Some("p1")),
            ("http://host/fhir/Patient/p1", Some("p1")),
            ("p1", Some("p1")),
            ("Patient/", None),
            ("Group/g1", None),
            ("", None),
        ] {
            assert_eq!(
                patient_id_of_reference(reference).as_deref(),
                expected,
                "{reference}"
            );
        }
    }

    #[test]
    fn a_resource_names_its_patient_when_it_has_one() {
        let of = |value: Value| patient_of_resource(&value);
        assert_eq!(
            of(json!({"resourceType": "Patient", "id": "p1"})).as_deref(),
            Some("p1")
        );
        assert_eq!(
            of(json!({"resourceType": "Observation", "subject": {"reference": "Patient/p2"}}))
                .as_deref(),
            Some("p2")
        );
        assert_eq!(
            of(json!({"resourceType": "Immunization", "patient": {"reference": "Patient/p3"}}))
                .as_deref(),
            Some("p3")
        );
        assert_eq!(
            of(json!({"resourceType": "Observation", "subject": {"reference": "Group/g"}})),
            None
        );
        assert_eq!(of(json!({"resourceType": "Organization"})), None);
    }

    #[test]
    fn a_calls_facts_come_from_its_name_and_arguments() {
        let facts = Facts::from_call(
            "Observation/search",
            &json!({"code": "8480-6", "_count": "5", "patient": "p1",
                    "_readScopes": ["user/*.*"], "_compartment": "Patient/p9"}),
            true,
        );
        assert_eq!(facts.resource_type.as_deref(), Some("Observation"));
        assert_eq!(facts.op, "search");
        // The compartment a patient-scoped token was confined to wins over the one asked for.
        assert_eq!(facts.patient.as_deref(), Some("p9"));
        assert_eq!(
            facts.query.as_deref(),
            Some("_count=5&code=8480-6&patient=p1"),
            "without what the server added itself"
        );
        let facts = Facts::from_call("Observation/search", &json!({"code": "x"}), false);
        assert_eq!(facts.query, None, "when the query is not to be kept");
        let facts = Facts::from_call("Patient/read", &json!({"id": "p1"}), true);
        assert_eq!(
            (facts.resource_id.as_deref(), facts.patient.as_deref()),
            (Some("p1"), Some("p1"))
        );
        let facts = Facts::from_call("Bundle/transaction", &json!({"resource": {}}), true);
        assert_eq!(
            (facts.resource_type, facts.op.as_str()),
            (None, "transaction")
        );
        let facts = Facts::from_call("system/history", &json!({}), true);
        assert_eq!(facts.resource_type, None);
        let facts = Facts::from_call(
            "Patient/conditional-update",
            &json!({"query": "identifier=123", "resource": {"resourceType": "Patient"}}),
            true,
        );
        assert_eq!(facts.query.as_deref(), Some("identifier=123"));
        let long = "a".repeat(5_000);
        let facts = Facts::from_call("Patient/search", &json!({"name": long}), true);
        assert!(facts.query.unwrap().chars().count() <= MAX_QUERY);
    }

    #[test]
    fn what_a_caller_chose_to_call_is_cut_to_a_row_of_bounded_size() {
        let long = "A".repeat(100_000);
        let facts = Facts::from_call(
            &format!("{long}/$op{long}"),
            &json!({"id": long, "patient": long, "resource": {"resourceType": "Patient", "id": long}}),
            true,
        );
        for text in [
            facts.resource_type.as_deref(),
            facts.resource_id.as_deref(),
            facts.patient.as_deref(),
            Some(facts.op.as_str()),
        ] {
            assert!(text.unwrap().chars().count() <= MAX_NAME);
        }
        let scope = scope();
        let rows = rows_for(
            &scope,
            &facts,
            &Err(RunError::Fhir(refhir_fhir::FhirError::NotFound("x".into()))),
            1,
        );
        let row = &rows[0];
        assert!(row.detail.as_deref().unwrap().chars().count() <= MAX_NAME + 20);
        denied(&scope, &format!("{long}/read"), &json!({}), 403, &long);
        let kept = scope.governance().buffers.lock().unwrap().audit.clone();
        assert!(kept[0].detail.as_deref().unwrap().chars().count() <= MAX_DETAIL);
        assert!(kept[0].resource_type.as_deref().unwrap().chars().count() <= MAX_NAME);
    }

    #[test]
    fn a_stored_document_gives_up_the_same_patient_as_its_parsed_form() {
        for document in [
            json!({"resourceType": "Patient", "id": "p1"}),
            json!({"resourceType": "Observation", "subject": {"reference": "Patient/p2"}}),
            json!({"resourceType": "Immunization", "patient": {"reference": "Patient/p3"}}),
            json!({"resourceType": "Observation", "subject": {"reference": "Group/g"},
                   "patient": {"reference": "Patient/p4"}}),
            json!({"resourceType": "Observation", "subject": "not an object"}),
            json!({"resourceType": "Organization"}),
            json!({"id": "no-type"}),
        ] {
            assert_eq!(
                patient_of_text(&document.to_string()),
                patient_of_resource(&document),
                "{document}"
            );
        }
        assert_eq!(patient_of_text("not json"), None);
    }

    #[test]
    fn engine_operations_map_to_fhir_interactions() {
        assert_eq!(
            interaction_of("read", Some("Patient"), true),
            ("R", "read".into())
        );
        assert_eq!(
            interaction_of("history", Some("Patient"), true).1,
            "history-instance"
        );
        assert_eq!(
            interaction_of("history", Some("Patient"), false).1,
            "history-type"
        );
        assert_eq!(interaction_of("history", None, false).1, "history-system");
        assert_eq!(
            interaction_of("search", Some("Patient"), false),
            ("E", "search-type".into())
        );
        assert_eq!(interaction_of("conditional-create", None, false).0, "C");
        assert_eq!(interaction_of("conditional-delete", None, false).0, "D");
        assert_eq!(
            interaction_of("$expand", Some("ValueSet"), false),
            ("E", "operation".into())
        );
        assert_eq!(outcome_of(200), 0);
        assert_eq!(outcome_of(404), 4);
        assert_eq!(outcome_of(403), 4);
        assert_eq!(outcome_of(500), 8);
    }

    #[test]
    fn a_stored_row_is_a_valid_looking_audit_event() {
        let row = AuditRow {
            id: 42,
            ts_ms: 1_700_000_000_123,
            request_id: "req-1".into(),
            source: "http".into(),
            action: "R".into(),
            subtype: "read".into(),
            outcome: 0,
            status: 200,
            principal: "client:app".into(),
            client_id: Some("app".into()),
            user: Some("u-1".into()),
            fhir_user: Some("Practitioner/dr".into()),
            remote_ip: Some("203.0.113.9".into()),
            resource_type: Some("Observation".into()),
            resource_id: Some("o1".into()),
            patient: Some("p1".into()),
            query: None,
            detail: None,
        };
        let event = audit_event(&row);
        assert_eq!(event["resourceType"], "AuditEvent");
        assert_eq!(event["id"], "42");
        assert_eq!(event["recorded"], "2023-11-14T22:13:20.123Z");
        assert_eq!(event["action"], "R");
        assert_eq!(event["outcome"], "0");
        assert_eq!(event["type"]["code"], "rest");
        assert_eq!(event["subtype"][0]["system"], SUBTYPE_SYSTEM);
        assert_eq!(event["subtype"][0]["code"], "read");
        assert_eq!(event["agent"][0]["who"]["reference"], "Practitioner/dr");
        assert_eq!(event["agent"][0]["who"]["identifier"]["value"], "app");
        assert_eq!(event["agent"][0]["requestor"], true);
        assert_eq!(event["agent"][0]["network"]["address"], "203.0.113.9");
        assert_eq!(event["agent"][0]["network"]["type"], "2");
        assert_eq!(event["source"]["observer"]["display"], "refhir");
        assert_eq!(event["entity"][0]["what"]["reference"], "Observation/o1");
        assert_eq!(event["entity"][1]["what"]["reference"], "Patient/p1");
        assert_eq!(event["entity"][1]["role"]["code"], "1");

        // A search names no resource: its query is the entity, encoded as FHIR wants.
        let search = AuditRow {
            subtype: "search-type".into(),
            action: "E".into(),
            resource_id: None,
            query: Some("code=8480-6".into()),
            ..row.clone()
        };
        let event = audit_event(&search);
        assert_eq!(event["entity"][0]["description"], "Observation");
        assert_eq!(event["entity"][0]["query"], "Y29kZT04NDgwLTY=");
        // A refusal says why, and what is not a restful interaction is not claimed to be one.
        let refused = AuditRow {
            subtype: "oauth-token".into(),
            outcome: 4,
            status: 401,
            detail: Some("invalid".into()),
            client_id: None,
            fhir_user: None,
            user: None,
            resource_type: None,
            resource_id: None,
            patient: None,
            ..row
        };
        let event = audit_event(&refused);
        assert_eq!(event["outcome"], "4");
        assert_eq!(event["outcomeDesc"], "invalid");
        assert_eq!(event["subtype"][0]["system"], OTHER_SUBTYPE_SYSTEM);
        assert_eq!(event["agent"][0]["who"]["display"], "anonymous");
        assert!(event.get("entity").is_none());
    }

    fn scope() -> Arc<RequestScope> {
        let governance =
            super::super::Governance::new(super::super::GovernanceConfig::observing(), false, None);
        let scope = RequestScope::new(
            governance,
            "req-9".into(),
            super::super::scope::Source::Http,
            Some("198.51.100.4".parse().unwrap()),
        );
        scope.set_identity(super::super::scope::Identity {
            client_id: "app".into(),
            user: Some("u-1".into()),
            fhir_user: None,
        });
        scope
    }

    fn result(value: Value, writes: Vec<DocumentWrite>, status: u16) -> RunResult {
        RunResult {
            value,
            ts: Timestamp::from_u64(5).unwrap(),
            reads: Default::default(),
            wrote: !writes.is_empty(),
            writes,
            status,
        }
    }

    #[test]
    fn a_transaction_leaves_one_row_per_document_it_wrote() {
        let scope = scope();
        let facts = Facts::from_call("Bundle/transaction", &json!({"resource": {}}), true);
        let outcome = Ok(result(
            json!({"resourceType": "Bundle", "type": "transaction-response", "entry": [
                {"response": {"status": "201 Created", "location": "Patient/new1/_history/5"}},
                {"response": {"status": "200 OK", "location": "Observation/o1/_history/5"}},
                {"response": {"status": "204 No Content"}}
            ]}),
            vec![
                write(
                    "Patient",
                    "new1",
                    Some(json!({"resourceType": "Patient", "id": "new1"})),
                ),
                write(
                    "Observation",
                    "o1",
                    Some(
                        json!({"resourceType": "Observation", "subject": {"reference": "Patient/new1"}}),
                    ),
                ),
                write("Encounter", "gone", None),
            ],
            0,
        ));
        let rows = rows_for(&scope, &facts, &outcome, 1_000);
        assert_eq!(rows.len(), 3);
        let summary: Vec<_> = rows
            .iter()
            .map(|row| {
                (
                    row.action.as_str(),
                    row.resource_type.clone().unwrap(),
                    row.resource_id.clone().unwrap(),
                    row.patient.clone(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    "C",
                    "Patient".to_string(),
                    "new1".to_string(),
                    Some("new1".to_string())
                ),
                (
                    "U",
                    "Observation".to_string(),
                    "o1".to_string(),
                    Some("new1".to_string())
                ),
                ("D", "Encounter".to_string(), "gone".to_string(), None),
            ]
        );
        assert!(rows
            .iter()
            .all(|row| row.subtype == "transaction" && row.outcome == 0));
        assert!(rows.iter().all(|row| row.principal == "client:app"
            && row.user.as_deref() == Some("u-1")
            && row.remote_ip.as_deref() == Some("198.51.100.4")
            && row.request_id == "req-9"
            && row.source == "http"));
    }

    #[test]
    fn single_writes_and_reads_each_leave_what_they_did() {
        let scope = scope();
        let created = rows_for(
            &scope,
            &Facts::from_call(
                "Patient/create",
                &json!({"resource": {"resourceType": "Patient"}}),
                true,
            ),
            &Ok(result(
                json!({"resourceType": "Patient", "id": "p1"}),
                vec![write(
                    "Patient",
                    "p1",
                    Some(json!({"resourceType": "Patient", "id": "p1"})),
                )],
                0,
            )),
            1,
        );
        assert_eq!((created[0].action.as_str(), created[0].status), ("C", 201));
        let updated = rows_for(
            &scope,
            &Facts::from_call("Patient/update", &json!({"id": "p1", "resource": {}}), true),
            &Ok(result(
                json!({}),
                vec![write(
                    "Patient",
                    "p1",
                    Some(json!({"resourceType": "Patient", "id": "p1"})),
                )],
                0,
            )),
            1,
        );
        assert_eq!((updated[0].action.as_str(), updated[0].status), ("U", 200));
        // Update that created: the engine says 201.
        let upserted = rows_for(
            &scope,
            &Facts::from_call("Patient/update", &json!({"id": "p1", "resource": {}}), true),
            &Ok(result(
                json!({}),
                vec![write(
                    "Patient",
                    "p1",
                    Some(json!({"resourceType": "Patient", "id": "p1"})),
                )],
                201,
            )),
            1,
        );
        assert_eq!(upserted[0].action, "C");
        let deleted = rows_for(
            &scope,
            &Facts::from_call("Patient/delete", &json!({"id": "p1"}), true),
            &Ok(result(json!({}), vec![write("Patient", "p1", None)], 0)),
            1,
        );
        assert_eq!(
            (
                deleted[0].action.as_str(),
                deleted[0].resource_id.as_deref()
            ),
            ("D", Some("p1"))
        );
        // A write that changed nothing still happened.
        let noop = rows_for(
            &scope,
            &Facts::from_call(
                "Patient/conditional-update",
                &json!({"query": "identifier=1", "resource": {}}),
                true,
            ),
            &Ok(result(json!({}), vec![], 200)),
            1,
        );
        assert_eq!(
            (
                noop.len(),
                noop[0].action.as_str(),
                noop[0].query.as_deref()
            ),
            (1, "U", Some("identifier=1"))
        );

        let read = rows_for(
            &scope,
            &Facts::from_call("Observation/read", &json!({"id": "o1"}), true),
            &Ok(result(
                json!({"resourceType": "Observation", "id": "o1", "subject": {"reference": "Patient/p7"}}),
                vec![],
                0,
            )),
            1,
        );
        assert_eq!(
            (read[0].action.as_str(), read[0].subtype.as_str()),
            ("R", "read")
        );
        assert_eq!(
            read[0].patient.as_deref(),
            Some("p7"),
            "told from what was read"
        );
        let operation = rows_for(
            &scope,
            &Facts::from_call("Patient/$everything", &json!({"id": "p1"}), true),
            &Ok(result(json!({"resourceType": "Bundle"}), vec![], 0)),
            1,
        );
        assert_eq!(operation[0].subtype, "operation");
        assert_eq!(operation[0].detail.as_deref(), Some("$everything"));
        assert_eq!(operation[0].patient.as_deref(), Some("p1"));
    }

    #[test]
    fn a_failure_is_recorded_with_its_issue_code_and_not_its_text() {
        let scope = scope();
        let rows = rows_for(
            &scope,
            &Facts::from_call("Patient/read", &json!({"id": "nope"}), true),
            &Err(RunError::Fhir(refhir_fhir::FhirError::NotFound(
                "Patient/nope: name Ann Smith".into(),
            ))),
            1,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].outcome, rows[0].status), (4, 404));
        assert_eq!(rows[0].detail.as_deref(), Some("not-found"));
        assert_eq!(rows[0].resource_id.as_deref(), Some("nope"));
        let rows = rows_for(
            &scope,
            &Facts::from_call("Patient/$validate", &json!({"resource": {}}), true),
            &Err(RunError::Commit(refhir_database::CommitError::Invalid(
                "disk".into(),
            ))),
            1,
        );
        assert_eq!((rows[0].outcome, rows[0].status), (8, 500));
        assert_eq!(rows[0].detail.as_deref(), Some("$validate: exception"));
    }
}
