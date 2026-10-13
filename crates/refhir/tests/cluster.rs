//! Several `refhir` processes on one Postgres database: the writer lease, failover, replicas.
//!
//! These start the real binary, so they cover what the in-process tests cannot: process
//! exit, a killed primary, the load balancer's view through `/healthz`. Each test creates a
//! database of its own, because the writer lease is an advisory lock and those are per
//! database. They need `HAIFHIR_POSTGRES_URL` (a role that may `CREATE DATABASE`); without it
//! they are skipped, unless `REFHIR_REQUIRE_POSTGRES` is set, as it is in CI.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

mod common;
use common::{admin_url, fresh_database};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A directory with the resource types a user needs to author conformance resources.
fn types_package() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "refhir-cluster-types-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"example.structures","version":"1.0.0","fhirVersion":"4.0.1"}"#,
    )
    .unwrap();
    for type_name in ["SearchParameter", "ValueSet", "CodeSystem", "ConceptMap"] {
        std::fs::write(
            dir.join(format!("StructureDefinition-{type_name}.json")),
            format!(
                r#"{{"resourceType":"StructureDefinition","url":"http://hl7.org/fhir/StructureDefinition/{type_name}","name":"{type_name}","status":"active","kind":"resource","abstract":false,"type":"{type_name}","fhirVersion":"4.0.1","derivation":"specialization","snapshot":{{"element":[{{"path":"{type_name}","min":0,"max":"*"}}]}}}}"#
            ),
        )
        .unwrap();
    }
    dir
}

struct Node {
    name: String,
    child: Child,
    port: u16,
    log: PathBuf,
    /// The node's working directory and its types package, removed when the node is dropped.
    scratch: Vec<PathBuf>,
}

impl Node {
    /// `args` picks the role: nothing for a primary, `--standby`, or `--role replica ...`.
    fn start(name: &str, database: &str, args: &[&str]) -> Node {
        Node::spawn(name, &["--postgres", database], args)
    }

    /// A node on a SQLite file, which several processes on one machine can share as a
    /// primary and standbys.
    fn start_sqlite(name: &str, file: &Path, args: &[&str]) -> Node {
        Node::spawn(name, &["--sqlite", file.to_str().unwrap()], args)
    }

    fn spawn(name: &str, database_args: &[&str], args: &[&str]) -> Node {
        let port = free_port();
        let dir = std::env::temp_dir().join(format!("refhir-node-{name}-{port}"));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("server.log");
        let log_file = std::fs::File::create(&log).unwrap();
        let minir4 = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fhir/fixtures/minir4");
        let types = types_package();
        let child = Command::new(env!("CARGO_BIN_EXE_refhir"))
            .current_dir(&dir)
            .args(database_args)
            .args(["--bind", &format!("127.0.0.1:{port}")])
            .arg("--package")
            .arg(&minir4)
            .arg("--package")
            .arg(&types)
            .args(["--export-dir", dir.join("export").to_str().unwrap()])
            .args(args)
            .env("REFHIR_SKIP_DEFAULT_PACKAGES", "1")
            // A POST gets an id of the server's choosing. These tests read their data back by
            // an id they picked, so they create it with a PUT, which needs this.
            .env("REFHIR_UPDATE_AS_CREATE", "true")
            .env("REFHIR_PACKAGE_CACHE", dir.join("packages"))
            .stdout(Stdio::from(log_file.try_clone().unwrap()))
            .stderr(Stdio::from(log_file))
            .spawn()
            .expect("start refhir");
        Node {
            name: name.to_string(),
            child,
            port,
            log,
            scratch: vec![dir, types],
        }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// `/healthz` status and body, or `None` if nothing is listening.
    fn health(&self) -> Option<(u16, Value)> {
        self.probe("/healthz")
    }

    /// Status and body of a GET, or `None` if nothing is listening.
    fn probe(&self, path: &str) -> Option<(u16, Value)> {
        match ureq::get(&format!("{}{path}", self.base()))
            .timeout(Duration::from_secs(2))
            .call()
        {
            Ok(response) => {
                let status = response.status();
                Some((
                    status,
                    serde_json::from_str(&response.into_string().ok()?).ok()?,
                ))
            }
            Err(ureq::Error::Status(status, response)) => Some((
                status,
                serde_json::from_str(&response.into_string().ok()?).unwrap_or(Value::Null),
            )),
            Err(_) => None,
        }
    }

    fn wait_ready(&mut self, seconds: u64) {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "{} exited ({status}) before it was ready:\n{}",
                    self.name,
                    self.log()
                );
            }
            if let Some((200, _)) = self.health() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{} was not ready in {seconds}s:\n{}",
                self.name,
                self.log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_for_status(&mut self, expected: u16, seconds: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            if let Some((status, body)) = self.health() {
                if status == expected {
                    return body;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{} never answered /healthz with {expected}:\n{}",
                self.name,
                self.log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.kill();
        for dir in &self.scratch {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

struct Reply {
    status: u16,
    ts: Option<u64>,
    role: String,
    body: Value,
}

fn request(
    method: &str,
    url: &str,
    body: Option<&Value>,
    headers: &[(&str, &str)],
) -> Result<Reply, String> {
    let mut request = ureq::request(method, url).timeout(Duration::from_secs(20));
    for (name, value) in headers {
        request = request.set(name, value);
    }
    let outcome = match body {
        Some(body) => request
            .set("content-type", "application/fhir+json")
            .send_string(&body.to_string()),
        None => request.call(),
    };
    let response = match outcome {
        Ok(response) | Err(ureq::Error::Status(_, response)) => response,
        Err(err) => return Err(err.to_string()),
    };
    let status = response.status();
    let ts = response
        .header("x-refhir-commit-ts")
        .and_then(|value| value.parse().ok());
    let role = response.header("x-refhir-role").unwrap_or("").to_string();
    let text = response.into_string().unwrap_or_default();
    Ok(Reply {
        status,
        ts,
        role,
        body: serde_json::from_str(&text).unwrap_or(Value::Null),
    })
}

fn patient(id: &str) -> Value {
    json!({"resourceType": "Patient", "id": id, "active": true, "name": [{"family": "Ann"}]})
}

/// Creates a Patient with this id.
fn put_patient(base: &str, id: &str) -> Result<Reply, String> {
    request(
        "PUT",
        &format!("{base}/Patient/{id}"),
        Some(&patient(id)),
        &[],
    )
}

fn get_patient(base: &str, id: &str, min_ts: Option<u64>) -> Result<Reply, String> {
    let min = min_ts.map(|ts| ts.to_string());
    let headers: Vec<(&str, &str)> = min
        .as_deref()
        .map(|value| vec![("x-refhir-min-ts", value)])
        .unwrap_or_default();
    request("GET", &format!("{base}/Patient/{id}"), None, &headers)
}

#[test]
fn a_second_primary_refuses_to_start() {
    let Some(admin) = admin_url("cluster: second primary") else {
        return;
    };
    let database = fresh_database(&admin, "second");
    let mut first = Node::start("first", &database, &[]);
    first.wait_ready(60);

    let mut second = Node::start("second", &database, &[]);
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = second.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the second primary kept running");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(!status.success());
    assert!(
        second
            .log()
            .contains("another process holds the writer lease"),
        "{}",
        second.log()
    );
    // The first is unaffected.
    assert!(put_patient(&first.base(), "still-works").unwrap().status < 300);
}

#[test]
fn a_sqlite_standby_takes_over_on_the_same_machine() {
    let dir = std::env::temp_dir().join(format!("refhir-sqlite-cluster-{}", free_port()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("shared.sqlite");
    let mut primary = Node::start_sqlite("sqlite-primary", &file, &[]);
    primary.wait_ready(60);
    let mut second = Node::start_sqlite("sqlite-second", &file, &[]);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if second.child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a second primary on the same file kept running"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        second
            .log()
            .contains("another process holds the writer lease"),
        "{}",
        second.log()
    );

    let mut standby = Node::start_sqlite("sqlite-standby", &file, &["--standby"]);
    standby.wait_for_status(503, 30);
    let before = put_patient(&primary.base(), "kept").unwrap();
    let before_ts = before.ts.expect("a commit timestamp");
    primary.kill();
    standby.wait_ready(60);
    let after = put_patient(&standby.base(), "next").unwrap();
    assert!(after.ts.expect("a commit timestamp") > before_ts);
    assert_eq!(
        get_patient(&standby.base(), "kept", None).unwrap().status,
        200
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_primary_answers_probes_while_it_starts_and_is_ready_afterwards() {
    let dir = std::env::temp_dir().join(format!("refhir-probes-{}", free_port()));
    std::fs::create_dir_all(&dir).unwrap();
    // The full R4 package takes many seconds to install, so the primary is still starting when
    // it first answers.
    let r4 = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fhir/fixtures/r4");
    let mut starting = Node::start_sqlite(
        "probes-starting",
        &dir.join("starting.sqlite"),
        &["--package", r4.to_str().unwrap()],
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while starting.probe("/livez").is_none() {
        assert!(
            starting.child.try_wait().unwrap().is_none() && Instant::now() < deadline,
            "never answered while starting:\n{}",
            starting.log()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Not a refused connection: the process is up, says so, and says it cannot take traffic.
    assert_eq!(
        starting.probe("/livez"),
        Some((200, json!({"status": "ok"})))
    );
    let (status, ready) = starting.probe("/readyz").unwrap();
    assert_eq!(status, 503, "{ready}");
    assert_eq!(ready["status"], "starting");
    assert!(ready["reason"].as_str().is_some_and(|r| !r.is_empty()));
    assert_eq!(starting.probe("/healthz").unwrap().0, 503);
    assert_eq!(starting.probe("/metadata").unwrap().0, 503);
    starting.kill();

    let mut ready = Node::start_sqlite("probes-ready", &dir.join("ready.sqlite"), &[]);
    ready.wait_ready(60);
    assert_eq!(ready.probe("/readyz"), Some((200, json!({"status": "ok"}))));
    assert_eq!(ready.probe("/livez"), Some((200, json!({"status": "ok"}))));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_standby_takes_over_and_timestamps_keep_increasing() {
    let Some(admin) = admin_url("cluster: standby takeover") else {
        return;
    };
    let database = fresh_database(&admin, "takeover");
    let mut primary = Node::start("primary", &database, &[]);
    primary.wait_ready(60);
    let mut standby = Node::start("standby", &database, &["--standby"]);
    // Until it holds the lease, a standby is up but not ready.
    let waiting = standby.wait_for_status(503, 30);
    assert_eq!(waiting["role"], "standby");
    assert_eq!(waiting["status"], "starting");

    let mut last = 0;
    for n in 0..5 {
        let reply = put_patient(&primary.base(), &format!("before-{n}")).unwrap();
        assert!(reply.status < 300, "{}", reply.body);
        last = reply.ts.expect("a commit timestamp");
    }
    assert_eq!(
        standby.health().unwrap().0,
        503,
        "still waiting while the primary lives"
    );

    primary.kill();
    standby.wait_ready(60);
    let ready = standby.health().unwrap();
    assert_eq!(ready.1["role"], "primary", "the standby is the primary now");

    for n in 0..5 {
        let reply = put_patient(&standby.base(), &format!("after-{n}")).unwrap();
        assert!(reply.status < 300, "{}", reply.body);
        let ts = reply.ts.expect("a commit timestamp");
        assert!(ts > last, "timestamp {ts} did not move past {last}");
        last = ts;
    }
    // Everything the old primary acknowledged is still there.
    for n in 0..5 {
        let read = get_patient(&standby.base(), &format!("before-{n}"), None).unwrap();
        assert_eq!(read.status, 200, "before-{n} was lost");
    }
}

#[test]
fn killing_the_primary_during_writes_loses_no_acknowledged_write() {
    let Some(admin) = admin_url("cluster: failover under load") else {
        return;
    };
    let database = fresh_database(&admin, "failover");
    let mut primary = Node::start("primary", &database, &[]);
    primary.wait_ready(60);
    let mut standby = Node::start("standby", &database, &["--standby"]);
    standby.wait_for_status(503, 30);

    let bases = [primary.base(), standby.base()];
    let acked: Arc<Mutex<Vec<(String, u64, usize)>>> = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (acked, stop) = (acked.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(Ordering::SeqCst) {
                // A write that gets no answer (the primary died under it) is not retried: its
                // outcome is unknown, so it is simply not counted. Every id is new.
                let id = format!("w{n}");
                n += 1;
                let mut done = false;
                for (which, base) in bases.iter().enumerate() {
                    if let Ok(reply) = put_patient(base, &id) {
                        if reply.status < 300 {
                            let ts = reply.ts.expect("a commit timestamp");
                            acked.lock().unwrap().push((id.clone(), ts, which));
                            done = true;
                            break;
                        }
                    }
                }
                if !done {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        })
    };

    std::thread::sleep(Duration::from_millis(2000));
    primary.kill();
    standby.wait_ready(60);
    std::thread::sleep(Duration::from_millis(2000));
    stop.store(true, Ordering::SeqCst);
    writer.join().unwrap();

    let acked = acked.lock().unwrap().clone();
    let from_primary = acked.iter().filter(|entry| entry.2 == 0).count();
    let from_standby = acked.iter().filter(|entry| entry.2 == 1).count();
    assert!(
        from_primary >= 3,
        "only {from_primary} writes reached the primary"
    );
    assert!(
        from_standby >= 3,
        "only {from_standby} writes reached the new primary"
    );
    // One writer, in order: every acknowledged commit has a later timestamp than the one
    // before it, across the failover.
    for pair in acked.windows(2) {
        assert!(
            pair[1].1 > pair[0].1,
            "timestamps went from {} ({}) to {} ({})",
            pair[0].1,
            pair[0].0,
            pair[1].1,
            pair[1].0
        );
    }
    for (id, _, _) in &acked {
        let read = get_patient(&standby.base(), id, None).unwrap();
        assert_eq!(read.status, 200, "{id} was acknowledged and is gone");
    }
}

#[test]
fn a_replica_follows_the_primary_and_forwards_writes() {
    let Some(admin) = admin_url("cluster: replica") else {
        return;
    };
    let database = fresh_database(&admin, "replica");
    let mut primary = Node::start("primary", &database, &[]);
    primary.wait_ready(60);
    let mut replica = Node::start(
        "replica",
        &database,
        &["--role", "replica", "--primary-url", &primary.base()],
    );
    replica.wait_ready(60);
    let health = replica.health().unwrap().1;
    assert_eq!(health["role"], "replica");
    assert!(health["lag_ts"].is_number());

    // A write sent to the replica is the primary's to make.
    let written = put_patient(&replica.base(), "via-replica").unwrap();
    assert!(written.status < 300, "{}", written.body);
    assert_eq!(written.role, "primary");
    let ts = written.ts.expect("a commit timestamp");

    // Asking for at least that timestamp always sees the write, wherever it is answered.
    let read = get_patient(&replica.base(), "via-replica", Some(ts)).unwrap();
    assert_eq!(read.status, 200, "{}", read.body);

    // Without it the replica catches up on its own within moments.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let read = get_patient(&replica.base(), "via-replica", None).unwrap();
        if read.status == 200 {
            assert_eq!(read.role, "replica");
            break;
        }
        assert!(Instant::now() < deadline, "the replica never caught up");
        std::thread::sleep(Duration::from_millis(50));
    }

    // A replica that is cut off from the database would leave rotation; one that follows is in.
    assert_eq!(replica.health().unwrap().0, 200);
}

/// A blocking `/sync` client: sends JSON text frames and reads them back with a deadline.
struct Sync {
    socket: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
}

impl Sync {
    fn connect(base: &str) -> Sync {
        let url = format!("{}/sync", base.replace("http://", "ws://"));
        let (socket, _) = tungstenite::connect(url).expect("open /sync");
        Sync { socket }
    }

    fn send(&mut self, message: Value) {
        self.socket
            .send(tungstenite::Message::Text(message.to_string()))
            .expect("send on /sync");
    }

    /// The next JSON message, or `None` if nothing arrives in `seconds`.
    fn next(&mut self, seconds: u64) -> Option<Value> {
        if let tungstenite::stream::MaybeTlsStream::Plain(stream) = self.socket.get_mut() {
            stream
                .set_read_timeout(Some(Duration::from_secs(seconds)))
                .unwrap();
        }
        loop {
            match self.socket.read() {
                Ok(tungstenite::Message::Text(text)) => return serde_json::from_str(&text).ok(),
                Ok(tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_)) => continue,
                Ok(other) => panic!("unexpected /sync frame {other:?}"),
                Err(tungstenite::Error::Io(err))
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return None
                }
                Err(err) => panic!("/sync failed: {err}"),
            }
        }
    }

    /// Reads until a message of this `type` arrives (live queries also send others).
    fn expect(&mut self, kind: &str, seconds: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            if let Some(message) = self.next(1) {
                if message["type"] == kind {
                    return message;
                }
            }
        }
        panic!("no {kind} message on /sync within {seconds}s");
    }
}

fn family_of(transition: &Value, id: &str) -> Option<String> {
    transition["modifications"][0]["value"]["entry"]
        .as_array()?
        .iter()
        .find(|entry| entry["resource"]["id"] == id)
        .and_then(|entry| entry["resource"]["name"][0]["family"].as_str())
        .map(str::to_string)
}

#[test]
fn a_replica_serves_sync_live_queries_and_sends_mutations_to_the_primary() {
    let Some(admin) = admin_url("cluster: /sync on a replica") else {
        return;
    };
    let database = fresh_database(&admin, "syncreplica");
    let mut primary = Node::start("primary", &database, &[]);
    primary.wait_ready(60);
    let mut replica = Node::start(
        "replica",
        &database,
        &["--role", "replica", "--primary-url", &primary.base()],
    );
    replica.wait_ready(60);

    // A live query on the replica, over a connection to the replica process.
    let mut subscriber = Sync::connect(&replica.base());
    subscriber.send(json!({
        "type": "ModifyQuerySet",
        "baseVersion": 0,
        "newVersion": 1,
        "modifications": [{
            "type": "Add", "queryId": 1, "udfPath": "Patient/search", "args": {"active": "true"}
        }]
    }));
    let first = subscriber.expect("Transition", 10);
    assert_eq!(first["modifications"][0]["value"]["total"], 0, "{first}");

    // 1. A mutation sent to the replica is committed by the primary. The caller hears the
    //    commit timestamp once the replica has the write too, and the live query on the
    //    replica gets the new row.
    let mut writer = Sync::connect(&replica.base());
    writer.send(json!({
        "type": "Mutation", "requestId": 7, "udfPath": "Patient/update",
        "args": {"id": "via-sync", "resource": patient("via-sync")}
    }));
    let reply = writer.expect("MutationResponse", 10);
    assert_eq!(reply["success"], true, "{reply}");
    assert_eq!(reply["requestId"], 7);
    let committed: u64 = reply["ts"].as_str().unwrap().parse().unwrap();
    let on_primary = get_patient(&primary.base(), "via-sync", Some(committed)).unwrap();
    assert_eq!(on_primary.status, 200, "{}", on_primary.body);
    assert_eq!(on_primary.role, "primary");
    let seen = subscriber.expect("Transition", 10);
    assert_eq!(
        family_of(&seen, "via-sync").as_deref(),
        Some("Ann"),
        "{seen}"
    );

    // 2. A write made directly on the primary reaches the same live query on the replica:
    //    the replica reads the commit's index entries from the database, not from a message.
    let direct = put_patient(&primary.base(), "via-rest").unwrap();
    assert!(direct.status < 300, "{}", direct.body);
    let seen = subscriber.expect("Transition", 10);
    assert_eq!(
        family_of(&seen, "via-rest").as_deref(),
        Some("Ann"),
        "{seen}"
    );

    // The replica answered both from its own snapshot, not by forwarding the query.
    assert_eq!(replica.health().unwrap().1["role"], "replica");
}

#[test]
fn processes_started_together_on_an_empty_database_all_come_up() {
    // The first start of a whole deployment: two writers and a replica launched at the same
    // moment, none of them with a schema to find. They all race to create it, and exactly one
    // writer must win the lease and serve while the other waits and the replica follows.
    let Some(admin) = admin_url("cluster: simultaneous first start") else {
        return;
    };
    let database = fresh_database(&admin, "together");
    let mut first = Node::start("writer-a", &database, &["--standby"]);
    let mut second = Node::start("writer-b", &database, &["--standby"]);
    let mut replica = Node::start("replica", &database, &["--role", "replica"]);

    let deadline = Instant::now() + Duration::from_secs(90);
    let roles = loop {
        for node in [&mut first, &mut second, &mut replica] {
            if let Some(status) = node.child.try_wait().unwrap() {
                panic!(
                    "{} exited ({status}) while starting:\n{}",
                    node.name,
                    node.log()
                );
            }
        }
        let a = first.health();
        let b = second.health();
        let is_primary = |health: &Option<(u16, Value)>| matches!(health, Some((200, body)) if body["role"] == "primary");
        let is_waiting = |health: &Option<(u16, Value)>| matches!(health, Some((503, _)));
        if (is_primary(&a) && is_waiting(&b)) || (is_primary(&b) && is_waiting(&a)) {
            break (a, b);
        }
        assert!(
            Instant::now() < deadline,
            "no writer became the primary:\n{}\n{}",
            first.log(),
            second.log()
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    println!("writer-a: {:?}\nwriter-b: {:?}", roles.0, roles.1);
    replica.wait_ready(90);

    // Nobody crashed, and the deployment works end to end.
    for node in [&mut first, &mut second, &mut replica] {
        assert!(
            node.child.try_wait().unwrap().is_none(),
            "{} exited:\n{}",
            node.name,
            node.log()
        );
    }
    let primary = if matches!(roles.0, Some((200, _))) {
        &first
    } else {
        &second
    };
    let written = put_patient(&primary.base(), "first-of-all").unwrap();
    assert!(written.status < 300, "{}", written.body);
    // This replica was started without --primary-url (nobody knew yet which writer would win),
    // so it cannot forward and just follows. It catches up within moments.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let read = get_patient(&replica.base(), "first-of-all", None).unwrap();
        if read.status == 200 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the replica never saw the write: {}",
            read.body
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_replica_started_first_waits_for_the_primary() {
    let Some(admin) = admin_url("cluster: replica first") else {
        return;
    };
    let database = fresh_database(&admin, "early");
    let mut replica = Node::start("replica", &database, &["--role", "replica"]);
    let waiting = replica.wait_for_status(503, 30);
    assert_eq!(waiting["role"], "replica");
    assert_eq!(waiting["status"], "starting");

    let mut primary = Node::start("primary", &database, &[]);
    primary.wait_ready(60);
    replica.wait_ready(60);
    assert_eq!(replica.health().unwrap().1["role"], "replica");
}

#[test]
fn a_catalog_change_on_the_primary_reaches_the_replica() {
    let Some(admin) = admin_url("cluster: catalog reload") else {
        return;
    };
    let database = fresh_database(&admin, "catalog");
    let mut primary = Node::start("primary", &database, &[]);
    primary.wait_ready(60);
    let mut replica = Node::start(
        "replica",
        &database,
        &["--role", "replica", "--primary-url", &primary.base()],
    );
    replica.wait_ready(60);

    let search_params = |node: &Node| -> Vec<String> {
        let metadata = request("GET", &format!("{}/metadata", node.base()), None, &[]).unwrap();
        metadata.body["rest"][0]["resource"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|resource| resource["type"] == "Patient")
            .flat_map(|resource| {
                resource["searchParam"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|param| param["name"].as_str().map(str::to_string))
            .collect()
    };
    assert!(!search_params(&replica).contains(&"label".to_string()));

    // Created through the replica, committed by the primary, which marks the parameter ready
    // once its index is built and bumps the catalog version.
    let created = request(
        "POST",
        &format!("{}/SearchParameter", replica.base()),
        Some(&json!({
            "resourceType": "SearchParameter",
            "id": "label",
            "url": "http://example.org/SearchParameter/label",
            "name": "label",
            "status": "active",
            "description": "A label",
            "code": "label",
            "base": ["Patient"],
            "type": "string",
            "expression": "Patient.name"
        })),
        &[],
    )
    .unwrap();
    assert!(created.status < 300, "{}", created.body);

    let deadline = Instant::now() + Duration::from_secs(15);
    while !search_params(&replica).contains(&"label".to_string()) {
        assert!(
            Instant::now() < deadline,
            "the replica never learned the new search parameter:\n{}",
            replica.log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // And it works there.
    let put = put_patient(&primary.base(), "named").unwrap();
    let ts = put.ts.unwrap().to_string();
    let found = request(
        "GET",
        &format!("{}/Patient?label=Ann", replica.base()),
        None,
        &[("x-refhir-min-ts", &ts)],
    )
    .unwrap();
    assert_eq!(found.status, 200, "{}", found.body);
    assert_eq!(found.body["total"], 1);
}
