//! What a billing client sends over REST: charge items, invoices, claims and their responses,
//! against a server that creates a resource on `PUT` (`--update-as-create`) and checks references
//! strictly, as a deployment that loads its own ids does.

use std::sync::{Arc, OnceLock, RwLock};

use axum::http::StatusCode;
use refhir_database::Database;
use refhir_fhir::{install_builtin_terminology, install_package_dir, r4_dir, Catalog};
use refhir_sqlite::SqlitePersistence;
use serde_json::{json, Value};

use crate::app;
use crate::tests::send;

/// One server for the module: installing the R4 package is most of the time a test would otherwise
/// take, and every test writes under ids of its own.
fn billing() -> axum::Router {
    static SERVER: OnceLock<axum::Router> = OnceLock::new();
    SERVER.get_or_init(server).clone()
}

fn server() -> axum::Router {
    let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
    let db = Arc::new(Database::open(store.clone()).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    install_package_dir(store.as_ref(), &db, &catalog, r4_dir()).unwrap();
    install_builtin_terminology(store.as_ref(), &db, &catalog).unwrap();
    catalog.write().unwrap().set_update_create(true);
    app(db, catalog, store)
}

async fn put(resource: Value) -> crate::tests::Reply {
    let uri = format!(
        "/{}/{}",
        resource["resourceType"].as_str().unwrap(),
        resource["id"].as_str().unwrap()
    );
    send(&billing(), "PUT", &uri, Some(resource), None).await
}

/// The text of every issue of an OperationOutcome, and the elements it names.
fn issues(outcome: &Value) -> String {
    outcome["issue"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|issue| {
            format!(
                "{} {}",
                issue["diagnostics"].as_str().unwrap_or(""),
                issue["expression"]
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn charge_item(id: &str, extra: Value) -> Value {
    let mut item = json!({
        "resourceType": "ChargeItem", "id": id, "status": "billable",
        "code": {"text": "consultation"}, "subject": {"reference": "Patient/billed"}
    });
    for (key, value) in extra.as_object().unwrap() {
        item[key] = value.clone();
    }
    item
}

async fn seed_patient() {
    let reply = put(json!({"resourceType": "Patient", "id": "billed"})).await;
    assert!(reply.status.is_success(), "{}", reply.body);
}

#[tokio::test]
async fn a_charge_item_or_invoice_that_names_nothing_that_exists_is_refused() {
    seed_patient().await;
    // The two requests of the report: a reference inside a choice element used to be saved
    // whatever it named.
    let invoice = put(json!({
        "resourceType": "Invoice", "id": "dangling", "status": "draft",
        "lineItem": [{"chargeItemReference": {"reference": "ChargeItem/nope"}}]
    }))
    .await;
    assert!(!invoice.status.is_success(), "{}", invoice.body);
    assert_eq!(invoice.body["resourceType"], "OperationOutcome");
    let text = issues(&invoice.body);
    assert!(
        text.contains("lineItem.chargeItemReference ChargeItem/nope does not exist"),
        "{text}"
    );
    let item = put(charge_item(
        "dangling",
        json!({"productReference": {"reference": "Medication/nope"}}),
    ))
    .await;
    assert!(!item.status.is_success(), "{}", item.body);
    let text = issues(&item.body);
    assert!(
        text.contains("productReference Medication/nope does not exist"),
        "{text}"
    );
    let gone = send(&billing(), "GET", "/Invoice/dangling", None, None).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);

    // Once the target exists, the same requests are saved.
    let medication = put(json!({"resourceType": "Medication", "id": "amoxicillin"})).await;
    assert!(medication.status.is_success(), "{}", medication.body);
    let item = put(charge_item(
        "kept",
        json!({"productReference": {"reference": "Medication/amoxicillin"}}),
    ))
    .await;
    assert_eq!(item.status, StatusCode::CREATED, "{}", item.body);
    let invoice = put(json!({
        "resourceType": "Invoice", "id": "kept", "status": "draft",
        "lineItem": [{"chargeItemReference": {"reference": "ChargeItem/kept"}}]
    }))
    .await;
    assert_eq!(invoice.status, StatusCode::CREATED, "{}", invoice.body);
}

#[tokio::test]
async fn a_transaction_names_a_charge_item_by_its_full_url() {
    seed_patient().await;
    let item = "urn:uuid:11111111-1111-4111-8111-111111111111";
    let transaction = |reference: &str| {
        json!({
            "resourceType": "Bundle", "type": "transaction",
            "entry": [
                {"request": {"method": "POST", "url": "Invoice"}, "resource": {
                    "resourceType": "Invoice", "status": "draft",
                    "lineItem": [{"chargeItemReference": {"reference": reference}}]
                }},
                {"fullUrl": item, "request": {"method": "POST", "url": "ChargeItem"},
                 "resource": charge_item("ignored", json!({}))}
            ]
        })
    };
    let missing = send(
        &billing(),
        "POST",
        "/",
        Some(transaction("urn:uuid:22222222-2222-4222-8222-222222222222")),
        None,
    )
    .await;
    assert!(!missing.status.is_success(), "{}", missing.body);
    let accepted = send(&billing(), "POST", "/", Some(transaction(item)), None).await;
    assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.body);
    let created = accepted.body["entry"][1]["resource"]["id"]
        .as_str()
        .unwrap();
    assert_eq!(
        accepted.body["entry"][0]["resource"]["lineItem"][0]["chargeItemReference"]["reference"],
        format!("ChargeItem/{created}")
    );
}

fn claim_response(id: &str, currency: &str) -> Value {
    json!({
        "resourceType": "ClaimResponse", "id": id, "status": "active",
        "type": {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/claim-type", "code": "institutional"}]},
        "use": "claim", "created": "2026-10-19",
        "patient": {"reference": "Patient/billed"}, "insurer": {"reference": "Organization/insurer"},
        "outcome": "queued",
        "item": [{"itemSequence": 1, "adjudication": [{
            "category": {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/adjudication", "code": "submitted"}]},
            "amount": {"value": 12500, "currency": currency}
        }]}]
    })
}

#[tokio::test]
async fn an_adjudication_amount_names_a_currency_that_exists() {
    seed_patient().await;
    let insurer =
        put(json!({"resourceType": "Organization", "id": "insurer", "name": "An insurer"})).await;
    assert!(insurer.status.is_success(), "{}", insurer.body);

    let naira = put(claim_response("naira", "NGN")).await;
    assert_eq!(naira.status, StatusCode::CREATED, "{}", naira.body);
    // It says nothing about the currency when asked to validate it.
    let validated = send(
        &billing(),
        "POST",
        "/ClaimResponse/$validate",
        Some(claim_response("naira", "NGN")),
        None,
    )
    .await;
    assert_eq!(validated.status, StatusCode::OK, "{}", validated.body);
    let text = issues(&validated.body);
    assert!(!text.contains("currenc"), "{text}");

    // A code that is not an ISO 4217 code is refused, and the outcome says which element.
    let refused = put(claim_response("nra", "NRA")).await;
    assert!(!refused.status.is_success(), "{}", refused.body);
    assert_eq!(refused.body["resourceType"], "OperationOutcome");
    let text = issues(&refused.body);
    assert!(
        text.contains("ClaimResponse.item[0].adjudication[0].amount.currency"),
        "{text}"
    );
    let gone = send(&billing(), "GET", "/ClaimResponse/nra", None, None).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
}
