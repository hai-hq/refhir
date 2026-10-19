//! `refhir package-bundle`, and a first start that installs what it made: no registry reached.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::{exits, free_port, serves, start, Scratch};

/// A registry nothing listens on: any use of it fails at once.
const UNREACHABLE_REGISTRY: &str = "http://127.0.0.1:1";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fhir/fixtures")
        .join(name)
}

/// The mini R4 fixture, named as the package a server installs first, so that a prepared file of
/// it and the SDC fixture holds what the server asks for by default.
fn core_named_as_released(dir: &Path) -> PathBuf {
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    for entry in std::fs::read_dir(fixture("minir4")).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), core.join(entry.file_name())).unwrap();
    }
    std::fs::write(
        core.join("package.json"),
        r#"{"name":"hl7.fhir.r4.core","version":"4.0.1","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    core
}

/// A file with the packages the server installs by default.
fn prepared_defaults(dir: &Path) -> PathBuf {
    let core = core_named_as_released(dir);
    let file = dir.join("refhir-packages.bundle");
    let out = exits(
        dir,
        &[
            "package-bundle",
            "--out",
            file.to_str().unwrap(),
            "--spec",
            core.to_str().unwrap(),
            "--spec",
            fixture("sdc").to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains("hl7.fhir.r4.core@4.0.1") && said.contains("hl7.fhir.uv.sdc@4.0.0"),
        "{said}"
    );
    file
}

#[test]
fn a_first_start_installs_the_prepared_packages_without_reaching_a_registry() {
    let dir = Scratch::new("bundle-first-start");
    let file = prepared_defaults(&dir);
    serves(
        &dir,
        &["--memory"],
        &[
            // The helper turns the default packages off; this test is about them.
            ("REFHIR_SKIP_DEFAULT_PACKAGES", "0"),
            ("REFHIR_PACKAGE_BUNDLE", file.to_str().unwrap()),
            ("REFHIR_PACKAGE_REGISTRY", UNREACHABLE_REGISTRY),
        ],
    );
    let log = std::fs::read_to_string(dir.join("serve.log")).unwrap();
    assert!(
        log.contains("installed 2 FHIR packages from"),
        "the log does not say the packages came from the file:\n{log}"
    );
    assert!(!log.contains("downloading FHIR package"), "{log}");
}

#[test]
fn a_prepared_file_beside_the_binary_is_found_without_being_named() {
    let dir = Scratch::new("bundle-beside");
    let file = prepared_defaults(&dir);
    // The binary is copied next to the file, as a release archive has them.
    let binary = dir.join("refhir");
    std::fs::copy(env!("CARGO_BIN_EXE_refhir"), &binary).unwrap();
    let bind = format!("127.0.0.1:{}", free_port());
    let log = dir.join("beside.log");
    let file_log = std::fs::File::create(&log).unwrap();
    let mut child = std::process::Command::new(&binary)
        .current_dir(&*dir)
        .args(["--memory", "--bind", &bind])
        .env_clear()
        .env("REFHIR_PACKAGE_REGISTRY", UNREACHABLE_REGISTRY)
        .env("REFHIR_PACKAGE_CACHE", dir.join("packages"))
        .stdout(Stdio::from(file_log.try_clone().unwrap()))
        .stderr(Stdio::from(file_log))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let ready = loop {
        if ureq::get(&format!("http://{bind}/readyz"))
            .timeout(Duration::from_secs(2))
            .call()
            .is_ok()
        {
            break true;
        }
        if child.try_wait().unwrap().is_some() || Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = child.kill();
    let _ = child.wait();
    let said = std::fs::read_to_string(&log).unwrap();
    assert!(ready, "not ready:\n{said}");
    assert!(
        said.contains("installed 2 FHIR packages from"),
        "the file beside the binary was not used:\n{said}"
    );
    assert!(file.is_file());
}

/// Starts a server on `sqlite`, asks it for the first currencies that have `filter` in their name,
/// and stops it. The codes of the value set of currencies are what a server that stores ISO 4217
/// itself can say and one that does not cannot.
fn currencies_named(
    dir: &Path,
    sqlite: &Path,
    env: &[(&str, &str)],
    filter: &str,
) -> (u16, Vec<String>) {
    use std::process::Stdio;
    let bind = format!("127.0.0.1:{}", free_port());
    let log = dir.join("currencies.log");
    let file = std::fs::File::create(&log).unwrap();
    let mut child = common::start(
        dir,
        &["--sqlite", sqlite.to_str().unwrap(), "--bind", &bind],
        env,
        Stdio::from(file.try_clone().unwrap()),
        Stdio::from(file),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let ready = loop {
        if child.try_wait().unwrap().is_some() || Instant::now() > deadline {
            break false;
        }
        if ureq::get(&format!("http://{bind}/readyz"))
            .timeout(Duration::from_secs(2))
            .call()
            .is_ok()
        {
            break true;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let answer = ready.then(|| {
        ureq::post(&format!("http://{bind}/ValueSet/$expand"))
            .set("content-type", "application/fhir+json")
            .send_string(
                &serde_json::json!({"url": "http://hl7.org/fhir/ValueSet/currencies", "filter": filter})
                    .to_string(),
            )
    });
    let _ = child.kill();
    let _ = child.wait();
    let said = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(ready, "not ready:\n{said}");
    let (status, body): (u16, serde_json::Value) = match answer.unwrap() {
        Ok(response) => (
            response.status(),
            serde_json::from_str(&response.into_string().unwrap()).unwrap(),
        ),
        Err(ureq::Error::Status(status, response)) => (
            status,
            serde_json::from_str(&response.into_string().unwrap()).unwrap(),
        ),
        Err(err) => panic!("$expand failed: {err}\n{said}"),
    };
    let codes = body["expansion"]["contains"]
        .as_array()
        .map(|codes| {
            codes
                .iter()
                .map(|code| code["code"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default();
    (status, codes)
}

#[test]
fn a_first_start_stores_the_currency_codes_and_a_restart_keeps_them() {
    let dir = Scratch::new("bundle-currencies");
    let file = prepared_defaults(&dir);
    let sqlite = dir.join("refhir.sqlite");
    let env = [
        // The helper turns the default packages off; this test is about them.
        ("REFHIR_SKIP_DEFAULT_PACKAGES", "0"),
        ("REFHIR_PACKAGE_BUNDLE", file.to_str().unwrap()),
        ("REFHIR_PACKAGE_REGISTRY", UNREACHABLE_REGISTRY),
    ];
    // ISO 4217 is in no package of the release, so a server that answers with the naira has stored
    // it itself.
    assert_eq!(
        currencies_named(&dir, &sqlite, &env, "naira"),
        (200, vec!["NGN".to_string()]),
        "a first start"
    );
    // The next start finds the release's packages and the codes there, and needs no file.
    let again = [
        ("REFHIR_SKIP_DEFAULT_PACKAGES", "0"),
        ("REFHIR_PACKAGE_BUNDLE", "off"),
        ("REFHIR_PACKAGE_REGISTRY", UNREACHABLE_REGISTRY),
    ];
    assert_eq!(
        currencies_named(&dir, &sqlite, &again, "naira"),
        (200, vec!["NGN".to_string()]),
        "a restart"
    );
    // A server told to leave the defaults alone leaves these alone too.
    let bare = Scratch::new("bundle-no-currencies");
    let skipped = bare.join("refhir.sqlite");
    let (status, codes) = currencies_named(&bare, &skipped, &[], "naira");
    assert_eq!(
        (status, codes.is_empty()),
        (422, true),
        "REFHIR_SKIP_DEFAULT_PACKAGES also skips the code systems the server carries"
    );
}

/// A registry that records whether anything connected to it.
struct WatchedRegistry {
    listener: std::net::TcpListener,
}

impl WatchedRegistry {
    fn new() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        Self { listener }
    }

    fn url(&self) -> String {
        format!("http://{}", self.listener.local_addr().unwrap())
    }

    fn was_reached(&self) -> bool {
        self.listener.accept().is_ok()
    }
}

/// Starts the server with `env` until it exits, which it must do, and returns what it said.
fn refuses_to_start(dir: &Path, env: &[(&str, &str)]) -> String {
    let log = dir.join("refused.log");
    let file = std::fs::File::create(&log).unwrap();
    let bind = format!("127.0.0.1:{}", free_port());
    let mut child = start(
        dir,
        &["--memory", "--bind", &bind],
        env,
        Stdio::from(file.try_clone().unwrap()),
        Stdio::from(file),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the server was still running:\n{}",
                std::fs::read_to_string(&log).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let said = std::fs::read_to_string(&log).unwrap();
    assert!(!status.success(), "the server started:\n{said}");
    said
}

#[test]
fn a_prepared_file_that_lacks_the_default_packages_stops_the_start() {
    let dir = Scratch::new("bundle-lacks");
    let file = dir.join("only-core.bundle");
    let out = exits(
        &dir,
        &[
            "package-bundle",
            "--out",
            file.to_str().unwrap(),
            "--spec",
            fixture("minir4").to_str().unwrap(),
        ],
    );
    assert!(out.status.success());
    // Even a server that may download does not: a file that is there and wrong is a mistake to
    // report, not to work around.
    let registry = WatchedRegistry::new();
    let said = refuses_to_start(
        &dir,
        &[
            ("REFHIR_SKIP_DEFAULT_PACKAGES", "0"),
            ("REFHIR_PACKAGE_BUNDLE", file.to_str().unwrap()),
            ("REFHIR_PACKAGE_REGISTRY", &registry.url()),
            ("REFHIR_PACKAGE_DOWNLOAD", "true"),
        ],
    );
    assert!(said.contains("does not hold"), "{said}");
    assert!(!registry.was_reached(), "the registry was reached");
}

#[test]
fn a_damaged_prepared_file_stops_the_start() {
    let dir = Scratch::new("bundle-damaged");
    let file = prepared_defaults(&dir);
    let bytes = std::fs::read(&file).unwrap();
    std::fs::write(&file, &bytes[..bytes.len() - 40]).unwrap();
    let registry = WatchedRegistry::new();
    let said = refuses_to_start(
        &dir,
        &[
            ("REFHIR_SKIP_DEFAULT_PACKAGES", "0"),
            ("REFHIR_PACKAGE_BUNDLE", file.to_str().unwrap()),
            ("REFHIR_PACKAGE_REGISTRY", &registry.url()),
            ("REFHIR_PACKAGE_DOWNLOAD", "true"),
        ],
    );
    assert!(said.contains("cannot install the packages in"), "{said}");
    assert!(!registry.was_reached(), "the registry was reached");
}

#[test]
fn a_named_prepared_file_that_is_not_there_stops_the_start() {
    let dir = Scratch::new("bundle-missing");
    let said = refuses_to_start(
        &dir,
        &[
            ("REFHIR_SKIP_DEFAULT_PACKAGES", "0"),
            ("REFHIR_PACKAGE_BUNDLE", "/nowhere/refhir-packages.bundle"),
        ],
    );
    assert!(said.contains("which is not a file"), "{said}");
}

#[test]
fn without_a_prepared_file_the_server_downloads_nothing_unless_told_to() {
    let dir = Scratch::new("bundle-none");
    let registry = WatchedRegistry::new();
    // The test binary has no prepared file beside it, and nothing says it may download.
    let said = refuses_to_start(
        &dir,
        &[
            ("REFHIR_SKIP_DEFAULT_PACKAGES", "0"),
            ("REFHIR_PACKAGE_REGISTRY", &registry.url()),
        ],
    );
    assert!(said.contains("does not download packages"), "{said}");
    assert!(said.contains("package_download = true"), "{said}");
    assert!(!registry.was_reached(), "the registry was reached");

    // `off` is the same.
    let said = refuses_to_start(
        &dir,
        &[
            ("REFHIR_SKIP_DEFAULT_PACKAGES", "0"),
            ("REFHIR_PACKAGE_BUNDLE", "off"),
            ("REFHIR_PACKAGE_REGISTRY", &registry.url()),
        ],
    );
    assert!(said.contains("does not download packages"), "{said}");
    assert!(!registry.was_reached(), "the registry was reached");
}

#[test]
fn a_server_told_to_download_goes_to_the_registry_when_there_is_no_file() {
    let dir = Scratch::new("bundle-download");
    let said = refuses_to_start(
        &dir,
        &[
            ("REFHIR_SKIP_DEFAULT_PACKAGES", "0"),
            ("REFHIR_PACKAGE_DOWNLOAD", "true"),
            ("REFHIR_PACKAGE_REGISTRY", UNREACHABLE_REGISTRY),
        ],
    );
    assert!(
        said.contains("cannot install the default FHIR packages"),
        "{said}"
    );
}

#[test]
fn package_bundle_refuses_what_it_does_not_understand() {
    let dir = Scratch::new("bundle-args");
    let out = exits(&dir, &["package-bundle", "--nonsense"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown argument --nonsense"));
    let out = exits(&dir, &["package-bundle", "--out"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--out needs a value"));
    let out = exits(&dir, &["package-bundle", "--help"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("refhir package-bundle"));
}
