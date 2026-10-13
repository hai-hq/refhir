use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use refhir_database::Database;
use refhir_fhir::Catalog;
use refhir_persistence::{
    ConflictStrategy, DocumentLogEntry, IndexScanRow, PersistError, Persistence,
    PersistenceGlobalKey, PersistenceIndexEntry, PersistenceReader, TimestampRange,
};
use refhir_sqlite::SqlitePersistence;
use refhir_types::{DocumentId, IndexId, Interval, Order, Timestamp};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::{app_with, bootstrap_router, ServerConfig};

struct Reply {
    status: StatusCode,
    content_type: String,
    body: Value,
    raw: String,
}

async fn get(app: &axum::Router, uri: &str, extra: &[(&str, &str)]) -> Reply {
    let mut request = Request::builder().method("GET").uri(uri);
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let raw = String::from_utf8_lossy(&bytes).to_string();
    Reply {
        status,
        content_type,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        raw,
    }
}

/// A database that can be made to fail or to hang, to see what `/readyz` does about it. Only the
/// one query the probe runs goes through the switches: everything else is the real SQLite.
struct Flaky {
    inner: SqlitePersistence,
    down: AtomicBool,
    hang: AtomicBool,
    pings: AtomicUsize,
}

impl PersistenceReader for Flaky {
    fn load_documents(
        &self,
        range: TimestampRange,
        order: Order,
    ) -> Result<Vec<DocumentLogEntry>, PersistError> {
        self.inner.load_documents(range, order)
    }
    fn latest_document(
        &self,
        id: &DocumentId,
        snapshot: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        self.inner.latest_document(id, snapshot)
    }
    fn previous_revision(
        &self,
        id: &DocumentId,
        at: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        self.inner.previous_revision(id, at)
    }
    fn exact_revision(
        &self,
        id: &DocumentId,
        ts: Timestamp,
    ) -> Result<Option<DocumentLogEntry>, PersistError> {
        self.inner.exact_revision(id, ts)
    }
    fn document_history(
        &self,
        id: &DocumentId,
        snapshot: Timestamp,
    ) -> Result<Vec<DocumentLogEntry>, PersistError> {
        self.inner.document_history(id, snapshot)
    }
    fn index_scan(
        &self,
        index_id: &IndexId,
        read_timestamp: Timestamp,
        interval: &Interval,
        order: Order,
    ) -> Result<Vec<IndexScanRow>, PersistError> {
        self.inner
            .index_scan(index_id, read_timestamp, interval, order)
    }
    fn get_persistence_global(
        &self,
        key: PersistenceGlobalKey,
    ) -> Result<Option<Value>, PersistError> {
        self.pings.fetch_add(1, Ordering::SeqCst);
        // Held for at most a few seconds, so a test that forgets to release it still ends.
        for _ in 0..100 {
            if !self.hang.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        if self.down.load(Ordering::SeqCst) {
            return Err(PersistError::Definite(
                "connection refused: db.internal:5432".into(),
            ));
        }
        self.inner.get_persistence_global(key)
    }
    fn max_ts(&self) -> Result<Option<Timestamp>, PersistError> {
        self.inner.max_ts()
    }
    fn load_index_writes(
        &self,
        range: TimestampRange,
    ) -> Result<Vec<PersistenceIndexEntry>, PersistError> {
        self.inner.load_index_writes(range)
    }
}

impl Persistence for Flaky {
    fn is_fresh(&self) -> bool {
        self.inner.is_fresh()
    }
    fn advance_published_ts(&self, ts: Timestamp) -> Result<(), PersistError> {
        self.inner.advance_published_ts(ts)
    }
    fn reader(&self) -> Arc<dyn PersistenceReader> {
        self.inner.reader()
    }
    fn write(
        &self,
        documents: &[DocumentLogEntry],
        indexes: &[PersistenceIndexEntry],
        conflict_strategy: ConflictStrategy,
    ) -> Result<(), PersistError> {
        self.inner.write(documents, indexes, conflict_strategy)
    }
    fn write_persistence_global(
        &self,
        key: PersistenceGlobalKey,
        value: &Value,
    ) -> Result<(), PersistError> {
        self.inner.write_persistence_global(key, value)
    }
}

fn flaky_app(config: ServerConfig) -> (axum::Router, Arc<Flaky>) {
    let inner = SqlitePersistence::open_in_memory().unwrap();
    let flaky = Arc::new(Flaky {
        inner: inner.clone(),
        down: AtomicBool::new(false),
        hang: AtomicBool::new(false),
        pings: AtomicUsize::new(0),
    });
    let db = Arc::new(Database::open(flaky.clone()).unwrap());
    let catalog = Arc::new(RwLock::new(Catalog::empty()));
    (app_with(db, catalog, Arc::new(inner), config), flaky)
}

/// Every probe asks the database, and a stuck query is given up on after 200ms.
fn uncached() -> ServerConfig {
    ServerConfig {
        ready_cache: Duration::ZERO,
        ready_timeout: Duration::from_millis(200),
        ..ServerConfig::default()
    }
}

fn app(config: ServerConfig) -> axum::Router {
    flaky_app(config).0
}

#[tokio::test]
async fn before_startup_completes_livez_is_200_and_readyz_is_503() {
    // What answers while packages install and indexes backfill: the real router does not exist
    // yet, so this is the whole server.
    let early = bootstrap_router("primary", "installing packages");

    let live = get(&early, "/livez", &[]).await;
    assert_eq!(live.status, StatusCode::OK);
    assert_eq!(live.body, json!({"status": "ok"}));

    let ready = get(&early, "/readyz", &[]).await;
    assert_eq!(ready.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(ready.content_type, "application/json");
    assert_eq!(
        ready.body,
        json!({"status": "starting", "reason": "installing packages"})
    );

    // The older endpoint keeps saying "not ready" here, as the load balancers rely on.
    let health = get(&early, "/healthz", &[]).await;
    assert_eq!(health.status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn after_startup_both_probes_are_200_and_small() {
    let app = app(ServerConfig::default());
    for uri in ["/livez", "/readyz"] {
        let reply = get(&app, uri, &[]).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        assert_eq!(reply.content_type, "application/json", "{uri}");
        assert_eq!(reply.body, json!({"status": "ok"}), "{uri}");
        assert_eq!(reply.raw, r#"{"status":"ok"}"#, "{uri}");
    }
}

#[tokio::test]
async fn the_probes_need_no_token_when_smart_is_on() {
    let app = app(ServerConfig {
        auth: true,
        smart: Some(Arc::new(crate::auth::SmartStore::with_defaults())),
        ..ServerConfig::default()
    });
    // SMART is really on: a FHIR read without a token is refused.
    assert_eq!(
        get(&app, "/Patient", &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
    for uri in ["/livez", "/readyz"] {
        let anonymous = get(&app, uri, &[]).await;
        assert_eq!(anonymous.status, StatusCode::OK, "{uri}");
        assert_eq!(anonymous.body, json!({"status": "ok"}), "{uri}");
        // A token that is wrong is not looked at either.
        let wrong = get(&app, uri, &[("authorization", "Bearer nonsense")]).await;
        assert_eq!(wrong.status, StatusCode::OK, "{uri}");
    }
}

#[tokio::test]
async fn with_the_database_unavailable_readyz_is_503_and_livez_stays_200() {
    let (app, db) = flaky_app(uncached());
    assert_eq!(get(&app, "/readyz", &[]).await.status, StatusCode::OK);

    db.down.store(true, Ordering::SeqCst);
    let down = get(&app, "/readyz", &[]).await;
    assert_eq!(down.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        down.body,
        json!({"status": "unavailable", "reason": "database unreachable"})
    );
    // The cause is for the log: an open endpoint does not say where the database is.
    assert!(!down.raw.contains("db.internal"), "{}", down.raw);
    // The process is still alive, so a liveness probe must not restart it.
    assert_eq!(get(&app, "/livez", &[]).await.status, StatusCode::OK);

    db.down.store(false, Ordering::SeqCst);
    assert_eq!(get(&app, "/readyz", &[]).await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_hung_database_check_is_cut_off_and_not_started_twice() {
    let (app, db) = flaky_app(uncached());
    db.hang.store(true, Ordering::SeqCst);

    let first = get(&app, "/readyz", &[]).await;
    assert_eq!(first.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(first.body["status"], "unavailable");
    assert_eq!(first.body["reason"], "database did not answer in time");
    assert_eq!(get(&app, "/livez", &[]).await.status, StatusCode::OK);

    // The first query is still stuck: more probes answer at once and add no more queries, so a
    // hung database holds one thread however often a kubelet asks.
    for _ in 0..5 {
        let again = get(&app, "/readyz", &[]).await;
        assert_eq!(again.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            again.body["reason"],
            "the previous database check has not returned"
        );
    }
    assert_eq!(db.pings.load(Ordering::SeqCst), 1);

    // When the database comes back the probe recovers by itself.
    db.hang.store(false, Ordering::SeqCst);
    let mut ready = false;
    for _ in 0..80 {
        if get(&app, "/readyz", &[]).await.status == StatusCode::OK {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(ready, "readyz never recovered");
}

#[tokio::test]
async fn overlapping_probes_share_one_query_and_a_result_is_reused() {
    let (app, db) = flaky_app(ServerConfig::default());
    // A slow query that several probes overlap with: none of them is turned away, and the
    // database sees one query between them.
    db.hang.store(true, Ordering::SeqCst);
    let release = db.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        release.hang.store(false, Ordering::SeqCst);
    });
    let probes = (0..8).map(|_| get(&app, "/readyz", &[]));
    for reply in futures_util::future::join_all(probes).await {
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
    }
    assert_eq!(db.pings.load(Ordering::SeqCst), 1);

    // Within the cache interval the answer is reused, even if the database has since failed.
    db.down.store(true, Ordering::SeqCst);
    assert_eq!(get(&app, "/readyz", &[]).await.status, StatusCode::OK);
    assert_eq!(db.pings.load(Ordering::SeqCst), 1);

    // After it, the database is asked again.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        get(&app, "/readyz", &[]).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(db.pings.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_replica_that_stopped_following_the_primary_is_not_ready() {
    struct Files(std::path::PathBuf);
    impl Drop for Files {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm", ".lock"] {
                let mut name = self.0.as_os_str().to_owned();
                name.push(suffix);
                let _ = std::fs::remove_file(name);
            }
        }
    }
    let files =
        Files(std::env::temp_dir().join(format!("refhir-readyz-{}.sqlite", std::process::id())));
    let writer = Arc::new(SqlitePersistence::open(&files.0).unwrap());
    let _primary = Database::open(writer).unwrap();
    let reader = Arc::new(SqlitePersistence::open(&files.0).unwrap());
    let replica = Arc::new(Database::open_read_only(reader.clone()).unwrap());
    let app = app_with(
        replica.clone(),
        Arc::new(RwLock::new(Catalog::empty())),
        reader,
        ServerConfig {
            max_staleness: Duration::from_secs(1),
            ..ServerConfig::default()
        },
    );

    assert_eq!(get(&app, "/readyz", &[]).await.status, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let stale = get(&app, "/readyz", &[]).await;
    assert_eq!(stale.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        stale.body,
        json!({
            "status": "unavailable",
            "reason": "replica has stopped following the primary"
        })
    );
    assert_eq!(get(&app, "/livez", &[]).await.status, StatusCode::OK);

    tokio::task::spawn_blocking(move || replica.refresh().unwrap())
        .await
        .unwrap();
    assert_eq!(get(&app, "/readyz", &[]).await.status, StatusCode::OK);
}

/// Counts the events a thread emits, so a test can say a request logged nothing.
struct CountEvents(Arc<AtomicUsize>);

impl tracing::Subscriber for CountEvents {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn the_probes_are_not_logged_at_any_level_and_other_requests_are() {
    // One thread runs the request, so the subscriber set here sees what the middleware logs.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let app = {
        let _enter = runtime.enter();
        app(ServerConfig::default())
    };
    let events = Arc::new(AtomicUsize::new(0));
    let subscriber = CountEvents(events.clone());
    tracing::subscriber::with_default(subscriber, || {
        runtime.block_on(async {
            // The control: an ordinary request is logged, so the counter works.
            assert_eq!(get(&app, "/metadata", &[]).await.status, StatusCode::OK);
            assert!(events.load(Ordering::SeqCst) >= 1);
            events.store(0, Ordering::SeqCst);

            for uri in ["/livez", "/readyz"] {
                assert_eq!(get(&app, uri, &[]).await.status, StatusCode::OK);
            }
            assert_eq!(events.load(Ordering::SeqCst), 0);
        });
    });
}
