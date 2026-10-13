//! Rate limits, metering, request logs, quotas and the audit trail.
//!
//! Five things a server that other people's software depends on has to have, and ReFHIR had
//! none of. They share one piece of state, [`Governance`], and one idea: **work out who is
//! asking once, at the edge, and let everything after that read it.**
//!
//! * A **rate limit** is a token bucket per caller, charged by what the request costs
//!   (`classify`): an export is dearer than a read. It runs after authentication, so the caller
//!   is a client and user, not an address, and before the handler, so a refused request costs
//!   nothing. A second, smaller bucket per address counts failed authentications, so that a
//!   guessed token is stopped before it reaches the database.
//! * **Metering** counts what each caller used per UTC day and class, into memory, and a
//!   background task writes it in batches: the stores serialise every write with the clinical
//!   data, so a write per request would queue behind (and in front of) patient reads.
//! * A **quota** is a ceiling on those daily and monthly totals. It is checked against the same
//!   counters the meter keeps, reread from the database now and then to see what other servers
//!   counted.
//! * The **request log** is one structured line per request, with a request id that the response
//!   carries back, and optionally a bounded copy in the database.
//! * The **audit trail** is written where the data is touched (`http::execute`), not where the
//!   request arrives, so REST, GraphQL and `/sync` are all covered, and a transaction leaves a
//!   row for every document it wrote.

mod admin;
mod audit;
mod civil;
mod classify;
mod config;
#[cfg(test)]
mod integration;
mod limiter;
mod message;
mod meter;
mod middleware;
mod scope;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, HeaderMap, HeaderValue, Request, StatusCode};
use axum::response::Response;
use refhir_persistence::{AuditRow, GovernanceStore, MemoryGovernanceStore, RequestLogRow};
use serde_json::json;
use tokio::sync::Notify;

use limiter::{Decision, Limiter};
use meter::{Breach, Meter, Observation};

pub use config::{
    AuditConfig, Cidr, ClientRate, Costs, GovernanceConfig, ProxyConfig, QuotaConfig, QuotaLimits,
    RateLimitConfig, RequestLogConfig,
};

pub(crate) use admin::{audit_events, audit_verify, requests, usage};
pub(crate) use audit::begin as begin_audit;
pub(crate) use classify::{classify, classify_operation, Class};
pub(crate) use message::{json_len, SyncMessage};
pub(crate) use middleware::{gate, govern};
pub(crate) use scope::{current, in_blocking, Identity, RequestScope};

/// Most audit rows held while the database cannot take them. Past this the newest are dropped
/// and counted, because a server that stops answering to keep a log is a worse failure than a
/// log with a hole in it, and the hole is reported.
const AUDIT_BUFFER_MAX: usize = 100_000;
const REQUEST_BUFFER_MAX: usize = 20_000;
/// Enough waiting rows to write now rather than at the next tick.
const FLUSH_AT: usize = 500;
/// Rows written in one transaction. The stores serve clinical reads and writes over the same
/// connection, so a backlog is written a slice at a time, and between slices the clinical work
/// gets its turn.
const FLUSH_CHUNK: usize = 1_000;
const MAINTENANCE_EVERY: Duration = Duration::from_secs(60);
/// How often a server says it is alive, and for how long that counts. Three beats and a half,
/// so that one missed beat does not move anyone's share.
const MEMBERS_EVERY: Duration = Duration::from_secs(10);
const MEMBERS_TTL: Duration = Duration::from_secs(35);
const LIMITER_CAPACITY: usize = 200_000;
/// The longest path, parameter list and search string kept in a row. A caller chooses all three,
/// including a caller nobody has signed in, so each row is bounded in size whatever was sent.
const MAX_PATH_STORED: usize = 512;
const MAX_PARAMS_STORED: usize = 256;
const MAX_QUERY_STORED: usize = 1_000;

/// Unix milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[derive(Default)]
struct Buffers {
    requests: Vec<RequestLogRow>,
    audit: Vec<AuditRow>,
}

/// One answered request, for [`Governance::finish`].
pub(crate) struct Finished<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub class: Class,
    pub status: u16,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub elapsed: Duration,
}

pub struct Governance {
    pub(crate) config: GovernanceConfig,
    /// Whether SMART is on, which sets what a caller with no token is allowed.
    auth: bool,
    store: Arc<dyn GovernanceStore>,
    limiter: Limiter,
    failures: Limiter,
    meter: Meter,
    buffers: Mutex<Buffers>,
    wake: Arc<Notify>,
    flushing: tokio::sync::Mutex<()>,
    epoch: Instant,
    sync_connections: Mutex<HashMap<String, u32>>,
    dropped_requests: AtomicU64,
    dropped_audit: AtomicU64,
    /// Per-second budgets for rows about refused traffic, by kind.
    budget: Limiter,
    /// Who this process is among the servers sharing the database, and how many of them there
    /// were at the last heartbeat. Read per request as one atomic, never as a query.
    owner: String,
    servers: AtomicU32,
    /// The cost of the dearest request a caller with a token can make, and one without.
    dearest: u32,
    dearest_anonymous: u32,
    /// Rows not kept for want of budget, since the server started.
    sampled_requests: AtomicU64,
    sampled_audit: AtomicU64,
    /// Audit rows not kept since the last summary row.
    unsummarised_audit: AtomicU64,
    /// Whether a verification of the audit chain is running, and the head last logged.
    verifying: std::sync::atomic::AtomicBool,
    last_checkpoint: Mutex<Option<(u64, Vec<u8>)>>,
    last_loss_log: AtomicU64,
    last_throttle_log: AtomicU64,
    suppressed_throttle_logs: AtomicU64,
}

impl Governance {
    /// `store` is where rows go. Without one they live in memory and end with the process.
    pub fn new(
        config: GovernanceConfig,
        auth: bool,
        store: Option<Arc<dyn GovernanceStore>>,
    ) -> Arc<Self> {
        let dearest = config.rate_limit.dearest(&Class::ALL);
        // With SMART on, a caller with no token can reach discovery and the OAuth endpoints.
        let dearest_anonymous = if auth {
            config.rate_limit.dearest(&[Class::Discovery, Class::Auth])
        } else {
            dearest
        };
        Arc::new(Self {
            dearest,
            dearest_anonymous,
            owner: format!("{}-{}", std::process::id(), uuid::Uuid::new_v4()),
            servers: AtomicU32::new(1),
            config,
            auth,
            store: store.unwrap_or_else(|| Arc::new(MemoryGovernanceStore::default())),
            limiter: Limiter::new(LIMITER_CAPACITY),
            failures: Limiter::new(LIMITER_CAPACITY),
            meter: Meter::default(),
            buffers: Mutex::new(Buffers::default()),
            wake: Arc::new(Notify::new()),
            flushing: tokio::sync::Mutex::new(()),
            epoch: Instant::now(),
            sync_connections: Mutex::new(HashMap::new()),
            dropped_requests: AtomicU64::new(0),
            dropped_audit: AtomicU64::new(0),
            budget: Limiter::new(8),
            sampled_requests: AtomicU64::new(0),
            sampled_audit: AtomicU64::new(0),
            unsummarised_audit: AtomicU64::new(0),
            verifying: std::sync::atomic::AtomicBool::new(false),
            last_checkpoint: Mutex::new(None),
            last_loss_log: AtomicU64::new(0),
            last_throttle_log: AtomicU64::new(0),
            suppressed_throttle_logs: AtomicU64::new(0),
        })
    }

    pub fn active(&self) -> bool {
        self.config.active()
    }

    pub(crate) fn store(&self) -> &Arc<dyn GovernanceStore> {
        &self.store
    }

    fn now_nanos(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    pub(crate) fn new_request_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// The caller's own request id when it sent a sane one, so that a trace can be followed
    /// across services; otherwise a new one. Anything else could carry a newline into a log.
    pub(crate) fn request_id(&self, headers: &HeaderMap) -> String {
        headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
            })
            .map(str::to_string)
            .unwrap_or_else(|| self.new_request_id())
    }

    /// The address of the client: the connection's, or with `trusted_hops` reverse proxies in
    /// front, the one they report.
    pub(crate) fn client_ip(&self, request: &Request<Body>) -> Option<IpAddr> {
        let peer = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|info| info.0.ip());
        let forwarded: Vec<&str> = request
            .headers()
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        // The header is the proxies' word only on a connection from one of them.
        let proxy = &self.config.proxy;
        let from_a_proxy = proxy.trusted_peers.is_empty()
            || peer.is_some_and(|peer| {
                let peer = scope::unmapped(peer);
                proxy.trusted_peers.iter().any(|net| net.contains(peer))
            });
        let hops = if from_a_proxy { proxy.trusted_hops } else { 0 };
        let ip = forwarded_ip(peer, &forwarded.join(","), hops).map(scope::unmapped);
        if ip.is_none() {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                tracing::warn!(
                    "a request carried no connection address, so callers with no token cannot be \
                     told apart and share one limit; serve the router with \
                     `into_make_service_with_connect_info::<SocketAddr>()` (`refhir_server::serve` \
                     does)"
                )
            });
        }
        ip
    }

    /// How many servers share each limit: the number that was set, or the number alive at the
    /// last heartbeat.
    pub(crate) fn share(&self) -> u32 {
        self.config
            .rate_limit
            .servers
            .unwrap_or_else(|| self.servers.load(Ordering::Relaxed))
            .max(1)
    }

    /// This server's part of a caller's limit: the configured rate and bucket, divided among the
    /// servers that share it.
    fn limits_for(&self, identity: &Option<Identity>) -> (f64, u32) {
        let rate = &self.config.rate_limit;
        let ((per_second, burst), dearest) = match identity {
            Some(identity) => (rate.for_client(&identity.client_id), self.dearest),
            None => (rate.anonymous(self.auth), self.dearest_anonymous),
        };
        rate.divided(per_second, burst, self.share(), dearest)
    }

    /// Whether this request may go on: the caller's quota, then their rate. A refused request
    /// is marked on the scope so that it is counted as throttled and not as served.
    pub(crate) async fn admit(&self, scope: &RequestScope, class: Class) -> Result<(), Rejection> {
        let config = &self.config;
        let identity = scope.identity();
        if config.quota.enabled && class.counts_toward_quota() {
            if let Some(identity) = &identity {
                let limits = config.quota.for_client(&identity.client_id);
                if limits.any() {
                    let principal = scope.principal();
                    let now = now_ms();
                    self.meter
                        .ensure_loaded(&self.store, &principal, now, config.quota.refresh)
                        .await;
                    if let Some(breach) = self.meter.check(&principal, &limits, class, now) {
                        scope.mark_throttled();
                        return Err(Rejection::quota(&breach, now));
                    }
                }
            }
        }
        if config.rate_limit.enabled {
            let (rate, burst) = self.limits_for(&identity);
            let cost = config.rate_limit.costs.of(class);
            // A rate of zero exempts the caller, and a cost of zero the request.
            if rate > 0.0 && cost > 0 {
                let decision =
                    self.limiter
                        .take(&scope.limiter_key(), rate, burst, cost, self.now_nanos());
                scope.set_decision(decision);
                if !decision.allowed {
                    scope.mark_throttled();
                    return Err(Rejection::rate(decision));
                }
            }
        }
        Ok(())
    }

    /// [`admit`](Self::admit), unless this request has already been let through. Work that has
    /// to happen before the limiting layer (reading a patient-scoped caller's body) asks first,
    /// and the layer then does not charge the caller twice.
    pub(crate) async fn admit_once(
        &self,
        scope: &RequestScope,
        class: Class,
    ) -> Result<(), Rejection> {
        if scope.admitted() {
            return Ok(());
        }
        self.admit(scope, class).await?;
        scope.mark_admitted();
        Ok(())
    }

    /// The limit on failed authentications, per address, as a per-second refill.
    fn failure_rate(&self) -> (f64, u32) {
        let per_minute = self.config.rate_limit.auth_failures_per_minute.max(1);
        self.config
            .rate_limit
            .divided(f64::from(per_minute) / 60.0, per_minute, self.share(), 1)
    }

    /// Whether this address has failed to authenticate so often lately that a request carrying
    /// a credential should be turned away before the credential is looked up.
    pub(crate) fn failures_exhausted(&self, ip: Option<IpAddr>) -> Option<Rejection> {
        if !self.config.rate_limit.enabled || self.config.rate_limit.auth_failures_per_minute == 0 {
            return None;
        }
        let (rate, burst) = self.failure_rate();
        if self
            .failures
            .has_token(&scope::ip_key(ip), rate, burst, self.now_nanos())
        {
            return None;
        }
        // A token comes back every 1 / rate seconds.
        Some(Rejection::failures((1.0 / rate).ceil() as u64))
    }

    pub(crate) fn note_auth_failure(&self, ip: Option<IpAddr>) {
        if !self.config.rate_limit.enabled || self.config.rate_limit.auth_failures_per_minute == 0 {
            return;
        }
        let (rate, burst) = self.failure_rate();
        self.failures
            .take(&scope::ip_key(ip), rate, burst, 1, self.now_nanos());
    }

    /// Hold a slot for a `/sync` connection, or say there are too many. The slot is given back
    /// when the permit is dropped.
    pub(crate) fn open_sync_connection(
        self: &Arc<Self>,
        scope: &RequestScope,
    ) -> Result<SyncPermit, Rejection> {
        let limit = self.config.rate_limit.max_sync_connections;
        if !self.config.rate_limit.enabled || limit == 0 {
            return Ok(SyncPermit { held: None });
        }
        // This server's part of it, rounded up so that no server is left with none.
        let limit = limit.div_ceil(self.share());
        let key = scope.limiter_key();
        let mut held = self.sync_connections.lock().expect("governance");
        let count = held.entry(key.clone()).or_insert(0);
        if *count >= limit {
            return Err(Rejection::connections(limit));
        }
        *count += 1;
        Ok(SyncPermit {
            held: Some((self.clone(), key)),
        })
    }

    /// Count a live query that was re-run and pushed to a `/sync` client. These are the server's
    /// doing, not requests, so they are metered and never refused: what bounds them is the cap
    /// on live queries.
    pub(crate) fn record_push(&self, scope: &RequestScope, bytes_out: usize) {
        if self.config.metering || self.config.quota.enabled {
            self.meter.record(
                &Observation {
                    principal: &scope.principal(),
                    class: Class::Push,
                    status: 200,
                    throttled: false,
                    bytes_in: 0,
                    bytes_out: bytes_out as u64,
                    elapsed: Duration::ZERO,
                },
                now_ms(),
            );
        }
    }

    /// The most live queries one `/sync` connection may hold, or `None` for no limit.
    pub(crate) fn live_query_cap(&self) -> Option<usize> {
        let rate = &self.config.rate_limit;
        (rate.enabled && rate.max_live_queries > 0).then_some(rate.max_live_queries as usize)
    }

    /// Count one answered request: usage, the request log, and an audit row where the engine
    /// did not write one.
    pub(crate) fn finish(&self, scope: &RequestScope, done: &Finished<'_>) {
        let now = now_ms();
        let principal = scope.principal();
        let throttled = scope.throttled();
        let forwarded = scope.forwarded();
        if (self.config.metering || self.config.quota.enabled) && !forwarded {
            self.meter.record(
                &Observation {
                    principal: &principal,
                    class: done.class,
                    status: done.status,
                    throttled,
                    bytes_in: done.bytes_in,
                    bytes_out: done.bytes_out,
                    elapsed: done.elapsed,
                },
                now,
            );
        }
        if self.config.request_log.enabled {
            self.log_request(scope, &principal, done, throttled, now);
        }
        if self.config.audit.enabled
            && scope.source == scope::Source::Http
            && done.class != Class::Discovery
            && scope.audited() == 0
            && !throttled
            && !forwarded
        {
            if let Some(row) = self.refusal_row(scope, done, now) {
                // A refusal with no caller known costs the server nothing to cause, so there is
                // no limit to how many an outsider can ask for; each keeps its detail only up
                // to a budget. What an identified caller does is never sampled.
                if row.outcome != 0
                    && scope.identity().is_none()
                    && !self.within_budget("audit", self.config.audit.unattributed_per_second)
                {
                    self.sampled_audit.fetch_add(1, Ordering::Relaxed);
                    self.unsummarised_audit.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.push_audit(vec![row]);
                }
            }
        }
    }

    /// Whether one more row of this `kind` may be kept now, at `per_second` rows a second with
    /// room for a burst of five seconds' worth. Zero keeps everything.
    fn within_budget(&self, kind: &str, per_second: f64) -> bool {
        if per_second <= 0.0 {
            return true;
        }
        let burst = (per_second * 5.0).ceil().max(1.0) as u32;
        self.budget
            .take(kind, per_second, burst, 1, self.now_nanos())
            .allowed
    }

    /// How many rows were not kept for want of budget: request log, then audit.
    pub(crate) fn sampled(&self) -> (u64, u64) {
        (
            self.sampled_requests.load(Ordering::Relaxed),
            self.sampled_audit.load(Ordering::Relaxed),
        )
    }

    /// The audit row for a request that never reached the engine: a refusal at the door, or an
    /// interaction that is not with a resource (the OAuth endpoints, `$export`, `/sync`, the
    /// admin endpoints).
    fn refusal_row(&self, scope: &RequestScope, done: &Finished<'_>, now: u64) -> Option<AuditRow> {
        let outcome = audit::outcome_of(done.status);
        let quiet = !self.config.audit.reads
            && outcome == 0
            && matches!(done.class, Class::Read | Class::Search);
        if quiet {
            return None;
        }
        let (action, subtype) = classify::describe(done.method, done.path, done.class);
        let identity = scope.identity();
        let segments: Vec<&str> = done
            .path
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        let on_resource = matches!(done.class, Class::Read | Class::Search | Class::Write)
            && segments
                .first()
                .is_some_and(|first| !first.starts_with(['_', '$']) && *first != "export");
        // What the path names is whatever the caller wrote, and this request may be one nobody
        // is signed in for. A segment is recorded as a resource type or id only if it is one.
        let resource_type = on_resource
            .then(|| audit::resource_type_name(segments[0]))
            .flatten()
            .map(str::to_string);
        let resource_id = resource_type
            .as_ref()
            .and_then(|_| segments.get(1))
            .and_then(|id| audit::resource_id_text(id))
            .map(str::to_string);
        let patient = match (&resource_type, &resource_id) {
            (Some(kind), Some(id)) if kind == "Patient" => Some(id.clone()),
            _ => None,
        };
        let query = (self.config.audit.include_query && done.class == Class::Search)
            .then(|| crate::http::redact_query(done.query))
            .flatten()
            .filter(|query| !query.is_empty())
            .map(|query| audit::truncate(&query, MAX_QUERY_STORED));
        Some(AuditRow {
            ts_ms: now,
            request_id: scope.request_id.clone(),
            source: scope.source.name().to_string(),
            action: action.to_string(),
            subtype,
            outcome,
            status: done.status,
            principal: scope.principal(),
            client_id: identity.as_ref().map(|identity| identity.client_id.clone()),
            user: identity.as_ref().and_then(|identity| identity.user.clone()),
            fhir_user: identity.and_then(|identity| identity.fhir_user),
            remote_ip: scope.remote_ip.map(|ip| ip.to_string()),
            resource_type,
            resource_id,
            patient,
            query,
            detail: None,
            ..AuditRow::default()
        })
    }

    fn log_request(
        &self,
        scope: &RequestScope,
        principal: &str,
        done: &Finished<'_>,
        throttled: bool,
        now: u64,
    ) {
        // A flood of refused requests would otherwise be a flood of log lines.
        let (logged, suppressed) = if throttled {
            match self.throttle_log_slot(now) {
                Some(suppressed) => (true, suppressed),
                None => (false, 0),
            }
        } else {
            (true, 0)
        };
        if logged {
            let query = crate::http::redact_query(done.query);
            let elapsed_ms = done.elapsed.as_secs_f64() * 1000.0;
            let remote = scope.remote_ip.map(|ip| ip.to_string());
            macro_rules! line {
                ($level:ident) => {
                    tracing::$level!(
                        request_id = %scope.request_id,
                        principal,
                        remote = remote.as_deref(),
                        source = scope.source.name(),
                        class = done.class.name(),
                        method = done.method,
                        path = done.path,
                        query = query.as_deref(),
                        status = done.status,
                        elapsed_ms,
                        bytes_in = done.bytes_in,
                        bytes_out = done.bytes_out,
                        throttled,
                        suppressed_throttled = suppressed,
                        "http request"
                    )
                };
            }
            if done.status >= 500 {
                line!(error);
            } else if done.status >= 400 {
                line!(warn);
            } else {
                line!(info);
            }
        }
        // A request that was turned away, or failed for a caller nobody knows, is not worth a
        // database write each: a flood of them would be a flood of writes made on its behalf.
        // Without SMART nobody is known, and a 404 is an answer like any other.
        let refused = throttled || (self.auth && principal == "anonymous" && done.status >= 400);
        if self.config.request_log.store
            && !(refused
                && !self.within_budget("requests", self.config.request_log.refused_per_second))
        {
            let params = done
                .query
                .map(|query| {
                    let names = query
                        .split('&')
                        .filter_map(|pair| pair.split('=').next())
                        .filter(|name| !name.is_empty() && *name != "access_token")
                        .collect::<Vec<_>>()
                        .join(",");
                    audit::truncate(&names, MAX_PARAMS_STORED)
                })
                .unwrap_or_default();
            self.push_request(RequestLogRow {
                id: 0,
                ts_ms: now,
                request_id: scope.request_id.clone(),
                principal: principal.to_string(),
                remote_ip: scope.remote_ip.map(|ip| ip.to_string()),
                method: done.method.to_string(),
                path: audit::truncate(done.path, MAX_PATH_STORED),
                params,
                class: done.class.name().to_string(),
                status: done.status,
                duration_ms: u64::try_from(done.elapsed.as_millis()).unwrap_or(u64::MAX),
                bytes_in: done.bytes_in,
                bytes_out: done.bytes_out,
            });
        } else if self.config.request_log.store {
            self.sampled_requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether this refused request may be logged now. At most one a second is, and it says how
    /// many were not.
    fn throttle_log_slot(&self, now: u64) -> Option<u64> {
        let last = self.last_throttle_log.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= 1_000
            && self
                .last_throttle_log
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            return Some(self.suppressed_throttle_logs.swap(0, Ordering::Relaxed));
        }
        self.suppressed_throttle_logs
            .fetch_add(1, Ordering::Relaxed);
        None
    }

    fn push_request(&self, row: RequestLogRow) {
        let mut buffers = self.buffers.lock().expect("governance");
        if buffers.requests.len() >= REQUEST_BUFFER_MAX {
            drop(buffers);
            self.dropped_requests.fetch_add(1, Ordering::Relaxed);
            self.report_loss("request log");
            return;
        }
        buffers.requests.push(row);
        let full = buffers.requests.len() >= FLUSH_AT;
        drop(buffers);
        if full {
            self.wake.notify_one();
        }
    }

    pub(crate) fn push_audit(&self, mut rows: Vec<AuditRow>) {
        if rows.is_empty() {
            return;
        }
        let mut buffers = self.buffers.lock().expect("governance");
        let room = AUDIT_BUFFER_MAX.saturating_sub(buffers.audit.len());
        let lost = rows.len().saturating_sub(room);
        if lost > 0 {
            rows.truncate(room);
        }
        buffers.audit.extend(rows);
        let full = buffers.audit.len() >= FLUSH_AT;
        drop(buffers);
        if lost > 0 {
            self.dropped_audit.fetch_add(lost as u64, Ordering::Relaxed);
            self.report_loss("audit trail");
        }
        if full {
            self.wake.notify_one();
        }
    }

    /// Say that rows are being dropped, at most once a minute.
    fn report_loss(&self, what: &str) {
        let now = now_ms();
        let last = self.last_loss_log.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= 60_000
            && self
                .last_loss_log
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::error!(
                audit_dropped = self.dropped_audit.load(Ordering::Relaxed),
                request_log_dropped = self.dropped_requests.load(Ordering::Relaxed),
                "the {what} buffer is full because the database is not taking writes; \
                 rows are being dropped"
            );
        }
    }

    /// How many rows were dropped because the database was not taking writes: request log,
    /// then audit.
    pub(crate) fn dropped(&self) -> (u64, u64) {
        (
            self.dropped_requests.load(Ordering::Relaxed),
            self.dropped_audit.load(Ordering::Relaxed),
        )
    }

    /// Write what is waiting. What cannot be written is kept for the next time.
    pub async fn flush(&self) {
        let _one_at_a_time = self.flushing.lock().await;
        self.summarise_sampled();
        let usage = self.meter.take_pending();
        let (requests, audit) = {
            let mut buffers = self.buffers.lock().expect("governance");
            (
                std::mem::take(&mut buffers.requests),
                std::mem::take(&mut buffers.audit),
            )
        };
        if usage.is_empty() && requests.is_empty() && audit.is_empty() {
            return;
        }
        let store = self.store.clone();
        let chain_key = self.config.audit.chain_key.clone();
        // What the write task holds is gone if it stops, so how much there was is kept here.
        let (usage_held, requests_held, audit_held) = (usage.len(), requests.len(), audit.len());
        let written = tokio::task::spawn_blocking(move || {
            let (usage_error, usage) = write_in_chunks(usage, |rows| store.usage_add(rows));
            let (request_error, requests) =
                write_in_chunks(requests, |rows| store.request_log_append(rows));
            let (audit_error, audit) =
                write_in_chunks(audit, |rows| store.audit_append(rows, chain_key.as_ref()));
            (
                usage_error,
                usage,
                request_error,
                requests,
                audit_error,
                audit,
            )
        })
        .await;
        let Ok((usage_error, usage, request_error, requests, audit_error, audit)) = written else {
            // A task that panicked took its rows with it, and would do so again with the next
            // batch if it was the store that panicked: they are counted as lost, so that
            // `/_usage` and the log say so.
            self.dropped_requests
                .fetch_add(requests_held as u64, Ordering::Relaxed);
            self.dropped_audit
                .fetch_add(audit_held as u64, Ordering::Relaxed);
            tracing::error!(
                usage_rows = usage_held,
                request_log_rows = requests_held,
                audit_rows = audit_held,
                "the task writing usage, request log and audit rows stopped; the rows it held \
                 are lost"
            );
            return;
        };
        // What is left is what was not written, so a chunk that went in is never written twice.
        if let Some(err) = usage_error {
            tracing::error!(error = %err, rows = usage.len(), "could not write usage; will retry");
            self.meter.restore(usage);
        }
        if let Some(err) = request_error {
            tracing::error!(error = %err, rows = requests.len(), "could not write the request log; will retry");
            self.requeue(
                requests,
                |buffers| &mut buffers.requests,
                REQUEST_BUFFER_MAX,
                &self.dropped_requests,
            );
        }
        if let Some(err) = audit_error {
            tracing::error!(error = %err, rows = audit.len(), "could not write the audit trail; will retry");
            self.requeue(
                audit,
                |buffers| &mut buffers.audit,
                AUDIT_BUFFER_MAX,
                &self.dropped_audit,
            );
        }
    }

    /// One audit row for the refusals that were counted and not kept, so that the gap in the
    /// trail is on record, with how big it was.
    fn summarise_sampled(&self) {
        let count = self.unsummarised_audit.swap(0, Ordering::Relaxed);
        if count == 0 {
            return;
        }
        self.push_audit(vec![AuditRow {
            ts_ms: now_ms(),
            source: "http".into(),
            action: "E".into(),
            subtype: "sampled".into(),
            outcome: 4,
            principal: "anonymous".into(),
            detail: Some(format!(
                "{count} refused requests with no caller known were counted and not recorded \
                 one by one"
            )),
            ..AuditRow::default()
        }]);
    }

    /// Put rows that could not be written back in front of those that arrived since, up to the
    /// most that are held.
    fn requeue<T>(
        &self,
        mut rows: Vec<T>,
        pick: impl FnOnce(&mut Buffers) -> &mut Vec<T>,
        max: usize,
        dropped: &AtomicU64,
    ) {
        let mut buffers = self.buffers.lock().expect("governance");
        let queue = pick(&mut buffers);
        rows.append(queue);
        if rows.len() > max {
            dropped.fetch_add((rows.len() - max) as u64, Ordering::Relaxed);
            rows.truncate(max);
        }
        *queue = rows;
    }

    /// Log the newest audit row's id and hash when it has changed. The log is a place the
    /// database's writers do not necessarily reach: copied somewhere that is append-only, these
    /// lines are what shows that the newest rows were not deleted, which a chain cannot show
    /// by itself.
    async fn log_checkpoint(&self) {
        if !self.config.audit.enabled {
            return;
        }
        let store = self.store.clone();
        let Ok(Ok(Some((id, hash)))) =
            tokio::task::spawn_blocking(move || store.audit_head()).await
        else {
            return;
        };
        {
            let mut last = self.last_checkpoint.lock().expect("governance");
            if last.as_ref().is_some_and(|(last_id, _)| *last_id == id) {
                return;
            }
            *last = Some((id, hash.clone()));
        }
        tracing::info!(
            target: "refhir_audit",
            id,
            hash = %refhir_persistence::to_hex(&hash),
            keyed = self.config.audit.chain_key.is_some(),
            "audit checkpoint"
        );
    }

    /// Claim the right to verify the audit chain, which reads the whole trail: one at a time.
    pub(crate) fn begin_verify(&self) -> Option<VerifyGuard<'_>> {
        (!self.verifying.swap(true, Ordering::AcqRel)).then_some(VerifyGuard(&self.verifying))
    }

    /// Say that this server is alive, and learn how many are. Called every ten seconds from the
    /// background task and not at all from a request. A database that cannot be reached leaves
    /// the last count in force: a server does not take the whole of a limit because it cannot
    /// see its neighbours.
    pub(crate) async fn refresh_members(&self) {
        if !self.config.rate_limit.enabled || self.config.rate_limit.servers.is_some() {
            return;
        }
        let store = self.store.clone();
        let owner = self.owner.clone();
        let ttl = u64::try_from(MEMBERS_TTL.as_millis()).unwrap_or(u64::MAX);
        let now = now_ms();
        match tokio::task::spawn_blocking(move || store.members_touch(&owner, now, ttl)).await {
            Ok(Ok(count)) => {
                let count = count.max(1);
                if self.servers.swap(count, Ordering::Relaxed) != count {
                    tracing::info!(
                        servers = count,
                        "the servers sharing this database changed; each takes an equal part of \
                         every rate limit"
                    );
                }
            }
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "could not count the servers sharing this database; keeping the last count")
            }
            Err(_) => tracing::warn!("the task counting the servers sharing this database stopped"),
        }
    }

    /// Leave the count, as a server does that is stopping, so that the others take its part at
    /// once.
    pub async fn leave(&self) {
        if !self.config.rate_limit.enabled || self.config.rate_limit.servers.is_some() {
            return;
        }
        let store = self.store.clone();
        let owner = self.owner.clone();
        let _ = tokio::task::spawn_blocking(move || store.members_leave(&owner)).await;
    }

    /// Housekeeping: forget idle buckets and windows, and delete rows past their retention.
    async fn maintain(&self) {
        self.log_checkpoint().await;
        let now = self.now_nanos();
        self.limiter.sweep(now);
        self.failures.sweep(now);
        self.meter.sweep(Duration::from_secs(3_600));
        let now = now_ms();
        let request_before = (self.config.request_log.enabled && self.config.request_log.store)
            .then(|| {
                now.saturating_sub(u64::from(self.config.request_log.retention_hours) * 3_600_000)
            });
        let audit_before = self
            .config
            .audit
            .retention_days
            .map(|days| now.saturating_sub(u64::from(days) * civil::day_start_ms(1)));
        if request_before.is_none() && audit_before.is_none() {
            return;
        }
        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || store.prune(request_before, audit_before)).await {
            Ok(Ok(pruned)) if pruned.request_log + pruned.audit > 0 => {
                tracing::info!(
                    request_log = pruned.request_log,
                    audit = pruned.audit,
                    "deleted rows past their retention"
                );
            }
            Ok(Ok(_)) => {}
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "could not delete rows past their retention")
            }
            Err(_) => tracing::warn!("the task deleting rows past their retention stopped"),
        }
    }

    /// Start writing buffered rows every flush interval, and sooner when enough are waiting.
    /// Ends when the last handle is dropped. Does nothing outside a tokio runtime.
    pub(crate) fn start_background(self: &Arc<Self>) {
        if !self.active() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let this = Arc::downgrade(self);
        let wake = self.wake.clone();
        let every = self.config.flush_interval.max(Duration::from_millis(100));
        runtime.spawn(async move {
            // Nothing has been recorded yet, so the first write is one interval away. That pass
            // also deletes what expired while the server was down.
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
            // A clock that began less than a minute ago cannot be set back by one: `Instant`
            // subtraction panics there, and this task would end before it wrote anything.
            let mut last_maintenance = Instant::now()
                .checked_sub(MAINTENANCE_EVERY)
                .unwrap_or_else(Instant::now);
            // Announce this server at once, so that it takes only its part from its first minute.
            if let Some(governance) = this.upgrade() {
                governance.refresh_members().await;
            }
            let mut last_heartbeat = Instant::now();
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = wake.notified() => {}
                }
                let Some(governance) = this.upgrade() else {
                    break;
                };
                governance.flush().await;
                if last_heartbeat.elapsed() >= MEMBERS_EVERY {
                    last_heartbeat = Instant::now();
                    governance.refresh_members().await;
                }
                if last_maintenance.elapsed() >= MAINTENANCE_EVERY {
                    last_maintenance = Instant::now();
                    governance.maintain().await;
                }
            }
        });
    }
}

/// Write `rows` a chunk at a time. On the first chunk that fails, the error and the rows not yet
/// written (that chunk and all after it) come back; the chunks before it are in the store.
fn write_in_chunks<T>(
    mut rows: Vec<T>,
    write: impl Fn(&[T]) -> Result<(), refhir_persistence::PersistError>,
) -> (Option<refhir_persistence::PersistError>, Vec<T>) {
    let mut written = 0;
    while written < rows.len() {
        let end = (written + FLUSH_CHUNK).min(rows.len());
        if let Err(err) = write(&rows[written..end]) {
            rows.drain(..written);
            return (Some(err), rows);
        }
        written = end;
    }
    (None, Vec::new())
}

/// A verification of the audit chain in progress; the next may begin when this is dropped.
pub(crate) struct VerifyGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for VerifyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A `/sync` connection's place among the caller's allowed few.
pub struct SyncPermit {
    held: Option<(Arc<Governance>, String)>,
}

impl Drop for SyncPermit {
    fn drop(&mut self) {
        if let Some((governance, key)) = self.held.take() {
            let mut held = governance.sync_connections.lock().expect("governance");
            if let Some(count) = held.get_mut(&key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    held.remove(&key);
                }
            }
        }
    }
}

/// The address of the client behind `trusted_hops` reverse proxies. Each proxy appends the
/// address it saw to `X-Forwarded-For`, so the client is that many entries from the right;
/// anything to its left is whatever the client chose to claim. A header with fewer entries
/// than there are proxies did not come through them all, and is not believed.
pub fn forwarded_ip(peer: Option<IpAddr>, forwarded: &str, trusted_hops: u8) -> Option<IpAddr> {
    if trusted_hops == 0 {
        return peer;
    }
    let entries: Vec<&str> = forwarded
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    let hops = usize::from(trusted_hops);
    if entries.len() < hops {
        return peer;
    }
    let entry = entries[entries.len() - hops];
    entry
        .parse::<IpAddr>()
        .ok()
        .or_else(|| entry.parse::<SocketAddr>().ok().map(|address| address.ip()))
        .or(peer)
}

/// Why a request was turned away.
pub struct Rejection {
    message: String,
    retry_after_secs: u64,
    decision: Option<Decision>,
}

impl Rejection {
    fn rate(decision: Decision) -> Self {
        Self {
            message: format!(
                "rate limit exceeded: try again in {}",
                seconds(decision.retry_after_secs())
            ),
            retry_after_secs: decision.retry_after_secs(),
            decision: Some(decision),
        }
    }

    fn quota(breach: &Breach, now_ms: u64) -> Self {
        let wait = breach
            .resets_ms
            .saturating_sub(now_ms)
            .div_ceil(1_000)
            .max(1);
        Self {
            message: format!(
                "{} used up ({} of {}); it starts again at {}",
                breach.what,
                breach.used,
                breach.limit,
                civil::instant_string(breach.resets_ms)
            ),
            retry_after_secs: wait,
            decision: None,
        }
    }

    fn failures(retry_after_secs: u64) -> Self {
        Self {
            message: "too many failed authentication attempts from this address".into(),
            retry_after_secs: retry_after_secs.max(1),
            decision: None,
        }
    }

    fn connections(limit: u32) -> Self {
        Self {
            message: format!("this caller already holds {limit} sync connections"),
            retry_after_secs: 5,
            decision: None,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn retry_after_secs(&self) -> u64 {
        self.retry_after_secs
    }

    pub fn into_response(self) -> Response {
        let mut response = crate::http::fhir_json_public(
            StatusCode::TOO_MANY_REQUESTS,
            json!({
                "resourceType": "OperationOutcome",
                "issue": [{
                    "severity": "error",
                    "code": "throttled",
                    "diagnostics": self.message
                }]
            }),
        );
        let headers = response.headers_mut();
        set_header(headers, "retry-after", &self.retry_after_secs.to_string());
        if let Some(decision) = &self.decision {
            set_rate_headers(headers, decision);
        }
        response
    }
}

fn seconds(count: u64) -> String {
    if count == 1 {
        "1 second".to_string()
    } else {
        format!("{count} seconds")
    }
}

/// `RateLimit-Limit`, `RateLimit-Remaining` and `RateLimit-Reset` (in seconds), as in the IETF
/// draft for these fields, so that a client can pace itself without being refused first.
pub(crate) fn set_rate_headers(headers: &mut HeaderMap, decision: &Decision) {
    set_header(headers, "ratelimit-limit", &decision.limit.to_string());
    set_header(
        headers,
        "ratelimit-remaining",
        &decision.remaining.to_string(),
    );
    set_header(
        headers,
        "ratelimit-reset",
        &decision.reset_secs().to_string(),
    );
}

pub(crate) fn set_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(header::HeaderName::from_static(name), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_is_the_entry_the_trusted_proxies_added() {
        let peer: Option<IpAddr> = Some("10.0.0.1".parse().unwrap());
        let ip = |text: &str| Some(text.parse::<IpAddr>().unwrap());
        // No proxy trusted: the header is whatever the client wrote.
        assert_eq!(forwarded_ip(peer, "6.6.6.6", 0), peer);
        // One proxy: the last entry is what it saw.
        assert_eq!(forwarded_ip(peer, "203.0.113.9", 1), ip("203.0.113.9"));
        // A client that forged an earlier entry gains nothing.
        assert_eq!(
            forwarded_ip(peer, "6.6.6.6, 203.0.113.9", 1),
            ip("203.0.113.9")
        );
        // Two proxies: the one before the last.
        assert_eq!(
            forwarded_ip(peer, "6.6.6.6, 203.0.113.9, 10.0.0.7", 2),
            ip("203.0.113.9")
        );
        // Fewer entries than proxies: it did not pass through them all.
        assert_eq!(forwarded_ip(peer, "203.0.113.9", 2), peer);
        assert_eq!(forwarded_ip(peer, "", 1), peer);
        // Ports are dropped and rubbish falls back to the connection.
        assert_eq!(forwarded_ip(peer, "203.0.113.9:4567", 1), ip("203.0.113.9"));
        assert_eq!(forwarded_ip(peer, "[2001:db8::1]:80", 1), ip("2001:db8::1"));
        assert_eq!(forwarded_ip(peer, "not-an-ip", 1), peer);
    }

    /// A store whose writes fail while `down` is set.
    struct Flaky {
        rows: MemoryGovernanceStore,
        down: std::sync::atomic::AtomicBool,
        /// How many more `audit_append` calls succeed before they start to fail.
        audit_calls: std::sync::atomic::AtomicUsize,
        /// Whether `audit_append` panics, as a store does whose connection mutex is poisoned.
        panics: std::sync::atomic::AtomicBool,
    }

    impl Flaky {
        fn new(down: bool) -> Arc<Self> {
            Arc::new(Self {
                rows: MemoryGovernanceStore::default(),
                down: std::sync::atomic::AtomicBool::new(down),
                audit_calls: std::sync::atomic::AtomicUsize::new(usize::MAX),
                panics: std::sync::atomic::AtomicBool::new(false),
            })
        }
    }

    impl Flaky {
        fn check(&self) -> Result<(), refhir_persistence::PersistError> {
            if self.down.load(Ordering::SeqCst) {
                Err(refhir_persistence::PersistError::Definite(
                    "database is down".into(),
                ))
            } else {
                Ok(())
            }
        }
    }

    impl GovernanceStore for Flaky {
        fn usage_add(
            &self,
            rows: &[refhir_persistence::UsageRow],
        ) -> Result<(), refhir_persistence::PersistError> {
            self.check()?;
            self.rows.usage_add(rows)
        }
        fn usage_query(
            &self,
            filter: &refhir_persistence::UsageFilter,
        ) -> Result<Vec<refhir_persistence::UsageRow>, refhir_persistence::PersistError> {
            self.rows.usage_query(filter)
        }
        fn request_log_append(
            &self,
            rows: &[RequestLogRow],
        ) -> Result<(), refhir_persistence::PersistError> {
            self.check()?;
            self.rows.request_log_append(rows)
        }
        fn request_log_query(
            &self,
            filter: &refhir_persistence::RequestLogFilter,
        ) -> Result<Vec<RequestLogRow>, refhir_persistence::PersistError> {
            self.rows.request_log_query(filter)
        }
        fn audit_append(
            &self,
            rows: &[AuditRow],
            key: Option<&refhir_persistence::ChainKey>,
        ) -> Result<(), refhir_persistence::PersistError> {
            if self.panics.load(Ordering::SeqCst) {
                panic!("the connection mutex is poisoned");
            }
            self.check()?;
            let left = self.audit_calls.load(Ordering::SeqCst);
            if left == 0 {
                return Err(refhir_persistence::PersistError::Definite(
                    "write failed".into(),
                ));
            }
            if left != usize::MAX {
                self.audit_calls.store(left - 1, Ordering::SeqCst);
            }
            self.rows.audit_append(rows, key)
        }
        fn audit_scan(
            &self,
            after_id: u64,
            limit: u32,
        ) -> Result<Vec<refhir_persistence::ChainedRow>, refhir_persistence::PersistError> {
            self.rows.audit_scan(after_id, limit)
        }
        fn audit_head(&self) -> Result<Option<(u64, Vec<u8>)>, refhir_persistence::PersistError> {
            self.rows.audit_head()
        }
        fn audit_query(
            &self,
            filter: &refhir_persistence::AuditFilter,
        ) -> Result<Vec<AuditRow>, refhir_persistence::PersistError> {
            self.rows.audit_query(filter)
        }
        fn members_touch(
            &self,
            owner: &str,
            now_ms: u64,
            ttl_ms: u64,
        ) -> Result<u32, refhir_persistence::PersistError> {
            self.rows.members_touch(owner, now_ms, ttl_ms)
        }
        fn members_leave(&self, owner: &str) -> Result<(), refhir_persistence::PersistError> {
            self.rows.members_leave(owner)
        }
        fn prune(
            &self,
            request_log_before_ms: Option<u64>,
            audit_before_ms: Option<u64>,
        ) -> Result<refhir_persistence::Pruned, refhir_persistence::PersistError> {
            self.rows.prune(request_log_before_ms, audit_before_ms)
        }
    }

    fn event(request_id: &str, ts_ms: u64) -> AuditRow {
        AuditRow {
            ts_ms,
            request_id: request_id.into(),
            source: "http".into(),
            action: "R".into(),
            subtype: "read".into(),
            principal: "client:a".into(),
            ..AuditRow::default()
        }
    }

    fn everything(store: &dyn GovernanceStore) -> Vec<AuditRow> {
        store
            .audit_query(&refhir_persistence::AuditFilter {
                limit: 10_000,
                ..Default::default()
            })
            .unwrap()
    }

    #[tokio::test]
    async fn rows_the_database_would_not_take_are_kept_and_written_once_it_will() {
        let store = Flaky::new(true);
        let governance = Governance::new(GovernanceConfig::observing(), false, Some(store.clone()));
        governance.push_audit(vec![event("a", 1), event("b", 2)]);
        governance.flush().await;
        assert!(
            everything(store.as_ref()).is_empty(),
            "the database is down"
        );
        // More arrive meanwhile: the old ones stay in front.
        governance.push_audit(vec![event("c", 3)]);
        governance.flush().await;
        store.down.store(false, Ordering::SeqCst);
        governance.flush().await;
        let kept: Vec<String> = everything(store.as_ref())
            .into_iter()
            .rev()
            .map(|row| row.request_id)
            .collect();
        assert_eq!(kept, ["a", "b", "c"], "in order, each once");
        governance.flush().await;
        assert_eq!(
            everything(store.as_ref()).len(),
            3,
            "nothing is written twice"
        );
        assert_eq!(governance.dropped(), (0, 0));
    }

    #[tokio::test]
    async fn rows_held_by_a_write_task_that_stopped_are_counted_as_lost_and_not_lost_silently() {
        let store = Flaky::new(false);
        store.panics.store(true, Ordering::SeqCst);
        let governance = Governance::new(GovernanceConfig::observing(), false, Some(store.clone()));
        governance.push_audit(vec![event("a", 1), event("b", 2)]);
        governance.flush().await;
        assert!(everything(store.as_ref()).is_empty());
        assert_eq!(governance.dropped(), (0, 2), "the audit rows are counted");
        // The next batch is lost the same way, and counted the same way.
        governance.push_audit(vec![event("c", 3)]);
        governance.flush().await;
        assert_eq!(governance.dropped(), (0, 3));
    }

    #[tokio::test]
    async fn a_backlog_is_written_a_chunk_at_a_time_and_a_chunk_that_landed_is_not_written_again() {
        let store = Flaky::new(false);
        // Two chunks go in and the third fails.
        store.audit_calls.store(2, Ordering::SeqCst);
        let governance = Governance::new(GovernanceConfig::observing(), false, Some(store.clone()));
        let total = FLUSH_CHUNK * 2 + 500;
        governance.push_audit((0..total).map(|n| event(&format!("r{n}"), 1)).collect());
        governance.flush().await;
        assert_eq!(
            everything(store.as_ref()).len(),
            FLUSH_CHUNK * 2,
            "two chunks are in"
        );
        // The store recovers; the rest goes in, once.
        store.audit_calls.store(usize::MAX, Ordering::SeqCst);
        governance.flush().await;
        let mut ids: Vec<String> = everything(store.as_ref())
            .into_iter()
            .map(|row| row.request_id)
            .collect();
        assert_eq!(ids.len(), total);
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), total, "no row was written twice");
        assert_eq!(governance.dropped(), (0, 0));
    }

    #[tokio::test]
    async fn usage_that_could_not_be_written_is_counted_again_not_lost() {
        let store = Flaky::new(true);
        let governance = Governance::new(GovernanceConfig::observing(), false, Some(store.clone()));
        let scope = RequestScope::new(governance.clone(), "r".into(), scope::Source::Http, None);
        let done = Finished {
            method: "GET",
            path: "/Patient",
            query: None,
            class: Class::Search,
            status: 200,
            bytes_in: 0,
            bytes_out: 10,
            elapsed: Duration::from_millis(3),
        };
        governance.finish(&scope, &done);
        governance.flush().await;
        governance.finish(&scope, &done);
        store.down.store(false, Ordering::SeqCst);
        governance.flush().await;
        let rows = store
            .usage_query(&refhir_persistence::UsageFilter {
                principal: Some("anonymous".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].counters.requests, 2);
        assert_eq!(rows[0].counters.bytes_out, 20);
    }

    #[tokio::test]
    async fn the_buffer_is_bounded_and_says_what_it_dropped() {
        let governance = Governance::new(GovernanceConfig::observing(), false, None);
        let many: Vec<AuditRow> = (0..AUDIT_BUFFER_MAX + 25)
            .map(|n| event(&n.to_string(), 1))
            .collect();
        governance.push_audit(many);
        assert_eq!(governance.dropped(), (0, 25));
        governance.flush().await;
        // The store numbers rows from 1 as they come in, so the newest row's id is how many
        // it holds: the buffer's worth, and not the 25 that did not fit.
        let newest = governance
            .store()
            .audit_query(&refhir_persistence::AuditFilter {
                limit: 1,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(newest[0].id, AUDIT_BUFFER_MAX as u64);
    }

    #[tokio::test]
    async fn rows_past_their_retention_are_deleted_and_the_rest_kept() {
        let mut config = GovernanceConfig::observing();
        config.audit.retention_days = Some(30);
        config.request_log.retention_hours = 2;
        let governance = Governance::new(config, false, None);
        let now = now_ms();
        let day = 86_400_000;
        governance.push_audit(vec![
            event("ancient", now - 31 * day),
            event("recent", now - 29 * day),
        ]);
        let request = |id: &str, ts_ms| RequestLogRow {
            request_id: id.into(),
            ts_ms,
            principal: "client:a".into(),
            method: "GET".into(),
            path: "/".into(),
            class: "read".into(),
            status: 200,
            ..RequestLogRow::default()
        };
        governance.push_request(request("old", now - 3 * 3_600_000));
        governance.push_request(request("new", now - 3_600_000));
        governance.flush().await;
        governance.maintain().await;
        let store = governance.store();
        let audit: Vec<String> = everything(store.as_ref())
            .into_iter()
            .map(|row| row.request_id)
            .collect();
        assert_eq!(audit, ["recent"]);
        let log: Vec<String> = store
            .request_log_query(&refhir_persistence::RequestLogFilter {
                limit: 10,
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .map(|row| row.request_id)
            .collect();
        assert_eq!(log, ["new"]);
    }

    #[tokio::test]
    async fn audit_rows_are_chained_with_the_key_when_they_are_written() {
        let key = refhir_persistence::ChainKey::new(b"a key for this test only".to_vec());
        let mut config = GovernanceConfig::observing();
        config.audit.chain_key = Some(key.clone());
        let governance = Governance::new(config, false, None);
        governance.push_audit(vec![event("a", 1), event("b", 2), event("c", 3)]);
        governance.flush().await;
        let store = governance.store().clone();
        let report =
            refhir_persistence::verify_audit_chain(store.as_ref(), Some(&key), None).unwrap();
        assert!(report.intact() && report.checked == 3, "{report:?}");
        let unkeyed = refhir_persistence::verify_audit_chain(store.as_ref(), None, None).unwrap();
        assert_eq!(
            unkeyed.breaks_total, 3,
            "without the key none of it can be vouched for"
        );
    }

    #[tokio::test]
    async fn the_newest_audit_row_is_logged_as_a_checkpoint_when_it_changes() {
        let governance = Governance::new(GovernanceConfig::observing(), false, None);
        governance.log_checkpoint().await;
        assert!(
            governance.last_checkpoint.lock().unwrap().is_none(),
            "nothing to say yet"
        );
        governance.push_audit(vec![event("a", 1), event("b", 2)]);
        governance.flush().await;
        governance.log_checkpoint().await;
        let first = governance
            .last_checkpoint
            .lock()
            .unwrap()
            .clone()
            .expect("a checkpoint");
        assert_eq!(first.0, 2);
        assert_eq!(first.1.len(), 32);
        governance.push_audit(vec![event("c", 3)]);
        governance.flush().await;
        governance.log_checkpoint().await;
        let second = governance.last_checkpoint.lock().unwrap().clone().unwrap();
        assert_eq!(second.0, 3);
        assert_ne!(second.1, first.1);
    }

    #[test]
    fn only_one_verification_of_the_chain_runs_at_a_time() {
        let governance = Governance::new(GovernanceConfig::observing(), false, None);
        let first = governance.begin_verify().expect("the first");
        assert!(governance.begin_verify().is_none(), "the second waits");
        drop(first);
        assert!(
            governance.begin_verify().is_some(),
            "and may begin once the first is done"
        );
    }

    #[tokio::test]
    async fn audit_is_kept_forever_unless_a_retention_is_set() {
        let governance = Governance::new(GovernanceConfig::observing(), false, None);
        governance.push_audit(vec![event("ancient", 1)]);
        governance.flush().await;
        governance.maintain().await;
        assert_eq!(everything(governance.store().as_ref()).len(), 1);
    }

    #[tokio::test]
    async fn a_caller_may_hold_only_so_many_sync_connections_and_a_closed_one_frees_its_place() {
        let mut config = GovernanceConfig::observing();
        config.rate_limit.enabled = true;
        config.rate_limit.max_sync_connections = 2;
        let governance = Governance::new(config, false, None);
        let scope = RequestScope::new(governance.clone(), "r".into(), scope::Source::Http, None);
        let first = governance.open_sync_connection(&scope).ok().expect("first");
        let second = governance
            .open_sync_connection(&scope)
            .ok()
            .expect("second");
        let third = governance.open_sync_connection(&scope);
        assert!(third.is_err(), "the third is refused");
        let message = third.err().unwrap();
        assert!(
            message.message().contains("2 sync connections"),
            "{}",
            message.message()
        );
        drop(first);
        let again = governance
            .open_sync_connection(&scope)
            .ok()
            .expect("a place was freed");
        drop((second, again));
        assert!(
            governance.sync_connections.lock().unwrap().is_empty(),
            "nothing is left behind"
        );
        // Another caller was never counted against this one.
        let other = RequestScope::new(
            governance.clone(),
            "r2".into(),
            scope::Source::Http,
            Some("198.51.100.1".parse().unwrap()),
        );
        assert!(governance.open_sync_connection(&other).is_ok());
    }

    fn limited(rate: f64, burst: u32) -> GovernanceConfig {
        let mut config = GovernanceConfig::observing();
        config.rate_limit.enabled = true;
        config.rate_limit.requests_per_second = rate;
        config.rate_limit.burst = burst;
        // Every request costs one, so that a bucket divides into whole requests and nothing is
        // held at the cost of the dearest.
        config.rate_limit.costs = Costs {
            read: 1,
            search: 1,
            write: 1,
            bundle: 1,
            operation: 1,
            export: 1,
            sync: 1,
            auth: 1,
            discovery: 1,
            admin: 1,
        };
        config
    }

    /// Anonymous requests of cost 1 let through before the first refusal.
    async fn let_through(governance: &Arc<Governance>) -> usize {
        let scope = RequestScope::new(governance.clone(), "r".into(), scope::Source::Http, None);
        let mut served = 0;
        while governance.admit(&scope, Class::Read).await.is_ok() && served < 1_000 {
            served += 1;
        }
        served
    }

    #[tokio::test]
    async fn servers_sharing_a_database_each_take_an_equal_part_of_a_limit() {
        let rows: Arc<dyn GovernanceStore> = Arc::new(MemoryGovernanceStore::default());
        let one = Governance::new(limited(0.001, 40), false, Some(rows.clone()));
        let two = Governance::new(limited(0.001, 40), false, Some(rows.clone()));
        // Before anyone has said anything, each takes the whole.
        assert_eq!(one.share(), 1);
        assert_eq!(let_through(&one).await, 40);
        // A second server arrives; the first learns of it at its next beat.
        let one = Governance::new(limited(0.001, 40), false, Some(rows.clone()));
        one.refresh_members().await;
        two.refresh_members().await;
        one.refresh_members().await;
        assert_eq!((one.share(), two.share()), (2, 2));
        assert_eq!(let_through(&one).await, 20, "half the bucket each");
        assert_eq!(let_through(&two).await, 20);
        // A third.
        let three = Governance::new(limited(0.001, 40), false, Some(rows.clone()));
        three.refresh_members().await;
        assert_eq!(three.share(), 3);
        assert_eq!(let_through(&three).await, 14, "a third, rounded up");
    }

    #[tokio::test]
    async fn a_server_that_leaves_gives_its_part_back_at_once() {
        let rows: Arc<dyn GovernanceStore> = Arc::new(MemoryGovernanceStore::default());
        let one = Governance::new(limited(1.0, 10), false, Some(rows.clone()));
        let two = Governance::new(limited(1.0, 10), false, Some(rows.clone()));
        one.refresh_members().await;
        two.refresh_members().await;
        one.refresh_members().await;
        assert_eq!(one.share(), 2);
        two.leave().await;
        one.refresh_members().await;
        assert_eq!(one.share(), 1);
    }

    #[tokio::test]
    async fn a_pinned_number_of_servers_ignores_the_heartbeat() {
        let rows: Arc<dyn GovernanceStore> = Arc::new(MemoryGovernanceStore::default());
        let mut config = limited(0.001, 40);
        config.rate_limit.servers = Some(4);
        let pinned = Governance::new(config, false, Some(rows.clone()));
        let other = Governance::new(limited(1.0, 10), false, Some(rows.clone()));
        other.refresh_members().await;
        pinned.refresh_members().await;
        assert_eq!(pinned.share(), 4);
        assert_eq!(let_through(&pinned).await, 10);
        // Pinned to one: the whole of it, whatever the heartbeat says.
        let mut config = limited(0.001, 40);
        config.rate_limit.servers = Some(1);
        let whole = Governance::new(config, false, Some(rows));
        assert_eq!(whole.share(), 1);
        assert_eq!(let_through(&whole).await, 40);
    }

    #[tokio::test]
    async fn a_servers_part_never_leaves_it_unable_to_afford_the_dearest_request() {
        // 100 divided by 50 servers is 2, and an export costs 20: the bucket holds one export.
        let mut config = limited(50.0, 100);
        config.rate_limit.costs.export = 20;
        config.rate_limit.servers = Some(50);
        let governance = Governance::new(config, false, None);
        let scope = RequestScope::new(governance.clone(), "r".into(), scope::Source::Http, None);
        assert!(governance.admit(&scope, Class::Export).await.is_ok());
        assert!(governance.admit(&scope, Class::Export).await.is_err());
    }

    #[test]
    fn the_connection_cap_and_the_failure_budget_are_divided_like_the_rate() {
        let mut config = limited(50.0, 100);
        config.rate_limit.max_sync_connections = 10;
        config.rate_limit.auth_failures_per_minute = 20;
        config.rate_limit.servers = Some(4);
        let governance = Governance::new(config, false, None);
        let scope = RequestScope::new(governance.clone(), "r".into(), scope::Source::Http, None);
        let mut permits = Vec::new();
        while let Ok(permit) = governance.open_sync_connection(&scope) {
            permits.push(permit);
        }
        assert_eq!(permits.len(), 3, "ten, divided by four, rounded up");
        let (rate, burst) = governance.failure_rate();
        assert_eq!(burst, 5, "twenty a minute, a quarter of it");
        assert!((rate - 20.0 / 60.0 / 4.0).abs() < 1e-9);
    }

    #[test]
    fn a_request_id_from_a_caller_is_kept_only_if_it_is_safe_in_a_log() {
        let governance = Governance::new(GovernanceConfig::observing(), false, None);
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("x-request-id", HeaderValue::from_str(value).unwrap());
            governance.request_id(&headers)
        };
        assert_eq!(with("trace-123.abc:4"), "trace-123.abc:4");
        assert_ne!(with("has space"), "has space");
        assert_ne!(with(&"a".repeat(129)), "a".repeat(129));
        assert_eq!(governance.request_id(&HeaderMap::new()).len(), 36, "a uuid");
    }
}
