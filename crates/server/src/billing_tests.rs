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

/// The people and the policy the tests name, written once: tests run side by side, and two that
/// wrote the same resource at the same moment would conflict.
async fn seed() {
    static SEEDED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    SEEDED
        .get_or_init(|| async {
            for resource in [
                json!({"resourceType": "Patient", "id": "billed"}),
                json!({"resourceType": "Organization", "id": "insurer", "name": "An insurer"}),
                json!({"resourceType": "Organization", "id": "clinic", "name": "A clinic"}),
                json!({"resourceType": "Coverage", "id": "policy", "status": "active",
                    "beneficiary": {"reference": "Patient/billed"},
                    "payor": [{"reference": "Organization/insurer"}]}),
            ] {
                let reply = put(resource).await;
                assert!(reply.status.is_success(), "{}", reply.body);
            }
        })
        .await;
}

#[tokio::test]
async fn a_charge_item_or_invoice_that_names_nothing_that_exists_is_refused() {
    seed().await;
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
    seed().await;
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
    seed().await;
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

// `$submit`: a request is stored, and a response that says it is waiting comes back.

/// A Claim as FHIR R4 requires one: `use` says whether it is a claim, a pre-authorisation or a
/// pre-determination.
fn claim(use_: &str) -> Value {
    json!({
        "resourceType": "Claim", "status": "active",
        "type": {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/claim-type", "code": "institutional"}]},
        "use": use_, "created": "2026-10-19",
        "patient": {"reference": "Patient/billed"},
        "insurer": {"reference": "Organization/insurer"},
        "provider": {"reference": "Organization/clinic"},
        "priority": {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/processpriority", "code": "normal"}]},
        "insurance": [{"sequence": 1, "focal": true, "coverage": {"reference": "Coverage/policy"}}]
    })
}

fn parameters(resource: Value) -> Value {
    json!({"resourceType": "Parameters", "parameter": [{"name": "resource", "resource": resource}]})
}

async fn post(uri: &str, body: Value) -> crate::tests::Reply {
    send(&billing(), "POST", uri, Some(body), None).await
}

async fn get(uri: &str) -> crate::tests::Reply {
    send(&billing(), "GET", uri, None, None).await
}

#[tokio::test]
async fn a_submitted_claim_is_stored_and_answered_with_a_queued_claim_response() {
    seed().await;
    let reply = post("/Claim/$submit", parameters(claim("claim"))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let response = &reply.body;
    assert_eq!(response["resourceType"], "ClaimResponse");
    assert_eq!(response["outcome"], "queued");
    assert_eq!(response["status"], "active");
    let id = response["id"].as_str().unwrap();
    assert!(response["meta"]["versionId"].is_string(), "{response}");
    // What the response repeats of the request.
    assert_eq!(response["use"], "claim");
    assert_eq!(response["patient"]["reference"], "Patient/billed");
    assert_eq!(response["insurer"]["reference"], "Organization/insurer");
    assert_eq!(response["requestor"]["reference"], "Organization/clinic");
    assert_eq!(response["type"], claim("claim")["type"]);
    // `request` names the Claim the server stored, and that Claim is there.
    let request = response["request"]["reference"].as_str().unwrap();
    let stored = get(&format!("/{request}")).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    assert_eq!(stored.body["resourceType"], "Claim");
    assert_eq!(stored.body["patient"]["reference"], "Patient/billed");
    // So is the response, and it can be found by the request, as an adjudicator does.
    assert_eq!(
        get(&format!("/ClaimResponse/{id}")).await.status,
        StatusCode::OK
    );
    let found = get(&format!("/ClaimResponse?request={request}")).await;
    assert_eq!(found.body["total"], 1, "{}", found.body);
    assert_eq!(found.body["entry"][0]["resource"]["id"], id);
}

#[tokio::test]
async fn a_claim_may_be_the_body_and_a_pre_authorisation_says_so_in_its_response() {
    seed().await;
    // R4 allows the resource as the body when it is the only input.
    let reply = post("/Claim/$submit", claim("preauthorization")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["resourceType"], "ClaimResponse");
    assert_eq!(reply.body["use"], "preauthorization");
    assert_eq!(reply.body["outcome"], "queued");
    let predetermination = post("/Claim/$submit", parameters(claim("predetermination"))).await;
    assert_eq!(predetermination.body["use"], "predetermination");
}

#[tokio::test]
async fn a_bundle_of_claims_is_a_bundle_of_responses_and_what_it_names_is_stored() {
    seed().await;
    let visitor = "urn:uuid:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let mut second = claim("preauthorization");
    second["patient"] = json!({"reference": visitor});
    let bundle = json!({
        "resourceType": "Bundle", "type": "collection",
        "entry": [
            // A Claim on its own.
            {"resource": claim("claim")},
            // A Bundle that holds one Claim and the Patient it names, which is new.
            {"resource": {"resourceType": "Bundle", "type": "collection", "entry": [
                {"fullUrl": visitor, "resource": {"resourceType": "Patient", "name": [{"family": "Visitor"}]}},
                {"resource": second}
            ]}}
        ]
    });
    let reply = post("/Claim/$submit", parameters(bundle)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["resourceType"], "Bundle");
    assert_eq!(reply.body["type"], "collection");
    let entries = reply.body["entry"].as_array().unwrap();
    assert_eq!(
        entries.len(),
        2,
        "one response for each Claim, not for the Patient"
    );
    for entry in entries {
        assert_eq!(entry["resource"]["resourceType"], "ClaimResponse");
        assert_eq!(entry["resource"]["outcome"], "queued");
    }
    // The responses come in the order of the claims.
    assert_eq!(entries[0]["resource"]["use"], "claim");
    assert_eq!(entries[1]["resource"]["use"], "preauthorization");
    // The Patient was stored, and the Claim that named it by its `fullUrl` names the stored one.
    let patient = entries[1]["resource"]["patient"]["reference"]
        .as_str()
        .unwrap();
    assert!(
        patient.starts_with("Patient/") && patient != "Patient/billed",
        "{patient}"
    );
    assert_eq!(
        get(&format!("/{patient}")).await.body["name"][0]["family"],
        "Visitor"
    );
    let request = entries[1]["resource"]["request"]["reference"]
        .as_str()
        .unwrap();
    assert_eq!(
        get(&format!("/{request}")).await.body["patient"]["reference"],
        patient
    );
}

#[tokio::test]
async fn a_claim_that_is_not_valid_is_refused_with_what_is_wrong_and_nothing_is_stored() {
    seed().await;
    // Counted for a patient of its own: what other tests submit at the same time is not the
    // subject here.
    let patient = put(json!({"resourceType": "Patient", "id": "careless"})).await;
    assert!(patient.status.is_success(), "{}", patient.body);
    let before = get("/ClaimResponse?outcome=queued&patient=Patient/careless&_summary=count")
        .await
        .body["total"]
        .as_u64()
        .unwrap();
    let claims_before = get("/Claim?patient=Patient/careless&_summary=count")
        .await
        .body["total"]
        .as_u64()
        .unwrap();
    // `use` is one of claim, preauthorization and predetermination.
    let careless = || {
        let mut claim = claim("claim");
        claim["patient"] = json!({"reference": "Patient/careless"});
        claim
    };
    let mut wrong = careless();
    wrong["use"] = json!("invoice");
    let refused = post("/Claim/$submit", parameters(wrong.clone())).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(refused.body["resourceType"], "OperationOutcome");
    assert!(
        issues(&refused.body).contains("Claim.use"),
        "{}",
        refused.body
    );
    // A Claim that names a Patient that does not exist.
    let mut dangling = careless();
    dangling["patient"] = json!({"reference": "Patient/nobody"});
    let refused = post("/Claim/$submit", parameters(dangling)).await;
    assert!(!refused.status.is_success(), "{}", refused.body);
    assert!(
        issues(&refused.body).contains("Patient/nobody does not exist"),
        "{}",
        refused.body
    );
    // One that is invalid in a Bundle of valid ones refuses the whole Bundle.
    let bundle = json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"resource": careless()}, {"resource": wrong}
    ]});
    let refused = post("/Claim/$submit", parameters(bundle)).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert!(
        issues(&refused.body).contains("entry 1"),
        "{}",
        refused.body
    );
    // Things that are not claims at all.
    for body in [
        parameters(json!({"resourceType": "Patient"})),
        json!({"resourceType": "Parameters", "parameter": []}),
        json!({"resourceType": "Bundle", "type": "collection", "entry": [{"resource": {"resourceType": "Patient"}}]}),
        json!({"resourceType": "Bundle", "type": "transaction", "entry": [
            {"resource": claim("claim"), "request": {"method": "DELETE", "url": "Claim/x"}}]}),
    ] {
        let refused = post("/Claim/$submit", body.clone()).await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            refused.body
        );
    }
    // None of it was stored.
    let after = get("/ClaimResponse?outcome=queued&patient=Patient/careless&_summary=count")
        .await
        .body["total"]
        .as_u64()
        .unwrap();
    let claims_after = get("/Claim?patient=Patient/careless&_summary=count")
        .await
        .body["total"]
        .as_u64()
        .unwrap();
    assert_eq!((before, claims_before), (after, claims_after));
}

/// Delivering a Subscription is a task that lives on the runtime the server was built on, and a
/// test's runtime ends with the test, so this test builds a server of its own rather than using
/// the one the others share.
#[tokio::test]
async fn a_subscription_on_queued_responses_is_told_of_each_submitted_claim() {
    let app = server();
    for resource in [
        json!({"resourceType": "Patient", "id": "billed"}),
        json!({"resourceType": "Organization", "id": "insurer", "name": "An insurer"}),
        json!({"resourceType": "Organization", "id": "clinic", "name": "A clinic"}),
        json!({"resourceType": "Coverage", "id": "policy", "status": "active",
            "beneficiary": {"reference": "Patient/billed"},
            "payor": [{"reference": "Organization/insurer"}]}),
    ] {
        let uri = format!(
            "/{}/{}",
            resource["resourceType"].as_str().unwrap(),
            resource["id"].as_str().unwrap()
        );
        let reply = send(&app, "PUT", &uri, Some(resource), None).await;
        assert!(reply.status.is_success(), "{}", reply.body);
    }
    let (port, received) = crate::tests::hook_receiver("204 No Content");
    let subscription = send(
        &app,
        "POST",
        "/Subscription",
        Some(json!({
            "resourceType": "Subscription", "status": "requested",
            "reason": "an adjudicator waits for claims",
            "criteria": "ClaimResponse?outcome=queued",
            "channel": {"type": "rest-hook", "endpoint": format!("http://127.0.0.1:{port}/hook"),
                "payload": "application/fhir+json"}
        })),
        None,
    )
    .await;
    assert_eq!(
        subscription.status,
        StatusCode::CREATED,
        "{}",
        subscription.body
    );
    let subscription_id = subscription.body["id"].as_str().unwrap().to_string();
    let reply = send(
        &app,
        "POST",
        "/Claim/$submit",
        Some(parameters(claim("claim"))),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let id = reply.body["id"].as_str().unwrap().to_string();
    let notified = tokio::task::spawn_blocking(move || {
        while let Ok(request) = received.recv_timeout(std::time::Duration::from_secs(10)) {
            if request.contains(&id) {
                return Some(request);
            }
        }
        None
    })
    .await
    .unwrap();
    let Some(notified) = notified else {
        let state = send(
            &app,
            "GET",
            &format!("/Subscription/{subscription_id}"),
            None,
            None,
        )
        .await;
        panic!(
            "the rest hook was not called for the response: {}",
            state.body
        );
    };
    assert!(notified.contains("ClaimResponse"), "{notified}");
    assert!(notified.contains("queued"), "{notified}");
}

#[tokio::test]
async fn the_capability_statement_lists_what_can_be_submitted() {
    let metadata = get("/metadata").await;
    assert_eq!(metadata.status, StatusCode::OK);
    for (resource, definition) in [
        (
            "Claim",
            "http://hl7.org/fhir/OperationDefinition/Claim-submit",
        ),
        (
            "CoverageEligibilityRequest",
            "http://hl7.org/fhir/OperationDefinition/CoverageEligibilityRequest-submit",
        ),
    ] {
        let entry = metadata.body["rest"][0]["resource"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["type"] == resource)
            .unwrap_or_else(|| panic!("{resource}"));
        assert_eq!(
            entry["operation"],
            json!([{"name": "submit", "definition": definition}]),
            "{resource}"
        );
    }
}

// Eligibility requests are submitted the same way.

fn eligibility_request() -> Value {
    json!({
        "resourceType": "CoverageEligibilityRequest", "status": "active",
        "purpose": ["validation", "benefits"],
        "patient": {"reference": "Patient/billed"},
        "created": "2026-10-19",
        "insurer": {"reference": "Organization/insurer"},
        "provider": {"reference": "Organization/clinic"},
        "servicedDate": "2026-10-20",
        "insurance": [{"focal": true, "coverage": {"reference": "Coverage/policy"}}]
    })
}

#[tokio::test]
async fn a_submitted_eligibility_request_is_stored_and_answered_with_a_queued_response() {
    seed().await;
    let reply = post(
        "/CoverageEligibilityRequest/$submit",
        parameters(eligibility_request()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let response = &reply.body;
    assert_eq!(response["resourceType"], "CoverageEligibilityResponse");
    assert_eq!(response["outcome"], "queued");
    assert_eq!(response["status"], "active");
    assert_eq!(response["purpose"], json!(["validation", "benefits"]));
    assert_eq!(response["patient"]["reference"], "Patient/billed");
    assert_eq!(response["insurer"]["reference"], "Organization/insurer");
    assert_eq!(response["requestor"]["reference"], "Organization/clinic");
    assert_eq!(response["servicedDate"], "2026-10-20");
    let request = response["request"]["reference"].as_str().unwrap();
    assert!(
        request.starts_with("CoverageEligibilityRequest/"),
        "{request}"
    );
    let stored = get(&format!("/{request}")).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    assert_eq!(stored.body["purpose"], json!(["validation", "benefits"]));
    let found = get(&format!("/CoverageEligibilityResponse?request={request}")).await;
    assert_eq!(found.body["total"], 1, "{}", found.body);

    // A Bundle of them, and one that is not valid.
    let bundle = json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"resource": eligibility_request()}, {"resource": eligibility_request()}]});
    let reply = post("/CoverageEligibilityRequest/$submit", parameters(bundle)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["entry"].as_array().unwrap().len(), 2);
    let mut wrong = eligibility_request();
    wrong["purpose"] = json!(["gossip"]);
    let refused = post("/CoverageEligibilityRequest/$submit", parameters(wrong)).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert!(
        issues(&refused.body).contains("purpose"),
        "{}",
        refused.body
    );
    // A Claim is not an eligibility request.
    let refused = post(
        "/CoverageEligibilityRequest/$submit",
        parameters(claim("claim")),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
}

// Who may submit: the token needs to be able to write everything a submission writes.

/// A server that asks for a token, with one for each case, and the people the claims name.
fn guarded() -> axum::Router {
    static SERVER: OnceLock<axum::Router> = OnceLock::new();
    SERVER
        .get_or_init(|| {
            let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
            let db = Arc::new(Database::open(store.clone()).unwrap());
            let catalog = Arc::new(RwLock::new(Catalog::empty()));
            install_package_dir(store.as_ref(), &db, &catalog, r4_dir()).unwrap();
            for resource in [
                json!({"resourceType": "Patient", "id": "billed"}),
                json!({"resourceType": "Organization", "id": "insurer", "name": "An insurer"}),
                json!({"resourceType": "Organization", "id": "clinic", "name": "A clinic"}),
                json!({"resourceType": "Coverage", "id": "policy", "status": "active",
                    "beneficiary": {"reference": "Patient/billed"},
                    "payor": [{"reference": "Organization/insurer"}]}),
            ] {
                refhir_fhir::import_resource(&db, &catalog.read().unwrap(), &resource).unwrap();
            }
            let smart = Arc::new(crate::auth::SmartStore::with_defaults());
            smart.install_token(
                "biller",
                &[
                    "user/Claim.write",
                    "user/ClaimResponse.write",
                    "user/Patient.write",
                ],
                None,
            );
            smart.install_token("no-response", &["user/Claim.write"], None);
            smart.install_token("reader", &["user/*.read"], None);
            smart.install_token(
                "patient",
                &["patient/*.read", "patient/*.write"],
                Some("billed"),
            );
            crate::app_with(
                db,
                catalog,
                store,
                crate::ServerConfig {
                    auth: true,
                    smart: Some(smart),
                    ..Default::default()
                },
            )
        })
        .clone()
}

async fn submit_as(token: &str, resource_type: &str, body: Value) -> (StatusCode, Value) {
    let (status, text) = crate::tests::send_text(
        &guarded(),
        "POST",
        &format!("/{resource_type}/$submit"),
        &body.to_string(),
        &[
            ("authorization", &format!("Bearer {token}")),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await;
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_token_submits_only_what_it_may_write() {
    // It may write claims, responses and patients.
    let (status, body) = submit_as("biller", "Claim", parameters(claim("claim"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], "queued");
    // The Patient the Bundle brings is a write too: allowed to this token, refused to one without.
    let with_patient = json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"resource": {"resourceType": "Patient"}}, {"resource": claim("claim")}]});
    let (status, body) = submit_as("biller", "Claim", parameters(with_patient.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A token that may write claims but not the responses a submission makes is refused, and
    // nothing is stored for it.
    let (status, body) = submit_as("no-response", "Claim", parameters(claim("claim"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(issues(&body).contains("ClaimResponse"), "{body}");
    let (status, body) = submit_as("no-response", "Claim", parameters(with_patient)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // One that may only read, and one that is limited to a patient, cannot submit at all.
    for token in ["reader", "patient"] {
        let (status, body) = submit_as(token, "Claim", parameters(claim("claim"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{token}: {body}");
    }
    let (status, _) = submit_as("nobody", "Claim", parameters(claim("claim"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Only the two submissions by `biller` left anything behind.
    let (status, text) = crate::tests::send_text(
        &guarded(),
        "GET",
        "/Claim?_summary=count",
        "",
        &[("authorization", "Bearer reader")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let total: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(total["total"], 2, "{text}");
}
