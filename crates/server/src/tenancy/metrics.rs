//! What a process counts for each tenant it serves, for `/admin/metrics` and the admin API.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{json, Value};

/// Counters of one tenant, since the process started serving it.
#[derive(Default)]
pub struct TenantCounters {
    requests: AtomicU64,
    status_2xx: AtomicU64,
    status_3xx: AtomicU64,
    status_4xx: AtomicU64,
    status_5xx: AtomicU64,
    throttled: AtomicU64,
    refused_storage: AtomicU64,
    in_flight: AtomicU64,
    /// Total time spent answering, in microseconds.
    busy_us: AtomicU64,
}

/// Counts a request as in flight until dropped.
pub struct InFlight<'a>(&'a TenantCounters);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl TenantCounters {
    pub(crate) fn begin(&self) -> InFlight<'_> {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        InFlight(self)
    }

    pub(crate) fn finish(&self, status: StatusCode, took: Duration) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let class = match status.as_u16() {
            200..=299 => &self.status_2xx,
            300..=399 => &self.status_3xx,
            400..=499 => &self.status_4xx,
            _ => &self.status_5xx,
        };
        class.fetch_add(1, Ordering::Relaxed);
        self.busy_us.fetch_add(
            u64::try_from(took.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    pub(crate) fn throttled(&self) {
        self.throttled.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn refused_storage(&self) {
        self.refused_storage.fetch_add(1, Ordering::Relaxed);
    }

    /// Requests being answered now.
    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Requests the tenant's server answered (not those refused before it, which are counted as
    /// `throttled` and `refused_storage`).
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    pub fn json(&self) -> Value {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        json!({
            "requests": load(&self.requests),
            "responses": {
                "2xx": load(&self.status_2xx),
                "3xx": load(&self.status_3xx),
                "4xx": load(&self.status_4xx),
                "5xx": load(&self.status_5xx),
            },
            "throttled": load(&self.throttled),
            "refused_storage": load(&self.refused_storage),
            "in_flight": load(&self.in_flight),
            "busy_seconds": load(&self.busy_us) as f64 / 1e6,
        })
    }

    /// The counters as Prometheus samples: family, type, help, labels after the tenant's, value.
    pub(crate) fn samples(
        &self,
    ) -> Vec<(&'static str, &'static str, &'static str, String, String)> {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed).to_string();
        let mut samples = Vec::new();
        for (class, counter) in [
            ("2xx", &self.status_2xx),
            ("3xx", &self.status_3xx),
            ("4xx", &self.status_4xx),
            ("5xx", &self.status_5xx),
        ] {
            samples.push((
                "refhir_tenant_responses_total",
                "counter",
                "Responses the tenant's server gave, by status class",
                format!(",class=\"{class}\""),
                load(counter),
            ));
        }
        samples.push((
            "refhir_tenant_throttled_total",
            "counter",
            "Requests refused for the tenant's request rate",
            String::new(),
            load(&self.throttled),
        ));
        samples.push((
            "refhir_tenant_refused_storage_total",
            "counter",
            "Requests refused for the tenant's storage limit",
            String::new(),
            load(&self.refused_storage),
        ));
        samples.push((
            "refhir_tenant_in_flight",
            "gauge",
            "Requests of the tenant being answered now",
            String::new(),
            load(&self.in_flight),
        ));
        samples.push((
            "refhir_tenant_busy_seconds_total",
            "counter",
            "Time spent answering the tenant's requests",
            String::new(),
            (self.busy_us.load(Ordering::Relaxed) as f64 / 1e6).to_string(),
        ));
        samples
    }
}
