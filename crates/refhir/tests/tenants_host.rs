//! One `refhir --all-tenants` process serving many tenants of a shared Postgres database, which
//! tenants are created in over HTTP. These start the real binary, as `cluster.rs` does, and need
//! `REFHIR_TEST_POSTGRES_URL` (a role that may `CREATE DATABASE` and `CREATE ROLE`).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

mod common;
use common::{admin_url, database_of_an_ordinary_role, free_port};

const TOKEN: &str = "test-admin-token";

/// A directory with the resource types a user needs, so a tenant has more than the core ones.
fn types_package(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "refhir-host-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        format!(r#"{{"name":"example.{tag}","version":"1.2.3","fhirVersion":"4.0.1"}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("StructureDefinition-Basic.json"),
        r#"{"resourceType":"StructureDefinition","url":"http://hl7.org/fhir/StructureDefinition/Basic","name":"Basic","status":"active","kind":"resource","abstract":false,"type":"Basic","fhirVersion":"4.0.1","derivation":"specialization","snapshot":{"element":[{"path":"Basic","min":0,"max":"*"}]}}"#,
    )
    .unwrap();
    dir
}

struct NoNetwork;

impl refhir_fhir::PackageFetcher for NoNetwork {
    fn fetch(&self, name: &str, version: &str, _dest: &Path) -> Result<(), refhir_fhir::FhirError> {
        panic!("{name}@{version} was fetched, and nothing here may fetch");
    }
}

/// A file of prepared packages for the release (R4 core and SDC, tiny ones), as a release archive
/// has, so that every tenant is given them from the one shared copy.
fn prepared_packages(tag: &str) -> PathBuf {
    let scratch =
        std::env::temp_dir().join(format!("refhir-host-bundle-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let minir4 = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fhir/fixtures/minir4");
    let core = scratch.join("core");
    std::fs::create_dir_all(&core).unwrap();
    for entry in std::fs::read_dir(&minir4).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), core.join(entry.file_name())).unwrap();
    }
    std::fs::write(
        core.join("package.json"),
        r#"{"name":"hl7.fhir.r4.core","version":"4.0.1","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    std::fs::write(core.join(".complete"), "ab".repeat(32)).unwrap();
    let sdc = types_package("sdc");
    std::fs::write(
        sdc.join("package.json"),
        r#"{"name":"hl7.fhir.uv.sdc","version":"4.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    std::fs::write(sdc.join(".complete"), "cd".repeat(32)).unwrap();
    let bundle = scratch.join("packages.bundle");
    refhir_fhir::write_prepared(
        &bundle,
        &[
            core.to_str().unwrap().to_string(),
            sdc.to_str().unwrap().to_string(),
        ],
        &NoNetwork,
        &scratch.join("cache"),
    )
    .unwrap();
    bundle
}

struct Host {
    child: Child,
    port: u16,
    dir: PathBuf,
    log: PathBuf,
}

impl Host {
    fn start(name: &str, database: &str, args: &[&str]) -> Host {
        Self::spawn(name, database, args, None, false)
    }

    /// A host whose tenants are given the release's packages from `bundle` (a file of prepared
    /// packages, as a release archive has), which tenants share, instead of a package directory.
    fn start_with_bundle(name: &str, database: &str, args: &[&str], bundle: &Path) -> Host {
        Self::spawn(name, database, args, Some(bundle), false)
    }

    /// [`Host::start_with_bundle`] for a host that gets ready for its first tenant, as it does by
    /// default, in the background.
    fn start_warming_up(name: &str, database: &str, args: &[&str], bundle: &Path) -> Host {
        Self::spawn(name, database, args, Some(bundle), true)
    }

    fn spawn(
        name: &str,
        database: &str,
        args: &[&str],
        bundle: Option<&Path>,
        warm_up: bool,
    ) -> Host {
        let port = free_port();
        let dir = std::env::temp_dir().join(format!("refhir-host-{name}-{port}"));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("server.log");
        let file = std::fs::File::create(&log).unwrap();
        let minir4 = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fhir/fixtures/minir4");
        let mut command = Command::new(env!("CARGO_BIN_EXE_refhir"));
        command
            .current_dir(&dir)
            .args(["--postgres", database, "--all-tenants"])
            .args(["--bind", &format!("127.0.0.1:{port}")])
            .args(["--export-dir", dir.join("export").to_str().unwrap()])
            .args(args);
        match bundle {
            Some(bundle) => command.env("REFHIR_PACKAGE_BUNDLE", bundle),
            None => command
                .arg("--package")
                .arg(&minir4)
                .env("REFHIR_SKIP_DEFAULT_PACKAGES", "1"),
        };
        let child = command
            .env("REFHIR_ADMIN_TOKEN", TOKEN)
            // What a host does in the background at its start is for the tests of it to ask for.
            .env("REFHIR_TENANTS_WARM_UP", if warm_up { "1" } else { "0" })
            // The same on every process of a deployment, as tenants' SMART secrets come from them.
            .env("REFHIR_SMART_DCR_TOKEN", "test-dcr-token")
            .env(
                "REFHIR_SMART_CONFIDENTIAL_SECRET",
                "test-confidential-secret",
            )
            .env("REFHIR_PACKAGE_CACHE", dir.join("packages"))
            .stdout(Stdio::from(file.try_clone().unwrap()))
            .stderr(Stdio::from(file))
            .spawn()
            .expect("start refhir");
        Host {
            child,
            port,
            dir,
            log,
        }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("the host exited ({status}):\n{}", self.log());
            }
            if call("GET", &format!("{}/readyz", self.base()), &[], None).0 == 200 {
                return;
            }
            assert!(Instant::now() < deadline, "not ready:\n{}", self.log());
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Waits until the host has made what a new tenant starts from (it does, in the background,
    /// from its start), so that what a test measures is not slowed by it.
    fn wait_warm(&self) {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let log = self.log();
            if log.contains("ready to make tenants") || log.contains("are not warmed up") {
                return;
            }
            assert!(Instant::now() < deadline, "not warmed up:\n{log}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn admin(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        call(
            method,
            &format!("{}{path}", self.base()),
            &[("authorization", &format!("Bearer {TOKEN}"))],
            body.as_ref(),
        )
    }

    /// Waits until a tenant's FHIR API answers 200 on `/metadata`.
    fn wait_serving(&self, tenant: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if call(
                "GET",
                &format!("{}/t/{tenant}/metadata", self.base()),
                &[],
                None,
            )
            .0 == 200
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "tenant {tenant} was not served:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn call(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&Value>) -> (u16, Value) {
    let mut request = ureq::request(method, url).timeout(Duration::from_secs(60));
    for (name, value) in headers {
        request = request.set(name, value);
    }
    let outcome = match body {
        Some(body) => request
            .set("content-type", "application/json")
            .send_string(&body.to_string()),
        None => request.call(),
    };
    match outcome {
        Ok(response) | Err(ureq::Error::Status(_, response)) => {
            let status = response.status();
            let text = response.into_string().unwrap_or_default();
            (
                status,
                serde_json::from_str(&text).unwrap_or(Value::String(text)),
            )
        }
        Err(_) => (0, Value::Null),
    }
}

fn put_patient(host: &Host, tenant: &str, id: &str, family: &str) -> u16 {
    call(
        "PUT",
        &format!("{}/t/{tenant}/Patient/{id}", host.base()),
        &[],
        Some(&json!({"resourceType": "Patient", "id": id, "active": true, "name": [{"family": family}]})),
    )
    .0
}

fn family(host: &Host, tenant: &str, id: &str) -> (u16, Value) {
    let (status, body) = call(
        "GET",
        &format!("{}/t/{tenant}/Patient/{id}", host.base()),
        &[],
        None,
    );
    (status, body["name"][0]["family"].clone())
}

fn cli(database: &str, args: &[&str]) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_refhir"))
        .arg("tenant")
        .args(args)
        .args(["--postgres", database])
        .output()
        .expect("run refhir tenant");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code().unwrap_or(-1), text)
}

fn create_body(name: &str) -> Value {
    json!({"name": name, "update_as_create": true, "smart": false})
}

#[test]
fn tenants_are_created_over_http_and_served_at_once_by_the_one_process() {
    let Some(admin) = admin_url("tenants host: admin API") else {
        return;
    };
    let (_database, app) = database_of_an_ordinary_role(&admin, "hostadmin");
    let mut host = Host::start("admin", &app, &[]);
    host.wait_ready();

    // Without the admin credentials nothing is created, listed or deleted.
    for (method, path, body) in [
        ("GET", "/admin/tenants", None),
        ("POST", "/admin/tenants", Some(create_body("nope"))),
        ("GET", "/admin/tenants/nope", None),
        ("DELETE", "/admin/tenants/nope", None),
    ] {
        let (status, _) = call(
            method,
            &format!("{}{path}", host.base()),
            &[],
            body.as_ref(),
        );
        assert_eq!(status, 401, "{method} {path}");
        let (status, _) = call(
            method,
            &format!("{}{path}", host.base()),
            &[("authorization", "Bearer wrong")],
            body.as_ref(),
        );
        assert_eq!(status, 401, "{method} {path} with a wrong token");
    }
    assert_eq!(
        host.admin("GET", "/admin/tenants", None).1["tenants"],
        json!([])
    );

    // Creating a tenant is one call, and idempotent.
    let package = types_package("acme");
    let mut acme = create_body("acme");
    acme["packages"] = json!([package.to_str().unwrap()]);
    acme["limits"] = json!({"requests_per_second": 1000.0, "connections": 2});
    let (status, created) = host.admin("POST", "/admin/tenants", Some(acme.clone()));
    assert_eq!(status, 201, "{created}\n{}", host.log());
    assert_eq!(created["name"], "acme");
    assert_eq!(created["status"], "active");
    assert_eq!(
        created["public_url"],
        format!("http://127.0.0.1:{}/t/acme", host.port)
    );
    assert_eq!(created["served"]["status"], "ready");
    let (status, again) = host.admin("POST", "/admin/tenants", Some(acme.clone()));
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["id"], created["id"]);
    // The same name with other settings is a conflict, not a change.
    let (status, _) = host.admin("POST", "/admin/tenants", Some(create_body("acme")));
    assert_eq!(status, 409);
    for bad in [
        json!({"name": "Not Valid"}),
        json!({"name": "x", "bogus": 1}),
        json!({}),
    ] {
        let (status, _) = host.admin("POST", "/admin/tenants", Some(bad));
        assert!(status == 400 || status == 422, "{status}");
    }
    let (status, body) = host.admin(
        "POST",
        "/admin/tenants",
        Some(json!({"name": "smarty", "smart": true})),
    );
    assert_eq!(status, 422, "SMART needs a login: {body}");
    let (status, body) = host.admin(
        "POST",
        "/admin/tenants",
        Some(json!({"name": "broken", "packages": ["/no/such/package/dir"]})),
    );
    assert_eq!(status, 422, "{body}");
    assert_eq!(
        host.admin("GET", "/admin/tenants/broken", None).0,
        404,
        "a tenant that cannot open is not kept"
    );

    let (status, _) = host.admin("POST", "/admin/tenants", Some(create_body("beta")));
    assert_eq!(status, 201);

    // Each is its own server at its own address.
    assert_eq!(put_patient(&host, "acme", "p1", "Ann"), 201);
    assert_eq!(put_patient(&host, "beta", "p1", "Bee"), 201);
    assert_eq!(family(&host, "acme", "p1"), (200, json!("Ann")));
    assert_eq!(family(&host, "beta", "p1"), (200, json!("Bee")));
    assert_eq!(
        call(
            "GET",
            &format!("{}/t/nobody/Patient/p1", host.base()),
            &[],
            None
        )
        .0,
        404
    );
    // The package made for acme alone is acme's.
    let basic = |tenant: &str| {
        call(
            "GET",
            &format!("{}/t/{tenant}/Basic", host.base()),
            &[],
            None,
        )
        .0
    };
    assert_eq!(basic("acme"), 200);
    assert_ne!(basic("beta"), 200, "beta has no Basic");

    // Status, size and package versions.
    let (status, shown) = host.admin("GET", "/admin/tenants/acme", None);
    assert_eq!(status, 200);
    assert!(shown["size"]["documents"].as_u64().unwrap() >= 1, "{shown}");
    assert!(shown["size"]["bytes"].as_u64().unwrap() > 0);
    let packages = shown["packages"].to_string();
    assert!(
        packages.contains("example.acme") && packages.contains("1.2.3"),
        "{packages}"
    );
    assert_eq!(shown["settings"]["limits"]["connections"], 2);
    let (_, listed) = host.admin("GET", "/admin/tenants", None);
    assert_eq!(listed["tenants"].as_array().unwrap().len(), 2);

    // Per-tenant counters.
    let metrics = call(
        "GET",
        &format!("{}/admin/metrics", host.base()),
        &[("authorization", &format!("Bearer {TOKEN}"))],
        None,
    )
    .1;
    let metrics = metrics.as_str().unwrap();
    assert!(
        metrics.contains("refhir_tenant_up{tenant=\"acme\"} 1"),
        "{metrics}"
    );
    assert!(
        metrics.contains("refhir_tenant_connections_max{tenant=\"acme\"} 2"),
        "{metrics}"
    );

    // The CLI is the same registry: a tenant it creates is served by the running process.
    let (code, said) = cli(&app, &["create", "gamma", "--update-as-create"]);
    assert_eq!(code, 0, "{said}");
    host.wait_serving("gamma");
    assert_eq!(put_patient(&host, "gamma", "p1", "Gus"), 201);
    let (code, said) = cli(&app, &["list"]);
    assert!(said.contains("gamma") && said.contains("acme"), "{said}");
    assert_eq!(code, 0);
    let (_, shown) = host.admin("GET", "/admin/tenants/gamma", None);
    assert_eq!(shown["status"], "active");
    let _ = std::fs::remove_dir_all(package);
}

/// What a process that serves many tenants costs per tenant depends on tenants with the same
/// definitions holding one compiled copy of them, which happens only if every step of opening a
/// tenant (its packages, the built-in terminology, the repair) leaves it with what the others
/// have. The admin API says whether a tenant holds the shared one.
#[test]
fn tenants_given_the_same_packages_share_one_catalog() {
    let Some(admin) = admin_url("tenants host: shared catalog") else {
        return;
    };
    let (_database, app) = database_of_an_ordinary_role(&admin, "hostshare");
    let bundle = prepared_packages("share");
    let mut host = Host::start_with_bundle("share", &app, &[], &bundle);
    host.wait_ready();
    for name in ["a", "b"] {
        let (status, created) = host.admin("POST", "/admin/tenants", Some(create_body(name)));
        assert_eq!(status, 201, "{created}\n{}", host.log());
        assert_eq!(created["served"]["status"], "ready", "{created}");
        host.wait_serving(name);
    }
    // One with a package of its own holds a catalog of its own.
    let own = types_package("own");
    let mut body = create_body("own");
    body["packages"] = json!([own.to_str().unwrap()]);
    let (status, created) = host.admin("POST", "/admin/tenants", Some(body));
    assert_eq!(status, 201, "{created}\n{}", host.log());
    host.wait_serving("own");
    for (name, shared) in [("a", true), ("b", true), ("own", false)] {
        let (status, tenant) = host.admin("GET", &format!("/admin/tenants/{name}"), None);
        assert_eq!(status, 200, "{tenant}");
        assert_eq!(
            tenant["served"]["shared_catalog"],
            json!(shared),
            "tenant {name}: {tenant}\n{}",
            host.log()
        );
    }
    // A restart gives the same: the tenants are opened again and put with each other's.
    host.kill();
    let mut host = Host::start_with_bundle("share-again", &app, &[], &bundle);
    host.wait_ready();
    for name in ["a", "b"] {
        host.wait_serving(name);
        let (_, tenant) = host.admin("GET", &format!("/admin/tenants/{name}"), None);
        assert_eq!(tenant["served"]["shared_catalog"], json!(true), "{tenant}");
    }
}

/// What a tenant costs to make: the process gets ready for the first before one is asked for, and
/// a tenant made after that is given the shared packages and the catalog that was compiled, and
/// nothing is read from the prepared file or compiled for it. A create that takes tens of seconds,
/// as one that compiled the catalog again does, is what this keeps out; it counts what is done and
/// not how long it takes, so a slow machine does not flake it.
#[test]
fn a_tenant_made_after_the_start_compiles_no_catalog() {
    let Some(admin) = admin_url("tenants host: tenants made later") else {
        return;
    };
    let (_database, app) = database_of_an_ordinary_role(&admin, "hostlater");
    let bundle = prepared_packages("later");
    let mut host = Host::start_warming_up("later", &app, &[], &bundle);
    host.wait_ready();
    // The process makes a tenant of its own to get ready, and removes it again.
    host.wait_warm();
    assert!(
        host.log().contains("ready to make tenants"),
        "{}",
        host.log()
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let (status, listed) = loop {
        let (status, listed) = host.admin("GET", "/admin/tenants", None);
        if status != 200
            || listed["tenants"]
                .as_array()
                .is_some_and(|tenants| tenants.is_empty())
        {
            break (status, listed);
        }
        assert!(
            Instant::now() < deadline,
            "the warm-up tenant stayed: {listed}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(status, 200, "{listed}");
    assert_eq!(
        listed["tenants"],
        json!([]),
        "no tenant is left from the warm-up"
    );
    // Nobody can have a tenant of that name, or see one.
    let (status, body) = host.admin("POST", "/admin/tenants", Some(create_body("refhir-warm")));
    assert!(status == 400 || status == 422, "{status} {body}");
    let (status, _) = host.admin("GET", "/admin/tenants/refhir-warm", None);
    assert_eq!(status, 404);
    let (status, _) = host.admin("DELETE", "/admin/tenants/refhir-warm", None);
    assert_eq!(status, 404);

    let compiled = |host: &Host| {
        host.log()
            .matches("compiled a catalog for tenants to share")
            .count()
    };
    let after_warm_up = compiled(&host);
    assert!(
        after_warm_up > 0,
        "the warm-up compiles the catalog:\n{}",
        host.log()
    );
    for name in ["first", "second", "third"] {
        let started = Instant::now();
        let (status, created) = host.admin("POST", "/admin/tenants", Some(create_body(name)));
        assert_eq!(status, 201, "{created}\n{}", host.log());
        assert_eq!(
            created["served"]["shared_catalog"],
            json!(true),
            "{created}"
        );
        eprintln!("tenant {name} was made in {:.1?}", started.elapsed());
        assert_eq!(
            compiled(&host),
            after_warm_up,
            "tenant {name} compiled a catalog that the warm-up had compiled:\n{}",
            host.log()
        );
    }
    // Each was given the packages from the copy the warm-up stored, and none was read for it.
    let log = host.log();
    assert!(
        log.matches("without reading it; linked").count() >= 3,
        "{log}"
    );
}

#[test]
fn a_restart_serves_every_tenant_as_it_was() {
    let Some(admin) = admin_url("tenants host: restart") else {
        return;
    };
    let (_database, app) = database_of_an_ordinary_role(&admin, "hostrestart");
    let mut host = Host::start("restart", &app, &[]);
    host.wait_ready();
    let names: Vec<String> = (0..6).map(|n| format!("t{n}")).collect();
    for name in &names {
        let (status, body) = host.admin("POST", "/admin/tenants", Some(create_body(name)));
        assert_eq!(status, 201, "{body}");
        assert_eq!(put_patient(&host, name, "p1", name), 201);
    }
    let (status, _) = host.admin("DELETE", "/admin/tenants/t5?export=false", None);
    assert_eq!(status, 200);
    host.kill();

    let mut host = Host::start("restart-again", &app, &[]);
    host.wait_ready();
    for name in &names[..5] {
        host.wait_serving(name);
        assert_eq!(family(&host, name, "p1"), (200, json!(name)));
    }
    // The tenant that was deleted is still deleted.
    let (status, _) = call(
        "GET",
        &format!("{}/t/t5/Patient/p1", host.base()),
        &[],
        None,
    );
    assert_eq!(status, 410);
    let (_, shown) = host.admin("GET", "/admin/tenants/t5", None);
    assert_eq!(shown["status"], "deleted");
    // And what the tenants have in common did not leak: one tenant's writes since are its own.
    assert_eq!(put_patient(&host, "t0", "p2", "new"), 201);
    assert_eq!(family(&host, "t1", "p2").0, 404);
}

#[test]
fn deleting_a_tenant_while_it_serves_traffic_leaves_the_others_serving() {
    let Some(admin) = admin_url("tenants host: delete under traffic") else {
        return;
    };
    let (_database, app) = database_of_an_ordinary_role(&admin, "hostdelete");
    let mut host = Host::start("delete", &app, &["--tenant-domain", "fhir.test"]);
    host.wait_ready();
    for name in ["doomed", "stays"] {
        let (status, body) = host.admin("POST", "/admin/tenants", Some(create_body(name)));
        assert_eq!(status, 201, "{body}");
        assert_eq!(put_patient(&host, name, "p1", name), 201);
    }
    let (status, _) = host.admin("DELETE", "/admin/tenants/ghost", None);
    assert_eq!(status, 404);

    // Traffic on both tenants, from threads, while one is deleted.
    let stop = Arc::new(AtomicBool::new(false));
    let base = host.base();
    let workers: Vec<_> = ["doomed", "stays"]
        .into_iter()
        .flat_map(|name| (0..3).map(move |worker| (name, worker)))
        .map(|(name, worker)| {
            let (stop, base) = (stop.clone(), base.clone());
            std::thread::spawn(move || {
                // (answers by status, 5xx or no answer)
                let mut seen: std::collections::BTreeMap<u16, u32> = Default::default();
                let mut count = 0u32;
                while !stop.load(Ordering::SeqCst) {
                    count += 1;
                    let status = if count % 3 == 0 {
                        call(
                            "PUT",
                            &format!("{base}/t/{name}/Patient/w{worker}-{count}"),
                            &[],
                            Some(&json!({"resourceType": "Patient", "id": format!("w{worker}-{count}")})),
                        )
                        .0
                    } else {
                        call("GET", &format!("{base}/t/{name}/Patient/p1"), &[], None).0
                    };
                    *seen.entry(status).or_default() += 1;
                    // Each request is a connection of its own: without a pause a worker uses up
                    // the machine's ports, and a request then fails in the client.
                    std::thread::sleep(Duration::from_millis(2));
                }
                (name, seen)
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(800));
    let started = Instant::now();
    let (status, deleted) = host.admin("DELETE", "/admin/tenants/doomed?grace=1h", None);
    assert_eq!(status, 200, "{deleted}\n{}", host.log());
    assert_eq!(deleted["status"], "deleted");
    // Its resources were written out first, for whoever takes over its data.
    let export = deleted["export"]["file"].as_str().expect("an export file");
    let exported = std::fs::read_to_string(export).unwrap();
    assert!(exported.contains("\"doomed\""), "{exported}");
    assert!(!exported.contains("\"stays\""));
    std::thread::sleep(Duration::from_millis(800));
    stop.store(true, Ordering::SeqCst);
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    eprintln!("deleted in {:?}", started.elapsed());

    for (name, seen) in &results {
        let bad: Vec<_> = seen
            .iter()
            .filter(|(status, _)| **status == 0 || **status >= 500)
            .collect();
        assert!(
            bad.is_empty(),
            "{name} saw {bad:?} (all: {seen:?})\n{}",
            host.log()
        );
        if *name == "stays" {
            assert!(
                seen.keys().all(|status| *status < 300),
                "stays saw {seen:?}"
            );
        } else {
            // Before it was deleted it answered, after it said 410, and nothing else.
            assert!(
                seen.keys().all(|status| *status < 300 || *status == 410),
                "doomed saw {seen:?}"
            );
            assert!(
                seen.contains_key(&410),
                "doomed never answered 410: {seen:?}"
            );
        }
    }
    assert_eq!(family(&host, "doomed", "p1").0, 410);
    assert_eq!(family(&host, "stays", "p1"), (200, json!("stays")));
    // Its lease is given up, and its rows are still there until the grace period is over.
    let (code, said) = cli(&app, &["list"]);
    assert_eq!(code, 0);
    let doomed_line = said
        .lines()
        .find(|line| line.starts_with("doomed"))
        .unwrap();
    assert!(
        doomed_line.contains("deleted") && !doomed_line.contains("running"),
        "{said}"
    );

    // Restoring brings back what it held.
    let (status, _) = host.admin("POST", "/admin/tenants/doomed/restore", None);
    assert_eq!(status, 200);
    assert_eq!(family(&host, "doomed", "p1"), (200, json!("doomed")));

    // Deleted for good, its rows are removed and its name is free.
    let (status, removed) =
        host.admin("DELETE", "/admin/tenants/doomed?grace=0&export=false", None);
    assert_eq!(status, 200, "{removed}");
    assert_eq!(removed["status"], "removed");
    assert!(removed["removed_rows"].as_u64().unwrap() > 0);
    assert_eq!(host.admin("GET", "/admin/tenants/doomed", None).0, 404);
    assert_eq!(
        call(
            "GET",
            &format!("{}/t/doomed/Patient/p1", host.base()),
            &[],
            None
        )
        .0,
        404
    );
    let (status, _) = host.admin("POST", "/admin/tenants", Some(create_body("doomed")));
    assert_eq!(status, 201);
    assert_eq!(
        family(&host, "doomed", "p1").0,
        404,
        "the new tenant holds nothing of the old one"
    );

    // Host names tell tenants apart too.
    let (status, body) = call(
        "GET",
        &format!("{}/Patient/p1", host.base()),
        &[("host", "stays.fhir.test")],
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(body["name"][0]["family"], "stays");
}

#[test]
fn a_replica_of_the_host_follows_its_tenants_and_sends_changes_to_the_primary() {
    let Some(admin) = admin_url("tenants host: replica") else {
        return;
    };
    let (_database, app) = database_of_an_ordinary_role(&admin, "hostreplica");
    let mut primary = Host::start("primary", &app, &[]);
    primary.wait_ready();
    let (status, body) = primary.admin("POST", "/admin/tenants", Some(create_body("acme")));
    assert_eq!(status, 201, "{body}");
    assert_eq!(put_patient(&primary, "acme", "p1", "Ann"), 201);

    let mut replica = Host::start(
        "replica",
        &app,
        &["--role", "replica", "--primary-url", &primary.base()],
    );
    replica.wait_ready();
    replica.wait_serving("acme");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (status, body) = call(
            "GET",
            &format!("{}/t/acme/Patient/p1", replica.base()),
            &[],
            None,
        );
        if status == 200 {
            assert_eq!(body["name"][0]["family"], "Ann");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the replica never caught up:\n{}",
            replica.log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // A write sent to the replica reaches the tenant's primary, and only that tenant.
    assert_eq!(put_patient(&replica, "acme", "via", "Via"), 201);
    assert_eq!(family(&primary, "acme", "via"), (200, json!("Via")));

    // A tenant created through the replica is created by the primary, and the replica follows.
    let (status, body) = replica.admin("POST", "/admin/tenants", Some(create_body("later")));
    assert_eq!(status, 201, "{body}");
    primary.wait_serving("later");
    replica.wait_serving("later");
    assert_eq!(put_patient(&primary, "later", "p1", "Lee"), 201);
    let deadline = Instant::now() + Duration::from_secs(30);
    while family(&replica, "later", "p1").0 != 200 {
        assert!(
            Instant::now() < deadline,
            "the replica never saw the new tenant's write"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Deleting it stops the replica serving it as well.
    let (status, _) = replica.admin("DELETE", "/admin/tenants/later?export=false", None);
    assert_eq!(status, 200);
    let deadline = Instant::now() + Duration::from_secs(30);
    while family(&replica, "later", "p1").0 != 410 {
        assert!(
            Instant::now() < deadline,
            "the replica kept serving a deleted tenant"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(family(&primary, "later", "p1").0, 410);
    assert_eq!(family(&replica, "acme", "p1"), (200, json!("Ann")));
}

#[test]
fn a_tenant_that_failed_to_open_is_tried_again_without_a_restart() {
    let Some(admin) = admin_url("tenants host: retry") else {
        return;
    };
    let (_database, app) = database_of_an_ordinary_role(&admin, "hostretry");
    let mut host = Host::start("retry", &app, &[]);
    host.wait_ready();
    // The tenant is registered (as the CLI does) naming a package that is not there yet.
    let package = std::env::temp_dir().join(format!("refhir-host-late-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&package);
    let (code, said) = cli(
        &app,
        &[
            "create",
            "late",
            "--update-as-create",
            "--package",
            package.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{said}");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, health) = call("GET", &format!("{}/healthz", host.base()), &[], None);
        if health["tenants"]["late"]["status"] == "unavailable" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "late never failed to open:\n{}",
            host.log()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        call(
            "GET",
            &format!("{}/t/late/metadata", host.base()),
            &[],
            None
        )
        .0,
        503
    );
    // Nothing else changes in the registry; the package appears, and the process finds it.
    let made = types_package("late");
    std::fs::rename(&made, &package).unwrap();
    host.wait_serving("late");
    assert_eq!(put_patient(&host, "late", "p1", "Lee"), 201);
    let _ = std::fs::remove_dir_all(package);
}

#[test]
fn a_replica_or_standby_of_the_host_refuses_to_start_without_pinned_smart_secrets() {
    let Some(admin) = admin_url("tenants host: pinned secrets") else {
        return;
    };
    let (_database, app) = database_of_an_ordinary_role(&admin, "hostsecrets");
    for role in [
        ["--standby"].as_slice(),
        ["--role", "replica", "--primary-url", "http://127.0.0.1:1"].as_slice(),
    ] {
        let dir = std::env::temp_dir().join(format!("refhir-host-secrets-{}", free_port()));
        std::fs::create_dir_all(&dir).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_refhir"))
            .current_dir(&dir)
            .args(["--postgres", &app, "--all-tenants"])
            .args(["--bind", &format!("127.0.0.1:{}", free_port())])
            .args(role)
            .env("REFHIR_SKIP_DEFAULT_PACKAGES", "1")
            .env_remove("REFHIR_SMART_DCR_TOKEN")
            .env_remove("REFHIR_SMART_CONFIDENTIAL_SECRET")
            .output()
            .expect("run refhir");
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{role:?} started");
        assert!(said.contains("--smart-dcr-token"), "{role:?}: {said}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
