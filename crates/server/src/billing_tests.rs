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
                json!({"resourceType": "ChargeItem", "id": "visit", "status": "billable",
                    "code": {"text": "consultation"}, "subject": {"reference": "Patient/billed"}}),
                json!({"resourceType": "ChargeItemDefinition", "id": "price", "status": "active",
                    "url": "http://example.org/cid/guarded",
                    "propertyGroup": [{"priceComponent": [
                        {"type": "base", "amount": {"value": 100, "currency": "NGN"}}]}]}),
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
            smart.install_token(
                "pricer",
                &[
                    "user/ChargeItemDefinition.read",
                    "user/ChargeItem.read",
                    "user/Account.read",
                ],
                None,
            );
            smart.install_token(
                "pricer-without-accounts",
                &["user/ChargeItemDefinition.read", "user/ChargeItem.read"],
                None,
            );
            smart.install_token(
                "definitions-only",
                &["user/ChargeItemDefinition.read"],
                None,
            );
            smart.install_token("charge-items-only", &["user/ChargeItem.read"], None);
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

// `$apply`: what a ChargeItem costs under a ChargeItemDefinition.

/// A consultation that costs 15 000 naira, with a quarter more after hours, 1 000 less for a
/// child, and 7.5 % tax on the base price. The co-payment is only shown.
fn consultation_prices(id: &str) -> Value {
    json!({
        "resourceType": "ChargeItemDefinition", "id": id, "status": "active",
        "url": format!("http://example.org/cid/{id}"),
        "applicability": [{
            "description": "only what can be billed", "language": "text/fhirpath",
            "expression": "%context.status = 'billable'"
        }],
        "propertyGroup": [
            {"priceComponent": [
                {"type": "base", "amount": {"value": 15000, "currency": "NGN"}},
                {"type": "tax", "code": {"text": "VAT"}, "factor": 0.075},
                {"type": "informational", "code": {"text": "co-payment"},
                 "amount": {"value": 2000, "currency": "NGN"}}
            ]},
            {"applicability": [{"language": "text/fhirpath",
                "expression": "%context.note.text.contains('after hours')"}],
             "priceComponent": [
                {"type": "surcharge", "code": {"text": "after hours"}, "factor": 0.25}]},
            // Under fourteen. The birth date is far enough ahead that the test stays right.
            {"applicability": [{"language": "text/fhirpath",
                "expression": "%context.subject.resolve().birthDate > today() - 14 years"}],
             "priceComponent": [
                {"type": "discount", "code": {"text": "child"},
                 "amount": {"value": 1000, "currency": "NGN"}}]}
        ]
    })
}

fn charged(id: &str, patient: &str, status: &str, note: Option<&str>) -> Value {
    let mut item = json!({
        "resourceType": "ChargeItem", "id": id, "status": status,
        "code": {"text": "consultation"}, "subject": {"reference": format!("Patient/{patient}")}
    });
    if let Some(note) = note {
        item["note"] = json!([{"text": note}]);
    }
    item
}

async fn put_all(resources: Vec<Value>) {
    for resource in resources {
        let reply = put(resource).await;
        assert!(reply.status.is_success(), "{}", reply.body);
    }
}

fn apply_body(item: &str, account: Option<&str>) -> Value {
    let mut parameter = vec![json!({"name": "chargeItem", "valueReference": {"reference": item}})];
    if let Some(account) = account {
        parameter.push(json!({"name": "account", "valueReference": {"reference": account}}));
    }
    json!({"resourceType": "Parameters", "parameter": parameter})
}

async fn apply(definition: &str, body: Value) -> crate::tests::Reply {
    post(&format!("/ChargeItemDefinition/{definition}/$apply"), body).await
}

fn component<'a>(invoice: &'a Value, kind: &str) -> &'a Value {
    invoice["lineItem"][0]["priceComponent"]
        .as_array()
        .unwrap()
        .iter()
        .find(|component| component["type"] == kind)
        .unwrap_or_else(|| panic!("no {kind} in {invoice}"))
}

#[tokio::test]
async fn a_price_is_the_base_plus_the_surcharges_less_the_discounts_that_apply() {
    put_all(vec![
        json!({"resourceType": "Patient", "id": "kid", "birthDate": "2030-01-01"}),
        json!({"resourceType": "Patient", "id": "grown", "birthDate": "1980-01-01"}),
        consultation_prices("consult"),
        charged("night-kid", "kid", "billable", Some("seen after hours")),
        charged("day-grown", "grown", "billable", None),
        charged("day-kid", "kid", "billable", None),
    ])
    .await;

    // Base 15 000, and a quarter of it more after hours, and 1 000 less for a child: 17 750. Tax
    // is 7.5 % of the base price, 1 125.
    let reply = apply("consult", apply_body("ChargeItem/night-kid", None)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let invoice = &reply.body;
    assert_eq!(invoice["resourceType"], "Invoice");
    assert_eq!(invoice["status"], "draft");
    assert_eq!(
        invoice["totalNet"],
        json!({"value": 17750, "currency": "NGN"})
    );
    assert_eq!(
        invoice["totalGross"],
        json!({"value": 18875, "currency": "NGN"})
    );
    assert_eq!(invoice["subject"]["reference"], "Patient/kid");
    assert_eq!(
        invoice["lineItem"][0]["chargeItemReference"]["reference"],
        "ChargeItem/night-kid"
    );
    // Each component is as the definition gives it, with the amount worked out for the ones
    // that were a fraction of the base price.
    assert_eq!(
        component(invoice, "base")["amount"],
        json!({"value": 15000, "currency": "NGN"})
    );
    assert_eq!(
        component(invoice, "surcharge")["amount"],
        json!({"value": 3750, "currency": "NGN"})
    );
    assert_eq!(component(invoice, "surcharge")["factor"], json!(0.25));
    assert_eq!(
        component(invoice, "surcharge")["code"]["text"],
        "after hours"
    );
    assert_eq!(
        component(invoice, "discount")["amount"],
        json!({"value": 1000, "currency": "NGN"})
    );
    assert_eq!(
        component(invoice, "tax")["amount"],
        json!({"value": 1125, "currency": "NGN"})
    );
    // What is only shown is shown, and is not part of the price.
    assert_eq!(component(invoice, "informational")["amount"]["value"], 2000);
    assert_eq!(
        invoice["lineItem"][0]["priceComponent"]
            .as_array()
            .unwrap()
            .len(),
        5
    );

    // By day, for an adult: the base price and tax, no surcharge and no discount.
    let reply = apply("consult", apply_body("ChargeItem/day-grown", None)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(
        reply.body["totalNet"],
        json!({"value": 15000, "currency": "NGN"})
    );
    assert_eq!(
        reply.body["totalGross"],
        json!({"value": 16125, "currency": "NGN"})
    );
    let kinds: Vec<&str> = reply.body["lineItem"][0]["priceComponent"]
        .as_array()
        .unwrap()
        .iter()
        .map(|component| component["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["base", "tax", "informational"]);

    // By day, for a child: only the discount applies of the two.
    let reply = apply("consult", apply_body("ChargeItem/day-kid", None)).await;
    assert_eq!(
        reply.body["totalNet"],
        json!({"value": 14000, "currency": "NGN"})
    );
    // The Invoice is the answer and is not stored.
    for patient in ["kid", "grown"] {
        let stored = get(&format!(
            "/Invoice?subject=Patient/{patient}&_summary=count"
        ))
        .await;
        assert_eq!(stored.body["total"], 0, "{patient}");
    }
}

#[tokio::test]
async fn amounts_are_added_exactly_in_the_currency_the_definition_gives() {
    put_all(vec![
        json!({"resourceType": "Patient", "id": "payer"}),
        charged("scan", "payer", "billable", None),
        json!({"resourceType": "ChargeItemDefinition", "id": "scan-usd", "status": "active",
        "url": "http://example.org/cid/scan-usd",
        "propertyGroup": [{"priceComponent": [
            {"type": "base", "amount": {"value": 10.1, "currency": "USD"}},
            {"type": "surcharge", "amount": {"value": 0.2, "currency": "USD"}},
            // The sign a discount is written with does not matter.
            {"type": "discount", "amount": {"value": -0.05, "currency": "USD"}},
            {"type": "deduction", "amount": {"value": 0.05, "currency": "USD"}}
        ]}]}),
        json!({"resourceType": "ChargeItemDefinition", "id": "scan-mixed", "status": "active",
        "url": "http://example.org/cid/scan-mixed",
        "propertyGroup": [{"priceComponent": [
            {"type": "base", "amount": {"value": 10, "currency": "USD"}},
            {"type": "surcharge", "amount": {"value": 1, "currency": "EUR"}}
        ]}]}),
    ])
    .await;
    // 10.1 + 0.2 - 0.05 - 0.05 is 10.2 with none of the error floating point adds.
    let reply = apply("scan-usd", apply_body("ChargeItem/scan", None)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(
        reply.body["totalNet"],
        json!({"value": 10.2, "currency": "USD"})
    );
    assert_eq!(
        reply.body["totalGross"],
        json!({"value": 10.2, "currency": "USD"})
    );
    // Two currencies are not added, and no rate is guessed.
    let reply = apply("scan-mixed", apply_body("ChargeItem/scan", None)).await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.body
    );
    assert!(
        issues(&reply.body).contains("USD") && issues(&reply.body).contains("EUR"),
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn a_condition_that_is_not_met_gives_no_price() {
    put_all(vec![
        json!({"resourceType": "Patient", "id": "waiting"}),
        charged("planned-visit", "waiting", "planned", None),
        charged("billable-visit", "waiting", "billable", None),
        consultation_prices("consult-conditions"),
        // Its one price is for a condition the ChargeItem does not meet.
        json!({"resourceType": "ChargeItemDefinition", "id": "weekend-only", "status": "active",
            "url": "http://example.org/cid/weekend-only",
            "propertyGroup": [{
                "applicability": [{"language": "text/fhirpath", "expression": "%context.note.exists()"}],
                "priceComponent": [{"type": "base", "amount": {"value": 500, "currency": "NGN"}}]}]}),
    ])
    .await;
    // The definition as a whole does not apply: the answer is the reason, as an OperationOutcome.
    let reply = apply(
        "consult-conditions",
        apply_body("ChargeItem/planned-visit", None),
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.body
    );
    assert_eq!(reply.body["resourceType"], "OperationOutcome");
    assert_eq!(reply.body["issue"][0]["code"], "business-rule");
    assert_eq!(
        reply.body["issue"][0]["expression"][0],
        "ChargeItemDefinition.applicability[0]"
    );
    assert!(
        issues(&reply.body).contains("only what can be billed"),
        "{}",
        reply.body
    );
    // No group applies: there is a line for the ChargeItem and no price.
    let reply = apply(
        "weekend-only",
        apply_body("ChargeItem/billable-visit", None),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["resourceType"], "Invoice");
    assert!(
        reply.body.get("totalNet").is_none() && reply.body.get("totalGross").is_none(),
        "{}",
        reply.body
    );
    assert!(
        reply.body["lineItem"][0].get("priceComponent").is_none(),
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn a_request_that_names_what_does_not_exist_is_refused_for_the_reason() {
    put_all(vec![
        json!({"resourceType": "Patient", "id": "named"}),
        charged("named-visit", "named", "billable", None),
        json!({"resourceType": "Account", "id": "named-account", "status": "active"}),
        json!({"resourceType": "ChargeItemDefinition", "id": "simple", "status": "active",
            "url": "http://example.org/cid/simple",
            "propertyGroup": [{"priceComponent": [
                {"type": "base", "amount": {"value": 1, "currency": "NGN"}}]}]}),
        json!({"resourceType": "ChargeItemDefinition", "id": "in-cql", "status": "active",
            "url": "http://example.org/cid/in-cql",
            "applicability": [{"language": "text/cql", "expression": "true"}]}),
        json!({"resourceType": "ChargeItemDefinition", "id": "gibberish", "status": "active",
            "url": "http://example.org/cid/gibberish",
            "applicability": [{"language": "text/fhirpath", "expression": "status ="}]}),
        json!({"resourceType": "ChargeItemDefinition", "id": "not-boolean", "status": "active",
            "url": "http://example.org/cid/not-boolean",
            "applicability": [{"language": "text/fhirpath", "expression": "%context.status"}]}),
    ])
    .await;
    // The definition is in the URL: a 404.
    let reply = apply("nope", apply_body("ChargeItem/named-visit", None)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
    // The ChargeItem and the Account are in the parameters: well formed, and not there, a 422.
    let reply = apply("simple", apply_body("ChargeItem/nope", None)).await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.body
    );
    assert!(
        issues(&reply.body).contains("ChargeItem/nope does not exist"),
        "{}",
        reply.body
    );
    let reply = apply(
        "simple",
        apply_body("ChargeItem/named-visit", Some("Account/nope")),
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.body
    );
    assert!(
        issues(&reply.body).contains("Account/nope does not exist"),
        "{}",
        reply.body
    );
    // The account in context is the Invoice's.
    let reply = apply(
        "simple",
        apply_body("ChargeItem/named-visit", Some("Account/named-account")),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["account"]["reference"], "Account/named-account");
    assert_eq!(
        reply.body["totalNet"],
        json!({"value": 1, "currency": "NGN"})
    );
    // A request that is not one: no chargeItem, the wrong type, an absolute URL, no Parameters.
    for body in [
        json!({"resourceType": "Parameters", "parameter": []}),
        apply_body("Patient/named", None),
        apply_body("http://elsewhere.example/ChargeItem/named-visit", None),
        json!({"resourceType": "ChargeItem"}),
        json!({"resourceType": "Parameters", "parameter": [{"name": "chargeItem", "valueString": "ChargeItem/named-visit"}]}),
    ] {
        let reply = apply("simple", body.clone()).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{body}: {}",
            reply.body
        );
    }
    // A condition that cannot be decided is not taken as true.
    for (definition, said) in [
        ("in-cql", "text/cql"),
        ("gibberish", "cannot be read"),
        ("not-boolean", "true or false"),
    ] {
        let reply = apply(definition, apply_body("ChargeItem/named-visit", None)).await;
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{definition}: {}",
            reply.body
        );
        assert!(
            issues(&reply.body).contains(said),
            "{definition}: {}",
            reply.body
        );
    }
    // A type that has no such operation.
    let reply = post(
        "/Patient/named/$apply",
        apply_body("ChargeItem/named-visit", None),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
}

#[tokio::test]
async fn the_capability_statement_lists_apply() {
    let metadata = get("/metadata").await;
    let entry = metadata.body["rest"][0]["resource"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["type"] == "ChargeItemDefinition")
        .unwrap();
    assert_eq!(
        entry["operation"],
        json!([{"name": "apply",
            "definition": "http://hl7.org/fhir/OperationDefinition/ChargeItemDefinition-apply"}])
    );
}

async fn apply_as(token: &str, definition: &str, body: Value) -> StatusCode {
    crate::tests::send_text(
        &guarded(),
        "POST",
        &format!("/ChargeItemDefinition/{definition}/$apply"),
        &body.to_string(),
        &[
            ("authorization", &format!("Bearer {token}")),
            ("content-type", "application/fhir+json"),
        ],
    )
    .await
    .0
}

#[tokio::test]
async fn applying_a_definition_needs_to_read_the_charge_item_and_nothing_more() {
    let body = apply_body("ChargeItem/visit", None);
    for (token, wanted) in [
        // Reading is enough: nothing is written.
        ("pricer", StatusCode::OK),
        // Not the ChargeItem the price is of: it is not this token's to read.
        ("definitions-only", StatusCode::FORBIDDEN),
        // Nor the definition.
        ("charge-items-only", StatusCode::FORBIDDEN),
        // The Account too, when one is named.
        ("pricer-without-accounts", StatusCode::OK),
        // A token that is limited to one patient cannot, whatever it may read.
        ("patient", StatusCode::FORBIDDEN),
        ("nobody", StatusCode::UNAUTHORIZED),
    ] {
        assert_eq!(
            apply_as(token, "price", body.clone()).await,
            wanted,
            "{token}"
        );
    }
    let with_account = apply_body("ChargeItem/visit", Some("Account/anything"));
    assert_eq!(
        apply_as("pricer-without-accounts", "price", with_account).await,
        StatusCode::FORBIDDEN
    );
}

// `_include` and `_revinclude` follow references, and a rule that cannot is not echoed as applied.

async fn get_preferring(uri: &str, prefer: Option<&str>) -> (StatusCode, Value) {
    let mut headers = Vec::new();
    if let Some(prefer) = prefer {
        headers.push(("prefer", prefer));
    }
    let (status, text) = crate::tests::send_text(&billing(), "GET", uri, "", &headers).await;
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn self_link(bundle: &Value) -> String {
    bundle["link"]
        .as_array()
        .unwrap()
        .iter()
        .find(|link| link["relation"] == "self")
        .unwrap()["url"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn an_include_of_a_parameter_that_is_not_a_reference_is_refused_or_left_out() {
    let service = put(json!({"resourceType": "HealthcareService", "id": "gp"})).await;
    assert!(service.status.is_success(), "{}", service.body);
    // `context` of a ChargeItemDefinition is a token: there is nothing in it to follow. It used to
    // be a `200` with nothing included and the rule in `self`, as if it had been applied.
    let uri = "/HealthcareService?_id=gp&_revinclude=ChargeItemDefinition:context";
    for prefer in [None, Some("handling=strict")] {
        let (status, body) = get_preferring(uri, prefer).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{prefer:?}: {body}");
        assert_eq!(body["resourceType"], "OperationOutcome");
        let text = issues(&body);
        assert!(
            text.contains("_revinclude")
                && text.contains("ChargeItemDefinition:context")
                && text.contains("token"),
            "{prefer:?}: {text}"
        );
    }
    // Lenient leaves the rule out of what it says it did, and says so.
    let (status, body) = get_preferring(uri, Some("handling=lenient")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let link = self_link(&body);
    assert!(
        link.contains("_id=gp") && !link.contains("_revinclude"),
        "{link}"
    );
    let entries = body["entry"].as_array().unwrap();
    let matched: Vec<_> = entries
        .iter()
        .filter(|entry| entry["search"]["mode"] == "match")
        .collect();
    assert_eq!(matched.len(), 1);
    assert!(entries
        .iter()
        .all(|entry| entry["search"]["mode"] != "include"));
    let outcome = entries
        .iter()
        .find(|entry| entry["search"]["mode"] == "outcome")
        .expect("an OperationOutcome entry that says what was left out");
    let text = issues(&outcome["resource"]);
    assert!(
        text.contains("ChargeItemDefinition:context") && text.contains("handling=lenient"),
        "{text}"
    );
    assert_eq!(body["total"], 1);

    // Every way a rule can name something that cannot be followed.
    for rule in [
        "_include=Nope:subject",
        "_include=ClaimResponse:nope",
        "_include=ClaimResponse:disposition",
        "_include:iterate=Claim:status",
        "_revinclude=ClaimResponse:outcome",
    ] {
        let (status, body) = get_preferring(&format!("/Claim?{rule}"), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{rule}: {body}");
        assert!(
            issues(&body).contains(rule.split_once('=').unwrap().1),
            "{rule}: {body}"
        );
    }
}

#[tokio::test]
async fn an_include_of_a_reference_parameter_still_follows_it() {
    seed().await;
    let submitted = post("/Claim/$submit", parameters(claim("claim"))).await;
    assert_eq!(submitted.status, StatusCode::OK, "{}", submitted.body);
    let response = submitted.body["id"].as_str().unwrap();
    let claim_id = submitted.body["request"]["reference"].as_str().unwrap();

    let kinds = |bundle: &Value| -> Vec<(String, String)> {
        let mut found: Vec<_> = bundle["entry"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["search"]["mode"].as_str().unwrap().to_string(),
                    entry["resource"]["resourceType"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                )
            })
            .collect();
        found.sort();
        found
    };
    // Forward: the response and the claim it answers.
    let uri = format!("/ClaimResponse?_id={response}&_include=ClaimResponse:request");
    let (status, body) = get_preferring(&uri, Some("handling=strict")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        kinds(&body),
        [
            ("include".into(), "Claim".into()),
            ("match".into(), "ClaimResponse".into())
        ]
    );
    assert!(self_link(&body).contains("_include=ClaimResponse:request"));
    // Reverse: the claim and the response to it.
    let claim_search = format!(
        "/Claim?_id={}&_revinclude=ClaimResponse:request",
        claim_id.trim_start_matches("Claim/")
    );
    let (status, body) = get_preferring(&claim_search, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        kinds(&body),
        [
            ("include".into(), "ClaimResponse".into()),
            ("match".into(), "Claim".into())
        ]
    );
    assert!(self_link(&body).contains("_revinclude=ClaimResponse:request"));
    // A rule for any parameter, and one that iterates, are not the server's to refuse.
    for rule in ["_include=*", "_include:iterate=ClaimResponse:request"] {
        let (status, body) =
            get_preferring(&format!("/ClaimResponse?_id={response}&{rule}"), None).await;
        assert_eq!(status, StatusCode::OK, "{rule}: {body}");
    }

    // Lenient drops only the rule that cannot be followed, and keeps the one that can.
    let uri = format!(
        "/ClaimResponse?_id={response}&_include=ClaimResponse:request,ClaimResponse:disposition"
    );
    let (status, body) = get_preferring(&uri, Some("handling=lenient")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let link = self_link(&body);
    assert!(link.contains("_include=ClaimResponse:request"), "{link}");
    assert!(!link.contains("disposition"), "{link}");
    assert!(
        kinds(&body).contains(&("include".into(), "Claim".into())),
        "{body}"
    );
}
