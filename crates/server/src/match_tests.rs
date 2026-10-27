//! `POST [base]/Patient/$match` over REST, against a store of Patients that stand for the cases a
//! registration desk sees: the same person with an identifier, with only a name and a phone number,
//! under another spelling, two people who could be one, and records that are no longer in use.
//!
//! Everything here only reads, so one server is built once for the module. It is over the small R4
//! package, not the whole release: a server test that loads the release for itself slows every
//! other test in the process, and the cluster tests there depend on being quick. The server asks
//! for a token, because what `$match` may be asked by whom is part of what is tested.

use std::sync::{Arc, OnceLock, RwLock};

use axum::http::StatusCode;
use refhir_database::Database;
use refhir_fhir::{import_resource, install_package_dir, patient_match_dir, Catalog};
use refhir_sqlite::SqlitePersistence;
use serde_json::{json, Value};

use crate::tests::{send, send_text, Reply};
use crate::{app_with, ServerConfig};

const PROFILE: &str = "https://id.example.org/profile";
const BOOLBYTE_ID: &str = "https://boolbyte.health/fhir/NamingSystem/boolbyte-id";
const MATCH_GRADE: &str = "http://hl7.org/fhir/StructureDefinition/match-grade";
const MATCH_EVIDENCE: &str = "https://refhir.dev/StructureDefinition/match-evidence";

fn person(id: &str, family: &str, given: &[&str], born: &str) -> Value {
    json!({
        "resourceType": "Patient", "id": id, "active": true,
        "name": [{"family": family, "given": given}],
        "birthDate": born
    })
}

fn with(mut patient: Value, field: &str, value: Value) -> Value {
    patient[field] = value;
    patient
}

fn phone(number: &str) -> Value {
    json!({"system": "phone", "value": number})
}

fn identifier(value: &str) -> Value {
    json!({"system": PROFILE, "value": value})
}

/// The identifier that two organizations of one platform both give a person.
fn boolbyte_id(value: &str) -> Value {
    json!({"system": BOOLBYTE_ID, "value": value})
}

/// `patient`, as the Patient of the organization `Organization/{id}`.
fn at(patient: Value, organization: &str) -> Value {
    with(
        patient,
        "managingOrganization",
        json!({"reference": format!("Organization/{organization}")}),
    )
}

fn seeded() -> axum::Router {
    static SEEDED: OnceLock<axum::Router> = OnceLock::new();
    SEEDED.get_or_init(seed).clone()
}

/// Store `resources`, in order, with the ids they have.
fn import_all(db: &Database, catalog: &RwLock<Catalog>, resources: &[Value]) {
    for resource in resources {
        import_resource(db, &catalog.read().unwrap(), resource)
            .unwrap_or_else(|err| panic!("{resource}: {err}"));
    }
}

fn seed() -> axum::Router {
    let store = Arc::new(SqlitePersistence::open_in_memory().unwrap());
    let db = Arc::new(Database::open(store.clone()).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    install_package_dir(store.as_ref(), &db, &catalog, patient_match_dir()).unwrap();
    let replaced_by = |id: &str| json!([{"other": {"reference": format!("Patient/{id}")}, "type": "replaced-by"}]);
    let patients = [
        // The same person as the requests below, found by the identifier, by name and phone, by
        // name and email, and by name alone.
        with(
            with(
                with(
                    person("ada", "Okafor", &["Ada", "Chinwe"], "1985-03-02"),
                    "identifier",
                    json!([identifier("PRF-1001"), {"system": "urn:example:mrn", "value": "88-1"}]),
                ),
                "telecom",
                json!([phone("+234 803 123 4567"), {"system": "email", "value": "ada.okafor@example.org"}]),
            ),
            "gender",
            json!("female"),
        ),
        with(
            person("tunde", "Adeyemi", &["Tunde"], "1990-07-15"),
            "telecom",
            json!([phone("0803 555 0101")]),
        ),
        person("musa", "Bello", &["Musa"], "1978-11-30"),
        // A name written the other way round, and one with the accents and dots Yoruba is written with.
        person("chidi", "Nwosu", &["Chidi"], "1988-08-08"),
        person("olawale", "Adébáyò", &["Ọláwálé"], "1975-01-20"),
        person("ngozi", "Eze", &["Ngozi"], "1992-05-05"),
        // Two people who could be one: one holds the identifier, the other has only the name.
        with(
            person("sani-a", "Danjuma", &["Sani"], "2001-01-01"),
            "identifier",
            json!([identifier("PRF-2001")]),
        ),
        person("sani-b", "Danjuma", &["Sani"], "2001-01-01"),
        // One identifier on two records.
        with(
            person("kemi-a", "Lawal", &["Kemi"], "1970-01-01"),
            "identifier",
            json!([identifier("PRF-2002")]),
        ),
        with(
            person("kemi-b", "Lawal", &["Kemisola"], "1971-01-01"),
            "identifier",
            json!([identifier("PRF-2002")]),
        ),
        // Records that are no longer in use: one switched off, two replaced, and what replaced them.
        // `femi-old` and `femi-older` hold an identifier that `femi` does not.
        with(
            with(
                person("obi", "Okeke", &["Obi"], "1980-02-02"),
                "active",
                json!(false),
            ),
            "identifier",
            json!([identifier("PRF-3001")]),
        ),
        person("femi", "Johnson", &["Femi"], "1983-04-04"),
        with(
            with(
                with(
                    person("femi-old", "Johnson", &["Femi"], "1983-04-04"),
                    "active",
                    json!(false),
                ),
                "identifier",
                json!([identifier("PRF-3002")]),
            ),
            "link",
            replaced_by("femi"),
        ),
        // Replaced, but never switched off.
        with(
            with(
                person("femi-older", "Johnson", &["Femi"], "1983-04-04"),
                "identifier",
                json!([identifier("PRF-3002")]),
            ),
            "link",
            replaced_by("femi"),
        ),
        // A chain of replacements, newest first (a reference needs its target to exist): `chain-a`
        // was replaced by `chain-b`, which was replaced by `chain-c`, which is in use.
        person("chain-c", "Balogun", &["Segun"], "1969-06-06"),
        with(
            person("chain-b", "Balogun", &["Segun"], "1969-06-06"),
            "link",
            replaced_by("chain-c"),
        ),
        with(
            with(
                with(
                    person("chain-a", "Balogun", &["Segun"], "1969-06-06"),
                    "active",
                    json!(false),
                ),
                "identifier",
                json!([identifier("PRF-4001")]),
            ),
            "link",
            replaced_by("chain-b"),
        ),
        // A replacement that leads to a Patient that was switched off, with nothing after it.
        with(
            person("end-b", "Fashola", &["Kunle"], "1972-02-02"),
            "active",
            json!(false),
        ),
        with(
            with(
                with(
                    person("end-a", "Fashola", &["Kunle"], "1972-02-02"),
                    "active",
                    json!(false),
                ),
                "identifier",
                json!([identifier("PRF-5001")]),
            ),
            "link",
            replaced_by("end-b"),
        ),
        // Three that no one could tell apart, for `count`.
        person("aisha-1", "Yusuf", &["Aisha"], "1999-09-09"),
        person("aisha-2", "Yusuf", &["Aisha"], "1999-09-09"),
        person("aisha-3", "Yusuf", &["Aisha"], "1999-09-09"),
        // A probable and a possible match for one request.
        with(
            person("zainab-a", "Garba", &["Zainab"], "1995-05-05"),
            "telecom",
            json!([phone("0801 111 2222")]),
        ),
        person("zainab-b", "Garba", &["Zainab"], "1995-05-05"),
        // A person whose phone number is the number of a relative who is a patient too.
        with(
            with(
                person("uche", "Obi", &["Uche"], "2010-10-10"),
                "telecom",
                json!([]),
            ),
            "contact",
            json!([{"telecom": [phone("0802 222 3333")]}]),
        ),
    ];
    import_all(&db, &catalog, &patients);
    // The Patients of two organizations. They are stored after the others, so that the others keep
    // their versions, and each organization before the Patients that name it.
    import_all(
        &db,
        &catalog,
        &[
            json!({"resourceType": "Organization", "id": "clinic-a"}),
            json!({"resourceType": "Organization", "id": "sender-b"}),
        ],
    );
    let organized = [
        // Two organizations that hold the same person, with the one identifier of the platform they
        // share: `bola` is at both. `dayo` is at both too, and only `sender-b` has her number.
        at(
            with(
                with(
                    person("bola-a", "Ajayi", &["Bola"], "1991-04-12"),
                    "identifier",
                    json!([boolbyte_id("BB-1")]),
                ),
                "telecom",
                json!([phone("0805 600 7001")]),
            ),
            "clinic-a",
        ),
        at(
            with(
                with(
                    person("bola-b", "Ajayi", &["Bola"], "1991-04-12"),
                    "identifier",
                    json!([boolbyte_id("BB-1")]),
                ),
                "telecom",
                json!([phone("0805 600 7001")]),
            ),
            "sender-b",
        ),
        at(
            person("dayo-a", "Peters", &["Dayo"], "1987-07-07"),
            "clinic-a",
        ),
        at(
            with(
                person("dayo-b", "Peters", &["Dayo"], "1987-07-07"),
                "telecom",
                json!([phone("0806 700 8001")]),
            ),
            "sender-b",
        ),
        // A record of `clinic-a` that was replaced by one of `sender-b`: the link leaves the
        // organization. `merge-b` is the one in use.
        at(
            person("merge-b", "Okon", &["Rita"], "1982-02-02"),
            "sender-b",
        ),
        at(
            with(
                with(
                    with(
                        person("merge-a", "Okon", &["Rita"], "1982-02-02"),
                        "active",
                        json!(false),
                    ),
                    "identifier",
                    json!([boolbyte_id("BB-2")]),
                ),
                "link",
                replaced_by("merge-b"),
            ),
            "clinic-a",
        ),
        // The same, with the way back in: `clinic-a`'s `ring-a` was replaced by `sender-b`'s
        // `ring-b`, which was replaced by `clinic-a`'s `ring-c`. Through `ring-b` the record in
        // use is reached, and it is not the organization's to reach.
        at(
            person("ring-c", "Dike", &["Ngozi"], "1979-09-19"),
            "clinic-a",
        ),
        at(
            with(
                person("ring-b", "Dike", &["Ngozi"], "1979-09-19"),
                "link",
                replaced_by("ring-c"),
            ),
            "sender-b",
        ),
        at(
            with(
                with(
                    with(
                        person("ring-a", "Dike", &["Ngozi"], "1979-09-19"),
                        "active",
                        json!(false),
                    ),
                    "identifier",
                    json!([boolbyte_id("BB-3")]),
                ),
                "link",
                replaced_by("ring-b"),
            ),
            "clinic-a",
        ),
        // A replacement within the organization, which is followed.
        at(person("own-b", "Eke", &["Tobi"], "1993-03-03"), "clinic-a"),
        at(
            with(
                with(
                    with(
                        person("own-a", "Eke", &["Tobi"], "1993-03-03"),
                        "active",
                        json!(false),
                    ),
                    "identifier",
                    json!([boolbyte_id("BB-4")]),
                ),
                "link",
                replaced_by("own-b"),
            ),
            "clinic-a",
        ),
    ];
    import_all(&db, &catalog, &organized);
    let smart = Arc::new(crate::auth::SmartStore::with_defaults());
    smart.install_token("reader", &["user/Patient.read"], None);
    smart.install_token("searcher", &["user/Patient.rs"], None);
    smart.install_token("creator", &["user/Patient.c"], None);
    smart.install_token("observer", &["user/Observation.read"], None);
    smart.install_token("patient", &["patient/Patient.read"], Some("ada"));
    smart.install_token("system", &["system/*.read"], None);
    app_with(
        db,
        catalog,
        store,
        ServerConfig {
            auth: true,
            smart: Some(smart),
            ..Default::default()
        },
    )
}

/// The `Parameters` a client sends.
fn parameters(patient: Value, only_certain: Option<bool>, count: Option<u32>) -> Value {
    let mut items = vec![json!({"name": "resource", "resource": patient})];
    if let Some(only_certain) = only_certain {
        items.push(json!({"name": "onlyCertainMatches", "valueBoolean": only_certain}));
    }
    if let Some(count) = count {
        items.push(json!({"name": "count", "valueInteger": count}));
    }
    json!({"resourceType": "Parameters", "parameter": items})
}

async fn post_match(token: &str, body: Value) -> Reply {
    send(
        &seeded(),
        "POST",
        &format!("/Patient/$match?access_token={token}"),
        Some(body),
        None,
    )
    .await
}

/// `$match` as a registration desk calls it, answered: the id and the grade of each entry, in order.
async fn matched(
    patient: Value,
    only_certain: Option<bool>,
    count: Option<u32>,
) -> Vec<(String, String)> {
    let reply = post_match("reader", parameters(patient, only_certain, count)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    entries(&reply.body)
}

fn entries(bundle: &Value) -> Vec<(String, String)> {
    assert_eq!(bundle["resourceType"], "Bundle");
    assert_eq!(bundle["type"], "searchset");
    let entries = bundle["entry"].as_array().cloned().unwrap_or_default();
    assert_eq!(bundle["total"], entries.len(), "{bundle}");
    let mut last = f64::INFINITY;
    let found: Vec<(String, String)> = entries
        .iter()
        .map(|entry| {
            let search = &entry["search"];
            assert_eq!(search["mode"], "match");
            let score = search["score"].as_f64().expect("score");
            assert!((0.0..=1.0).contains(&score), "{score}");
            assert!(score <= last, "not best first: {bundle}");
            last = score;
            let grades: Vec<&Value> = search["extension"]
                .as_array()
                .expect("extension")
                .iter()
                .filter(|extension| extension["url"] == MATCH_GRADE)
                .collect();
            assert_eq!(grades.len(), 1, "{entry}");
            (
                entry["resource"]["id"].as_str().unwrap().to_string(),
                grades[0]["valueCode"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    found
}

fn ids_and_grades(found: &[(&str, &str)]) -> Vec<(String, String)> {
    found
        .iter()
        .map(|(id, grade)| (id.to_string(), grade.to_string()))
        .collect()
}

#[tokio::test]
async fn an_identifier_that_is_on_a_record_is_certain() {
    // Only the identifier is known, so nothing else is needed.
    let request = json!({"resourceType": "Patient", "identifier": [identifier("PRF-1001")]});
    let reply = post_match("reader", parameters(request, None, None)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(entries(&reply.body), ids_and_grades(&[("ada", "certain")]));
    let entry = &reply.body["entry"][0];
    assert_eq!(entry["search"]["score"], 1.0);
    // The whole record, as a search would give it, with its version.
    assert_eq!(entry["resource"]["birthDate"], "1985-03-02");
    assert_eq!(entry["resource"]["meta"]["versionId"], "1");

    // The name and birth date it has on file are not asked about: another name on the request and
    // another date do not take the identifier back.
    let elsewhere = json!({
        "resourceType": "Patient", "identifier": [identifier("PRF-1001")],
        "name": [{"family": "Someone", "given": ["Else"]}], "birthDate": "2020-02-02"
    });
    assert_eq!(
        matched(elsewhere, None, None).await,
        ids_and_grades(&[("ada", "certain")])
    );
    // Among several identifiers, one is enough, whatever system the others are of.
    let several = json!({"resourceType": "Patient", "identifier": [
        {"system": "urn:example:mrn", "value": "99"}, {"system": "urn:example:mrn", "value": "88-1"}]});
    assert_eq!(
        matched(several, None, None).await,
        ids_and_grades(&[("ada", "certain")])
    );
}

#[tokio::test]
async fn the_same_value_in_another_system_or_another_case_is_not_the_identifier() {
    for identifier in [
        json!({"system": "https://id.example.org/other", "value": "PRF-1001"}),
        json!({"system": PROFILE, "value": "prf-1001"}),
        json!({"system": PROFILE, "value": "PRF-1001 "}),
        // No system names nothing, and a temporary number is not anyone's.
        json!({"value": "PRF-1001"}),
        json!({"system": PROFILE, "value": "PRF-1001", "use": "temp"}),
    ] {
        let request = json!({"resourceType": "Patient", "identifier": [identifier]});
        assert!(
            matched(request, None, None).await.is_empty(),
            "{identifier}"
        );
    }
}

#[tokio::test]
async fn name_birth_date_and_phone_or_email_is_probable_in_any_format() {
    let request = |telecom: Value| {
        json!({
            "resourceType": "Patient", "birthDate": "1985-03-02", "telecom": telecom,
            "name": [{"family": "Okafor", "given": ["Ada", "Chinwe"]}]
        })
    };
    for telecom in [
        json!([phone("+234 803 123 4567")]),
        json!([phone("0803 123 4567")]),
        json!([phone("08031234567")]),
        json!([phone("2348031234567")]),
        json!([phone("+234 (0) 803-123-4567")]),
        json!([phone("00234 803 123 4567")]),
        json!([{"system": "email", "value": "Ada.Okafor@Example.org"}]),
        json!([phone("0701 000 0000"), phone("0803 123 4567")]),
    ] {
        let found = matched(request(telecom.clone()), None, None).await;
        assert_eq!(found, ids_and_grades(&[("ada", "probable")]), "{telecom}");
    }
    // A number that is not hers is no help: the name and birth date alone are possible.
    let found = matched(request(json!([phone("0803 123 4568")])), None, None).await;
    assert_eq!(found, ids_and_grades(&[("ada", "possible")]));
    // The probable grade scores above the possible one and below the certain one.
    let reply = post_match(
        "reader",
        parameters(request(json!([phone("0803 123 4567")])), None, None),
    )
    .await;
    let score = reply.body["entry"][0]["search"]["score"].as_f64().unwrap();
    assert!((0.75..=0.95).contains(&score), "{score}");
}

#[tokio::test]
async fn name_and_birth_date_alone_is_possible() {
    let request = json!({"resourceType": "Patient", "birthDate": "1978-11-30",
        "name": [{"family": "Bello", "given": ["Musa"]}]});
    let reply = post_match("reader", parameters(request, None, None)).await;
    assert_eq!(
        entries(&reply.body),
        ids_and_grades(&[("musa", "possible")])
    );
    let score = reply.body["entry"][0]["search"]["score"].as_f64().unwrap();
    assert!((0.4..=0.7).contains(&score), "{score}");
}

#[tokio::test]
async fn a_phone_number_of_a_relative_does_not_make_two_people_one() {
    // `uche` has no number of her own: the number on her record is her contact's, and a person who
    // has that number is not for that reason her.
    let request = json!({"resourceType": "Patient", "birthDate": "2010-10-10",
        "name": [{"family": "Obi", "given": ["Uche"]}], "telecom": [phone("0802 222 3333")]});
    assert_eq!(
        matched(request, None, None).await,
        ids_and_grades(&[("uche", "possible")])
    );
}

#[tokio::test]
async fn names_written_in_another_order_or_with_other_letters_are_the_same_name() {
    let by = |name: Value, born: &str| json!({"resourceType": "Patient", "birthDate": born, "name": [name]});
    // The family name and the given name exchanged, and the whole name in one string.
    for name in [
        json!({"family": "Chidi", "given": ["Nwosu"]}),
        json!({"family": "CHIDI NWOSU"}),
        json!({"text": "Chidi Nwosu"}),
        json!({"text": "nwosu  chidi"}),
        json!({"family": "Nwosu", "given": ["Chidi"]}),
    ] {
        let found = matched(by(name.clone(), "1988-08-08"), None, None).await;
        assert_eq!(found, ids_and_grades(&[("chidi", "possible")]), "{name}");
    }
    // The exact order scores higher than the exchanged one.
    let exact = post_match(
        "reader",
        parameters(
            by(json!({"family": "Nwosu", "given": ["Chidi"]}), "1988-08-08"),
            None,
            None,
        ),
    )
    .await;
    let swapped = post_match(
        "reader",
        parameters(
            by(json!({"family": "Chidi", "given": ["Nwosu"]}), "1988-08-08"),
            None,
            None,
        ),
    )
    .await;
    assert!(
        exact.body["entry"][0]["search"]["score"].as_f64()
            > swapped.body["entry"][0]["search"]["score"].as_f64(),
        "{} {}",
        exact.body,
        swapped.body
    );
    // Accents and the dots under Yoruba letters, in either direction.
    for name in [
        json!({"family": "Adebayo", "given": ["Olawale"]}),
        json!({"family": "ADÉBÁYÒ", "given": ["ỌLÁWÁLÉ"]}),
        json!({"family": "Olawale", "given": ["Adebayo"]}),
    ] {
        let found = matched(by(name.clone(), "1975-01-20"), None, None).await;
        assert_eq!(found, ids_and_grades(&[("olawale", "possible")]), "{name}");
    }
}

#[tokio::test]
async fn the_same_name_with_a_different_birth_date_is_not_a_match() {
    for born in ["1992-05-06", "1992-06-05", "1993-05-05", "1992-05", "1992"] {
        let request = json!({"resourceType": "Patient", "birthDate": born,
            "name": [{"family": "Eze", "given": ["Ngozi"]}], "telecom": [phone("0803 123 4567")]});
        assert!(matched(request, None, None).await.is_empty(), "{born}");
    }
    // The same date, with another name.
    let request = json!({"resourceType": "Patient", "birthDate": "1992-05-05",
        "name": [{"family": "Eze", "given": ["Ngozi", "Grace"]}]});
    assert!(matched(request, None, None).await.is_empty());
    let request = json!({"resourceType": "Patient", "birthDate": "1992-05-05",
        "name": [{"family": "Eze"}]});
    assert!(matched(request, None, None).await.is_empty());
}

/// What an entry says it was found by: each kind, and the value it carries, if it carries one.
fn evidence(entry: &Value) -> Vec<String> {
    entry["search"]["extension"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|extension| extension["url"] == MATCH_EVIDENCE)
        .map(|extension| {
            let parts = extension["extension"].as_array().unwrap();
            let kind = parts[0]["valueCode"].as_str().unwrap();
            match parts.get(1) {
                None => kind.to_string(),
                Some(part) => {
                    assert_eq!(part["url"], "value");
                    if let Some(reference) = part.get("valueReference") {
                        return format!("{kind} {}", reference["reference"].as_str().unwrap());
                    }
                    let value = part
                        .get("valueIdentifier")
                        .or_else(|| part.get("valueContactPoint"))
                        .unwrap_or_else(|| panic!("{part}"));
                    match value.get("system").and_then(Value::as_str) {
                        Some(system) if kind == "identifier" => {
                            format!("{kind} {system}|{}", value["value"].as_str().unwrap())
                        }
                        _ => format!("{kind} {}", value["value"].as_str().unwrap()),
                    }
                }
            }
        })
        .collect()
}

#[tokio::test]
async fn each_entry_says_what_it_was_found_by() {
    let by_grade = |reply: &Reply| -> Vec<Vec<String>> {
        reply.body["entry"]
            .as_array()
            .unwrap()
            .iter()
            .map(evidence)
            .collect()
    };
    // An identifier: which one, as the request wrote it. The name and birth date are the
    // request's too, and so they are said as well.
    let request = json!({"resourceType": "Patient", "gender": "female",
        "identifier": [identifier("PRF-1001"), {"system": "urn:example:mrn", "value": "99"}],
        "birthDate": "1985-03-02", "name": [{"family": "Okafor", "given": ["Ada", "Chinwe"]}]});
    let reply = post_match("reader", parameters(request, None, None)).await;
    assert_eq!(
        by_grade(&reply),
        [[
            "identifier https://id.example.org/profile|PRF-1001",
            "name",
            "birth-date",
            "gender"
        ]]
    );

    // A probable match by a phone number, written the way the request wrote it and not the way
    // the record has it, and by an email address, and by both.
    let person = |telecom: Value| {
        json!({"resourceType": "Patient", "birthDate": "1985-03-02", "telecom": telecom,
            "name": [{"family": "Okafor", "given": ["Ada", "Chinwe"]}]})
    };
    for (telecom, expected) in [
        (
            json!([phone("0803 123 4567")]),
            vec!["name", "birth-date", "phone 0803 123 4567"],
        ),
        (
            json!([{"system": "email", "value": "ADA.Okafor@example.org"}]),
            vec!["name", "birth-date", "email ADA.Okafor@example.org"],
        ),
        (
            json!([phone("0701 000 0000"), phone("08031234567"),
                {"system": "email", "value": "ada.okafor@example.org"}]),
            vec![
                "name",
                "birth-date",
                "phone 08031234567",
                "email ada.okafor@example.org",
            ],
        ),
    ] {
        let reply = post_match("reader", parameters(person(telecom.clone()), None, None)).await;
        assert_eq!(
            entries(&reply.body),
            ids_and_grades(&[("ada", "probable")]),
            "{telecom}"
        );
        assert_eq!(by_grade(&reply), [expected], "{telecom}");
    }

    // A possible match has no phone or email in what it was found by, though the request had them.
    let reply = post_match(
        "reader",
        parameters(person(json!([phone("0803 123 4568")])), None, None),
    )
    .await;
    assert_eq!(by_grade(&reply), [["name", "birth-date"]]);

    // The name in another arrangement is a name, and is said to be reordered. A gender that
    // differs is said, so that someone looking at the match sees it.
    let reordered = json!({"resourceType": "Patient", "birthDate": "1985-03-02", "gender": "male",
        "name": [{"family": "Ada", "given": ["Okafor", "Chinwe"]}]});
    let reply = post_match("reader", parameters(reordered, None, None)).await;
    assert_eq!(
        by_grade(&reply),
        [["name", "name-reordered", "birth-date", "gender-differs"]]
    );
}

#[tokio::test]
async fn a_patient_with_nothing_to_look_up_matches_nothing() {
    for request in [
        json!({"resourceType": "Patient"}),
        json!({"resourceType": "Patient", "gender": "female"}),
        json!({"resourceType": "Patient", "name": [{"family": "Okafor", "given": ["Ada", "Chinwe"]}]}),
        json!({"resourceType": "Patient", "birthDate": "1985-03-02"}),
    ] {
        let reply = post_match("reader", parameters(request.clone(), None, None)).await;
        assert_eq!(reply.status, StatusCode::OK, "{request}: {}", reply.body);
        // An empty Bundle: FHIR JSON has no empty `entry`.
        assert_eq!(reply.body["total"], 0);
        assert!(reply.body.get("entry").is_none(), "{}", reply.body);
    }
}

#[tokio::test]
async fn results_come_best_first() {
    // `sani-a` has the identifier; `sani-b` shares only the name and the birth date.
    let request = json!({"resourceType": "Patient", "identifier": [identifier("PRF-2001")],
        "birthDate": "2001-01-01", "name": [{"family": "Danjuma", "given": ["Sani"]}]});
    assert_eq!(
        matched(request, None, None).await,
        ids_and_grades(&[("sani-a", "certain"), ("sani-b", "possible")])
    );
    // A probable match before a possible one.
    let request = json!({"resourceType": "Patient", "birthDate": "1995-05-05",
        "name": [{"family": "Garba", "given": ["Zainab"]}], "telecom": [phone("0801 111 2222")]});
    assert_eq!(
        matched(request, None, None).await,
        ids_and_grades(&[("zainab-a", "probable"), ("zainab-b", "possible")])
    );
}

#[tokio::test]
async fn only_certain_matches_returns_nothing_when_more_than_one_record_could_be_the_person() {
    let request = json!({"resourceType": "Patient", "identifier": [identifier("PRF-2001")],
        "birthDate": "2001-01-01", "name": [{"family": "Danjuma", "given": ["Sani"]}]});
    // Two potential matches: the one with the identifier is certain, and still is not returned.
    assert!(matched(request.clone(), Some(true), None).await.is_empty());
    assert_eq!(matched(request.clone(), Some(false), None).await.len(), 2);
    assert_eq!(matched(request.clone(), None, None).await.len(), 2);
    // `count` is applied after, so it cannot turn two potential matches into one certain one.
    assert!(matched(request.clone(), Some(true), Some(1))
        .await
        .is_empty());
    let one = matched(request, Some(false), Some(1)).await;
    assert_eq!(one, ids_and_grades(&[("sani-a", "certain")]));

    // The same identifier on two records: both are certain, and neither is returned for the flag.
    let twice = json!({"resourceType": "Patient", "identifier": [identifier("PRF-2002")]});
    assert_eq!(
        matched(twice.clone(), None, None).await,
        ids_and_grades(&[("kemi-a", "certain"), ("kemi-b", "certain")])
    );
    assert!(matched(twice, Some(true), None).await.is_empty());
}

#[tokio::test]
async fn only_certain_matches_returns_the_one_record_when_it_is_certain() {
    let request = json!({"resourceType": "Patient", "identifier": [identifier("PRF-1001")],
        "birthDate": "1985-03-02", "name": [{"family": "Okafor", "given": ["Ada", "Chinwe"]}]});
    assert_eq!(
        matched(request, Some(true), None).await,
        ids_and_grades(&[("ada", "certain")])
    );
    // One potential match that is not certain is not returned either.
    let request = json!({"resourceType": "Patient", "birthDate": "1978-11-30",
        "name": [{"family": "Bello", "given": ["Musa"]}]});
    assert_eq!(
        matched(request.clone(), Some(false), None).await,
        ids_and_grades(&[("musa", "possible")])
    );
    assert!(matched(request, Some(true), None).await.is_empty());
    // A probable one is not certain: whether it may be linked is the caller's to say.
    let request = json!({"resourceType": "Patient", "birthDate": "1990-07-15",
        "name": [{"family": "Adeyemi", "given": ["Tunde"]}], "telecom": [phone("+2348035550101")]});
    assert_eq!(
        matched(request.clone(), None, None).await,
        ids_and_grades(&[("tunde", "probable")])
    );
    assert!(matched(request, Some(true), None).await.is_empty());
}

#[tokio::test]
async fn count_limits_the_entries() {
    let request = json!({"resourceType": "Patient", "birthDate": "1999-09-09",
        "name": [{"family": "Yusuf", "given": ["Aisha"]}]});
    let all = matched(request.clone(), None, None).await;
    assert_eq!(all.len(), 3);
    for (count, expected) in [(0, 0), (1, 1), (2, 2), (3, 3), (10, 3)] {
        let found = matched(request.clone(), None, Some(count)).await;
        assert_eq!(found.len(), expected, "count {count}");
        // The best ones, in the same order.
        assert_eq!(found, all[..expected]);
    }
    // The best is kept, not the first found.
    let request = json!({"resourceType": "Patient", "identifier": [identifier("PRF-2001")],
        "birthDate": "2001-01-01", "name": [{"family": "Danjuma", "given": ["Sani"]}]});
    assert_eq!(
        matched(request, None, Some(1)).await,
        ids_and_grades(&[("sani-a", "certain")])
    );
}

#[tokio::test]
async fn a_record_that_was_switched_off_is_never_returned() {
    let by_identifier = json!({"resourceType": "Patient", "identifier": [identifier("PRF-3001")]});
    assert!(matched(by_identifier, None, None).await.is_empty());
    let by_name = json!({"resourceType": "Patient", "birthDate": "1980-02-02",
        "name": [{"family": "Okeke", "given": ["Obi"]}]});
    assert!(matched(by_name, None, None).await.is_empty());
}

/// The entries, as `(id, grade, score, what it was found by)`.
async fn found_by(
    request: Value,
    only_certain: Option<bool>,
) -> Vec<(String, String, f64, Vec<String>)> {
    let reply = post_match("reader", parameters(request, only_certain, None)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let grades = entries(&reply.body);
    reply.body["entry"]
        .as_array()
        .into_iter()
        .flatten()
        .zip(grades)
        .map(|(entry, (id, grade))| {
            (
                id,
                grade,
                entry["search"]["score"].as_f64().unwrap(),
                evidence(entry),
            )
        })
        .collect()
}

fn told(strings: &[&str]) -> Vec<String> {
    strings.iter().map(|text| text.to_string()).collect()
}

#[tokio::test]
async fn a_record_that_was_replaced_leads_to_the_one_that_replaced_it() {
    // Two records that were replaced by `femi` hold the identifier, and `femi` does not. Each is
    // certain by it, so `femi` is what is found, and it is not certain: no one compared `femi`
    // with the request, and the link was made by whoever merged them.
    let by_identifier = json!({"resourceType": "Patient", "identifier": [identifier("PRF-3002")]});
    let found = found_by(by_identifier.clone(), None).await;
    assert_eq!(
        found,
        [(
            "femi".to_string(),
            "probable".to_string(),
            0.95,
            // The identifier is on the replaced records, and `femi` has none, so it is not said.
            told(&[
                "replaced-record Patient/femi-old",
                "replaced-record Patient/femi-older",
            ])
        )]
    );
    // It is the one record, and it is not what a request that will not be linked on a guess may
    // be given.
    assert!(found_by(by_identifier, Some(true)).await.is_empty());

    // `femi` has the name and the birth date, which is possible on its own. Reached through the
    // records that were replaced, which are better, it is probable, once.
    let request = json!({"resourceType": "Patient", "identifier": [identifier("PRF-3002")],
        "birthDate": "1983-04-04", "name": [{"family": "Johnson", "given": ["Femi"]}]});
    let found = found_by(request, None).await;
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].0.as_str(), found[0].1.as_str(), found[0].2),
        ("femi", "probable", 0.95)
    );
    assert_eq!(
        found[0].3,
        told(&[
            "replaced-record Patient/femi-old",
            "replaced-record Patient/femi-older",
            "name",
            "birth-date",
        ])
    );
    // Without the identifier it is what the name and birth date make it: possible, and found
    // itself and not through a replaced record.
    let by_name = json!({"resourceType": "Patient", "birthDate": "1983-04-04",
        "name": [{"family": "Johnson", "given": ["Femi"]}]});
    let found = found_by(by_name, None).await;
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].0.as_str(), found[0].1.as_str()),
        ("femi", "possible")
    );
    assert_eq!(found[0].3, told(&["name", "birth-date"]));
}

#[tokio::test]
async fn a_chain_of_replacements_is_followed_to_the_end() {
    // `chain-a` holds the identifier and was replaced by `chain-b`, which was replaced by
    // `chain-c`, which is in use.
    let by_identifier = json!({"resourceType": "Patient", "identifier": [identifier("PRF-4001")]});
    let found = found_by(by_identifier, None).await;
    assert_eq!(
        found,
        [(
            "chain-c".to_string(),
            "probable".to_string(),
            0.95,
            told(&["replaced-record Patient/chain-a"])
        )]
    );
    // All three have the name and the birth date, and lead to the same one: it is possible, once,
    // and found by itself.
    let by_name = json!({"resourceType": "Patient", "birthDate": "1969-06-06",
        "name": [{"family": "Balogun", "given": ["Segun"]}]});
    let found = found_by(by_name, None).await;
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].0.as_str(), found[0].1.as_str()),
        ("chain-c", "possible")
    );
    assert_eq!(found[0].3, told(&["name", "birth-date"]));
}

#[tokio::test]
async fn a_replacement_that_leads_to_no_patient_in_use_is_not_a_match() {
    // `end-a` was replaced by `end-b`, which was switched off.
    let by_identifier = json!({"resourceType": "Patient", "identifier": [identifier("PRF-5001")]});
    assert!(matched(by_identifier, None, None).await.is_empty());
    let by_name = json!({"resourceType": "Patient", "birthDate": "1972-02-02",
        "name": [{"family": "Fashola", "given": ["Kunle"]}]});
    assert!(matched(by_name, None, None).await.is_empty());
}

#[tokio::test]
async fn a_request_that_cannot_be_read_is_a_400_with_an_operation_outcome() {
    let patient = json!({"resourceType": "Patient", "identifier": [identifier("PRF-1001")]});
    let resource = json!({"name": "resource", "resource": patient});
    let bad_bodies = [
        ("an empty body", None),
        ("not JSON", Some("{\"resourceType\": ".to_string())),
        ("a Patient and not Parameters", Some(patient.to_string())),
        ("no parameters", Some(json!({"resourceType": "Parameters"}).to_string())),
        (
            "no resource",
            Some(json!({"resourceType": "Parameters", "parameter": [{"name": "count", "valueInteger": 1}]}).to_string()),
        ),
        (
            "a resource that is not a Patient",
            Some(parameters(json!({"resourceType": "Observation", "status": "final"}), None, None).to_string()),
        ),
        (
            "a resource given twice",
            Some(json!({"resourceType": "Parameters", "parameter": [resource, resource]}).to_string()),
        ),
        (
            "a parameter it does not know",
            Some(json!({"resourceType": "Parameters", "parameter": [resource, {"name": "onlyCertainMatch", "valueBoolean": true}]}).to_string()),
        ),
        (
            "onlyCertainMatches that is not a boolean",
            Some(json!({"resourceType": "Parameters", "parameter": [resource, {"name": "onlyCertainMatches", "valueString": "true"}]}).to_string()),
        ),
        (
            "a negative count",
            Some(json!({"resourceType": "Parameters", "parameter": [resource, {"name": "count", "valueInteger": -1}]}).to_string()),
        ),
        (
            "a birth date that is not a date",
            Some(parameters(json!({"resourceType": "Patient", "birthDate": "2nd March 1985"}), None, None).to_string()),
        ),
        (
            "a name that is a string",
            Some(parameters(json!({"resourceType": "Patient", "name": "Ada Okafor"}), None, None).to_string()),
        ),
    ];
    for (what, body) in bad_bodies {
        let (status, text) = send_text(
            &seeded(),
            "POST",
            "/Patient/$match?access_token=reader",
            body.as_deref().unwrap_or(""),
            &[("content-type", "application/fhir+json")],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {text}");
        let outcome: Value =
            serde_json::from_str(&text).unwrap_or_else(|_| panic!("{what}: {text}"));
        assert_eq!(outcome["resourceType"], "OperationOutcome", "{what}");
        assert_eq!(outcome["issue"][0]["severity"], "error", "{what}");
        assert!(
            outcome["issue"][0]["diagnostics"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "{what}: {text}"
        );
    }
}

#[tokio::test]
async fn an_xml_body_is_refused_for_what_it_is() {
    // The standard XML form of the request. It is not read, and it is not answered as if it had no
    // resource in it.
    let xml = r#"<Parameters xmlns="http://hl7.org/fhir"><parameter><name value="resource"/><resource><Patient><identifier><system value="https://id.example.org/profile"/><value value="PRF-1001"/></identifier></Patient></resource></parameter></Parameters>"#;
    let (status, text) = send_text(
        &seeded(),
        "POST",
        "/Patient/$match?access_token=reader",
        xml,
        &[("content-type", "application/fhir+xml")],
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{text}");
    let outcome: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(outcome["resourceType"], "OperationOutcome");
    assert!(outcome["issue"][0]["diagnostics"]
        .as_str()
        .unwrap()
        .contains("XML"));
}

#[tokio::test]
async fn it_is_a_post() {
    let get = send(
        &seeded(),
        "GET",
        "/Patient/$match?access_token=reader",
        None,
        None,
    )
    .await;
    assert_eq!(get.status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn the_capability_statement_lists_the_operation() {
    // `/metadata` is open, with or without a token.
    let metadata = send(&seeded(), "GET", "/metadata", None, None).await;
    assert_eq!(metadata.status, StatusCode::OK);
    let resources = metadata.body["rest"][0]["resource"].as_array().unwrap();
    let operations = |name: &str| -> Vec<Value> {
        resources
            .iter()
            .find(|resource| resource["type"] == name)
            .unwrap_or_else(|| panic!("no {name}"))["operation"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    };
    let patient = operations("Patient");
    let found = patient
        .iter()
        .find(|operation| operation["name"] == "match")
        .expect("match");
    assert_eq!(
        found["definition"],
        "http://hl7.org/fhir/OperationDefinition/Patient-match"
    );
    let documentation = found["documentation"].as_str().unwrap();
    assert!(documentation.contains("onlyCertainMatches"));
    // The managingOrganization of the Patient sent limits it to one organization's Patients, and
    // there is no parameter of its own for that.
    assert!(
        documentation.contains("`managingOrganization` of the Patient in `resource`"),
        "{documentation}"
    );
    assert!(
        !documentation.contains("?organization=Organization/id"),
        "{documentation}"
    );
    // And only Patient has it.
    for resource in resources
        .iter()
        .filter(|resource| resource["type"] != "Patient")
    {
        let names: Vec<_> = resource["operation"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|operation| operation["name"].clone())
            .collect();
        assert!(!names.contains(&json!("match")), "{}", resource["type"]);
    }
}

#[tokio::test]
async fn it_needs_a_token_that_may_search_patients() {
    let request = parameters(
        json!({"resourceType": "Patient", "identifier": [identifier("PRF-1001")]}),
        None,
        None,
    );
    let ok = |reply: &Reply| reply.status == StatusCode::OK && reply.body["total"] == 1;
    // Searching Patients is what it is. A scope that only reads them (v1 `read`, v2 `rs`) may.
    for token in ["reader", "searcher", "system"] {
        let reply = post_match(token, request.clone()).await;
        assert!(ok(&reply), "{token}: {} {}", reply.status, reply.body);
    }
    // A scope that may only create Patients cannot find them, and one for another type may not.
    for token in ["creator", "observer"] {
        let reply = post_match(token, request.clone()).await;
        assert_eq!(
            reply.status,
            StatusCode::FORBIDDEN,
            "{token}: {}",
            reply.body
        );
        assert_eq!(reply.body["resourceType"], "OperationOutcome");
    }
    // Nobody at all, and somebody unknown.
    let nobody = send(
        &seeded(),
        "POST",
        "/Patient/$match",
        Some(request.clone()),
        None,
    )
    .await;
    assert_eq!(nobody.status, StatusCode::UNAUTHORIZED);
    let stranger = post_match("not-a-token", request.clone()).await;
    assert_eq!(stranger.status, StatusCode::UNAUTHORIZED);
    // A patient's own token reads one record. `$match` reads them all, so it is refused, and what
    // it was refused for is not in the answer.
    let own = post_match("patient", request).await;
    assert_eq!(own.status, StatusCode::FORBIDDEN, "{}", own.body);
    assert!(!own.body.to_string().contains("PRF-1001"));
}

/// `body`, the Parameters of `parameters`, with the Patient in it as the Patient of `organization`
/// (`Organization/id`): the way the operation is told whose Patients to compare with.
fn in_organization(mut body: Value, organization: &str) -> Value {
    body["parameter"][0]["resource"]["managingOrganization"] = json!({"reference": organization});
    body
}

/// `$match` for the Patient of one organization, answered: the id and the grade of each entry, in order.
async fn matched_in(
    organization: &str,
    patient: Value,
    only_certain: Option<bool>,
    count: Option<u32>,
) -> Vec<(String, String)> {
    let body = in_organization(parameters(patient, only_certain, count), organization);
    let reply = post_match("reader", body).await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "{organization}: {}",
        reply.body
    );
    entries(&reply.body)
}

/// Bola Ajayi as `clinic-a` knows her, and `sender-b` too: the name, the birth date and the number.
fn bola() -> Value {
    json!({"resourceType": "Patient", "birthDate": "1991-04-12", "telecom": [phone("0805 600 7001")],
        "name": [{"family": "Ajayi", "given": ["Bola"]}]})
}

/// Her Boolbyte ID, which both organizations have given her.
fn bola_by_id() -> Value {
    json!({"resourceType": "Patient", "identifier": [boolbyte_id("BB-1")]})
}

#[tokio::test]
async fn without_a_managing_organization_the_patients_of_every_organization_are_compared() {
    // The same person at two organizations is two matches: by the identifier they share, and by
    // what they have in common otherwise.
    assert_eq!(
        matched(bola_by_id(), None, None).await,
        ids_and_grades(&[("bola-a", "certain"), ("bola-b", "certain")])
    );
    assert_eq!(
        matched(bola(), None, None).await,
        ids_and_grades(&[("bola-a", "probable"), ("bola-b", "probable")])
    );
    // So the flag, which is for linking without a person looking, finds nothing to link.
    assert!(matched(bola_by_id(), Some(true), None).await.is_empty());
}

#[tokio::test]
async fn the_managing_organization_limits_the_candidates_to_its_own_patients() {
    for (organization, own) in [("clinic-a", "bola-a"), ("sender-b", "bola-b")] {
        let reference = format!("Organization/{organization}");
        // By the identifier, which is an `identifier` search, and by the birth date, which is a
        // `birthdate` one: both are limited.
        assert_eq!(
            matched_in(&reference, bola_by_id(), None, None).await,
            ids_and_grades(&[(own, "certain")]),
            "{organization}"
        );
        assert_eq!(
            matched_in(&reference, bola(), None, None).await,
            ids_and_grades(&[(own, "probable")]),
            "{organization}"
        );
        // Both together: the record of the other organization has the same evidence, and is left out.
        let both = with(bola(), "identifier", json!([boolbyte_id("BB-1")]));
        let found = matched_in(&reference, both, None, None).await;
        assert_eq!(found, ids_and_grades(&[(own, "certain")]), "{organization}");
    }
    // The record that is returned is the whole of the Patient, as it is without a scope.
    let reply = post_match(
        "reader",
        in_organization(
            parameters(bola_by_id(), None, None),
            "Organization/clinic-a",
        ),
    )
    .await;
    assert_eq!(
        reply.body["entry"][0]["resource"]["managingOrganization"]["reference"],
        "Organization/clinic-a"
    );
    assert_eq!(reply.body["entry"][0]["search"]["score"], 1.0);
}

#[tokio::test]
async fn the_managing_organization_is_no_evidence_and_changes_no_grade_or_score() {
    // What is sent is the same Patient with and without it; the entry for the organization's own
    // record has the same score and the same evidence, and none of it names the organization.
    for request in [bola_by_id(), bola()] {
        let unscoped = post_match("reader", parameters(request.clone(), None, None)).await;
        let scoped = post_match(
            "reader",
            in_organization(parameters(request, None, None), "Organization/clinic-a"),
        )
        .await;
        assert_eq!(scoped.status, StatusCode::OK, "{}", scoped.body);
        let own = unscoped.body["entry"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["resource"]["id"] == "bola-a")
            .unwrap();
        assert_eq!(scoped.body["entry"][0]["search"], own["search"]);
        for evidence in evidence(&scoped.body["entry"][0]) {
            assert!(!evidence.contains("rganization"), "{evidence}");
        }
    }
}

#[tokio::test]
async fn an_organization_with_no_match_is_an_empty_searchset() {
    // An organization that no Patient has, one that has Patients and not this person, and Patients
    // of no organization at all: none is in any organization's scope.
    let musa = json!({"resourceType": "Patient", "birthDate": "1978-11-30",
        "name": [{"family": "Bello", "given": ["Musa"]}]});
    assert_eq!(matched(musa.clone(), None, None).await.len(), 1);
    for (organization, request) in [
        ("Organization/nowhere", bola_by_id()),
        ("Organization/nowhere", bola()),
        ("Organization/clinic-a", musa.clone()),
        ("Organization/sender-b", musa),
        // The reference is compared as written, as a search does: an id is not another's prefix.
        ("Organization/clinic", bola_by_id()),
        ("Organization/clinic-a1", bola_by_id()),
        ("Organization/CLINIC-A", bola_by_id()),
    ] {
        let reply = post_match(
            "reader",
            in_organization(parameters(request, None, None), organization),
        )
        .await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{organization}: {}",
            reply.body
        );
        assert_eq!(reply.body["resourceType"], "Bundle", "{organization}");
        assert_eq!(reply.body["type"], "searchset", "{organization}");
        assert_eq!(reply.body["total"], 0, "{organization}");
        assert!(reply.body.get("entry").is_none(), "{}", reply.body);
    }
}

#[tokio::test]
async fn only_certain_matches_is_decided_among_the_organizations_patients() {
    // Two certain matches without a scope, and so nothing for the flag; with one, the
    // organization's one record.
    assert!(matched(bola_by_id(), Some(true), None).await.is_empty());
    for (organization, own) in [("clinic-a", "bola-a"), ("sender-b", "bola-b")] {
        let reference = format!("Organization/{organization}");
        assert_eq!(
            matched_in(&reference, bola_by_id(), Some(true), None).await,
            ids_and_grades(&[(own, "certain")]),
            "{organization}"
        );
        // A match that is not certain is still not returned for the flag, scoped or not.
        assert!(
            matched_in(&reference, bola(), Some(true), None)
                .await
                .is_empty(),
            "{organization}"
        );
    }
    // More than one potential match in the organization is still more than one.
    let sani = json!({"resourceType": "Patient", "identifier": [identifier("PRF-2001")],
        "birthDate": "2001-01-01", "name": [{"family": "Danjuma", "given": ["Sani"]}]});
    assert!(matched(sani, Some(true), None).await.is_empty());
}

#[tokio::test]
async fn count_is_applied_to_the_organizations_patients() {
    // `dayo-b` has her number, so for this request it is probable, and `dayo-a` is possible.
    let request = json!({"resourceType": "Patient", "birthDate": "1987-07-07",
        "name": [{"family": "Peters", "given": ["Dayo"]}], "telecom": [phone("0806 700 8001")]});
    assert_eq!(
        matched(request.clone(), None, Some(1)).await,
        ids_and_grades(&[("dayo-b", "probable")])
    );
    assert_eq!(
        matched(request.clone(), None, Some(2)).await,
        ids_and_grades(&[("dayo-b", "probable"), ("dayo-a", "possible")])
    );
    // The other organization's better match does not take `clinic-a`'s one place.
    assert_eq!(
        matched_in("Organization/clinic-a", request.clone(), None, Some(1)).await,
        ids_and_grades(&[("dayo-a", "possible")])
    );
    assert_eq!(
        matched_in("Organization/sender-b", request.clone(), None, Some(1)).await,
        ids_and_grades(&[("dayo-b", "probable")])
    );
    // And the count is of what is left: 0 is nothing, and more than there is, is all there is.
    assert!(
        matched_in("Organization/clinic-a", request.clone(), None, Some(0))
            .await
            .is_empty()
    );
    assert_eq!(
        matched_in("Organization/clinic-a", request, None, Some(10)).await,
        ids_and_grades(&[("dayo-a", "possible")])
    );
}

#[tokio::test]
async fn a_replacement_in_another_organization_is_not_followed_out_of_the_scope() {
    // `merge-a` of `clinic-a` was replaced by `merge-b` of `sender-b`. Without a scope the
    // person is found at `sender-b`. Limited to `clinic-a` the link leaves the organization, and
    // limited to `sender-b` the record that has the identifier is not a candidate.
    let by_identifier = json!({"resourceType": "Patient", "identifier": [boolbyte_id("BB-2")]});
    assert_eq!(
        matched(by_identifier.clone(), None, None).await,
        ids_and_grades(&[("merge-b", "probable")])
    );
    for organization in ["Organization/clinic-a", "Organization/sender-b"] {
        assert!(
            matched_in(organization, by_identifier.clone(), None, None)
                .await
                .is_empty(),
            "{organization}"
        );
    }
    // By name, the same: `merge-b` itself is possible for `sender-b`, found as itself.
    let by_name = json!({"resourceType": "Patient", "birthDate": "1982-02-02",
        "name": [{"family": "Okon", "given": ["Rita"]}]});
    assert_eq!(
        matched_in("Organization/sender-b", by_name.clone(), None, None).await,
        ids_and_grades(&[("merge-b", "possible")])
    );
    assert!(matched_in("Organization/clinic-a", by_name, None, None)
        .await
        .is_empty());

    // A link that leaves the organization and comes back in is no way in: `ring-a` and `ring-c`
    // are `clinic-a`'s, and what is between them is `sender-b`'s.
    let ring = json!({"resourceType": "Patient", "identifier": [boolbyte_id("BB-3")]});
    assert_eq!(
        matched(ring.clone(), None, None).await,
        ids_and_grades(&[("ring-c", "probable")])
    );
    assert!(matched_in("Organization/clinic-a", ring, None, None)
        .await
        .is_empty());
    // `ring-c` is still `clinic-a`'s, and is found as itself by what it has.
    let by_name = json!({"resourceType": "Patient", "birthDate": "1979-09-19",
        "name": [{"family": "Dike", "given": ["Ngozi"]}]});
    let reply = post_match(
        "reader",
        in_organization(parameters(by_name, None, None), "Organization/clinic-a"),
    )
    .await;
    assert_eq!(
        entries(&reply.body),
        ids_and_grades(&[("ring-c", "possible")])
    );
    assert_eq!(evidence(&reply.body["entry"][0]), ["name", "birth-date"]);

    // A link inside the organization is followed as it is without a scope.
    let own = json!({"resourceType": "Patient", "identifier": [boolbyte_id("BB-4")]});
    for found in [
        matched(own.clone(), None, None).await,
        matched_in("Organization/clinic-a", own.clone(), None, None).await,
    ] {
        assert_eq!(found, ids_and_grades(&[("own-b", "probable")]));
    }
    let reply = post_match(
        "reader",
        in_organization(parameters(own, None, None), "Organization/clinic-a"),
    )
    .await;
    assert_eq!(
        evidence(&reply.body["entry"][0]),
        ["replaced-record Patient/own-a"]
    );
}

/// `$match` with the text `body`, answered with the status and the OperationOutcome.
async fn refused(body: String) -> (StatusCode, Value) {
    let (status, reply) = send_text(
        &seeded(),
        "POST",
        "/Patient/$match?access_token=reader",
        &body,
        &[("content-type", "application/fhir+json")],
    )
    .await;
    let outcome: Value = serde_json::from_str(&reply).unwrap_or_else(|_| panic!("{reply}"));
    (status, outcome)
}

#[tokio::test]
async fn a_managing_organization_that_is_not_a_reference_to_one_is_a_400() {
    let sent = |managing: Value| {
        parameters(
            with(bola_by_id(), "managingOrganization", managing),
            None,
            None,
        )
        .to_string()
    };
    let given = |reference: &str| sent(json!({"reference": reference}));
    let bad_bodies = [
        ("an id and not a reference", given("clinic-a")),
        ("a reference to nobody", given("Organization/")),
        ("an empty string", given("")),
        ("an id with a space", given("Organization/clinic a")),
        (
            "an id of 65 characters",
            given(&format!("Organization/{}", "a".repeat(65))),
        ),
        (
            "an id with a slash",
            given("Organization/clinic-a/_history/1"),
        ),
        (
            "two organizations",
            given("Organization/clinic-a,Organization/sender-b"),
        ),
        ("a reference to a Patient", given("Patient/bola-a")),
        (
            "another type's name in lower case",
            given("organization/clinic-a"),
        ),
        (
            "a URL",
            given("https://example.org/fhir/Organization/clinic-a"),
        ),
        ("a contained reference", given("#clinic-a")),
        (
            "a reference that is an identifier",
            sent(json!({"identifier": {"system": "urn:example", "value": "clinic-a"}})),
        ),
        (
            "a display and nothing else",
            sent(json!({"display": "Clinic A"})),
        ),
        ("an empty reference", sent(json!({}))),
    ];
    for (what, body) in bad_bodies {
        let (status, outcome) = refused(body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {outcome}");
        assert_eq!(outcome["resourceType"], "OperationOutcome", "{what}");
        assert_eq!(outcome["issue"][0]["code"], "invalid", "{what}");
        let diagnostics = outcome["issue"][0]["diagnostics"].as_str().unwrap_or("");
        assert!(
            diagnostics.contains("managingOrganization"),
            "{what}: {outcome}"
        );
    }
}

#[tokio::test]
async fn an_organization_is_not_a_parameter() {
    // The scope is the Patient sent, so a parameter of that name is one that $match does not take,
    // and is refused as any other is, whatever its value, and whether or not it agrees with the
    // Patient.
    let resource = json!({"name": "resource", "resource": bola_by_id()});
    for (what, organization) in [
        (
            "a reference",
            json!({"name": "organization", "valueReference": {"reference": "Organization/clinic-a"}}),
        ),
        (
            "a string",
            json!({"name": "organization", "valueString": "Organization/clinic-a"}),
        ),
        ("no value", json!({"name": "organization"})),
    ] {
        let body = json!({"resourceType": "Parameters", "parameter": [resource, organization]});
        let (status, outcome) = refused(body.to_string()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {outcome}");
        assert_eq!(
            outcome["issue"][0]["diagnostics"],
            "unknown parameter organization: $match takes resource, onlyCertainMatches and count",
            "{what}"
        );
    }
    // The same in a plain object, which holds the three parameters as members.
    let (status, outcome) = refused(
        json!({"resource": bola_by_id(), "organization": "Organization/clinic-a"}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{outcome}");
    assert!(
        outcome["issue"][0]["diagnostics"]
            .as_str()
            .unwrap()
            .starts_with("unknown parameter organization"),
        "{outcome}"
    );
    // And a query parameter is not a way to say it either: it is refused too, not ignored, which
    // would answer for every organization. Any other query parameter is ignored, as before.
    for (query, status, entries_found) in [
        (
            "organization=Organization/clinic-a",
            StatusCode::BAD_REQUEST,
            0,
        ),
        (
            "organization=Organization%2Fsender-b",
            StatusCode::BAD_REQUEST,
            0,
        ),
        ("organization", StatusCode::BAD_REQUEST, 0),
        ("_format=json&organization%3D=x&other=1", StatusCode::OK, 2),
        ("_format=json&other=organization", StatusCode::OK, 2),
    ] {
        let url = format!("/Patient/$match?access_token=reader&{query}");
        let reply = send(
            &seeded(),
            "POST",
            &url,
            Some(parameters(bola_by_id(), None, None)),
            None,
        )
        .await;
        assert_eq!(reply.status, status, "{query}: {}", reply.body);
        if status == StatusCode::BAD_REQUEST {
            assert_eq!(
                reply.body["issue"][0]["diagnostics"],
                "unknown parameter organization: $match takes resource, onlyCertainMatches and count",
                "{query}"
            );
        } else {
            assert_eq!(entries(&reply.body).len(), entries_found, "{query}");
        }
    }
}

#[tokio::test]
async fn a_parameter_list_that_is_not_a_list_is_refused_for_that_with_or_without_the_query() {
    // The query does not replace what is wrong with the body: it is the body that is refused.
    let body = json!({"resourceType": "Parameters", "parameter": {"name": "resource"}});
    for query in ["", "&_format=json"] {
        let url = format!("/Patient/$match?access_token=reader{query}");
        let reply = send(&seeded(), "POST", &url, Some(body.clone()), None).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{query}");
        let diagnostics = reply.body["issue"][0]["diagnostics"].as_str().unwrap();
        assert!(
            diagnostics.contains("must be an array"),
            "{query}: {diagnostics}"
        );
    }
}

#[test]
fn a_patient_token_is_refused_whatever_the_route_to_it() {
    let (db, catalog, _store) = crate::tests::memory();
    let loaded = catalog.read().unwrap();
    let args = json!({"resource": {"resourceType": "Patient", "id": "p1"}});
    // The guard is the one a live query goes through, with the patient's own id in the body.
    assert!(crate::auth::guard_patient_operation(
        &db,
        &loaded,
        "Patient/p1",
        "Patient/$match",
        &args
    )
    .is_err());
    // It is not a live query's to run in the first place.
    let refused = crate::auth::SyncSession::default()
        .prepare("Patient/$match", &args, false)
        .unwrap_err();
    assert!(refused.contains("unsupported operation"), "{refused}");
}
