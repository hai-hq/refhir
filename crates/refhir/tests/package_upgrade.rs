//! A server that is started again, on the same database, with a newer version of a package it
//! already holds: through the real binary, the way a deployment moves to a new release of its
//! implementation guide. It starts, and serves what the newer version defines.

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

mod common;
use common::{free_port, start, Scratch};

const MINI_R4: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../fhir/fixtures/minir4");
const PACKAGE: &str = "example.upgrade";

/// Version `version` of a package of string search parameters on Patient, each as
/// (name, code, expression); the name makes the parameter's URL.
fn package(dir: &Path, version: &str, parameters: &[(&str, &str, &str)]) -> PathBuf {
    let dir = dir.join(format!("package-{version}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        json!({"name": PACKAGE, "version": version, "fhirVersion": "4.0.1"}).to_string(),
    )
    .unwrap();
    for (name, code, expression) in parameters {
        std::fs::write(
            dir.join(format!("SearchParameter-{name}.json")),
            json!({
                "resourceType": "SearchParameter",
                "url": format!("https://example.org/fhir/SearchParameter/{name}"),
                "name": name,
                "status": "active",
                "code": code,
                "base": ["Patient"],
                "type": "string",
                "expression": expression
            })
            .to_string(),
        )
        .unwrap();
    }
    dir
}

/// A server on the database, stopped when this is dropped.
struct Server {
    child: Child,
    base: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    /// Starts the server on `database` with the small R4 package and `package`, and waits until it
    /// answers. One that exits first fails the test with what it logged.
    fn start(dir: &Path, database: &Path, package: &Path, run: usize) -> Server {
        let bind = format!("127.0.0.1:{}", free_port());
        let log = dir.join(format!("server-{run}.log"));
        let file = std::fs::File::create(&log).unwrap();
        let mut child = start(
            dir,
            &[
                "--sqlite",
                database.to_str().unwrap(),
                "--bind",
                &bind,
                "--update-as-create",
                "--package",
                MINI_R4,
                "--package",
                package.to_str().unwrap(),
            ],
            &[],
            Stdio::from(file.try_clone().unwrap()),
            Stdio::from(file),
        );
        let base = format!("http://{bind}");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                panic!(
                    "the server exited ({status}) before it was ready:\n{}",
                    std::fs::read_to_string(&log).unwrap_or_default()
                );
            }
            if ureq::get(&format!("{base}/readyz"))
                .timeout(Duration::from_secs(2))
                .call()
                .is_ok()
            {
                return Server { child, base };
            }
            assert!(
                Instant::now() < deadline,
                "the server was not ready in 60s:\n{}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The body of a GET, or the status it was refused with.
    fn get(&self, path: &str) -> Result<Value, u16> {
        match ureq::get(&format!("{}{path}", self.base)).call() {
            Ok(response) => Ok(serde_json::from_str(&response.into_string().unwrap()).unwrap()),
            Err(ureq::Error::Status(status, _)) => Err(status),
            Err(err) => panic!("GET {path}: {err}"),
        }
    }

    fn total(&self, query: &str) -> u64 {
        self.get(&format!("/Patient?{query}"))
            .unwrap_or_else(|status| panic!("{query}: {status}"))["total"]
            .as_u64()
            .unwrap()
    }

    fn put_patient(&self, id: &str, family: &str, birth_date: &str) {
        ureq::put(&format!("{}/Patient/{id}", self.base))
            .set("content-type", "application/fhir+json")
            .send_string(
                &json!({
                    "resourceType": "Patient",
                    "id": id,
                    "name": [{"family": family}],
                    "birthDate": birth_date
                })
                .to_string(),
            )
            .unwrap();
    }

    /// The Patient search parameters the CapabilityStatement lists.
    fn advertised(&self) -> Vec<String> {
        let statement = self.get("/metadata").unwrap();
        statement["rest"][0]["resource"]
            .as_array()
            .unwrap()
            .iter()
            .find(|resource| resource["type"] == "Patient")
            .unwrap()["searchParam"]
            .as_array()
            .unwrap()
            .iter()
            .map(|param| param["name"].as_str().unwrap().to_string())
            .collect()
    }
}

#[test]
fn a_server_started_on_a_newer_package_serves_what_the_newer_version_defines() {
    let dir = Scratch::new("package-upgrade");
    let database = dir.join("refhir.sqlite");
    let v1 = package(&dir, "0.1.0", &[("label", "label", "Patient.name")]);
    let v2 = package(&dir, "0.2.0", &[("label", "label", "Patient.birthDate")]);
    let v3 = package(&dir, "0.3.0", &[("label", "alias", "Patient.birthDate")]);

    // 0.1.0: the parameter reads the name.
    {
        let server = Server::start(&dir, &database, &v1, 1);
        server.put_patient("p1", "Smith", "1990-05-05");
        assert_eq!(server.total("label=Smith"), 1);
        assert_eq!(server.total("label=1990"), 0);
    }

    // 0.2.0 reads the birth date instead. The server starts and the resource that was stored is
    // found by it.
    {
        let server = Server::start(&dir, &database, &v2, 2);
        assert_eq!(server.total("label=1990"), 1);
        assert_eq!(server.total("label=Smith"), 0);
        server.put_patient("p2", "Jones", "2001-02-03");
        assert_eq!(server.total("label=2001"), 1);
    }

    // 0.3.0 gives it another code: the old one is refused and not advertised.
    {
        let server = Server::start(&dir, &database, &v3, 3);
        assert_eq!(server.get("/Patient?label=1990"), Err(400));
        assert_eq!(server.total("alias=1990"), 1);
        assert_eq!(server.total("alias=2001"), 1);
        let advertised = server.advertised();
        assert!(advertised.contains(&"alias".to_string()), "{advertised:?}");
        assert!(!advertised.contains(&"label".to_string()), "{advertised:?}");
    }

    // A server that is started on 0.1.0 again, or on 0.3.0, keeps what the newest version
    // defines.
    for (run, older) in [(4, &v1), (5, &v3)] {
        let server = Server::start(&dir, &database, older, run);
        assert_eq!(server.get("/Patient?label=1990"), Err(400), "run {run}");
        assert_eq!(server.total("alias=1990"), 1, "run {run}");
        let advertised = server.advertised();
        assert!(!advertised.contains(&"label".to_string()), "{advertised:?}");
    }

    // An older release that this database never held is not installed either. It names a parameter
    // that the newest version has under the same code, so installing it would be refused.
    let never = package(&dir, "0.0.5", &[("label", "alias", "Patient.name")]);
    let server = Server::start(&dir, &database, &never, 6);
    assert_eq!(server.total("alias=1990"), 1);
    let log = std::fs::read_to_string(dir.join("server-6.log")).unwrap();
    assert!(
        log.contains("not installing example.upgrade@0.0.5"),
        "{log}"
    );
}
