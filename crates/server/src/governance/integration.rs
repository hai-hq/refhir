//! Governance through the real router: the layers, the engine, the `/sync` socket and the
//! endpoints that read back what was recorded.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode};
use futures_util::SinkExt;
use http_body_util::BodyExt;
use refhir_persistence::{
    AuditFilter, AuditRow, GovernanceStore, MemoryGovernanceStore, RequestLogFilter, UsageFilter,
    UsageRow,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

use super::{
    civil, now_ms, Cidr, Costs, Governance, GovernanceConfig, ProxyConfig, QuotaLimits,
    RateLimitConfig,
};
use crate::auth::{SmartStore, Token};
use crate::tests::{connect, memory, next_json, patient};
use crate::{QuotaConfig, ServerConfig, SharedGovernanceStore};

struct Rig {
    app: axum::Router,
    governance: Arc<Governance>,
    store: Arc<MemoryGovernanceStore>,
    smart: Arc<SmartStore>,
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Answer {
    fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string()
    }
}

/// Record everything, and flush by hand so that a test decides when rows reach the store.
fn observing() -> GovernanceConfig {
    let mut config = GovernanceConfig::observing();
    config.flush_interval = Duration::from_secs(3_600);
    config
}

/// Rate limits where a caller with a token and one without get the same bucket.
fn limits(per_second: f64, burst: u32) -> RateLimitConfig {
    RateLimitConfig {
        enabled: true,
        requests_per_second: per_second,
        burst,
        anonymous_requests_per_second: Some(per_second),
        anonymous_burst: Some(burst),
        ..RateLimitConfig::default()
    }
}

fn rig(config: GovernanceConfig, auth: bool) -> Rig {
    let (db, catalog, store) = memory();
    let rows = Arc::new(MemoryGovernanceStore::default());
    let smart = Arc::new(SmartStore::with_defaults());
    let (app, governance) = crate::app_parts(
        db,
        catalog,
        store,
        ServerConfig {
            auth,
            smart_demo_login: auth,
            smart: Some(smart.clone()),
            governance: config,
            governance_store: Some(SharedGovernanceStore::new(rows.clone())),
            ..Default::default()
        },
    );
    Rig {
        app,
        governance,
        store: rows,
        smart,
    }
}

impl Rig {
    /// A token of `client` for the user `u-1`, as if it had been issued.
    fn token(&self, client: &str, scopes: &[&str]) -> String {
        let token = format!("tok-{}", uuid::Uuid::new_v4());
        self.smart
            .put_token(
                token.clone(),
                Token {
                    scopes: scopes.iter().map(|scope| scope.to_string()).collect(),
                    patient: None,
                    encounter: None,
                    fhir_user: Some("Practitioner/dr".into()),
                    client_id: client.into(),
                    exp: now_ms() / 1_000 + 3_600,
                    sub: Some("u-1".into()),
                    iss: None,
                    need_patient_banner: None,
                    smart_style_url: None,
                },
            )
            .unwrap();
        token
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        token: Option<&str>,
    ) -> Answer {
        self.call_from(method, uri, body, token, &[], None).await
    }

    async fn call_from(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        token: Option<&str>,
        headers: &[(&str, &str)],
        peer: Option<SocketAddr>,
    ) -> Answer {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/fhir+json");
        }
        let payload = body.map(|value| value.to_string()).unwrap_or_default();
        let mut request = builder.body(Body::from(payload)).unwrap();
        if let Some(peer) = peer {
            request.extensions_mut().insert(ConnectInfo(peer));
        }
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        Answer {
            status,
            headers,
            body,
        }
    }

    /// Everything the audit trail holds, oldest first.
    async fn audit(&self) -> Vec<AuditRow> {
        self.governance.flush().await;
        let mut rows = self
            .store
            .audit_query(&AuditFilter {
                limit: 1_000,
                ..Default::default()
            })
            .unwrap();
        rows.reverse();
        rows
    }

    async fn usage(&self, principal: &str) -> Vec<UsageRow> {
        self.governance.flush().await;
        self.store
            .usage_query(&UsageFilter {
                principal: Some(principal.into()),
                limit: 1_000,
                ..Default::default()
            })
            .unwrap()
    }
}

fn class_row<'a>(rows: &'a [UsageRow], class: &str) -> &'a UsageRow {
    rows.iter()
        .find(|row| row.key.class == class)
        .unwrap_or_else(|| panic!("no {class} row in {rows:?}"))
}

fn today() -> u32 {
    civil::day_of(now_ms())
}

#[tokio::test]
async fn a_caller_over_their_rate_is_told_when_to_come_back() {
    let rig = rig(
        GovernanceConfig {
            rate_limit: limits(0.1, 3),
            ..observing()
        },
        false,
    );
    for remaining in ["2", "1", "0"] {
        let answer = rig.call("GET", "/metadata", None, None).await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.header("ratelimit-limit"), "3");
        assert_eq!(answer.header("ratelimit-remaining"), remaining);
        assert!(!answer.header("x-request-id").is_empty());
    }
    let refused = rig.call("GET", "/metadata", None, None).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.body["resourceType"], "OperationOutcome");
    assert_eq!(refused.body["issue"][0]["code"], "throttled");
    // A token comes back every ten seconds at this rate.
    let wait: u64 = refused.header("retry-after").parse().unwrap();
    assert!((1..=10).contains(&wait), "{wait}");
    assert_eq!(refused.header("ratelimit-remaining"), "0");
    assert!(
        !refused.header("x-request-id").is_empty(),
        "a refusal can be traced too"
    );

    // What was refused is counted as such, not as served.
    let usage = rig.usage("anonymous").await;
    let discovery = class_row(&usage, "discovery");
    assert_eq!(
        (discovery.counters.requests, discovery.counters.throttled),
        (3, 1)
    );
}

#[tokio::test]
async fn dearer_requests_drain_the_bucket_faster() {
    let rig = rig(
        GovernanceConfig {
            rate_limit: RateLimitConfig {
                costs: Costs {
                    search: 3,
                    ..Costs::default()
                },
                ..limits(0.01, 4)
            },
            ..observing()
        },
        false,
    );
    let first = rig.call("GET", "/Patient", None, None).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.header("ratelimit-remaining"), "1");
    let second = rig.call("GET", "/Patient", None, None).await;
    assert_eq!(
        second.status,
        StatusCode::TOO_MANY_REQUESTS,
        "a search costs three of the four"
    );
    // A read costs one, and is still within what is left. It is a 404 and not a 429.
    let read = rig.call("GET", "/Patient/nobody", None, None).await;
    assert_eq!(read.status, StatusCode::NOT_FOUND);
    assert_eq!(read.header("ratelimit-remaining"), "0");
}

#[tokio::test]
async fn each_client_has_their_own_bucket_and_one_can_be_exempt() {
    let mut rate = limits(0.01, 2);
    rate.clients.insert(
        "trusted".into(),
        super::ClientRate {
            requests_per_second: Some(0.0),
            burst: None,
        },
    );
    let rig = rig(
        GovernanceConfig {
            rate_limit: rate,
            ..observing()
        },
        true,
    );
    let slow = rig.token("slow", &["user/*.*"]);
    let other = rig.token("other", &["user/*.*"]);
    let trusted = rig.token("trusted", &["user/*.*"]);
    for _ in 0..2 {
        assert_eq!(
            rig.call("GET", "/Patient/x", None, Some(&slow))
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        rig.call("GET", "/Patient/x", None, Some(&slow))
            .await
            .status,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Another client is not held back by it, and an exempt one is never held back.
    assert_eq!(
        rig.call("GET", "/Patient/x", None, Some(&other))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    for _ in 0..10 {
        let answer = rig.call("GET", "/Patient/x", None, Some(&trusted)).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND);
        assert_eq!(
            answer.header("ratelimit-limit"),
            "",
            "no bucket, no headers"
        );
    }
}

#[tokio::test]
async fn a_caller_behind_a_proxy_is_told_apart_by_the_address_the_proxy_saw() {
    let mut config = GovernanceConfig {
        rate_limit: limits(0.01, 2),
        ..observing()
    };
    config.proxy.trusted_hops = 1;
    let rig = rig(config, false);
    let proxy: SocketAddr = "10.0.0.1:5000".parse().unwrap();
    let get = |forwarded: &'static str| {
        let rig = &rig;
        async move {
            rig.call_from(
                "GET",
                "/metadata",
                None,
                None,
                &[("x-forwarded-for", forwarded)],
                Some(proxy),
            )
            .await
            .status
        }
    };
    // The client forges an earlier entry; the proxy appended what it saw last.
    assert_eq!(get("1.1.1.1, 203.0.113.9").await, StatusCode::OK);
    assert_eq!(get("2.2.2.2, 203.0.113.9").await, StatusCode::OK);
    assert_eq!(
        get("3.3.3.3, 203.0.113.9").await,
        StatusCode::TOO_MANY_REQUESTS,
        "one caller"
    );
    assert_eq!(
        get("1.1.1.1, 198.51.100.4").await,
        StatusCode::OK,
        "another caller"
    );
}

#[tokio::test]
async fn a_caller_who_names_their_own_request_id_gets_it_back() {
    let rig = rig(observing(), false);
    let answer = rig
        .call_from(
            "GET",
            "/metadata",
            None,
            None,
            &[("x-request-id", "trace-42")],
            None,
        )
        .await;
    assert_eq!(answer.header("x-request-id"), "trace-42");
    // Anything that is not safe to put in a log line is replaced, not echoed.
    for unsafe_id in [
        "has spaces in it",
        "<script>alert(1)</script>",
        &"x".repeat(129),
    ] {
        let answer = rig
            .call_from(
                "GET",
                "/metadata",
                None,
                None,
                &[("x-request-id", unsafe_id)],
                None,
            )
            .await;
        assert_ne!(answer.header("x-request-id"), unsafe_id);
        assert_eq!(answer.header("x-request-id").len(), 36, "a uuid instead");
    }
}

#[tokio::test]
async fn a_server_with_governance_off_behaves_as_it_always_did() {
    let rig = rig(GovernanceConfig::default(), false);
    let answer = rig.call("GET", "/metadata", None, None).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.header("x-request-id"), "");
    assert_eq!(answer.header("ratelimit-limit"), "");
    assert!(rig.audit().await.is_empty());
}

#[tokio::test]
async fn a_quota_stops_a_client_at_their_total_and_no_other() {
    let started = today();
    let rig = rig(
        GovernanceConfig {
            quota: QuotaConfig {
                enabled: true,
                limits: QuotaLimits {
                    requests_per_day: Some(3),
                    ..QuotaLimits::default()
                },
                ..QuotaConfig::default()
            },
            ..observing()
        },
        true,
    );
    let acme = rig.token("acme", &["user/*.*"]);
    let other = rig.token("other", &["user/*.*"]);
    for _ in 0..3 {
        assert_eq!(
            rig.call("GET", "/Patient/x", None, Some(&acme))
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }
    let refused = rig.call("GET", "/Patient/x", None, Some(&acme)).await;
    if today() != started {
        return; // The UTC day rolled over during the test, which splits what it counts.
    }
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.body["issue"][0]["code"], "throttled");
    let message = refused.body["issue"][0]["diagnostics"].as_str().unwrap();
    assert!(
        message.contains("daily request quota") && message.contains("3 of 3"),
        "{message}"
    );
    let wait: u64 = refused.header("retry-after").parse().unwrap();
    assert!((1..=86_400).contains(&wait), "until the day ends: {wait}");
    // Discovery is how a client gets to its quota, and does not use it.
    assert_eq!(
        rig.call("GET", "/metadata", None, Some(&acme)).await.status,
        StatusCode::OK
    );
    // Another client has a quota of their own.
    assert_eq!(
        rig.call("GET", "/Patient/x", None, Some(&other))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // The refused request is on record as throttled, and the three as served.
    let usage = rig.usage("client:acme").await;
    let read = class_row(&usage, "read");
    assert_eq!((read.counters.requests, read.counters.throttled), (3, 1));
}

#[tokio::test]
async fn a_quota_counts_what_the_other_servers_sharing_the_database_have_counted() {
    let started = today();
    let rig = rig(
        GovernanceConfig {
            quota: QuotaConfig {
                enabled: true,
                limits: QuotaLimits {
                    requests_per_day: Some(5),
                    ..QuotaLimits::default()
                },
                ..QuotaConfig::default()
            },
            ..observing()
        },
        true,
    );
    // Another server has already counted four requests for this client today.
    rig.store
        .usage_add(&[UsageRow {
            key: refhir_persistence::UsageKey {
                day: started,
                principal: "client:acme".into(),
                class: "search".into(),
            },
            counters: refhir_persistence::UsageCounters {
                requests: 4,
                ..Default::default()
            },
        }])
        .unwrap();
    let acme = rig.token("acme", &["user/*.*"]);
    assert_eq!(
        rig.call("GET", "/Patient/x", None, Some(&acme))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let refused = rig.call("GET", "/Patient/x", None, Some(&acme)).await;
    if today() != started {
        return;
    }
    assert_eq!(
        refused.status,
        StatusCode::TOO_MANY_REQUESTS,
        "four there and one here"
    );
}

#[tokio::test]
async fn an_address_that_keeps_failing_to_authenticate_is_stopped_before_its_token_is_looked_up() {
    let mut rate = limits(100.0, 100);
    rate.auth_failures_per_minute = 3;
    let rig = rig(
        GovernanceConfig {
            rate_limit: rate,
            ..observing()
        },
        true,
    );
    let known = rig.token("acme", &["user/*.*"]);
    // A token the server has seen is in memory, and stays welcome.
    assert_eq!(
        rig.call("GET", "/Patient/x", None, Some(&known))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    for guess in ["tok-a", "tok-b", "tok-c"] {
        assert_eq!(
            rig.call("GET", "/Patient", None, Some(guess)).await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    let refused = rig.call("GET", "/Patient", None, Some("tok-d")).await;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(refused.body["issue"][0]["diagnostics"]
        .as_str()
        .unwrap()
        .contains("failed authentication"));
    assert!(refused.header("retry-after").parse::<u64>().unwrap() >= 1);
    // The guesses are all that were stopped: a caller whose token is known is not locked out
    // by the address having been busy, and a request with no token is not a guess.
    assert_eq!(
        rig.call("GET", "/Patient/x", None, Some(&known))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        rig.call("GET", "/metadata", None, None).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_oauth_endpoints_stop_a_secret_being_guessed() {
    let mut rate = limits(100.0, 100);
    rate.auth_failures_per_minute = 2;
    let rig = rig(
        GovernanceConfig {
            rate_limit: rate,
            ..observing()
        },
        true,
    );
    let wrong = "grant_type=authorization_code&code=nope&client_id=public&redirect_uri=http%3A%2F%2F127.0.0.1%2Fcb&code_verifier=x";
    let mut statuses = Vec::new();
    for _ in 0..3 {
        let request = Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(wrong))
            .unwrap();
        statuses.push(rig.app.clone().oneshot(request).await.unwrap().status());
    }
    assert!(statuses[0].is_client_error() && statuses[0] != StatusCode::TOO_MANY_REQUESTS);
    assert!(statuses[1].is_client_error() && statuses[1] != StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(statuses[2], StatusCode::TOO_MANY_REQUESTS);
    // The attempts are in the audit trail, as the OAuth token interaction and with no client.
    let audit = rig.audit().await;
    let tokens: Vec<_> = audit
        .iter()
        .filter(|row| row.subtype == "oauth-token")
        .collect();
    assert_eq!(
        tokens.len(),
        2,
        "the refused third is throttled, not audited"
    );
    assert!(tokens
        .iter()
        .all(|row| row.outcome == 4 && row.principal == "anonymous"));
}

#[tokio::test]
async fn usage_is_counted_per_client_and_class_and_written_in_batches() {
    let started = today();
    let rig = rig(observing(), true);
    let acme = rig.token("acme", &["user/*.*"]);
    rig.call("GET", "/metadata", None, Some(&acme)).await;
    for _ in 0..2 {
        assert_eq!(
            rig.call("GET", "/Patient", None, Some(&acme)).await.status,
            StatusCode::OK
        );
    }
    assert_eq!(
        rig.call("POST", "/Patient", Some(patient("ignored")), Some(&acme))
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        rig.call("GET", "/Patient/zzz", None, Some(&acme))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // Nothing is written until the flush: the request path never touches the database.
    assert!(rig
        .store
        .usage_query(&UsageFilter {
            limit: 10,
            ..Default::default()
        })
        .unwrap()
        .is_empty());
    let usage = rig.usage("client:acme").await;
    if today() != started {
        return;
    }
    let search = class_row(&usage, "search");
    assert_eq!(search.counters.requests, 2);
    assert!(
        search.counters.bytes_out > 0,
        "the size of the answer is counted"
    );
    let write = class_row(&usage, "write");
    assert_eq!(write.counters.requests, 1);
    // The request had no Content-Length, as a chunked upload has none; what was read is counted.
    assert!(write.counters.bytes_in > 20, "{}", write.counters.bytes_in);
    let read = class_row(&usage, "read");
    assert_eq!(
        (read.counters.requests, read.counters.client_errors),
        (1, 1)
    );
    // The metadata request carried a token the open endpoints do not look at, so it is
    // anonymous, and counted as discovery.
    let anonymous = rig.usage("anonymous").await;
    assert_eq!(class_row(&anonymous, "discovery").counters.requests, 1);
    // Flushing again adds nothing that was already written.
    rig.governance.flush().await;
    let again = rig.usage("client:acme").await;
    assert_eq!(class_row(&again, "search").counters.requests, 2);
}

#[tokio::test]
async fn the_request_log_keeps_parameter_names_and_never_their_values() {
    let rig = rig(observing(), false);
    let answer = rig
        .call(
            "GET",
            "/Patient?name=ann&identifier=SSN-123-45-6789&_count=5",
            None,
            None,
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK);
    let id = answer.header("x-request-id");
    rig.governance.flush().await;
    let rows = rig
        .store
        .request_log_query(&RequestLogFilter {
            request_id: Some(id.clone()),
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(
        (row.method.as_str(), row.path.as_str(), row.class.as_str()),
        ("GET", "/Patient", "search")
    );
    assert_eq!(row.params, "name,identifier,_count");
    assert_eq!((row.status, row.principal.as_str()), (200, "anonymous"));
    assert!(row.bytes_out > 0);
    let everything = format!("{row:?}");
    assert!(
        !everything.contains("SSN") && !everything.contains("ann"),
        "{everything}"
    );
}

#[tokio::test]
async fn the_audit_trail_records_who_did_what_to_which_resource() {
    let rig = rig(observing(), true);
    let acme = rig.token("acme", &["user/*.*"]);
    let created = rig
        .call("POST", "/Patient", Some(patient("ignored")), Some(&acme))
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.body["id"].as_str().unwrap().to_string();
    rig.call("GET", &format!("/Patient/{id}"), None, Some(&acme))
        .await;
    rig.call("GET", "/Patient?name=ann", None, Some(&acme))
        .await;
    let mut changed = patient(&id);
    changed["active"] = json!(false);
    assert_eq!(
        rig.call("PUT", &format!("/Patient/{id}"), Some(changed), Some(&acme))
            .await
            .status,
        StatusCode::OK
    );
    let bundle = rig
        .call(
            "POST",
            "/",
            Some(json!({"resourceType": "Bundle", "type": "transaction", "entry": [
                {"fullUrl": "urn:uuid:5b1d1b0e-0000-4000-8000-000000000001",
                 "request": {"method": "POST", "url": "Patient"}, "resource": patient("x")},
                {"request": {"method": "POST", "url": "Observation"},
                 "resource": {"resourceType": "Observation",
                              "subject": {"reference": "urn:uuid:5b1d1b0e-0000-4000-8000-000000000001"},
                              "code": {"coding": [{"code": "8480-6"}]},
                              "effectiveDateTime": "2020-01-02"}}
            ]})),
            Some(&acme),
        )
        .await;
    assert_eq!(bundle.status, StatusCode::OK);
    let new_patient = bundle.body["entry"][0]["resource"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        rig.call("DELETE", &format!("/Patient/{id}"), None, Some(&acme))
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        rig.call("GET", "/Patient/nobody", None, Some(&acme))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let rows = rig.audit().await;
    let summary: Vec<(String, String, Option<String>, u8)> = rows
        .iter()
        .map(|row| {
            (
                row.action.clone(),
                row.subtype.clone(),
                row.resource_type.clone(),
                row.outcome,
            )
        })
        .collect();
    let find = |action: &str, subtype: &str, kind: &str| {
        rows.iter()
            .find(|row| {
                row.action == action
                    && row.subtype == subtype
                    && row.resource_type.as_deref() == Some(kind)
            })
            .unwrap_or_else(|| panic!("no {action} {subtype} {kind} in {summary:?}"))
    };
    let create = find("C", "create", "Patient");
    assert_eq!(create.resource_id.as_deref(), Some(id.as_str()));
    assert_eq!((create.status, create.outcome), (201, 0));
    assert_eq!(
        find("R", "read", "Patient").resource_id.as_deref(),
        Some(id.as_str())
    );
    let search = find("E", "search-type", "Patient");
    assert_eq!(search.query.as_deref(), Some("name=ann"));
    assert_eq!(search.resource_id, None, "a search touches no one resource");
    assert_eq!(
        find("U", "update", "Patient").resource_id.as_deref(),
        Some(id.as_str())
    );
    assert_eq!(
        find("D", "delete", "Patient").resource_id.as_deref(),
        Some(id.as_str())
    );
    // A transaction leaves a row for each document it wrote, and says whose chart each is on.
    let made_patient = find("C", "transaction", "Patient");
    assert_eq!(
        made_patient.resource_id.as_deref(),
        Some(new_patient.as_str())
    );
    let made_observation = find("C", "transaction", "Observation");
    assert_eq!(
        made_observation.patient.as_deref(),
        Some(new_patient.as_str())
    );
    // A read that found nothing is on record as a failure, with its code and not its text.
    let missing = rows
        .iter()
        .find(|row| row.resource_id.as_deref() == Some("nobody"))
        .expect("the failed read");
    assert_eq!((missing.outcome, missing.status), (4, 404));
    assert_eq!(missing.detail.as_deref(), Some("not-found"));
    // Every row says who, and from which request.
    assert!(rows.iter().all(|row| row.principal == "client:acme"
        && row.client_id.as_deref() == Some("acme")
        && row.user.as_deref() == Some("u-1")
        && row.fhir_user.as_deref() == Some("Practitioner/dr")
        && row.source == "http"
        && !row.request_id.is_empty()));
}

#[tokio::test]
async fn a_patient_match_is_audited_as_an_operation_and_not_by_what_it_looked_for() {
    let rig = rig(observing(), true);
    let acme = rig.token("acme", &["user/Patient.read", "user/Patient.write"]);
    let mut known = patient("ignored");
    known["identifier"] = json!([{"system": "urn:example:id", "value": "SECRET-ID-42"}]);
    assert_eq!(
        rig.call("POST", "/Patient", Some(known), Some(&acme))
            .await
            .status,
        StatusCode::CREATED
    );
    let asked = json!({"resourceType": "Parameters", "parameter": [{"name": "resource", "resource": {
        "resourceType": "Patient", "identifier": [{"system": "urn:example:id", "value": "SECRET-ID-42"}]}}]});
    let found = rig
        .call("POST", "/Patient/$match", Some(asked), Some(&acme))
        .await;
    assert_eq!(found.status, StatusCode::OK, "{}", found.body);
    assert_eq!(found.body["total"], 1);

    let rows = rig.audit().await;
    let matched: Vec<_> = rows
        .iter()
        .filter(|row| row.detail.as_deref() == Some("$match"))
        .collect();
    assert_eq!(matched.len(), 1, "{rows:?}");
    let row = matched[0];
    assert_eq!(
        (row.action.as_str(), row.subtype.as_str()),
        ("E", "operation")
    );
    assert_eq!(row.resource_type.as_deref(), Some("Patient"));
    assert_eq!((row.status, row.outcome), (200, 0));
    assert_eq!(row.principal, "client:acme");
    // What it looked for names a person, and is in no row.
    assert_eq!(row.query, None);
    assert!(!format!("{rows:?}").contains("SECRET-ID-42"));
}

#[tokio::test]
async fn what_was_refused_at_the_door_is_audited_with_whoever_asked() {
    let rig = rig(observing(), true);
    let narrow = rig.token("acme", &["user/Patient.read"]);
    assert_eq!(
        rig.call("GET", "/Patient", None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        rig.call("POST", "/Patient", Some(patient("p")), Some(&narrow))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let rows = rig.audit().await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let no_token = &rows[0];
    assert_eq!(
        (
            no_token.status,
            no_token.outcome,
            no_token.principal.as_str()
        ),
        (401, 4, "anonymous")
    );
    assert_eq!(
        (no_token.action.as_str(), no_token.subtype.as_str()),
        ("E", "search-type")
    );
    let no_scope = &rows[1];
    assert_eq!((no_scope.status, no_scope.outcome), (403, 4));
    assert_eq!(
        no_scope.client_id.as_deref(),
        Some("acme"),
        "known, and refused"
    );
    assert_eq!(
        (no_scope.action.as_str(), no_scope.subtype.as_str()),
        ("C", "create")
    );
    assert_eq!(no_scope.resource_type.as_deref(), Some("Patient"));
}

#[tokio::test]
async fn audit_can_be_left_to_changes_and_refusals() {
    let mut config = observing();
    config.audit.reads = false;
    let rig = rig(config, true);
    let acme = rig.token("acme", &["user/*.*"]);
    let created = rig
        .call("POST", "/Patient", Some(patient("p")), Some(&acme))
        .await;
    let id = created.body["id"].as_str().unwrap().to_string();
    rig.call("GET", &format!("/Patient/{id}"), None, Some(&acme))
        .await;
    rig.call("GET", "/Patient", None, Some(&acme)).await;
    rig.call("GET", "/Patient/nobody", None, Some(&acme)).await;
    rig.call("GET", "/Patient", None, None).await;
    let rows = rig.audit().await;
    let kinds: Vec<(&str, u8)> = rows
        .iter()
        .map(|row| (row.subtype.as_str(), row.outcome))
        .collect();
    assert_eq!(
        kinds,
        vec![("create", 0), ("read", 4), ("search-type", 4)],
        "the successful reads are not recorded; the failed read and the refused search are"
    );
}

#[tokio::test]
async fn the_search_text_can_be_kept_out_of_the_audit_trail() {
    let mut config = observing();
    config.audit.include_query = false;
    let rig = rig(config, false);
    rig.call("GET", "/Patient?identifier=SSN-123", None, None)
        .await;
    let rows = rig.audit().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].query, None);
}

#[tokio::test]
async fn the_audit_trail_is_read_back_as_audit_events_by_whoever_may() {
    let rig = rig(observing(), true);
    let acme = rig.token("acme", &["user/*.*"]);
    let admin = rig.token("ops", &["system/AuditEvent.read"]);
    let mut ids = Vec::new();
    for _ in 0..3 {
        let created = rig
            .call("POST", "/Patient", Some(patient("p")), Some(&acme))
            .await;
        ids.push(created.body["id"].as_str().unwrap().to_string());
    }
    // The trail itself is not for a clinician's broad read scope, nor for a patient app's.
    let denied = rig.call("GET", "/_audit", None, Some(&acme)).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert!(denied.body["issue"][0]["diagnostics"]
        .as_str()
        .unwrap()
        .contains("system/"));
    assert_eq!(
        rig.call("GET", "/_audit", None, None).await.status,
        StatusCode::UNAUTHORIZED
    );

    let all = rig
        .call(
            "GET",
            "/_audit?entity=Patient&action=C&_count=2",
            None,
            Some(&admin),
        )
        .await;
    assert_eq!(all.status, StatusCode::OK);
    assert_eq!(all.body["resourceType"], "Bundle");
    assert_eq!(all.body["type"], "searchset");
    let entries = all.body["entry"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "a page of two");
    let first = &entries[0]["resource"];
    assert_eq!(first["resourceType"], "AuditEvent");
    assert_eq!(first["action"], "C");
    assert_eq!(first["agent"][0]["who"]["identifier"]["value"], "acme");
    assert_eq!(first["agent"][0]["who"]["reference"], "Practitioner/dr");
    assert_eq!(
        first["entity"][0]["what"]["reference"],
        format!("Patient/{}", ids[2]),
        "newest first"
    );
    // The next page continues from where this one stopped, and carries no token in its link.
    let next = all.body["link"]
        .as_array()
        .unwrap()
        .iter()
        .find(|link| link["relation"] == "next")
        .expect("a next link");
    let url = next["url"].as_str().unwrap();
    assert!(
        url.starts_with("/_audit?") && url.contains("before=") && !url.contains("access_token"),
        "{url}"
    );
    let page_two = rig.call("GET", url, None, Some(&admin)).await;
    let rest = page_two.body["entry"].as_array().unwrap();
    assert!(rest
        .iter()
        .any(|entry| entry["resource"]["entity"][0]["what"]["reference"]
            == format!("Patient/{}", ids[0])));
    assert!(page_two.body["link"]
        .as_array()
        .unwrap()
        .iter()
        .all(|link| link["relation"] != "next"));

    // Filtering by patient finds what touched that chart, and a parameter it does not know is
    // refused and not dropped.
    let by_patient = rig
        .call(
            "GET",
            &format!("/_audit?patient=Patient/{}", ids[1]),
            None,
            Some(&admin),
        )
        .await;
    assert_eq!(by_patient.body["entry"].as_array().unwrap().len(), 1);
    let unknown = rig
        .call("GET", "/_audit?patent=x", None, Some(&admin))
        .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);

    // The refusal of the clinician's token is in the trail, and so is the admin's own reading.
    let refusals = rig
        .call("GET", "/_audit?outcome=4&client=acme", None, Some(&admin))
        .await;
    let refused = refusals.body["entry"].as_array().unwrap();
    assert!(refused.iter().any(
        |entry| entry["resource"]["subtype"][0]["code"] == "audit-read"
            && entry["resource"]["extension"][2]["valueInteger"] == 403
    ));
    let reads = rig
        .call("GET", "/_audit?client=ops", None, Some(&admin))
        .await;
    assert!(
        !reads.body["entry"].as_array().unwrap().is_empty(),
        "reading the trail is audited"
    );
}

#[tokio::test]
async fn usage_and_the_request_log_are_read_back_by_whoever_may() {
    let rig = rig(observing(), true);
    let acme = rig.token("acme", &["user/*.*"]);
    let admin = rig.token("ops", &["system/*.read"]);
    for _ in 0..2 {
        rig.call("GET", "/Patient", None, Some(&acme)).await;
    }
    rig.call("GET", "/Patient/nobody", None, Some(&acme)).await;
    assert_eq!(
        rig.call("GET", "/_usage", None, Some(&acme)).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        rig.call("GET", "/_requests", None, Some(&acme))
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    let usage = rig
        .call("GET", "/_usage?client=acme", None, Some(&admin))
        .await;
    assert_eq!(usage.status, StatusCode::OK);
    let rows = usage.body["usage"].as_array().unwrap();
    let search = rows
        .iter()
        .find(|row| row["class"] == "search")
        .expect("search");
    assert_eq!(search["requests"], 2);
    assert_eq!(search["principal"], "client:acme");
    assert_eq!(search["date"].as_str().unwrap().len(), 10);
    assert_eq!(usage.body["dropped"], json!({"requestLog": 0, "audit": 0}));
    assert_eq!(usage.body["sampled"], json!({"requestLog": 0, "audit": 0}));
    assert_eq!(usage.body["servers"], 1);
    assert_eq!(
        rig.call("GET", "/_usage?from=not-a-date", None, Some(&admin))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );

    let log = rig
        .call(
            "GET",
            "/_requests?client=acme&status=400",
            None,
            Some(&admin),
        )
        .await;
    assert_eq!(log.status, StatusCode::OK);
    let requests = log.body["requests"].as_array().unwrap();
    // The failed read, and the two attempts at the admin endpoints that were refused: the
    // request log holds refusals too. The two successful searches are not at 400 or above.
    let seen: Vec<(String, u64)> = requests
        .iter()
        .map(|row| {
            (
                row["path"].as_str().unwrap().to_string(),
                row["status"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            ("/_requests".to_string(), 403),
            ("/_usage".to_string(), 403),
            ("/Patient/nobody".to_string(), 404),
        ],
        "newest first"
    );
    assert!(requests[0]["time"].as_str().unwrap().ends_with('Z'));
    assert_eq!(requests[0]["class"], "admin");
}

#[tokio::test]
async fn a_websocket_does_not_get_around_the_limits_the_audit_or_the_meter() {
    let started = today();
    let mut rate = limits(0.001, 24);
    rate.costs = Costs {
        write: 6,
        sync: 1,
        ..Costs::default()
    };
    rate.max_sync_connections = 1;
    rate.max_live_queries = 1;
    let rig = rig(
        GovernanceConfig {
            rate_limit: rate,
            ..observing()
        },
        true,
    );
    let acme = rig.token("acme", &["user/*.*"]);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = rig.app.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let url = format!("ws://{addr}/sync?access_token={acme}");
    let (mut socket, _) = connect(&url).await;
    // One connection at a time for this caller.
    assert!(
        tokio_tungstenite::connect_async(&url).await.is_err(),
        "the second is refused"
    );

    // A patient to watch. A live read keeps the arithmetic of the test simple: each write to the
    // patient re-runs exactly one query.
    let id = rig
        .call("POST", "/Patient", Some(patient("p")), Some(&acme))
        .await
        .body["id"]
        .as_str()
        .unwrap()
        .to_string();
    let add = |query_id: u64, base: u64| {
        json!({"type": "ModifyQuerySet", "baseVersion": base, "newVersion": base + 1,
               "modifications": [{"type": "Add", "queryId": query_id, "udfPath": "Patient/read",
                                  "args": {"id": id}}]})
    };
    // One live query is allowed, and a second is not.
    socket
        .send(Message::Text(add(1, 0).to_string()))
        .await
        .unwrap();
    let first = next_json(&mut socket).await;
    assert_eq!(first["modifications"][0]["type"], "QueryUpdated", "{first}");
    socket
        .send(Message::Text(add(2, 1).to_string()))
        .await
        .unwrap();
    let second = next_json(&mut socket).await;
    assert_eq!(second["modifications"][0]["type"], "QueryFailed");
    assert!(second["modifications"][0]["error"]
        .as_str()
        .unwrap()
        .contains("too many live queries"));
    assert_eq!(second["modifications"][0]["code"], "too-costly");

    // Writes through the socket spend the same bucket. The two connection attempts, the HTTP
    // create and the live query took 1 + 1 + 6 + 1 of the 24, so two more writes fit (12) and a
    // third does not.
    let mut results = Vec::new();
    let mut pushes = 0;
    for request_id in 1..=3u64 {
        let mut resource = patient(&id);
        resource["active"] = json!(request_id % 2 == 0);
        let mutation = json!({"type": "Mutation", "requestId": request_id, "udfPath": "Patient/update",
                              "args": {"id": id, "resource": resource}});
        socket
            .send(Message::Text(mutation.to_string()))
            .await
            .unwrap();
        // The live query is re-run by each write, so its updates arrive between the answers.
        loop {
            let message = next_json(&mut socket).await;
            if message["type"] == "Transition" {
                pushes += 1;
                continue;
            }
            results.push(message);
            break;
        }
    }
    assert!(
        results[..2].iter().all(|result| result["success"] == true),
        "{results:?}"
    );
    assert_eq!(results[2]["success"], false);
    assert_eq!(results[2]["result"]["issue"][0]["code"], "throttled");
    assert!(results[2]["retryAfterSeconds"].as_u64().unwrap() >= 1);
    assert!(pushes >= 1, "a write overlapped the live query");

    if today() != started {
        return;
    }
    // The three writes are in the audit trail as the caller's, over the socket and from where
    // the connection came from.
    let rows = rig.audit().await;
    let updated: Vec<_> = rows
        .iter()
        .filter(|row| row.source == "sync" && row.subtype == "update")
        .collect();
    assert_eq!(updated.len(), 2, "{rows:?}");
    assert!(updated.iter().all(|row| row.principal == "client:acme"
        && row.remote_ip.as_deref() == Some("127.0.0.1")
        && row.action == "U"
        && row.resource_id.as_deref() == Some(id.as_str())));
    // The live query's first answer is a read by the caller; the re-runs are not requests and
    // are not audited one by one.
    let reads: Vec<_> = rows
        .iter()
        .filter(|row| row.source == "sync" && row.subtype == "read")
        .collect();
    assert_eq!(reads.len(), 1, "{rows:?}");
    // And in the meter: the writes, the one throttled, the live query, and what was pushed.
    let usage = rig.usage("client:acme").await;
    let write = class_row(&usage, "write");
    assert_eq!(
        (write.counters.requests, write.counters.throttled),
        (3, 1),
        "the HTTP create, two writes over the socket, and the refused one"
    );
    assert!(class_row(&usage, "push").counters.requests >= 1);
    assert!(class_row(&usage, "push").counters.bytes_out > 0);
    let sync = class_row(&usage, "sync");
    assert_eq!(sync.counters.throttled, 1, "the second live query");
}

#[tokio::test]
async fn a_sync_write_outside_the_callers_scope_is_audited_as_refused() {
    let rig = rig(observing(), true);
    let reader = rig.token("acme", &["user/Patient.read"]);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = rig.app.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let (mut socket, _) = connect(&format!("ws://{addr}/sync?access_token={reader}")).await;
    let mutation = json!({"type": "Mutation", "requestId": 1, "udfPath": "Patient/create",
                          "args": {"resource": patient("p")}});
    socket
        .send(Message::Text(mutation.to_string()))
        .await
        .unwrap();
    let answer = next_json(&mut socket).await;
    assert_eq!(answer["result"]["issue"][0]["code"], "forbidden");
    let rows = rig.audit().await;
    let refused = rows
        .iter()
        .find(|row| row.source == "sync" && row.status == 403)
        .expect("refusal");
    assert_eq!(
        (refused.action.as_str(), refused.subtype.as_str()),
        ("C", "create")
    );
    assert_eq!(refused.resource_type.as_deref(), Some("Patient"));
    assert_eq!(refused.client_id.as_deref(), Some("acme"));
    assert_eq!(refused.outcome, 4);
}

#[tokio::test]
async fn rows_still_buffered_are_written_when_the_server_is_told_to_stop() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (db, catalog, store) = memory();
    let rows = Arc::new(MemoryGovernanceStore::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(crate::serve_until(
        db,
        catalog,
        store,
        listener,
        ServerConfig {
            // Nothing would be written for an hour but for the stop.
            governance: observing(),
            governance_store: Some(SharedGovernanceStore::new(rows.clone())),
            ..Default::default()
        },
        async move {
            let _ = stopped.await;
        },
        std::future::pending(),
    ));
    let body = patient("p").to_string();
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(
            format!(
                "POST /Patient HTTP/1.1\r\nHost: x\r\nContent-Type: application/fhir+json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut answer = String::new();
    socket.read_to_string(&mut answer).await.unwrap();
    assert!(answer.starts_with("HTTP/1.1 201"), "{answer}");
    let audit = || {
        rows.audit_query(&AuditFilter {
            limit: 10,
            ..Default::default()
        })
        .unwrap()
    };
    assert!(
        audit().is_empty(),
        "still buffered: the flush is an hour away"
    );
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    let written = audit();
    assert_eq!(written.len(), 1);
    assert_eq!(
        (written[0].action.as_str(), written[0].remote_ip.as_deref()),
        ("C", Some("127.0.0.1"))
    );
}

#[tokio::test]
async fn what_a_stranger_puts_in_a_request_cannot_make_a_row_large() {
    let rig = rig(observing(), true);
    let long = "a".repeat(5_000);
    let query = format!(
        "{}={}&access_token=secret",
        "p".repeat(2_000),
        "x".repeat(20_000)
    );
    // No token: refused at the door, and recorded.
    rig.call("GET", &format!("/{long}?{query}"), None, None)
        .await;
    rig.call("GET", &format!("/Patient/{long}"), None, None)
        .await;
    rig.call("POST", &format!("/oauth/{long}"), None, None)
        .await;
    rig.call("GET", "/Patient?name=ok", None, None).await;
    let rows = rig.audit().await;
    assert_eq!(rows.len(), 4, "{rows:?}");
    for row in &rows {
        assert!(
            row.resource_type
                .as_ref()
                .is_none_or(|text| text.len() <= 64),
            "{row:?}"
        );
        assert!(
            row.resource_id.as_ref().is_none_or(|text| text.len() <= 64),
            "{row:?}"
        );
        assert!(
            row.query
                .as_ref()
                .is_none_or(|text| text.chars().count() <= 1_000),
            "{row:?}"
        );
        assert!(
            row.subtype.len() <= 32,
            "nothing from the path is in a subtype: {}",
            row.subtype
        );
    }
    assert_eq!(
        rows[0].resource_type, None,
        "5,000 letters is not a resource type"
    );
    assert_eq!(rows[1].resource_type.as_deref(), Some("Patient"));
    assert_eq!(rows[1].resource_id, None, "nor an id");
    assert_eq!(
        rows[2].subtype, "oauth",
        "an endpoint that does not exist is not named"
    );
    assert_eq!(rows[3].query.as_deref(), Some("name=ok"));
    rig.governance.flush().await;
    let log = rig
        .store
        .request_log_query(&RequestLogFilter {
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(log.len(), 4);
    for row in &log {
        assert!(
            row.path.chars().count() <= 512 && row.params.chars().count() <= 256,
            "{row:?}"
        );
        assert!(!format!("{row:?}").contains("secret"));
    }
}

/// A patient, a terminology call and a tag change, in that order.
async fn patient_expand_and_tag(rig: &Rig) -> String {
    let created = rig.call("POST", "/Patient", Some(patient("p")), None).await;
    let id = created.body["id"].as_str().unwrap().to_string();
    let expand = rig
        .call(
            "POST",
            "/ValueSet/$expand",
            Some(json!({"resourceType": "Parameters"})),
            None,
        )
        .await;
    assert!(
        expand.header("x-request-id").len() == 36,
        "it did reach the server"
    );
    let tag = json!({"resourceType": "Meta", "tag": [{"system": "http://x", "code": "t"}]});
    let added = rig
        .call("POST", &format!("/Patient/{id}/$meta"), Some(tag), None)
        .await;
    assert_eq!(added.status, StatusCode::OK, "{:?}", added.body);
    id
}

#[tokio::test]
async fn terminology_lookups_are_not_audited_but_a_tag_change_is() {
    let rig = rig(observing(), false);
    let id = patient_expand_and_tag(&rig).await;
    let rows = rig.audit().await;
    let summary: Vec<(&str, &str, Option<&str>)> = rows
        .iter()
        .map(|row| {
            (
                row.action.as_str(),
                row.subtype.as_str(),
                row.detail.as_deref(),
            )
        })
        .collect();
    // The create and the tag change; the terminology call is in neither the engine's rows nor
    // picked up again at the edge as a request nobody accounted for.
    assert_eq!(
        summary,
        vec![("C", "create", None), ("U", "operation", Some("$meta-add"))],
        "{rows:?}"
    );
    assert_eq!(
        rows[1].resource_id.as_deref(),
        Some(id.as_str()),
        "on that patient's chart"
    );
    assert_eq!(rows[1].patient.as_deref(), Some(id.as_str()));
}

#[tokio::test]
async fn a_tag_change_is_a_change_when_only_changes_are_kept() {
    let mut config = observing();
    config.audit.reads = false;
    let rig = rig(config, false);
    patient_expand_and_tag(&rig).await;
    let rows = rig.audit().await;
    let kept: Vec<(&str, Option<&str>)> = rows
        .iter()
        .map(|row| (row.subtype.as_str(), row.detail.as_deref()))
        .collect();
    assert_eq!(
        kept,
        vec![("create", None), ("operation", Some("$meta-add"))],
        "{rows:?}"
    );
}

/// A server with a socket open for `watcher` (limited) and an exempt `writer`, with a patient to
/// watch.
async fn watched_patient(
    rate: RateLimitConfig,
) -> (
    Rig,
    String,
    String,
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) {
    let mut rate = rate;
    rate.clients.insert(
        "writer".into(),
        super::ClientRate {
            requests_per_second: Some(0.0),
            burst: None,
        },
    );
    let rig = rig(
        GovernanceConfig {
            rate_limit: rate,
            ..observing()
        },
        true,
    );
    let writer = rig.token("writer", &["user/*.*"]);
    let watcher = rig.token("watcher", &["user/*.*"]);
    let id = rig
        .call("POST", "/Patient", Some(patient("p")), Some(&writer))
        .await
        .body["id"]
        .as_str()
        .unwrap()
        .to_string();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = rig.app.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let (socket, _) = connect(&format!("ws://{addr}/sync?access_token={watcher}")).await;
    (rig, writer, id, socket)
}

fn add_read(query_id: u64, base: u64, id: &str) -> String {
    json!({"type": "ModifyQuerySet", "baseVersion": base, "newVersion": base + 1,
           "modifications": [{"type": "Add", "queryId": query_id, "udfPath": "Patient/read",
                              "args": {"id": id}}]})
    .to_string()
}

#[tokio::test]
async fn a_client_refused_for_going_too_fast_keeps_the_live_query_it_tried_to_replace() {
    let mut rate = limits(0.001, 6);
    rate.costs = Costs {
        sync: 2,
        ..Costs::default()
    };
    let (rig, writer, id, mut socket) = watched_patient(rate).await;
    // The upgrade took 2 of the 6, the query 2, and replacing it 2: nothing is left.
    socket
        .send(Message::Text(add_read(1, 0, &id)))
        .await
        .unwrap();
    assert_eq!(
        next_json(&mut socket).await["modifications"][0]["type"],
        "QueryUpdated"
    );
    socket
        .send(Message::Text(add_read(1, 1, &id)))
        .await
        .unwrap();
    assert_eq!(
        next_json(&mut socket).await["modifications"][0]["type"],
        "QueryUpdated"
    );
    // A second replacement is over the limit, and is refused.
    socket
        .send(Message::Text(add_read(1, 2, &id)))
        .await
        .unwrap();
    let refused = next_json(&mut socket).await;
    assert_eq!(
        refused["modifications"][0]["type"], "QueryFailed",
        "{refused}"
    );
    assert_eq!(
        refused["modifications"][0]["code"], "throttled",
        "{refused}"
    );
    // The subscription it would have replaced is still there: a write reaches it.
    let mut changed = patient(&id);
    changed["active"] = json!(false);
    let put = rig
        .call(
            "PUT",
            &format!("/Patient/{id}"),
            Some(changed),
            Some(&writer),
        )
        .await;
    assert_eq!(put.status, StatusCode::OK);
    let pushed = next_json(&mut socket).await;
    assert_eq!(
        pushed["modifications"][0]["type"], "QueryUpdated",
        "{pushed}"
    );
    assert_eq!(pushed["modifications"][0]["value"]["active"], false);
}

#[tokio::test]
async fn a_frame_of_several_live_queries_is_counted_once_not_once_each() {
    let (rig, _writer, id, mut socket) = watched_patient(limits(1_000.0, 1_000)).await;
    let modifications: Vec<Value> = (1..=5)
        .map(|query_id| {
            json!({"type": "Add", "queryId": query_id, "udfPath": "Patient/read", "args": {"id": id}})
        })
        .collect();
    let frame = json!({"type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1,
                       "modifications": modifications})
    .to_string();
    socket.send(Message::Text(frame.clone())).await.unwrap();
    let answer = next_json(&mut socket).await;
    assert_eq!(answer["modifications"].as_array().unwrap().len(), 5);
    let usage = rig.usage("client:watcher").await;
    let sync = class_row(&usage, "sync");
    // The upgrade, and five queries that together are about what the frame was. Counted once
    // each, the frame would have been five times over.
    assert!(
        sync.counters.bytes_in < frame.len() as u64,
        "{} bytes in for a frame of {}",
        sync.counters.bytes_in,
        frame.len()
    );
    assert!(sync.counters.bytes_in > 0);
}

#[tokio::test]
async fn a_flood_of_unauthenticated_refusals_leaves_a_bounded_number_of_rows_and_says_how_many_were_not_kept(
) {
    let mut config = observing();
    config.audit.unattributed_per_second = 1.0;
    let rig = rig(config, true);
    let acme = rig.token("acme", &["user/*.*"]);
    for _ in 0..60 {
        rig.call("GET", "/Patient", None, None).await;
    }
    // An identified caller's work is never sampled, however busy the door has been.
    let created = rig
        .call("POST", "/Patient", Some(patient("p")), Some(&acme))
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let rows = rig.audit().await;
    let refusals: Vec<_> = rows.iter().filter(|row| row.status == 401).collect();
    // The budget is one a second with room for five: the first few are kept in full.
    assert!(
        (5..=7).contains(&refusals.len()),
        "{} refusals kept",
        refusals.len()
    );
    let summary: Vec<_> = rows.iter().filter(|row| row.subtype == "sampled").collect();
    assert_eq!(summary.len(), 1, "{rows:?}");
    let kept = refusals.len();
    let detail = summary[0].detail.as_deref().unwrap();
    assert!(
        detail.starts_with(&format!("{} refused requests", 60 - kept)),
        "{detail}"
    );
    assert_eq!(
        (summary[0].outcome, summary[0].principal.as_str()),
        (4, "anonymous")
    );
    assert!(rows
        .iter()
        .any(|row| row.subtype == "create" && row.client_id.as_deref() == Some("acme")));
    assert_eq!(rig.governance.sampled().1, (60 - kept) as u64);
    // Each flush says what it has not yet said, once.
    rig.governance.flush().await;
    assert_eq!(
        rig.audit()
            .await
            .iter()
            .filter(|row| row.subtype == "sampled")
            .count(),
        1
    );
}

#[tokio::test]
async fn requests_turned_away_by_a_limit_cost_a_bounded_number_of_database_rows() {
    let mut config = GovernanceConfig {
        rate_limit: limits(0.001, 3),
        ..observing()
    };
    config.request_log.refused_per_second = 1.0;
    let rig = rig(config, false);
    for _ in 0..100 {
        rig.call("GET", "/metadata", None, None).await;
    }
    rig.governance.flush().await;
    let stored = rig
        .store
        .request_log_query(&RequestLogFilter {
            limit: 1_000,
            ..Default::default()
        })
        .unwrap();
    let throttled = stored.iter().filter(|row| row.status == 429).count();
    assert!(
        throttled <= 7,
        "{throttled} refused requests were written, of 97"
    );
    assert_eq!(
        stored.iter().filter(|row| row.status == 200).count(),
        3,
        "what was served is all there"
    );
    // The meter counts every one, in memory, at no cost per request.
    let usage = rig.usage("anonymous").await;
    let discovery = class_row(&usage, "discovery");
    assert_eq!(
        (discovery.counters.requests, discovery.counters.throttled),
        (3, 97)
    );
    assert_eq!(rig.governance.sampled().0, (97 - throttled) as u64);
}

#[tokio::test]
async fn sampling_can_be_turned_off_to_keep_every_row() {
    let mut config = observing();
    config.audit.unattributed_per_second = 0.0;
    let rig = rig(config, true);
    for _ in 0..40 {
        rig.call("GET", "/Patient", None, None).await;
    }
    let rows = rig.audit().await;
    assert_eq!(rows.iter().filter(|row| row.status == 401).count(), 40);
    assert!(rows.iter().all(|row| row.subtype != "sampled"));
}

fn keyed() -> GovernanceConfig {
    let mut config = observing();
    config.audit.chain_key = Some(refhir_persistence::ChainKey::new(
        b"a key for these tests".to_vec(),
    ));
    config
}

/// A rig with some audit rows in it, and a token to verify them with.
async fn rig_with_a_trail(config: GovernanceConfig) -> (Rig, String) {
    let rig = rig(config, true);
    let acme = rig.token("acme", &["user/*.*"]);
    let ops = rig.token("ops", &["system/AuditEvent.read"]);
    for _ in 0..5 {
        rig.call("POST", "/Patient", Some(patient("p")), Some(&acme))
            .await;
    }
    (rig, ops)
}

async fn creates(rig: &Rig) -> Vec<AuditRow> {
    rig.governance.flush().await;
    let mut rows = rig
        .store
        .audit_query(&AuditFilter {
            action: Some("C".into()),
            limit: 100,
            ..Default::default()
        })
        .unwrap();
    rows.reverse();
    rows
}

#[tokio::test]
async fn the_audit_chain_is_verified_through_the_endpoint() {
    let (rig, ops) = rig_with_a_trail(keyed()).await;
    let verified = rig.call("GET", "/_audit/verify", None, Some(&ops)).await;
    assert_eq!(verified.status, StatusCode::OK, "{:?}", verified.body);
    assert_eq!(verified.body["verified"], true, "{}", verified.body);
    assert_eq!(verified.body["keyed"], true);
    assert_eq!(verified.body["checked"], 5);
    assert_eq!(verified.body["breaksTotal"], 0);
    assert!(verified.body["note"].is_null());
    let head = &verified.body["head"];
    assert_eq!(head["hash"].as_str().unwrap().len(), 64);
    assert_eq!(head["id"], verified.body["lastId"]);
    // Reading the chain is itself audited, so the next verification has more to check.
    let again = rig.call("GET", "/_audit/verify", None, Some(&ops)).await;
    assert!(again.body["checked"].as_u64().unwrap() > 5);
    assert_eq!(again.body["verified"], true);
    // Who may read the trail may verify it, and nobody else.
    let acme = rig.token("acme", &["user/*.*"]);
    assert_eq!(
        rig.call("GET", "/_audit/verify", None, Some(&acme))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        rig.call("GET", "/_audit/verify", None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        rig.call("GET", "/_audit/verify?nonsense=1", None, Some(&ops))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn an_edit_a_deletion_and_a_forged_row_each_show_in_the_verification() {
    let (rig, ops) = rig_with_a_trail(keyed()).await;
    let ids: Vec<u64> = creates(&rig).await.iter().map(|row| row.id).collect();
    // Who did it, rewritten.
    rig.store
        .edit_audit_row(ids[1], |row| row.principal = "client:nobody".into());
    let edited = rig.call("GET", "/_audit/verify", None, Some(&ops)).await;
    assert_eq!(edited.body["verified"], false);
    assert_eq!(
        edited.body["breaks"],
        json!([{"id": ids[1], "problem": "edited"}])
    );
    // A row taken out of the middle: the one after it no longer follows.
    rig.store
        .edit_audit_row(ids[1], |row| row.principal = "client:acme".into());
    assert_eq!(
        rig.call("GET", "/_audit/verify", None, Some(&ops))
            .await
            .body["verified"],
        true
    );
    rig.store.remove_audit_row(ids[2]);
    let removed = rig.call("GET", "/_audit/verify", None, Some(&ops)).await;
    assert_eq!(removed.body["verified"], false);
    assert_eq!(
        removed.body["breaks"][0],
        json!({"id": ids[3], "problem": "detached"})
    );
    assert_eq!(removed.body["breaksTotal"], 1);
}

#[tokio::test]
async fn a_checkpoint_shows_that_the_newest_rows_were_not_deleted() {
    let (rig, ops) = rig_with_a_trail(keyed()).await;
    let head = rig
        .call("GET", "/_audit/verify", None, Some(&ops))
        .await
        .body["head"]
        .clone();
    let checkpoint = format!("{}:{}", head["id"], head["hash"].as_str().unwrap());
    // More happens, and the checkpoint is still in the trail.
    let acme = rig.token("acme", &["user/*.*"]);
    rig.call("POST", "/Patient", Some(patient("p")), Some(&acme))
        .await;
    let url = format!("/_audit/verify?checkpoint={checkpoint}");
    let held = rig.call("GET", &url, None, Some(&ops)).await;
    assert_eq!(
        (
            held.body["checkpoint"].clone(),
            held.body["verified"].clone()
        ),
        (json!("matches"), json!(true))
    );
    // A hash that is not the row's.
    let other = format!(
        "/_audit/verify?checkpoint={}:{}",
        head["id"],
        "ab".repeat(32)
    );
    let different = rig.call("GET", &other, None, Some(&ops)).await;
    assert_eq!(different.body["checkpoint"], "different");
    assert_eq!(different.body["verified"], false);
    // The newest rows deleted: every link that is left is sound, and the checkpoint is gone. The
    // server goes on to add rows after it, so the trail now runs past the checkpoint's id
    // without containing it. (A trail that ends *before* it is reported as `truncated`.)
    rig.store
        .truncate_audit_after(head["id"].as_u64().unwrap() - 1);
    let gone = rig.call("GET", &url, None, Some(&ops)).await;
    assert_eq!(gone.body["breaksTotal"], 0, "the chain itself is whole");
    assert_eq!(gone.body["checkpoint"], "missing");
    assert_eq!(gone.body["verified"], false);
    // A checkpoint that is not one.
    for bad in ["nonsense", "x:abcd", "5:zz", "5"] {
        let answer = rig
            .call(
                "GET",
                &format!("/_audit/verify?checkpoint={bad}"),
                None,
                Some(&ops),
            )
            .await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{bad}");
    }
}

#[tokio::test]
async fn without_a_key_the_verification_says_what_it_cannot_prove() {
    let (rig, ops) = rig_with_a_trail(observing()).await;
    let verified = rig.call("GET", "/_audit/verify", None, Some(&ops)).await;
    assert_eq!(verified.body["verified"], true);
    assert_eq!(verified.body["keyed"], false);
    assert!(verified.body["note"]
        .as_str()
        .unwrap()
        .contains("proves nothing"));
}

async fn post_form(rig: &Rig, uri: &str, form: &str) -> StatusCode {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form.to_string()))
        .unwrap();
    rig.app.clone().oneshot(request).await.unwrap().status()
}

#[tokio::test]
async fn what_a_credential_proves_at_the_oauth_endpoints_is_recorded_under_its_client() {
    let rig = rig(observing(), true);
    let mine = rig.token("public", &["user/*.*"]);
    let theirs = rig.token("someone-else", &["user/*.*"]);
    // Holding one of a client's tokens is the proof; naming the client is not.
    assert_eq!(
        post_form(
            &rig,
            "/oauth/revoke",
            &format!("token={mine}&client_id=public")
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        post_form(
            &rig,
            "/oauth/revoke",
            &format!("token={theirs}&client_id=public")
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    let rows: Vec<AuditRow> = rig
        .audit()
        .await
        .into_iter()
        .filter(|row| row.subtype == "oauth-revoke")
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (
            rows[0].outcome,
            rows[0].principal.as_str(),
            rows[0].client_id.as_deref(),
            rows[0].user.as_deref()
        ),
        (0, "client:public", Some("public"), Some("u-1"))
    );
    assert_eq!(
        (
            rows[1].outcome,
            rows[1].principal.as_str(),
            rows[1].client_id.as_deref()
        ),
        (4, "anonymous", None),
        "a refusal names no one"
    );
    assert!(rig
        .usage("client:public")
        .await
        .iter()
        .any(|row| row.key.class == "auth"));
}

#[tokio::test]
async fn a_patient_scoped_caller_is_limited_before_their_body_is_read() {
    // The bucket holds exactly one write, and refills too slowly to matter.
    let rig = rig(
        GovernanceConfig {
            rate_limit: limits(0.001, 2),
            ..observing()
        },
        true,
    );
    let token = format!("tok-{}", uuid::Uuid::new_v4());
    rig.smart
        .put_token(
            token.clone(),
            Token {
                scopes: vec!["patient/*.*".into()],
                patient: Some("p1".into()),
                encounter: None,
                fhir_user: None,
                client_id: "app".into(),
                exp: now_ms() / 1_000 + 3_600,
                sub: Some("u-1".into()),
                iss: None,
                need_patient_banner: None,
                smart_style_url: None,
            },
        )
        .unwrap();
    // About someone else, so what reads and parses the body refuses it: the 403 is that work.
    let other = json!({"resourceType": "Observation", "status": "final", "code": {"text": "x"},
                       "subject": {"reference": "Patient/someone-else"}});
    let first = rig
        .call("POST", "/Observation", Some(other.clone()), Some(&token))
        .await;
    assert_eq!(first.status, StatusCode::FORBIDDEN, "{}", first.body);
    let second = rig
        .call("POST", "/Observation", Some(other), Some(&token))
        .await;
    assert_eq!(
        second.status,
        StatusCode::TOO_MANY_REQUESTS,
        "turned away before the body was touched: {}",
        second.body
    );
}

#[tokio::test]
async fn without_smart_ordinary_client_errors_are_not_budgeted_as_probing() {
    let mut config = observing();
    // A budget of one a second, five in a burst: nothing like twelve.
    config.request_log.refused_per_second = 1.0;
    let rig = rig(config, false);
    for n in 0..12 {
        let answer = rig
            .call("GET", &format!("/Patient/missing{n}"), None, None)
            .await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND);
    }
    rig.governance.flush().await;
    let stored = rig
        .store
        .request_log_query(&RequestLogFilter {
            min_status: Some(404),
            limit: 100,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(stored.len(), 12, "every one is kept: nobody is a stranger");
    assert_eq!(rig.governance.sampled().0, 0);
}

#[tokio::test]
async fn x_forwarded_for_is_believed_only_on_a_connection_from_a_proxy() {
    let rig = rig(
        GovernanceConfig {
            proxy: ProxyConfig {
                trusted_hops: 1,
                trusted_peers: vec![Cidr::parse("10.0.0.0/8").unwrap()],
            },
            ..observing()
        },
        false,
    );
    let claimed = [("x-forwarded-for", "198.51.100.7")];
    rig.call_from(
        "GET",
        "/Patient/direct",
        None,
        None,
        &claimed,
        Some("203.0.113.5:4000".parse().unwrap()),
    )
    .await;
    rig.call_from(
        "GET",
        "/Patient/proxied",
        None,
        None,
        &claimed,
        Some("10.0.0.2:4000".parse().unwrap()),
    )
    .await;
    let rows = rig.audit().await;
    let address = |id: &str| {
        rows.iter()
            .find(|row| row.resource_id.as_deref() == Some(id))
            .and_then(|row| row.remote_ip.clone())
    };
    assert_eq!(
        address("direct").as_deref(),
        Some("203.0.113.5"),
        "a client that got past the proxies does not choose its address"
    );
    assert_eq!(
        address("proxied").as_deref(),
        Some("198.51.100.7"),
        "a proxy's word is taken"
    );
}

#[tokio::test]
async fn a_usage_answer_that_was_cut_short_says_so() {
    let (rig, ops) = rig_with_a_trail(observing()).await;
    let acme = rig.token("acme", &["user/*.*"]);
    rig.call("GET", "/Patient", None, Some(&acme)).await;
    let cut = rig
        .call("GET", "/_usage?client=acme&_count=1", None, Some(&ops))
        .await;
    assert_eq!(cut.body["usage"].as_array().unwrap().len(), 1);
    assert_eq!(cut.body["truncated"], true, "{}", cut.body);
    let whole = rig
        .call("GET", "/_usage?client=acme", None, Some(&ops))
        .await;
    assert_eq!(whole.body["usage"].as_array().unwrap().len(), 2);
    assert_eq!(whole.body["truncated"], false);
}

#[tokio::test]
async fn a_long_resource_type_that_reaches_the_engine_cannot_make_a_row_large() {
    let rig = rig(observing(), true);
    let token = rig.token("acme", &["user/*.*"]);
    let long = "A".repeat(5_000);
    let answer = rig
        .call("GET", &format!("/{long}/{long}"), None, Some(&token))
        .await;
    assert!(answer.status.is_client_error(), "{:?}", answer.status);
    let rows = rig.audit().await;
    assert!(!rows.is_empty(), "the refusal is recorded");
    for row in &rows {
        for text in [
            &row.resource_type,
            &row.resource_id,
            &row.patient,
            &row.detail,
        ] {
            assert!(
                text.as_deref().map_or(0, |text| text.chars().count()) <= 256,
                "{row:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_health_probe_is_never_refused_counted_logged_or_audited() {
    // A bucket of two, refilling once in a thousand seconds: a caller is refused on its third.
    let rig = rig(
        GovernanceConfig {
            rate_limit: limits(0.001, 2),
            ..observing()
        },
        false,
    );
    for _ in 0..20 {
        let probe = rig.call("GET", "/healthz", None, None).await;
        assert_eq!(probe.status, StatusCode::OK, "{}", probe.body);
    }
    rig.governance.flush().await;
    assert!(
        rig.audit().await.is_empty(),
        "a probe is not an interaction"
    );
    assert!(rig
        .store
        .request_log_query(&RequestLogFilter {
            limit: 100,
            ..Default::default()
        })
        .unwrap()
        .is_empty());
    assert!(rig.usage("anonymous").await.is_empty());
    // And it did not use up what the same address gets for everything else.
    assert_eq!(
        rig.call("GET", "/metadata", None, None).await.status,
        StatusCode::OK
    );
}
