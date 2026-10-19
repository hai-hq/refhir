//! Run a named operation with OCC retry.
//!
//! Handlers only touch `Transaction`, so retrying them after an OCC conflict is safe.

use std::cell::Cell;
use std::collections::BTreeMap;

use refhir_database::{CommitError, Database, DocumentWrite, ReadSet};
use refhir_types::Timestamp;
use refhir_validate::validate;
use serde_json::Value;

use crate::bundle;
use crate::catalog::Catalog;
use crate::error::FhirError;
use crate::history;
use crate::ops;
use crate::resolve::TxHost;
use crate::search;

/// Arguments the server puts beside a caller's own to say what it knows about the caller: the
/// scopes of the token (`_readScopes`, read by the SDC operations and by stores that must not
/// read what the token may not), whether it is an archiver, and the compartment a patient-scoped
/// token is limited to (`_patientCompartment`, read by every search), and how a search is to treat a
/// value it cannot read (`_handling`, from the `Prefer` header of a `GET`). They are not the
/// client's, so a search does not take them for search parameters, and the code that adds them (and
/// the code that makes sure a client did not) uses this same list.
pub const CONTROL_ARGS: [&str; 5] = [
    "_readScopes",
    "_sdcArchiver",
    search::PATIENT_COMPARTMENT,
    search::HANDLING,
    search::DEFAULT_COUNT,
];

/// Retries after an OCC conflict. The first attempt is not counted.
pub const MAX_OCC_FAILURES: usize = 4;

const ENGINE: &str = "refhir_engine";

#[derive(Debug)]
pub enum RunError {
    Fhir(FhirError),
    Commit(CommitError),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Fhir(err) => write!(f, "{err}"),
            RunError::Commit(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for RunError {}

#[derive(Debug)]
pub struct RunResult {
    pub value: Value,
    pub ts: Timestamp,
    pub reads: ReadSet,
    pub wrote: bool,
    /// Document creates, updates, and deletes from the committed transaction.
    pub writes: Vec<DocumentWrite>,
    /// HTTP status when the handler chooses one. `0` means the route default.
    pub status: u16,
}

thread_local! {
    static STATUS: Cell<u16> = const { Cell::new(0) };
}

fn set_status(status: u16) {
    STATUS.set(status);
}

fn take_status() -> u16 {
    STATUS.replace(0)
}

pub fn run(
    db: &Database,
    catalog: &Catalog,
    path: &str,
    args: &Value,
) -> Result<RunResult, RunError> {
    if path == "Bundle/batch" {
        return bundle::batch(db, catalog, args);
    }
    run_in_transaction(db, path, |tx| dispatch(tx, catalog, path, args))
}

/// Load a resource under the id it names, for code that embeds the server and has data with ids
/// of its own: fixtures, a restore, a migration.
///
/// This is not an interaction. The `create` a client can reach assigns the id whatever the body
/// says, and no operation name leads here, so `/sync`, GraphQL and REST cannot. The resource is
/// validated and indexed like any other; an id already in use is a conflict.
pub fn import_resource(
    db: &Database,
    catalog: &Catalog,
    resource: &Value,
) -> Result<RunResult, RunError> {
    let type_name = resource
        .get("resourceType")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let path = format!("{type_name}/import");
    run_in_transaction(db, &path, |tx| {
        let model = catalog.get(type_name)?;
        let value = ops::create_with(tx, catalog, model, resource.clone(), ops::IdPolicy::Keep)?;
        set_status(201);
        Ok(value)
    })
}

fn run_in_transaction(
    db: &Database,
    path: &str,
    mut body: impl FnMut(&mut refhir_database::Transaction) -> Result<Value, FhirError>,
) -> Result<RunResult, RunError> {
    for attempt in 0..=MAX_OCC_FAILURES {
        let mut tx = db.begin();
        let snapshot_ts = tx.begin_timestamp();
        tracing::debug!(
            target: ENGINE,
            operation = path,
            attempt,
            snapshot_ts = snapshot_ts.as_u64(),
            "operation started"
        );
        set_status(0);
        // A definition or code system that could not be read from the store was answered as if it
        // were not there. That is not an answer to give, or to commit a write on: the request
        // fails, as any other failure of the store does.
        let mut value = match refhir_validate::watching_reads(|| body(&mut tx)) {
            Ok(Ok(value)) => value,
            Ok(Err(err)) => return Err(RunError::Fhir(err)),
            Err(failure) => return Err(RunError::Fhir(FhirError::Storage(failure))),
        };
        let status = take_status();
        let reads = tx.read_set().clone();
        let written = tx.written_ids();
        let wrote = tx.has_writes();
        let writes = tx.document_writes();
        tracing::debug!(
            target: ENGINE,
            operation = path,
            attempt,
            wrote,
            reads = reads.summary(),
            documents_written = writes.len(),
            "operation dispatched, committing"
        );
        match db.commit_with(tx, stamp_stored_json) {
            Ok(ts) => {
                if wrote {
                    bundle::finalize(&mut value, &written, ts);
                }
                tracing::debug!(
                    target: ENGINE,
                    operation = path,
                    attempt,
                    commit_ts = ts.as_u64(),
                    wrote,
                    reads = reads.summary(),
                    "operation committed"
                );
                return Ok(RunResult {
                    value,
                    ts,
                    reads,
                    wrote,
                    writes,
                    status,
                });
            }
            Err(CommitError::Occ(conflict)) if wrote && attempt < MAX_OCC_FAILURES => {
                tracing::debug!(
                    target: ENGINE,
                    operation = path,
                    attempt,
                    next_attempt = attempt + 1,
                    index = conflict.index_id.as_str(),
                    write_ts = conflict.write_ts.as_u64(),
                    "occ conflict, retrying operation"
                );
                continue;
            }
            Err(err) => return Err(RunError::Commit(err)),
        }
    }
    unreachable!("OCC retry loop returns on every attempt");
}

pub fn stamp_stored_json(json: &mut String, ts: Timestamp) {
    let Ok(mut value) = serde_json::from_str::<Value>(json) else {
        return;
    };
    if value.get("resourceType").is_none() {
        return;
    }
    ops::set_version(&mut value, ts);
    if let Ok(text) = serde_json::to_string(&value) {
        *json = text;
    }
}

pub fn dispatch(
    tx: &mut refhir_database::Transaction,
    catalog: &Catalog,
    path: &str,
    args: &Value,
) -> Result<Value, FhirError> {
    if path == "Bundle/transaction" {
        let bundle = args
            .get("resource")
            .cloned()
            .unwrap_or_else(|| args.clone());
        return bundle::transaction(tx, catalog, &bundle, args);
    }
    let (type_name, op) = path
        .split_once('/')
        .ok_or_else(|| FhirError::Invalid(format!("bad operation {path}")))?;
    if matches!(
        op,
        "$expand" | "$expandability" | "$validate-code" | "$lookup" | "$translate" | "$subsumes"
    ) {
        return crate::terms::operation(catalog, type_name, op, args);
    }
    if path == "system/history" {
        return history::system_history(
            tx,
            catalog,
            optional_ts(args, "_since")?,
            optional_usize(args, "_count")?,
            optional_usize(args, "_offset")?.unwrap_or(0),
        );
    }
    let model = catalog.get(type_name)?;
    match op {
        "create" => {
            let resource = crate::sdc::prepare_store(tx, catalog, args, resource_arg(args)?)?;
            let value = ops::create(tx, catalog, model, resource)?;
            set_status(201);
            Ok(value)
        }
        "read" => {
            catalog.deny_response_read(type_name)?;
            ops::read(tx, model, &id_arg(args)?)
        }
        "update" => {
            let resource = crate::sdc::prepare_store(tx, catalog, args, resource_arg(args)?)?;
            let (value, status) = ops::update_status(
                tx,
                catalog,
                model,
                &id_arg(args)?,
                resource,
                args.get("ifMatch").and_then(|value| value.as_str()),
            )?;
            if status == 201 {
                set_status(201);
            }
            Ok(value)
        }
        "delete" => ops::delete(tx, catalog, model, &id_arg(args)?),
        "patch" => {
            catalog.deny_response_read(type_name)?;
            let id = id_arg(args)?;
            let patch = args
                .get("patch")
                .cloned()
                .ok_or_else(|| FhirError::Invalid("patch is required".into()))?;
            let mut resource = ops::read(tx, model, &id)?;
            crate::patch::apply(&mut resource, &patch)?;
            let resource = crate::sdc::prepare_store(tx, catalog, args, resource)?;
            ops::update(
                tx,
                catalog,
                model,
                &id,
                resource,
                args.get("ifMatch").and_then(|value| value.as_str()),
            )
        }
        "conditional-create" => {
            let resource = crate::sdc::prepare_store(tx, catalog, args, resource_arg(args)?)?;
            let (value, status) = ops::conditional_create(
                tx,
                catalog,
                model,
                resource,
                args.get("ifNoneExist")
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
            )?;
            set_status(status);
            Ok(value)
        }
        "conditional-update" => {
            let resource = crate::sdc::prepare_store(tx, catalog, args, resource_arg(args)?)?;
            let (value, status) = ops::conditional_update(
                tx,
                catalog,
                model,
                resource,
                args.get("query")
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
            )?;
            set_status(status);
            Ok(value)
        }
        "conditional-delete" => ops::conditional_delete(
            tx,
            catalog,
            model,
            args.get("query")
                .and_then(|value| value.as_str())
                .unwrap_or(""),
        ),
        "vread" => {
            catalog.deny_response_read(type_name)?;
            let version = args
                .get("versionId")
                .and_then(|value| value.as_str())
                .ok_or_else(|| FhirError::Invalid("versionId is required".into()))?;
            history::vread(tx, model, &id_arg(args)?, version)
        }
        "history" => {
            catalog.deny_response_read(type_name)?;
            if let Some(id) = args.get("id").and_then(|value| value.as_str()) {
                if !id.is_empty() {
                    return history::history(tx, model, id);
                }
            }
            history::type_history(
                tx,
                model,
                optional_ts(args, "_since")?,
                optional_usize(args, "_count")?,
                optional_usize(args, "_offset")?.unwrap_or(0),
            )
        }
        "search" => {
            let mut params = search_args(args)?;
            page_by_default(&mut params, args)?;
            search::search_handling(tx, catalog, model, &params, handling_arg(args))
        }
        "$populate" | "$assemble" | "$next-question" | "$behavior" | "$extract" => {
            crate::sdc::operation(tx, catalog, type_name, op, args)
        }
        "$validate" => {
            let resource = if args.get("resource").is_some() {
                resource_arg(args)?
            } else {
                catalog.deny_response_read(type_name)?;
                ops::read(tx, model, &id_arg(args)?)?
            };
            let profiles = profile_arg(args);
            Ok(validation_with_profile(tx, catalog, &resource, &profiles))
        }
        "$meta" => {
            catalog.deny_response_read(type_name)?;
            crate::meta::get(tx, model, &id_arg(args)?)
        }
        "$meta-add" => {
            catalog.deny_response_read(type_name)?;
            crate::meta::add(tx, catalog, model, &id_arg(args)?, &resource_arg(args)?)
        }
        "$meta-delete" => {
            catalog.deny_response_read(type_name)?;
            crate::meta::delete(tx, catalog, model, &id_arg(args)?, &resource_arg(args)?)
        }
        "$everything" => {
            catalog.deny_response_read(type_name)?;
            crate::meta::everything(tx, catalog, model, &id_arg(args)?)
        }
        "$document" => crate::meta::document(tx, catalog, model, &id_arg(args)?),
        "$submit" => crate::submit::operation(tx, catalog, type_name, args),
        "$apply" => crate::apply::operation(tx, catalog, type_name, args),
        other => Err(FhirError::Unsupported(format!(
            "unsupported operation {type_name}/{other}"
        ))),
    }
}

fn resource_arg(args: &Value) -> Result<Value, FhirError> {
    args.get("resource")
        .cloned()
        .ok_or_else(|| FhirError::Invalid("resource is required".into()))
}

fn id_arg(args: &Value) -> Result<String, FhirError> {
    args.get("id")
        .and_then(|value| value.as_str())
        .map(|id| id.to_string())
        .ok_or_else(|| FhirError::Invalid("id is required".into()))
}

fn search_args(args: &Value) -> Result<BTreeMap<String, String>, FhirError> {
    let object = args
        .as_object()
        .ok_or_else(|| FhirError::Invalid("search args must be an object".into()))?;
    let mut params = BTreeMap::new();
    for (key, value) in object {
        if CONTROL_ARGS.contains(&key.as_str()) {
            continue;
        }
        match value {
            Value::String(text) => {
                params.insert(key.clone(), text.clone());
            }
            Value::Array(_) if search::is_single_valued_control(key) => {
                return Err(FhirError::Invalid(format!(
                    "search parameter {key} takes one value, not an array"
                )));
            }
            // A JSON object has one value for a name, so a parameter given more than once, which
            // FHIR reads as AND, is an array: `"schedule.actor": ["HealthcareService/h1",
            // "Location/l1"]` is what `schedule.actor=..&schedule.actor=..` says in a URL.
            Value::Array(items) if !items.is_empty() => {
                for item in items {
                    let Some(text) = item.as_str() else {
                        return Err(FhirError::Invalid(format!(
                            "search parameter {key} must be a string or an array of strings"
                        )));
                    };
                    search::add_param(&mut params, key.clone(), text.to_string());
                }
            }
            _ => {
                return Err(FhirError::Invalid(format!(
                    "search parameter {key} must be a string or a non-empty array of strings"
                )));
            }
        }
    }
    limit_to_caller(&mut params, args)?;
    Ok(params)
}

/// How the request asked a search to treat a value it cannot read. Only the server puts
/// `_handling` in the arguments of a search, so this is not a client's say beyond the header.
fn handling_arg(args: &Value) -> search::Handling {
    match args.get(search::HANDLING).and_then(Value::as_str) {
        Some("lenient") => search::Handling::Lenient,
        _ => search::Handling::Strict,
    }
}

/// A search that names no `_count` gets the page size the server asked for (`_defaultCount`), if
/// it did. Only the server puts that argument in a search, so this is its word, not a client's.
/// The REST handlers say it (`ask_for_default_page` in the server crate); this is where it applies.
fn page_by_default(params: &mut BTreeMap<String, String>, args: &Value) -> Result<(), FhirError> {
    let Some(size) = args.get(search::DEFAULT_COUNT) else {
        return Ok(());
    };
    let size = size
        .as_str()
        .and_then(|text| text.parse::<usize>().ok())
        .ok_or_else(|| FhirError::Invalid(format!("{} must be a number", search::DEFAULT_COUNT)))?;
    if size > 0 {
        params
            .entry("_count".to_string())
            .or_insert_with(|| size.to_string());
    }
    Ok(())
}

/// Put the server's limit on the caller, if it set one, on the parameters of a search that was
/// built from a request. Whatever the request had under that name is replaced, never kept: it is
/// not the client's to set.
pub(crate) fn limit_to_caller(
    params: &mut BTreeMap<String, String>,
    controls: &Value,
) -> Result<(), FhirError> {
    params.remove(search::PATIENT_COMPARTMENT);
    match controls.get(search::PATIENT_COMPARTMENT) {
        None => Ok(()),
        Some(Value::String(compartment)) => {
            params.insert(search::PATIENT_COMPARTMENT.into(), compartment.clone());
            Ok(())
        }
        Some(_) => Err(FhirError::Invalid(format!(
            "{} must be a string",
            search::PATIENT_COMPARTMENT
        ))),
    }
}

fn profile_arg(args: &Value) -> Vec<String> {
    let mut profiles = Vec::new();
    if let Some(profile) = args.get("profile").and_then(|value| value.as_str()) {
        profiles.push(profile.to_string());
    }
    if let Some(items) = args.get("profile").and_then(|value| value.as_array()) {
        for item in items {
            if let Some(profile) = item.as_str() {
                profiles.push(profile.to_string());
            }
        }
    }
    profiles
}

fn validation_with_profile(
    tx: &mut refhir_database::Transaction,
    catalog: &Catalog,
    resource: &Value,
    profiles: &[String],
) -> Value {
    let host = TxHost::new(tx, catalog);
    validate(resource, profiles, catalog.schema(), catalog.terms(), &host).operation_outcome()
}

fn optional_ts(args: &Value, name: &str) -> Result<Option<Timestamp>, FhirError> {
    match args.get(name).and_then(|value| value.as_str()) {
        None => Ok(None),
        Some(text) => Timestamp::from_version_id(text)
            .map(Some)
            .map_err(|_| FhirError::Invalid(format!("invalid {name}"))),
    }
}

fn optional_usize(args: &Value, name: &str) -> Result<Option<usize>, FhirError> {
    match args.get(name).and_then(|value| value.as_str()) {
        None => Ok(None),
        Some(text) => text
            .parse()
            .map(Some)
            .map_err(|_| FhirError::Invalid(format!("{name} must be an integer"))),
    }
}
