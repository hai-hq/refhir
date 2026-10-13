//! What each caller has used, and whether they have used their share.
//!
//! Every request adds to two things. The first is a pending set of counters per (day, caller,
//! class) that the flusher writes to the database in one batch, so the hot path never touches
//! the database. The second is the caller's *window*: running totals for today and this month
//! that a quota is checked against. A window is read from the database the first time a caller
//! is seen and again every refresh interval, which is how a server learns what the others
//! sharing the database have counted. In between, a quota is enforced on what this server
//! knows, so with several servers a caller can overshoot by about what the others served in
//! one interval.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use refhir_persistence::{GovernanceStore, UsageCounters, UsageFilter, UsageKey, UsageRow};

use super::civil;
use super::classify::Class;
use super::config::QuotaLimits;

/// Totals a quota is measured in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub requests: u64,
    pub writes: u64,
    pub bytes: u64,
}

impl Totals {
    fn add(&mut self, other: Totals) {
        self.requests = self.requests.saturating_add(other.requests);
        self.writes = self.writes.saturating_add(other.writes);
        self.bytes = self.bytes.saturating_add(other.bytes);
    }

    /// What `counters` for one class add to a quota.
    fn of(class: Option<Class>, counters: &UsageCounters) -> Totals {
        if !class.is_none_or(Class::counts_toward_quota) {
            return Totals::default();
        }
        Totals {
            requests: counters.requests,
            writes: if class.is_some_and(Class::is_write) {
                counters.requests
            } else {
                0
            },
            bytes: counters.bytes_in.saturating_add(counters.bytes_out),
        }
    }
}

struct Window {
    day: u32,
    month_start: u32,
    today: Totals,
    month: Totals,
    loaded_at: Instant,
}

/// One request, as the meter sees it.
pub struct Observation<'a> {
    pub principal: &'a str,
    pub class: Class,
    pub status: u16,
    /// Turned away by a rate limit or a quota.
    pub throttled: bool,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub elapsed: Duration,
}

/// A quota that has been used up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breach {
    pub what: &'static str,
    pub limit: u64,
    pub used: u64,
    /// When the quota starts again, in Unix milliseconds.
    pub resets_ms: u64,
}

#[derive(Default)]
pub struct Meter {
    pending: Mutex<HashMap<UsageKey, UsageCounters>>,
    windows: Mutex<HashMap<String, Window>>,
}

impl Meter {
    pub fn record(&self, observation: &Observation<'_>, now_ms: u64) {
        let day = civil::day_of(now_ms);
        let counters = UsageCounters {
            requests: u64::from(!observation.throttled),
            client_errors: u64::from(
                !observation.throttled && (400..500).contains(&observation.status),
            ),
            server_errors: u64::from(observation.status >= 500),
            throttled: u64::from(observation.throttled),
            // A request that was turned away moved no data that anyone was served: its declared
            // length is a claim, its body was never read, and a data quota is not spent on it.
            bytes_in: if observation.throttled {
                0
            } else {
                observation.bytes_in
            },
            bytes_out: if observation.throttled {
                0
            } else {
                observation.bytes_out
            },
            duration_ms: u64::try_from(observation.elapsed.as_millis()).unwrap_or(u64::MAX),
        };
        self.pending
            .lock()
            .expect("meter")
            .entry(UsageKey {
                day,
                principal: observation.principal.to_string(),
                class: observation.class.name().to_string(),
            })
            .or_default()
            .add(&counters);
        if let Some(window) = self
            .windows
            .lock()
            .expect("meter")
            .get_mut(observation.principal)
        {
            roll(window, day);
            window
                .today
                .add(Totals::of(Some(observation.class), &counters));
            window
                .month
                .add(Totals::of(Some(observation.class), &counters));
        }
    }

    /// Take what has not been written yet.
    pub fn take_pending(&self) -> Vec<UsageRow> {
        std::mem::take(&mut *self.pending.lock().expect("meter"))
            .into_iter()
            .map(|(key, counters)| UsageRow { key, counters })
            .collect()
    }

    /// Put back what could not be written, to be written with the next batch.
    pub fn restore(&self, rows: Vec<UsageRow>) {
        let mut pending = self.pending.lock().expect("meter");
        for row in rows {
            pending.entry(row.key).or_default().add(&row.counters);
        }
    }

    /// Whether `principal`'s window must be read from the database now, and if so, claim the
    /// read: concurrent requests of a caller seen for the first time then do one read, not one
    /// each.
    fn claim_load(&self, principal: &str, now_ms: u64, refresh: Duration) -> bool {
        let day = civil::day_of(now_ms);
        let month_start = civil::month_start(day);
        let mut windows = self.windows.lock().expect("meter");
        match windows.get_mut(principal) {
            Some(window)
                if window.loaded_at.elapsed() < refresh
                    && window.day == day
                    && window.month_start == month_start =>
            {
                false
            }
            Some(window) => {
                roll(window, day);
                window.loaded_at = Instant::now();
                true
            }
            None => {
                windows.insert(
                    principal.to_string(),
                    Window {
                        day,
                        month_start,
                        today: Totals::default(),
                        month: Totals::default(),
                        loaded_at: Instant::now(),
                    },
                );
                true
            }
        }
    }

    /// Replace a window's totals with what the database holds plus what is waiting to be
    /// written here.
    fn install(&self, principal: &str, rows: &[UsageRow], now_ms: u64) {
        let day = civil::day_of(now_ms);
        let month_start = civil::month_start(day);
        let mut today = Totals::default();
        let mut month = Totals::default();
        let mut count = |key: &UsageKey, counters: &UsageCounters| {
            if key.principal != principal || key.day < month_start || key.day > day {
                return;
            }
            let totals = Totals::of(Class::parse(&key.class), counters);
            month.add(totals);
            if key.day == day {
                today.add(totals);
            }
        };
        for row in rows {
            count(&row.key, &row.counters);
        }
        for (key, counters) in self.pending.lock().expect("meter").iter() {
            count(key, counters);
        }
        let mut windows = self.windows.lock().expect("meter");
        let window = windows
            .entry(principal.to_string())
            .or_insert_with(|| Window {
                day,
                month_start,
                today,
                month,
                loaded_at: Instant::now(),
            });
        window.day = day;
        window.month_start = month_start;
        window.today = today;
        window.month = month;
        window.loaded_at = Instant::now();
    }

    /// Bring `principal`'s totals up to date if they are older than `refresh`. A database that
    /// cannot be read leaves the quota enforced on what this server has counted itself, rather
    /// than refusing everyone because of it.
    pub async fn ensure_loaded(
        &self,
        store: &Arc<dyn GovernanceStore>,
        principal: &str,
        now_ms: u64,
        refresh: Duration,
    ) {
        if !self.claim_load(principal, now_ms, refresh) {
            return;
        }
        let day = civil::day_of(now_ms);
        let filter = UsageFilter {
            principal: Some(principal.to_string()),
            from_day: Some(civil::month_start(day)),
            to_day: Some(day),
            limit: 10_000,
        };
        let store = store.clone();
        match tokio::task::spawn_blocking(move || store.usage_query(&filter)).await {
            Ok(Ok(rows)) => self.install(principal, &rows, now_ms),
            Ok(Err(err)) => tracing::warn!(
                principal,
                error = %err,
                "could not read usage for a quota; enforcing it on what this server has counted"
            ),
            Err(_) => tracing::warn!(principal, "usage read for a quota stopped"),
        }
    }

    /// The quota that `class` of request by `principal` would run past, if any. Where several
    /// are used up, the one that lasts longest is reported, since waiting for a sooner one
    /// would not help.
    pub fn check(
        &self,
        principal: &str,
        limits: &QuotaLimits,
        class: Class,
        now_ms: u64,
    ) -> Option<Breach> {
        let windows = self.windows.lock().expect("meter");
        let window = windows.get(principal)?;
        let day = civil::day_of(now_ms);
        let (today, month) = if window.day == day && window.month_start == civil::month_start(day) {
            (window.today, window.month)
        } else {
            (Totals::default(), Totals::default())
        };
        let day_end = civil::day_start_ms(day + 1);
        let month_end = civil::day_start_ms(civil::next_month_start(day));
        let mut breaches = Vec::new();
        let mut test = |what: &'static str, limit: Option<u64>, used: u64, resets_ms: u64| {
            if let Some(limit) = limit.filter(|limit| *limit > 0) {
                if used >= limit {
                    breaches.push(Breach {
                        what,
                        limit,
                        used,
                        resets_ms,
                    });
                }
            }
        };
        test(
            "daily request quota",
            limits.requests_per_day,
            today.requests,
            day_end,
        );
        test(
            "monthly request quota",
            limits.requests_per_month,
            month.requests,
            month_end,
        );
        test(
            "daily data quota",
            limits.bytes_per_day,
            today.bytes,
            day_end,
        );
        test(
            "monthly data quota",
            limits.bytes_per_month,
            month.bytes,
            month_end,
        );
        if class.is_write() {
            test(
                "daily write quota",
                limits.writes_per_day,
                today.writes,
                day_end,
            );
            test(
                "monthly write quota",
                limits.writes_per_month,
                month.writes,
                month_end,
            );
        }
        breaches.into_iter().max_by_key(|breach| breach.resets_ms)
    }

    /// Forget windows that have not been looked at for a while.
    pub fn sweep(&self, idle: Duration) {
        self.windows
            .lock()
            .expect("meter")
            .retain(|_, window| window.loaded_at.elapsed() < idle);
    }

    /// The totals this server holds for `principal`: today and this month.
    #[cfg(test)]
    fn totals(&self, principal: &str) -> Option<(Totals, Totals)> {
        self.windows
            .lock()
            .expect("meter")
            .get(principal)
            .map(|window| (window.today, window.month))
    }
}

/// Start a new day, and a new month where the day starts one, with nothing counted in it yet.
fn roll(window: &mut Window, day: u32) {
    if window.day == day {
        return;
    }
    window.day = day;
    window.today = Totals::default();
    let month_start = civil::month_start(day);
    if window.month_start != month_start {
        window.month_start = month_start;
        window.month = Totals::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use refhir_persistence::MemoryGovernanceStore;

    const DAY_MS: u64 = 86_400_000;
    /// 2026-10-12T00:00:00Z
    const OCT_12: u64 = 20_738 * DAY_MS;

    fn served(principal: &str, class: Class, bytes: u64) -> Observation<'_> {
        Observation {
            principal,
            class,
            status: 200,
            throttled: false,
            bytes_in: bytes,
            bytes_out: bytes,
            elapsed: Duration::from_millis(5),
        }
    }

    fn store() -> Arc<dyn GovernanceStore> {
        Arc::new(MemoryGovernanceStore::default())
    }

    #[test]
    fn requests_are_counted_by_day_caller_and_class() {
        let meter = Meter::default();
        meter.record(&served("client:a", Class::Read, 10), OCT_12);
        meter.record(&served("client:a", Class::Read, 10), OCT_12 + 1);
        meter.record(&served("client:a", Class::Write, 1), OCT_12);
        meter.record(
            &Observation {
                status: 404,
                ..served("client:a", Class::Read, 0)
            },
            OCT_12,
        );
        meter.record(
            &Observation {
                status: 500,
                ..served("client:a", Class::Search, 0)
            },
            OCT_12 + DAY_MS,
        );
        meter.record(
            &Observation {
                status: 429,
                throttled: true,
                // Declared, and never read: not counted as data moved.
                ..served("client:a", Class::Read, 500)
            },
            OCT_12,
        );
        let mut rows = meter.take_pending();
        rows.sort_by(|a, b| a.key.cmp(&b.key));
        let find = |day: u32, class: &str| {
            rows.iter()
                .find(|row| row.key.day == day && row.key.class == class)
                .map(|row| row.counters)
                .unwrap()
        };
        let read = find(20_738, "read");
        assert_eq!(
            (
                read.requests,
                read.client_errors,
                read.throttled,
                read.bytes_in,
                read.bytes_out
            ),
            (3, 1, 1, 20, 20),
            "a throttled request is not a request, and not a client error"
        );
        assert_eq!(find(20_738, "write").requests, 1);
        assert_eq!(
            find(20_739, "search").server_errors,
            1,
            "the next day is its own row"
        );
        assert_eq!(rows.len(), 3);
        assert!(meter.take_pending().is_empty(), "taken once");
    }

    #[test]
    fn rows_that_could_not_be_written_are_added_back() {
        let meter = Meter::default();
        meter.record(&served("client:a", Class::Read, 1), OCT_12);
        let first = meter.take_pending();
        meter.record(&served("client:a", Class::Read, 1), OCT_12);
        meter.restore(first);
        let rows = meter.take_pending();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].counters.requests, 2);
    }

    #[tokio::test]
    async fn a_callers_totals_are_read_once_and_then_kept_current() {
        let backend = store();
        backend
            .usage_add(&[
                UsageRow {
                    key: UsageKey {
                        day: 20_738,
                        principal: "client:a".into(),
                        class: "read".into(),
                    },
                    counters: UsageCounters {
                        requests: 40,
                        bytes_in: 5,
                        bytes_out: 5,
                        ..Default::default()
                    },
                },
                UsageRow {
                    key: UsageKey {
                        day: 20_738,
                        principal: "client:a".into(),
                        class: "write".into(),
                    },
                    counters: UsageCounters {
                        requests: 4,
                        ..Default::default()
                    },
                },
                // Earlier in the month: in the month's total, not today's.
                UsageRow {
                    key: UsageKey {
                        day: 20_730,
                        principal: "client:a".into(),
                        class: "read".into(),
                    },
                    counters: UsageCounters {
                        requests: 100,
                        ..Default::default()
                    },
                },
                // Last month, and another caller, and a class that is not metered.
                UsageRow {
                    key: UsageKey {
                        day: 20_700,
                        principal: "client:a".into(),
                        class: "read".into(),
                    },
                    counters: UsageCounters {
                        requests: 9_999,
                        ..Default::default()
                    },
                },
                UsageRow {
                    key: UsageKey {
                        day: 20_738,
                        principal: "client:b".into(),
                        class: "read".into(),
                    },
                    counters: UsageCounters {
                        requests: 7_777,
                        ..Default::default()
                    },
                },
                UsageRow {
                    key: UsageKey {
                        day: 20_738,
                        principal: "client:a".into(),
                        class: "discovery".into(),
                    },
                    counters: UsageCounters {
                        requests: 500,
                        ..Default::default()
                    },
                },
            ])
            .unwrap();
        let meter = Meter::default();
        let refresh = Duration::from_secs(30);
        // Counted here but not written yet: it is part of the total too.
        meter.record(&served("client:a", Class::Read, 0), OCT_12);
        meter
            .ensure_loaded(&backend, "client:a", OCT_12, refresh)
            .await;
        let (today, month) = meter.totals("client:a").unwrap();
        assert_eq!(
            (today.requests, today.writes, today.bytes),
            (45, 4, 10),
            "44 stored + 1 waiting; the write is among the requests"
        );
        assert_eq!(month.requests, 145);
        // Fresh: not read again, and what is counted adds to it.
        backend.usage_add(&[]).unwrap();
        meter.record(&served("client:a", Class::Write, 0), OCT_12);
        meter
            .ensure_loaded(&backend, "client:a", OCT_12, refresh)
            .await;
        let (today, _) = meter.totals("client:a").unwrap();
        assert_eq!((today.requests, today.writes), (46, 5));
        // The next day starts empty, and the month carries on.
        meter
            .ensure_loaded(&backend, "client:a", OCT_12 + DAY_MS, refresh)
            .await;
        let (today, month) = meter.totals("client:a").unwrap();
        assert_eq!(today.requests, 0);
        assert_eq!(month.requests, 146);
    }

    #[test]
    fn a_quota_is_used_up_when_the_total_reaches_it() {
        let meter = Meter::default();
        let limits = QuotaLimits {
            requests_per_day: Some(3),
            writes_per_day: Some(1),
            bytes_per_month: Some(100),
            ..QuotaLimits::default()
        };
        // A caller the meter has not read yet has nothing to be checked against.
        assert_eq!(meter.check("client:a", &limits, Class::Read, OCT_12), None);
        assert!(meter.claim_load("client:a", OCT_12, Duration::from_secs(30)));
        assert!(
            !meter.claim_load("client:a", OCT_12, Duration::from_secs(30)),
            "claimed once"
        );
        for _ in 0..2 {
            meter.record(&served("client:a", Class::Read, 0), OCT_12);
        }
        assert_eq!(meter.check("client:a", &limits, Class::Read, OCT_12), None);
        meter.record(&served("client:a", Class::Read, 0), OCT_12);
        let breach = meter
            .check("client:a", &limits, Class::Read, OCT_12)
            .unwrap();
        assert_eq!(
            breach,
            Breach {
                what: "daily request quota",
                limit: 3,
                used: 3,
                resets_ms: OCT_12 + DAY_MS,
            }
        );
        // Tomorrow the day's quota is new, though the month's is not.
        assert_eq!(
            meter.check("client:a", &limits, Class::Read, OCT_12 + DAY_MS),
            None
        );
    }

    #[test]
    fn the_write_quota_applies_to_writes_only_and_the_longest_reset_is_reported() {
        let meter = Meter::default();
        let limits = QuotaLimits {
            writes_per_day: Some(1),
            requests_per_month: Some(2),
            ..QuotaLimits::default()
        };
        assert!(meter.claim_load("client:a", OCT_12, Duration::from_secs(30)));
        meter.record(&served("client:a", Class::Write, 0), OCT_12);
        assert_eq!(
            meter.check("client:a", &limits, Class::Read, OCT_12),
            None,
            "a read is not stopped by the write quota"
        );
        let breach = meter
            .check("client:a", &limits, Class::Write, OCT_12)
            .unwrap();
        assert_eq!(
            (breach.what, breach.resets_ms),
            ("daily write quota", OCT_12 + DAY_MS)
        );
        meter.record(&served("client:a", Class::Read, 0), OCT_12);
        // Now the month's request quota is used up too, and waiting for the day would not help.
        let breach = meter
            .check("client:a", &limits, Class::Write, OCT_12)
            .unwrap();
        assert_eq!(breach.what, "monthly request quota");
        assert_eq!(breach.resets_ms, 20_758 * DAY_MS, "2026-11-01");
    }

    #[test]
    fn discovery_auth_and_admin_requests_use_no_quota() {
        let meter = Meter::default();
        let limits = QuotaLimits {
            requests_per_day: Some(1),
            ..QuotaLimits::default()
        };
        assert!(meter.claim_load("client:a", OCT_12, Duration::from_secs(30)));
        for class in [Class::Discovery, Class::Auth, Class::Admin] {
            meter.record(&served("client:a", class, 0), OCT_12);
        }
        assert_eq!(meter.check("client:a", &limits, Class::Read, OCT_12), None);
        meter.record(&served("client:a", Class::Read, 0), OCT_12);
        assert!(meter
            .check("client:a", &limits, Class::Read, OCT_12)
            .is_some());
    }
}
