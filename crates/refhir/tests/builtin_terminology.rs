//! The terminology the server carries itself (ISO 4217), through a real start: installed after the
//! packages that refer to it, expanded for the value sets that were waiting for it, and installed
//! once.

mod common;

use std::path::Path;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use common::{free_port, start, Scratch};
use serde_json::Value;

/// A package of the one value set HL7 binds `Money.currency` to, which includes ISO 4217 and
/// nothing in the package holds it.
fn currencies_package(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"example.currencies","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("ValueSet-currencies.json"),
        r#"{"resourceType":"ValueSet","id":"currencies","url":"http://hl7.org/fhir/ValueSet/currencies","status":"active","compose":{"include":[{"system":"urn:iso:std:iso:4217"}]}}"#,
    )
    .unwrap();
}

/// A running server, stopped when this is dropped, so that a test that fails does not leave it
/// running with its database and its port.
struct Server {
    child: Child,
    bind: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A server on `database` with the package, once it answers.
fn serve(dir: &Path, database: &Path, package: &Path) -> Server {
    let bind = format!("127.0.0.1:{}", free_port());
    let log = std::fs::File::create(dir.join("serve.log")).unwrap();
    let mut child = start(
        dir,
        &[
            "--sqlite",
            database.to_str().unwrap(),
            "--package",
            package.to_str().unwrap(),
            "--bind",
            &bind,
        ],
        &[],
        Stdio::from(log.try_clone().unwrap()),
        Stdio::from(log),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "exited ({status}) before it was ready:\n{}",
                std::fs::read_to_string(dir.join("serve.log")).unwrap_or_default()
            );
        }
        if ureq::get(&format!("http://{bind}/readyz"))
            .timeout(Duration::from_secs(2))
            .call()
            .is_ok()
        {
            return Server { child, bind };
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "not ready in 60s:\n{}",
                std::fs::read_to_string(dir.join("serve.log")).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn get(bind: &str, path: &str) -> Value {
    let body = ureq::get(&format!("http://{bind}{path}"))
        .call()
        .unwrap_or_else(|err| panic!("GET {path}: {err}"))
        .into_string()
        .unwrap();
    serde_json::from_str(&body).unwrap()
}

fn codes(expanded: &Value) -> Vec<String> {
    expanded["expansion"]["contains"]
        .as_array()
        .unwrap_or_else(|| panic!("{expanded}"))
        .iter()
        .map(|entry| entry["code"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_started_server_expands_the_currencies_it_carries_and_installs_them_once() {
    let dir = Scratch::new("builtin-terminology");
    let package = dir.join("currencies");
    currencies_package(&package);
    let database = dir.join("refhir.db");
    let expand = "/ValueSet/$expand?url=http://hl7.org/fhir/ValueSet/currencies";

    // The first start installs the package, then the terminology the value set was waiting for.
    let first = serve(&dir, &database, &package);
    let all = codes(&get(&first.bind, expand));
    assert_eq!(all.len(), 178);
    for code in ["NGN", "USD", "GBP"] {
        assert!(all.contains(&code.to_string()), "{code}");
    }
    assert_eq!(
        codes(&get(&first.bind, &format!("{expand}&filter=naira"))),
        ["NGN"]
    );
    drop(first);
    let log = std::fs::read_to_string(dir.join("serve.log")).unwrap_or_default();
    assert!(
        log.contains("installed refhir.builtin.terminology@1.0.0"),
        "the first start did not say it installed it:\n{log}"
    );

    // A second start on the same database finds it there and answers the same.
    let second = serve(&dir, &database, &package);
    assert_eq!(codes(&get(&second.bind, expand)), all);
    let log = std::fs::read_to_string(dir.join("serve.log")).unwrap_or_default();
    assert!(
        !log.contains("refhir.builtin.terminology"),
        "installed again:\n{log}"
    );
}
