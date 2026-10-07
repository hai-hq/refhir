//! Work that runs after `publish_commit`: Subscription delivery, bulk export, remote terminology.
//!
//! Convex analogue is a scheduled job after the commit, not another transaction engine.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use refhir_database::Database;
use refhir_fhir::{Catalog, ConformanceStore};
use refhir_persistence::TimestampRange;
use refhir_types::{Order, Timestamp};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use crate::http::{fhir_json_public, AppState, ServerConfig};

struct ExportJob {
    id: String,
    status: String,
    output: String,
    error: Option<String>,
}

fn jobs() -> &'static Mutex<BTreeMap<String, ExportJob>> {
    static JOBS: std::sync::OnceLock<Mutex<BTreeMap<String, ExportJob>>> =
        std::sync::OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn kick() -> &'static Mutex<Option<UnboundedSender<Timestamp>>> {
    static KICK: std::sync::OnceLock<Mutex<Option<UnboundedSender<Timestamp>>>> =
        std::sync::OnceLock::new();
    KICK.get_or_init(|| Mutex::new(None))
}

pub fn attach(
    db: Arc<Database>,
    catalog: Arc<std::sync::RwLock<Catalog>>,
    store: Arc<dyn ConformanceStore>,
    config: &ServerConfig,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    db.on_commit(tx.clone());
    *kick().lock().expect("kick") = Some(tx);
    let urls = config.terminology_urls.clone();
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let pending = Arc::new(Mutex::new(Vec::<PendingHook>::new()));
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                Some(ts) = rx.recv() => {
                    let db = db.clone();
                    let catalog = catalog.clone();
                    let store = store.clone();
                    let urls = urls.clone();
                    let pending = pending.clone();
                    tokio::task::spawn_blocking(move || {
                        drain_hooks(&pending);
                        deliver_subscriptions(&db, &catalog, ts, &pending);
                        if !urls.is_empty() {
                            fetch_terminology(store.as_ref(), &db, &catalog, &urls);
                        }
                    })
                    .await
                    .ok();
                }
                _ = ticker.tick() => {
                    let pending = pending.clone();
                    tokio::task::spawn_blocking(move || drain_hooks(&pending)).await.ok();
                }
            }
        }
    });
}

pub async fn accept_export(
    app: &AppState,
    type_name: Option<String>,
    id: Option<String>,
    body: &[u8],
) -> Response {
    let _ = body;
    let job_id = Uuid::new_v4().to_string();
    let dir = PathBuf::from(&app.config.export_dir);
    let _ = fs::create_dir_all(&dir);
    let output = dir.join(format!("{job_id}.ndjson"));
    jobs().lock().expect("jobs").insert(
        job_id.clone(),
        ExportJob {
            id: job_id.clone(),
            status: "accepted".into(),
            output: output.display().to_string(),
            error: None,
        },
    );
    let db = app.db.clone();
    let catalog = app.catalog.clone();
    let type_name = type_name.clone();
    let id = id.clone();
    let path = output.clone();
    let id_for_job = job_id.clone();
    tokio::task::spawn_blocking(move || {
        let result = write_export(&db, &catalog, type_name.as_deref(), id.as_deref(), &path);
        let mut table = jobs().lock().expect("jobs");
        if let Some(job) = table.get_mut(&id_for_job) {
            match result {
                Ok(()) => job.status = "completed".into(),
                Err(message) => {
                    job.status = "error".into();
                    job.error = Some(message);
                }
            }
        }
    });
    let mut response = fhir_json_public(
        StatusCode::ACCEPTED,
        json!({
            "resourceType": "OperationOutcome",
            "issue": [{"severity": "information", "code": "informational", "diagnostics": "export accepted"}]
        }),
    );
    if let Ok(location) = format!("/export/{job_id}").parse() {
        response
            .headers_mut()
            .insert(header::CONTENT_LOCATION, location);
    }
    response
}

pub fn status(app: &AppState, job: &str) -> Response {
    let _ = app;
    let table = jobs().lock().expect("jobs");
    let Some(job) = table.get(job) else {
        return fhir_json_public(
            StatusCode::NOT_FOUND,
            json!({"resourceType": "OperationOutcome", "issue": [{"severity": "error", "code": "not-found", "diagnostics": "export job was not found"}]}),
        );
    };
    (
        StatusCode::OK,
        Json(json!({"id": job.id, "status": job.status, "error": job.error})),
    )
        .into_response()
}

pub fn file(app: &AppState, job: &str) -> Response {
    let _ = app;
    let table = jobs().lock().expect("jobs");
    let Some(job) = table.get(job) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match fs::read_to_string(&job.output) {
        Ok(text) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/fhir+ndjson")],
            text,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn write_export(
    db: &Database,
    catalog: &std::sync::RwLock<Catalog>,
    type_name: Option<&str>,
    id: Option<&str>,
    path: &PathBuf,
) -> Result<(), String> {
    let snapshot = db.latest_ts();
    let end = snapshot
        .succ()
        .map_err(|_| "timestamp overflow".to_string())?;
    let rows = db
        .reader()
        .load_documents(
            TimestampRange {
                start_inclusive: Timestamp::ZERO,
                end_exclusive: end,
            },
            Order::Asc,
        )
        .map_err(|err| err.to_string())?;
    let catalog = catalog.read().expect("catalog");
    let mut seen = BTreeMap::new();
    for row in rows {
        let key = (row.id.tablet.as_str().to_string(), row.id.clone());
        if row.value.is_none() {
            seen.remove(&key);
            continue;
        }
        seen.insert(key, row.value.clone());
    }
    let members = group_members(&seen, type_name, id);
    let mut file = fs::File::create(path).map_err(|err| err.to_string())?;
    for ((tablet, doc_id), value) in &seen {
        let Some(text) = value.as_deref() else {
            continue;
        };
        if !export_keeps(
            &catalog,
            tablet,
            doc_id.id_str().unwrap_or(""),
            text,
            type_name,
            id,
            &members,
        ) {
            continue;
        }
        writeln!(file, "{text}").map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn group_members(
    seen: &BTreeMap<(String, refhir_types::DocumentId), Option<String>>,
    type_name: Option<&str>,
    id: Option<&str>,
) -> Vec<String> {
    if type_name != Some("Group") {
        return Vec::new();
    }
    let Some(id) = id else {
        return Vec::new();
    };
    let Some(text) = seen.iter().find_map(|((tablet, doc_id), value)| {
        if tablet == "Group" && doc_id.id_str() == Some(id) {
            value.as_deref()
        } else {
            None
        }
    }) else {
        return Vec::new();
    };
    let Ok(group) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let mut patients = Vec::new();
    let Some(members) = group.get("member").and_then(|value| value.as_array()) else {
        return patients;
    };
    for member in members {
        let Some(reference) = member
            .pointer("/entity/reference")
            .and_then(|value| value.as_str())
        else {
            continue;
        };
        if let Some(patient) = reference
            .rsplit_once('/')
            .filter(|(type_name, _)| *type_name == "Patient" || type_name.ends_with("/Patient"))
            .map(|(_, id)| id.to_string())
        {
            patients.push(patient);
        }
    }
    patients
}

fn export_keeps(
    catalog: &Catalog,
    tablet: &str,
    id: &str,
    text: &str,
    type_name: Option<&str>,
    focus: Option<&str>,
    group_patients: &[String],
) -> bool {
    if catalog.sdc_archiver() && tablet == "QuestionnaireResponse" {
        return false;
    }
    let Some(type_name) = type_name else {
        return true;
    };
    if type_name == "Group" {
        if focus.is_none() {
            return tablet == "Group";
        }
        if tablet == "Group" && Some(id) == focus {
            return true;
        }
        return group_patients
            .iter()
            .any(|patient| patient_export(catalog, tablet, id, text, patient));
    }
    if type_name == "Patient" {
        let Some(patient) = focus else {
            return tablet == "Patient" || in_patient_compartment(catalog, tablet);
        };
        return patient_export(catalog, tablet, id, text, patient);
    }
    tablet == type_name
}

fn patient_export(catalog: &Catalog, tablet: &str, id: &str, text: &str, patient: &str) -> bool {
    if tablet == "Patient" {
        return id == patient;
    }
    if !in_patient_compartment(catalog, tablet) {
        return false;
    }
    let Ok(resource) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    let Some(compartment) = catalog.compartment("Patient") else {
        return false;
    };
    let Some(params) = compartment.params_for(tablet) else {
        return false;
    };
    params
        .iter()
        .any(|name| field_references(resource.get(name), patient))
}

fn in_patient_compartment(catalog: &Catalog, tablet: &str) -> bool {
    catalog
        .compartment("Patient")
        .is_some_and(|compartment| compartment.params_for(tablet).is_some())
}

fn field_references(value: Option<&Value>, patient: &str) -> bool {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .any(|item| field_references(Some(item), patient)),
        Some(Value::Object(map)) => map
            .get("reference")
            .and_then(|item| item.as_str())
            .is_some_and(|reference| {
                reference == format!("Patient/{patient}")
                    || reference.ends_with(&format!("/Patient/{patient}"))
            }),
        _ => false,
    }
}

fn deliver_subscriptions(
    db: &Database,
    catalog: &std::sync::RwLock<Catalog>,
    ts: Timestamp,
    pending: &Mutex<Vec<PendingHook>>,
) {
    let catalog = catalog.read().expect("catalog");
    let Ok(_model) = catalog.get("Subscription") else {
        return;
    };
    let mut tx = db.begin();
    let Ok(bundle) = refhir_fhir::dispatch(&mut tx, &catalog, "Subscription/search", &json!({}))
    else {
        return;
    };
    let entries = bundle
        .get("entry")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    drop(tx);
    let end = ts.succ().unwrap_or(Timestamp::MAX_PERSISTED);
    let Ok(rows) = db.reader().load_documents(
        TimestampRange {
            start_inclusive: ts,
            end_exclusive: end,
        },
        Order::Asc,
    ) else {
        return;
    };
    for entry in entries {
        let Some(subscription) = entry.get("resource") else {
            continue;
        };
        if subscription.get("status").and_then(|value| value.as_str()) == Some("off") {
            continue;
        }
        let criteria = subscription
            .get("criteria")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let channel = subscription
            .pointer("/channel/type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let endpoint = subscription
            .pointer("/channel/endpoint")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let Some((type_name, query)) = criteria.split_once('?').or(Some((criteria, ""))) else {
            continue;
        };
        for row in &rows {
            if row.id.tablet.as_str() != type_name {
                continue;
            }
            let Some(text) = row.value.as_deref() else {
                continue;
            };
            let Ok(_resource) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            if !criteria_matches(
                &catalog,
                db,
                type_name,
                query,
                row.id.id_str().unwrap_or(""),
            ) {
                continue;
            }
            if channel == "rest-hook" && !endpoint.is_empty() {
                post_hook(pending, endpoint, text);
            }
            if channel == "websocket" {
                let id = subscription
                    .get("id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                notify_socket(id, text);
            }
        }
    }
}

fn criteria_matches(
    catalog: &Catalog,
    db: &Database,
    type_name: &str,
    query: &str,
    id: &str,
) -> bool {
    if query.is_empty() {
        return true;
    }
    if catalog.get(type_name).is_err() {
        return false;
    }
    let mut args = serde_json::Map::new();
    for (key, value) in jobs_query(query) {
        args.insert(key, Value::String(value));
    }
    let mut tx = db.begin();
    let Ok(bundle) = refhir_fhir::dispatch(
        &mut tx,
        catalog,
        &format!("{type_name}/search"),
        &Value::Object(args),
    ) else {
        return false;
    };
    bundle
        .get("entry")
        .and_then(|value| value.as_array())
        .is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry
                    .pointer("/resource/id")
                    .and_then(|value| value.as_str())
                    == Some(id)
            })
        })
}

fn socket_senders() -> &'static Mutex<BTreeMap<String, UnboundedSender<String>>> {
    static SOCKETS: std::sync::OnceLock<Mutex<BTreeMap<String, UnboundedSender<String>>>> =
        std::sync::OnceLock::new();
    SOCKETS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub fn register_socket(id: String) -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    socket_senders().lock().expect("sockets").insert(id, tx);
    rx
}

fn notify_socket(id: &str, payload: &str) {
    if let Some(sender) = socket_senders().lock().expect("sockets").get(id) {
        let _ = sender.send(payload.to_string());
    }
}

pub(crate) fn jobs_query(query: &str) -> BTreeMap<String, String> {
    let mut params = BTreeMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(key.to_string(), value.to_string());
    }
    params
}

fn fetch_terminology(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &std::sync::RwLock<Catalog>,
    urls: &[String],
) {
    for url in urls {
        let Ok(body) = http_get(url) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&body) else {
            continue;
        };
        let Some(resource_url) = value.get("url").and_then(|item| item.as_str()) else {
            continue;
        };
        let Some(resource_type) = value.get("resourceType").and_then(|item| item.as_str()) else {
            continue;
        };
        let Ok(package_id) = store.ensure_local_package("4.0.1") else {
            continue;
        };
        let row = refhir_persistence::NewTerminology {
            resource_type: resource_type.to_string(),
            url: resource_url.to_string(),
            json: value.to_string(),
        };
        if store.upsert_terminology(package_id, &row).is_err() {
            continue;
        }
        if refhir_fhir::repair_conformance(store, db, catalog).is_err() {
            continue;
        }
    }
}

struct PendingHook {
    endpoint: String,
    body: String,
    attempts: u8,
}

fn post_hook(pending: &Mutex<Vec<PendingHook>>, endpoint: &str, body: &str) {
    for attempt in 1u8..=3 {
        if http_post(endpoint, body).is_ok() {
            return;
        }
        if attempt < 3 {
            std::thread::sleep(Duration::from_millis(40 * u64::from(attempt)));
        }
    }
    pending.lock().expect("hooks").push(PendingHook {
        endpoint: endpoint.to_string(),
        body: body.to_string(),
        attempts: 3,
    });
}

fn drain_hooks(pending: &Mutex<Vec<PendingHook>>) {
    let batch = std::mem::take(&mut *pending.lock().expect("hooks"));
    for mut hook in batch {
        hook.attempts = hook.attempts.saturating_add(1);
        if http_post(&hook.endpoint, &hook.body).is_err() && hook.attempts < 8 {
            pending.lock().expect("hooks").push(hook);
        }
    }
}

fn http_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(5))
            .build()
    })
}

fn http_post(url: &str, body: &str) -> Result<(), String> {
    let response = http_agent()
        .post(url)
        .set("Content-Type", "application/fhir+json")
        .send_string(body)
        .map_err(|err| err.to_string())?;
    if (200..300).contains(&response.status()) {
        Ok(())
    } else {
        Err(format!("hook status {}", response.status()))
    }
}

fn http_get(url: &str) -> Result<String, String> {
    http_agent()
        .get(url)
        .call()
        .map_err(|err| err.to_string())?
        .into_string()
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::http_post;

    #[test]
    fn https_endpoints_are_attempted() {
        let err = http_post("https://127.0.0.1:1/hook", "{}").unwrap_err();
        assert!(!err.contains("only http"), "{err}");
    }
}
