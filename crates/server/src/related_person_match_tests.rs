//! `POST [base]/RelatedPerson/$match` over REST: whether a carer or parent who is about to be
//! registered for a Patient is already registered, at the same organization.
//!
//! The cases are the ones a registration desk meets: the same phone number written two ways, the
//! same name with nothing else, the same person at another organization, and a record that was
//! switched off. It is a server of its own, over the small package, so that the Patients of the
//! `Patient/$match` tests (which compare every Patient there is) are not changed by it.

use std::sync::{Arc, OnceLock, RwLock};

use axum::http::StatusCode;
use futures_util::SinkExt;
use refhir_database::Database;
use refhir_fhir::{import_resource, install_package_dir, patient_match_dir, Catalog};
use refhir_sqlite::SqlitePersistence;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::tests::{connect, next_json, send, Reply};
use crate::{app_with, ServerConfig};

const BOOLBYTE_ID: &str = "https://boolbyte.health/fhir/NamingSystem/boolbyte-id";
const MATCH_GRADE: &str = "http://hl7.org/fhir/StructureDefinition/match-grade";
const MATCH_EVIDENCE: &str = "https://refhir.dev/StructureDefinition/match-evidence";

fn patient(id: &str, organization: Option<&str>) -> Value {
    let mut patient = json!({
        "resourceType": "Patient", "id": id, "active": true,
        "name": [{"family": "Child", "given": [id]}], "birthDate": "2015-05-05"
    });
    if let Some(organization) = organization {
        patient["managingOrganization"] =
            json!({"reference": format!("Organization/{organization}")});
    }
    patient
}

/// A RelatedPerson of `patient`, with the name `family` and `given`.
fn carer(id: &str, patient: &str, family: &str, given: &[&str]) -> Value {
    json!({
        "resourceType": "RelatedPerson", "id": id, "active": true,
        "patient": {"reference": format!("Patient/{patient}")},
        "name": [{"family": family, "given": given}]
    })
}

fn with(mut resource: Value, field: &str, value: Value) -> Value {
    resource[field] = value;
    resource
}

fn phone(number: &str) -> Value {
    json!({"system": "phone", "value": number})
}

fn identifier(value: &str) -> Value {
    json!({"system": BOOLBYTE_ID, "value": value})
}

fn seeded() -> axum::Router {
    static SEEDED: OnceLock<axum::Router> = OnceLock::new();
    SEEDED.get_or_init(|| seed(true)).clone()
}

fn seed(auth: bool) -> axum::Router {
    let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
    let db = Arc::new(Database::open(store.clone()).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    install_package_dir(store.as_ref(), &db, &catalog, patient_match_dir()).unwrap();
    let records = [
        json!({"resourceType": "Organization", "id": "clinic-a"}),
        json!({"resourceType": "Organization", "id": "sender-b"}),
        // Two children of `clinic-a`, one of `sender-b`, and one of nobody's.
        patient("kid-a1", Some("clinic-a")),
        patient("kid-a2", Some("clinic-a")),
        patient("kid-b1", Some("sender-b")),
        patient("kid-free", None),
        // A mother, registered for a child of `clinic-a`, with her phone number written with
        // spaces and a country code, an email address and an identifier.
        with(
            with(
                with(
                    carer("ngozi-a", "kid-a1", "Eke", &["Ngozi"]),
                    "identifier",
                    json!([identifier("BB-R1")]),
                ),
                "telecom",
                json!([phone("+234 803 123 4567"), {"system": "email", "value": "Ngozi.Eke@example.org"}]),
            ),
            "gender",
            json!("female"),
        ),
        // The same woman at `sender-b`, as a RelatedPerson of its child, with every detail the same.
        with(
            with(
                carer("ngozi-b", "kid-b1", "Eke", &["Ngozi"]),
                "identifier",
                json!([identifier("BB-R1")]),
            ),
            "telecom",
            json!([phone("+234 803 123 4567")]),
        ),
        // The same woman again at `clinic-a`, for her other child: the same person, so a match.
        with(
            carer("ngozi-a2", "kid-a2", "Eke", &["Ngozi"]),
            "telecom",
            json!([phone("0803 123 4567")]),
        ),
        // Written with the accents of Igbo, and found by what her number dials.
        with(
            carer("chioma", "kid-a1", "Ékwé", &["Chíoma"]),
            "telecom",
            json!([phone("+2348055500001")]),
        ),
        // A name and nothing else, and one with a birth date.
        carer("tunde", "kid-a1", "Bello", &["Tunde"]),
        with(
            carer("kemi", "kid-a2", "Lawal", &["Kemi"]),
            "birthDate",
            json!("1984-04-04"),
        ),
        // Switched off, with the number and the name of the request that asks about her.
        with(
            with(
                carer("ife", "kid-a1", "Obi", &["Ife"]),
                "active",
                json!(false),
            ),
            "telecom",
            json!([phone("0806 700 8001")]),
        ),
        // A RelatedPerson of a Patient that is deleted below: nobody's organization to be in.
        patient("kid-gone", Some("clinic-a")),
        carer("adrift", "kid-gone", "Eke", &["Ngozi"]),
        // Nobody's organization.
        carer("free", "kid-free", "Adamu", &["Hauwa"]),
        // A number that was given up is no evidence: only the name is left.
        with(
            carer("olive", "kid-a1", "Peters", &["Olive"]),
            "telecom",
            json!([{"system": "phone", "value": "0807 111 2222", "use": "old"}]),
        ),
    ];
    for record in &records {
        import_resource(&db, &catalog.read().unwrap(), record)
            .unwrap_or_else(|err| panic!("{record}: {err}"));
    }
    // Nothing stops a Patient being deleted while a RelatedPerson still names it.
    refhir_fhir::run(
        &db,
        &catalog.read().unwrap(),
        "Patient/delete",
        &json!({"id": "kid-gone"}),
    )
    .unwrap_or_else(|err| panic!("{err}"));
    let smart = Arc::new(crate::auth::SmartStore::with_defaults());
    // The Patients are read to find the organization, so the token needs to search them too.
    smart.install_token("carer", &["user/RelatedPerson.rs", "user/Patient.rs"], None);
    smart.install_token("carer-only", &["user/RelatedPerson.rs"], None);
    smart.install_token("patients", &["user/Patient.read"], None);
    app_with(
        db,
        catalog,
        store,
        ServerConfig {
            auth,
            smart: auth.then_some(smart),
            ..Default::default()
        },
    )
}

/// `$match` on RelatedPerson with `resource` and the other parameters, as `token` sends it.
async fn post_match(token: &str, resource: Value, others: &[Value]) -> Reply {
    let mut parameter = vec![json!({"name": "resource", "resource": resource})];
    parameter.extend_from_slice(others);
    send(
        &seeded(),
        "POST",
        &format!("/RelatedPerson/$match?access_token={token}"),
        Some(json!({"resourceType": "Parameters", "parameter": parameter})),
        None,
    )
    .await
}

/// A RelatedPerson of `patient` that is being registered, as the desk has it.
fn asking(patient: &str, family: &str, given: &[&str]) -> Value {
    json!({
        "resourceType": "RelatedPerson",
        "patient": {"reference": format!("Patient/{patient}")},
        "name": [{"family": family, "given": given}]
    })
}

/// The id, grade and score of each entry, in order.
fn found(reply: &Reply) -> Vec<(String, String, f64)> {
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let bundle = &reply.body;
    assert_eq!(bundle["resourceType"], "Bundle");
    assert_eq!(bundle["type"], "searchset");
    let entries = bundle["entry"].as_array().cloned().unwrap_or_default();
    assert_eq!(bundle["total"], entries.len(), "{bundle}");
    let mut last = f64::INFINITY;
    entries
        .iter()
        .map(|entry| {
            assert_eq!(entry["resource"]["resourceType"], "RelatedPerson");
            let score = entry["search"]["score"].as_f64().expect("score");
            assert!(score <= last, "not best first: {bundle}");
            last = score;
            let grade = entry["search"]["extension"]
                .as_array()
                .unwrap()
                .iter()
                .find(|extension| extension["url"] == MATCH_GRADE)
                .expect("grade")["valueCode"]
                .as_str()
                .unwrap()
                .to_string();
            (
                entry["resource"]["id"].as_str().unwrap().to_string(),
                grade,
                score,
            )
        })
        .collect()
}

async fn grades(resource: Value) -> Vec<(String, String)> {
    found(&post_match("carer", resource, &[]).await)
        .into_iter()
        .map(|(id, grade, _)| (id, grade))
        .collect()
}

fn graded(entries: &[(&str, &str)]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|(id, grade)| (id.to_string(), grade.to_string()))
        .collect()
}

#[tokio::test]
async fn the_same_phone_number_written_two_ways_is_probable() {
    // The number of the request has no spaces and no country code; the stored ones have a `+234`
    // and spaces, or a `0` and spaces. Both are what the request dials.
    let request = with(
        asking("kid-a1", "Eke", &["Ngozi"]),
        "telecom",
        json!([phone("08031234567")]),
    );
    let reply = post_match("carer", request, &[]).await;
    let entries = found(&reply);
    let grades: Vec<(&str, &str)> = entries
        .iter()
        .map(|(id, grade, _)| (id.as_str(), grade.as_str()))
        .collect();
    assert_eq!(grades, [("ngozi-a", "probable"), ("ngozi-a2", "probable")]);
    assert!(entries
        .iter()
        .all(|(_, _, score)| (0.75..=0.95).contains(score)));
    // It says which number of the request matched, as the request wrote it.
    let evidence: Vec<Value> = reply.body["entry"][0]["search"]["extension"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|extension| extension["url"] == MATCH_EVIDENCE)
        .cloned()
        .collect();
    let said = |kind: &str| {
        evidence
            .iter()
            .any(|item| item["extension"][0]["valueCode"] == kind)
    };
    assert!(said("name") && said("phone"), "{evidence:?}");
    assert!(!said("birth-date"), "{evidence:?}");
    assert_eq!(
        evidence
            .iter()
            .find(|item| item["extension"][0]["valueCode"] == "phone")
            .unwrap()["extension"][1]["valueContactPoint"]["value"],
        "08031234567"
    );
}

#[tokio::test]
async fn an_email_address_is_the_same_whatever_its_case() {
    let request = with(
        asking("kid-a1", "Eke", &["Ngozi"]),
        "telecom",
        json!([{"system": "email", "value": "ngozi.eke@EXAMPLE.org"}]),
    );
    // `ngozi-a2` has her name and no email address on file: the same name, and no more.
    assert_eq!(
        grades(request).await,
        graded(&[("ngozi-a", "probable"), ("ngozi-a2", "possible")])
    );
}

#[tokio::test]
async fn a_number_is_found_by_what_it_dials_when_the_name_is_spelt_otherwise() {
    // The stored name has the accents of Igbo; the request has none. The number is stored as
    // `+2348055500001` and is written here with a `0`: that is how it is found, and the name,
    // which has the same words, is what makes it a match.
    let request = with(
        asking("kid-a1", "Ekwe", &["Chioma"]),
        "telecom",
        json!([phone("0805 550 0001")]),
    );
    assert_eq!(grades(request).await, graded(&[("chioma", "probable")]));
}

#[tokio::test]
async fn the_same_name_alone_is_possible_and_a_number_alone_is_nothing() {
    let reply = post_match("carer", asking("kid-a1", "Bello", &["Tunde"]), &[]).await;
    let entries = found(&reply);
    assert_eq!(
        entries
            .iter()
            .map(|(id, grade, _)| (id.as_str(), grade.as_str()))
            .collect::<Vec<_>>(),
        [("tunde", "possible")]
    );
    assert!((0.40..=0.70).contains(&entries[0].2), "{entries:?}");

    // A number is nobody's name: the one of Ngozi, with another name, finds no one.
    let someone_else = with(
        asking("kid-a1", "Someone", &["Else"]),
        "telecom",
        json!([phone("+234 803 123 4567")]),
    );
    assert_eq!(grades(someone_else).await, graded(&[]));
}

#[tokio::test]
async fn a_name_in_another_order_or_spelling_is_the_same_name() {
    assert_eq!(
        grades(asking("kid-a1", "bello", &["TUNDE"])).await,
        graded(&[("tunde", "possible")])
    );
    assert_eq!(
        grades(asking("kid-a1", "Tunde", &["Bello"])).await,
        graded(&[("tunde", "possible")])
    );
}

#[tokio::test]
async fn a_birth_date_that_agrees_raises_the_score_and_one_that_differs_is_not_a_match() {
    let kemi = |born: Option<&str>| {
        let mut request = asking("kid-a2", "Lawal", &["Kemi"]);
        if let Some(born) = born {
            request["birthDate"] = json!(born);
        }
        request
    };
    let without = found(&post_match("carer", kemi(None), &[]).await);
    let agreeing = found(&post_match("carer", kemi(Some("1984-04-04")), &[]).await);
    assert_eq!(agreeing.len(), 1);
    assert_eq!(agreeing[0].0, "kemi");
    assert_eq!(agreeing[0].1, "possible");
    assert!(agreeing[0].2 > without[0].2, "{agreeing:?} {without:?}");
    // The RelatedPerson on file has a birth date; the request has another. Another person.
    assert_eq!(grades(kemi(Some("1990-01-01"))).await, graded(&[]));
    // A year is not a date to compare.
    assert_eq!(
        grades(kemi(Some("1990"))).await,
        graded(&[("kemi", "possible")])
    );
}

#[tokio::test]
async fn an_identifier_is_certain_whatever_the_name() {
    let request = json!({
        "resourceType": "RelatedPerson", "patient": {"reference": "Patient/kid-a1"},
        "identifier": [identifier("BB-R1")]
    });
    assert_eq!(grades(request).await, graded(&[("ngozi-a", "certain")]));
}

#[tokio::test]
async fn another_organizations_related_person_is_never_returned() {
    // `ngozi-b` has the same name, number and identifier, and belongs to a Patient of `sender-b`.
    let asked = |patient: &str| {
        with(
            with(
                asking(patient, "Eke", &["Ngozi"]),
                "identifier",
                json!([identifier("BB-R1")]),
            ),
            "telecom",
            json!([phone("+234 803 123 4567")]),
        )
    };
    let at_clinic = grades(asked("kid-a1")).await;
    assert_eq!(at_clinic[0], ("ngozi-a".to_string(), "certain".to_string()));
    assert!(
        at_clinic
            .iter()
            .all(|(id, _)| id != "ngozi-b" && id != "adrift"),
        "{at_clinic:?}"
    );
    // The other organization is asked the same, and has its own.
    assert_eq!(
        grades(asked("kid-b1")).await,
        graded(&[("ngozi-b", "certain")])
    );
    // Neither of those is the answer for a request by identifier alone.
    let only_identifier = |patient: &str| {
        json!({
            "resourceType": "RelatedPerson", "patient": {"reference": format!("Patient/{patient}")},
            "identifier": [identifier("BB-R1")]
        })
    };
    assert_eq!(
        grades(only_identifier("kid-a2")).await,
        graded(&[("ngozi-a", "certain")])
    );
    assert_eq!(
        grades(only_identifier("kid-b1")).await,
        graded(&[("ngozi-b", "certain")])
    );
}

#[tokio::test]
async fn one_certain_match_of_the_organization_is_the_only_one_that_only_certain_matches_returns() {
    let request = json!({
        "resourceType": "RelatedPerson", "patient": {"reference": "Patient/kid-a1"},
        "identifier": [identifier("BB-R1")]
    });
    // `ngozi-b` is certain too, at another organization: it is not a second potential match.
    let only_certain = [json!({"name": "onlyCertainMatches", "valueBoolean": true})];
    let reply = post_match("carer", request, &only_certain).await;
    assert_eq!(
        found(&reply)
            .into_iter()
            .map(|(id, ..)| id)
            .collect::<Vec<_>>(),
        ["ngozi-a"]
    );
    // By name, two of the organization's are potential matches and nothing is returned.
    let by_name = asking("kid-a1", "Eke", &["Ngozi"]);
    let reply = post_match("carer", by_name.clone(), &only_certain).await;
    assert_eq!(found(&reply), []);
    let count = [json!({"name": "count", "valueInteger": 1})];
    assert_eq!(found(&post_match("carer", by_name, &count).await).len(), 1);
}

#[tokio::test]
async fn a_related_person_that_is_switched_off_is_not_returned() {
    let by_name = asking("kid-a1", "Obi", &["Ife"]);
    assert_eq!(grades(by_name.clone()).await, graded(&[]));
    let with_phone = with(by_name, "telecom", json!([phone("08067008001")]));
    assert_eq!(grades(with_phone).await, graded(&[]));
}

#[tokio::test]
async fn a_patient_with_no_organization_compares_every_organizations_related_persons() {
    // As `Patient/$match` does for a Patient with none: nothing is limited.
    let reply = post_match("carer", asking("kid-free", "Eke", &["Ngozi"]), &[]).await;
    let mut ids: Vec<String> = found(&reply).into_iter().map(|(id, ..)| id).collect();
    ids.sort();
    assert_eq!(ids, ["adrift", "ngozi-a", "ngozi-a2", "ngozi-b"]);
}

#[tokio::test]
async fn what_cannot_be_matched_is_refused_and_not_answered_for_every_organization() {
    let refused = |reply: Reply, mentions: &str| {
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
        let text = reply.body["issue"][0]["diagnostics"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(text.contains(mentions), "{text}");
    };
    // No Patient to say whose organization it is.
    let bare =
        json!({"resourceType": "RelatedPerson", "name": [{"family": "Eke", "given": ["Ngozi"]}]});
    refused(
        post_match("carer", bare, &[]).await,
        "RelatedPerson.patient",
    );
    // Not a Patient of this server.
    for reference in [
        "Patient/nobody",
        "https://example.org/Patient/kid-a1",
        "#kid",
        "kid-a1",
    ] {
        let request = json!({
            "resourceType": "RelatedPerson", "patient": {"reference": reference},
            "name": [{"family": "Eke", "given": ["Ngozi"]}]
        });
        refused(
            post_match("carer", request, &[]).await,
            "RelatedPerson.patient",
        );
    }
    // A Patient is not what this operation is asked about, and it is told where the other is.
    refused(
        post_match("carer", patient("x", None), &[]).await,
        "resource must be a RelatedPerson",
    );
    let wrong = send(
        &seeded(),
        "POST",
        "/Patient/$match?access_token=patients",
        Some(json!({"resourceType": "Parameters", "parameter": [
            {"name": "resource", "resource": asking("kid-a1", "Eke", &["Ngozi"])}]})),
        None,
    )
    .await;
    refused(wrong, "resource must be a Patient");
    // The same parameters as `Patient/$match`, and no others.
    let other = [json!({"name": "organization", "valueString": "Organization/clinic-a"})];
    refused(
        post_match("carer", asking("kid-a1", "Eke", &["Ngozi"]), &other).await,
        "unknown parameter organization",
    );
}

#[tokio::test]
async fn a_token_needs_to_search_related_persons_and_the_patients_it_reads() {
    let asked = || asking("kid-a1", "Eke", &["Ngozi"]);
    // No RelatedPerson scope.
    let reply = post_match("patients", asked(), &[]).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    // The organization is read from the Patient, so a token that may not search Patients may not
    // learn which exist: not by the answer, and not by the difference between two refusals.
    for patient in ["kid-a1", "nobody"] {
        let reply = post_match("carer-only", asking(patient, "Eke", &["Ngozi"]), &[]).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    }
    assert_eq!(
        post_match("carer", asked(), &[]).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_capability_statement_says_it_is_there() {
    let reply = send(&seeded(), "GET", "/metadata?access_token=carer", None, None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let resources = reply.body["rest"][0]["resource"].as_array().unwrap();
    let operations = |name: &str| -> Vec<String> {
        resources
            .iter()
            .find(|resource| resource["type"] == name)
            .and_then(|resource| resource["operation"].as_array())
            .into_iter()
            .flatten()
            .map(|operation| operation["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(operations("RelatedPerson"), ["match"]);
    assert!(operations("Patient").contains(&"match".to_string()));
}

#[tokio::test]
async fn a_number_that_was_given_up_is_no_evidence() {
    let request = with(
        asking("kid-a1", "Peters", &["Olive"]),
        "telecom",
        json!([phone("08071112222")]),
    );
    assert_eq!(grades(request).await, graded(&[("olive", "possible")]));
}

#[tokio::test]
async fn a_phone_search_is_for_phones_and_an_email_search_for_addresses() {
    let total = |query: &'static str| async move {
        let reply = send(
            &seeded(),
            "GET",
            &format!("/RelatedPerson?{query}&access_token=carer"),
            None,
            None,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        reply.body["total"].as_u64().unwrap()
    };
    assert_eq!(total("phone=%2B234%20803%20123%204567").await, 2);
    assert_eq!(total("email=Ngozi.Eke@example.org").await, 1);
    // The address is not a phone number, and the number is not an address.
    assert_eq!(total("phone=Ngozi.Eke@example.org").await, 0);
    assert_eq!(total("email=%2B234%20803%20123%204567").await, 0);
}

#[tokio::test]
async fn over_sync_a_server_without_smart_matches_related_persons_as_rest_does() {
    let app = seed(false);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (mut socket, _) = connect(&format!("ws://{addr}/sync")).await;
    let request = json!({"resourceType": "Parameters", "parameter": [
        {"name": "resource", "resource": asking("kid-a1", "Bello", &["Tunde"])}]});
    socket
        .send(Message::Text(
            json!({"type": "Mutation", "requestId": 1, "udfPath": "RelatedPerson/$match", "args": request})
                .to_string(),
        ))
        .await
        .unwrap();
    let answer = next_json(&mut socket).await;
    assert_eq!(answer["success"], true, "{answer}");
    assert_eq!(answer["result"]["total"], 1, "{answer}");
    assert_eq!(answer["result"]["entry"][0]["resource"]["id"], "tunde");
}
