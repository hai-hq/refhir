//! `Claim/$submit` and `CoverageEligibilityRequest/$submit`.
//!
//! FHIR defines both as asking a payer to decide: the operation takes a request (or a Bundle of
//! them, with the resources they refer to) and answers with the payer's response. A FHIR server
//! is not a payer, so the operation here does what a server can: it validates and stores what was
//! submitted, as a transaction would, and stores and returns a response that says the request was
//! received and is waiting (`outcome` `queued`). An application that adjudicates reads the
//! requests that are waiting, or is told of each response as it is created by a `Subscription` on
//! `ClaimResponse?outcome=queued`, and updates the response when it has decided.
//!
//! Everything a submission writes is one transaction: it is stored whole, or not at all.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use refhir_database::Transaction;
use refhir_fhirpath::{EmptyHost, Host};
use serde_json::{json, Map, Value};

use crate::bundle;
use crate::catalog::Catalog;
use crate::error::FhirError;
use crate::ops;

/// A kind of request that can be submitted, and the response it is answered with.
struct Kind {
    request: &'static str,
    response: &'static str,
    /// What the response repeats of the request: `(element of the response, element of the
    /// request)`. They are the elements R4 requires of a response and the request already says,
    /// and what a reader needs to tell whose response it is without finding the request.
    copied: &'static [(&'static str, &'static str)],
}

const KINDS: [Kind; 2] = [
    Kind {
        request: "Claim",
        response: "ClaimResponse",
        copied: &[
            ("type", "type"),
            ("subType", "subType"),
            ("use", "use"),
            ("patient", "patient"),
            ("insurer", "insurer"),
            ("requestor", "provider"),
        ],
    },
    Kind {
        request: "CoverageEligibilityRequest",
        response: "CoverageEligibilityResponse",
        copied: &[
            ("purpose", "purpose"),
            ("patient", "patient"),
            ("servicedDate", "servicedDate"),
            ("servicedPeriod", "servicedPeriod"),
            ("insurer", "insurer"),
            ("requestor", "provider"),
        ],
    },
];

/// The most resources one submission may hold, the referenced ones included. It is a transaction,
/// and one that is not bounded holds the whole of it in memory.
const MAX_RESOURCES: usize = 500;

/// How deep Bundles may be inside Bundles: a batch of Bundles that each hold one request and what
/// it refers to is two.
const MAX_DEPTH: usize = 3;

/// Say in a CapabilityStatement that the resource types that can be submitted can be.
pub(crate) fn annotate_capability(statement: &mut Value) {
    for kind in &KINDS {
        crate::catalog::add_operation(
            statement,
            kind.request,
            "submit",
            &format!(
                "http://hl7.org/fhir/OperationDefinition/{}-submit",
                kind.request
            ),
        );
    }
}

/// The interaction each type needs that a submission of `body` to `type_name/$submit` writes: the
/// requests and what they come with, as the entries say (a `PUT` is an update, and a create too
/// where an update may create, and always for a conditional one, which creates when nothing
/// matches), and the responses. A body that cannot be read writes nothing, and the operation
/// refuses it itself. The resources are not copied: only their types and the entries' requests
/// are read.
///
/// A caller is allowed to submit only what its scopes let it write, which the path of the request
/// (`Claim/$submit`) cannot say.
pub fn submission_writes(
    type_name: &str,
    body: &Value,
    update_creates: bool,
) -> Vec<(String, &'static str)> {
    let Some(kind) = KINDS.iter().find(|kind| kind.request == type_name) else {
        return Vec::new();
    };
    let args = json!({"resource": body});
    let Ok(input) = submitted(&args) else {
        return Vec::new();
    };
    let mut plan = Plan::shallow();
    let planned = if input.get("resourceType").and_then(Value::as_str) == Some("Bundle") {
        plan.add_bundle(kind, input, 1)
    } else {
        plan.add_resource(kind, input, None)
    };
    if planned.is_err() {
        return Vec::new();
    }
    let mut writes: Vec<(String, &'static str)> = Vec::new();
    let mut note = |type_name: &str, interaction: &'static str| {
        let write = (type_name.to_string(), interaction);
        if !writes.contains(&write) {
            writes.push(write);
        }
    };
    for entry in &plan.entries {
        let Some(written) = entry
            .pointer("/resource/resourceType")
            .and_then(Value::as_str)
        else {
            continue;
        };
        if entry.pointer("/request/method").and_then(Value::as_str) == Some("PUT") {
            note(written, "update");
            let conditional = entry
                .pointer("/request/url")
                .and_then(Value::as_str)
                .is_some_and(|url| url.contains('?'));
            if update_creates || conditional {
                note(written, "create");
            }
        } else {
            note(written, "create");
        }
    }
    if !plan.requests.is_empty() {
        note(kind.response, "create");
    }
    writes
}

pub(crate) fn operation(
    tx: &mut Transaction,
    catalog: &Catalog,
    type_name: &str,
    args: &Value,
) -> Result<Value, FhirError> {
    let kind = KINDS
        .iter()
        .find(|kind| kind.request == type_name)
        .ok_or_else(|| {
            FhirError::Unsupported(format!("unsupported operation {type_name}/$submit"))
        })?;
    let input = submitted(args)?;
    let mut plan = Plan::default();
    let is_bundle = input.get("resourceType").and_then(Value::as_str) == Some("Bundle");
    if is_bundle {
        plan.add_bundle(kind, input, 1)?;
    } else if input.get("resourceType").and_then(Value::as_str) == Some(kind.request) {
        plan.add_resource(kind, input, None)?;
    } else {
        return Err(FhirError::Invalid(format!(
            "resource must be a {} or a Bundle that holds them",
            kind.request
        )));
    }
    if plan.requests.is_empty() {
        return Err(FhirError::Invalid(format!(
            "the Bundle holds no {} to submit",
            kind.request
        )));
    }
    let response_model = catalog.get(kind.response)?;
    let stored = bundle::transaction(
        tx,
        catalog,
        &json!({"resourceType": "Bundle", "type": "transaction", "entry": plan.entries}),
        args,
    )?;
    let created = EmptyHost.now();
    let created = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        created.year.unwrap_or(1970),
        created.month.unwrap_or(1),
        created.day.unwrap_or(1),
        created.hour.unwrap_or(0),
        created.minute.unwrap_or(0),
        created.second.unwrap_or(0),
    );
    let mut responses = Vec::with_capacity(plan.requests.len());
    for index in &plan.requests {
        let request = stored
            .pointer(&format!("/entry/{index}/resource"))
            .ok_or_else(|| {
                FhirError::Storage(format!("entry {index} was stored without a body"))
            })?;
        let matched = matched_by_condition(&plan.entries[*index], &stored, *index);
        if matched {
            if let Some(answer) = answer_to(tx, catalog, kind, response_model, request) {
                responses.push(answer);
                continue;
            }
        }
        let response = ops::create(tx, catalog, response_model, queued(kind, request, &created))?;
        responses.push(response);
    }
    if !is_bundle {
        return Ok(responses.remove(0));
    }
    Ok(json!({
        "resourceType": "Bundle",
        "type": "collection",
        "entry": responses.into_iter().map(|resource| json!({"resource": resource})).collect::<Vec<_>>()
    }))
}

/// The resource the caller submitted: the `resource` parameter of a `Parameters`, or else the body
/// itself (R4 gives a single resource as the only input a body of its own).
fn submitted(args: &Value) -> Result<&Value, FhirError> {
    let body = args
        .get("resource")
        .ok_or_else(|| FhirError::Invalid("resource is required".into()))?;
    if body.get("resourceType").and_then(Value::as_str) != Some("Parameters") {
        return Ok(body);
    }
    let mut found = body
        .get("parameter")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|parameter| parameter.get("name").and_then(Value::as_str) == Some("resource"));
    let parameter = found.next().ok_or_else(|| {
        FhirError::Invalid("the Parameters have no parameter named resource".into())
    })?;
    if found.next().is_some() {
        return Err(FhirError::Invalid(
            "the parameter resource takes one value".into(),
        ));
    }
    parameter
        .get("resource")
        .ok_or_else(|| FhirError::Invalid("the parameter resource has no resource".into()))
}

/// What a submission writes: the entries of the transaction, and which of them are the requests.
#[derive(Default)]
struct Plan {
    entries: Vec<Value>,
    /// The position in `entries` of each request, in the order they were submitted.
    requests: Vec<usize>,
    /// The `urn:uuid:` full URLs the entries have, which each name one entry of the transaction
    /// every entry is flattened into.
    full_urls: BTreeSet<String>,
    /// Keep of each resource only its type: for working out what a submission would write, which
    /// needs no copy of what is written.
    shallow: bool,
}

impl Plan {
    fn shallow() -> Self {
        Plan {
            shallow: true,
            ..Plan::default()
        }
    }

    fn add_bundle(&mut self, kind: &Kind, bundle: &Value, depth: usize) -> Result<(), FhirError> {
        if depth > MAX_DEPTH {
            return Err(FhirError::Invalid(format!(
                "Bundles are nested more than {MAX_DEPTH} deep"
            )));
        }
        // A full URL names an entry only inside its own Bundle, and a nested Bundle may use one that
        // another uses for something else.
        let scoped = if depth > 1 {
            self.scoped(bundle)?
        } else {
            Cow::Borrowed(bundle)
        };
        for entry in scoped
            .get("entry")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let resource = entry
                .get("resource")
                .ok_or_else(|| FhirError::Invalid("a Bundle entry has no resource".into()))?;
            if resource.get("resourceType").and_then(Value::as_str) == Some("Bundle") {
                self.add_bundle(kind, resource, depth + 1)?;
            } else {
                self.add_resource(kind, resource, Some(entry))?;
            }
        }
        Ok(())
    }

    /// `bundle`, a Bundle inside another, as its own full URLs mean in the transaction: one that an
    /// entry already added uses is replaced, in the Bundle and everything that names it, by a new
    /// one, so that what it names is the entry of this Bundle and not that one. The same full URL
    /// on two entries of one Bundle is refused, since nothing says which a reference means.
    fn scoped<'a>(&self, bundle: &'a Value) -> Result<Cow<'a, Value>, FhirError> {
        let mut own = BTreeSet::new();
        let mut renames = BTreeMap::new();
        for full in bundle
            .get("entry")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("fullUrl").and_then(Value::as_str))
            .filter(|full| full.starts_with("urn:uuid:"))
        {
            if !own.insert(full) {
                return Err(shared_url(full));
            }
            if self.full_urls.contains(full) {
                renames.insert(
                    full.to_string(),
                    format!("urn:uuid:{}", uuid::Uuid::new_v4()),
                );
            }
        }
        if renames.is_empty() {
            return Ok(Cow::Borrowed(bundle));
        }
        let mut renamed = bundle.clone();
        rename_urls(&mut renamed, &renames);
        Ok(Cow::Owned(renamed))
    }

    /// Add a resource as a `POST` of its type, or as the entry's own `request` says: a conditional
    /// create (`ifNoneExist`) keeps a Patient or an Organization from being made again each time
    /// it is submitted with a request.
    fn add_resource(
        &mut self,
        kind: &Kind,
        resource: &Value,
        entry: Option<&Value>,
    ) -> Result<(), FhirError> {
        if self.entries.len() >= MAX_RESOURCES {
            return Err(FhirError::Invalid(format!(
                "a submission holds at most {MAX_RESOURCES} resources"
            )));
        }
        let type_name = resource
            .get("resourceType")
            .and_then(Value::as_str)
            .ok_or_else(|| FhirError::Invalid("a Bundle entry has no resourceType".into()))?
            .to_string();
        let mut added = Map::new();
        if let Some(full_url) = entry.and_then(|entry| entry.get("fullUrl")) {
            if let Some(full) = full_url
                .as_str()
                .filter(|full| full.starts_with("urn:uuid:"))
            {
                if !self.full_urls.insert(full.to_string()) {
                    return Err(shared_url(full));
                }
            }
            added.insert("fullUrl".into(), full_url.clone());
        }
        let request = match entry.and_then(|entry| entry.get("request")) {
            Some(request) => {
                let method = request.get("method").and_then(Value::as_str).unwrap_or("");
                if method != "POST" && method != "PUT" {
                    return Err(FhirError::Invalid(format!(
                        "a submission stores what it holds: an entry of {type_name} has the method \
                         {method:?}, and only POST and PUT are allowed"
                    )));
                }
                request.clone()
            }
            None => json!({"method": "POST", "url": type_name}),
        };
        added.insert("request".into(), request);
        added.insert(
            "resource".into(),
            if self.shallow {
                json!({"resourceType": type_name})
            } else {
                resource.clone()
            },
        );
        if type_name == kind.request {
            self.requests.push(self.entries.len());
        }
        self.entries.push(Value::Object(added));
        Ok(())
    }
}

fn shared_url(full: &str) -> FhirError {
    FhirError::Invalid(format!(
        "the fullUrl {full} names more than one entry of the submission"
    ))
}

/// Replace each string of `value` that is a key of `renames` by its value.
fn rename_urls(value: &mut Value, renames: &BTreeMap<String, String>) {
    match value {
        Value::String(text) => {
            if let Some(renamed) = renames.get(text.as_str()) {
                *text = renamed.clone();
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| rename_urls(item, renames)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|field| rename_urls(field, renames)),
        _ => {}
    }
}

/// Whether the entry was a conditional create (`ifNoneExist`) that found the request already
/// stored, so that it is not a new one: the transaction answers `200`, and a create answers `201`.
fn matched_by_condition(entry: &Value, stored: &Value, index: usize) -> bool {
    entry.pointer("/request/ifNoneExist").is_some()
        && stored
            .pointer(&format!("/entry/{index}/response/status"))
            .and_then(Value::as_str)
            == Some("200 OK")
}

/// The response a request that was submitted before already has, if it has one. A request sent
/// again (a client that did not see the answer, and tried again with `ifNoneExist`) is answered with
/// that, and not with a second response for an adjudicator to decide.
fn answer_to(
    tx: &mut Transaction,
    catalog: &Catalog,
    kind: &Kind,
    response_model: &crate::catalog::ResourceModel,
    request: &Value,
) -> Option<Value> {
    let id = request.get("id").and_then(Value::as_str)?;
    let params = BTreeMap::from([
        ("request".to_string(), format!("{}/{id}", kind.request)),
        ("_count".to_string(), "1".to_string()),
    ]);
    let found = crate::search::search(tx, catalog, response_model, &params).ok()?;
    found.pointer("/entry/0/resource").cloned()
}

/// The response that says `request`, as it was stored, is waiting to be decided.
fn queued(kind: &Kind, request: &Value, created: &str) -> Value {
    let mut response = json!({
        "resourceType": kind.response,
        "status": "active",
        "created": created,
        "outcome": "queued",
        "disposition": "Received, and waiting to be decided",
    });
    for (to, from) in kind.copied {
        if let Some(value) = request.get(*from) {
            response[*to] = value.clone();
        }
    }
    if let Some(id) = request.get("id").and_then(Value::as_str) {
        response["request"] = json!({"reference": format!("{}/{id}", kind.request)});
    }
    response
}
