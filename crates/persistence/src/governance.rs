//! What the server records about its own traffic: what each caller used (metering), a bounded
//! log of recent requests, and the audit trail of who touched which resource.
//!
//! These rows live in tables of their own, outside the FHIR document log. They are not clinical
//! data, they are written on the side of every request, and an audit row must not be something
//! a FHIR client can update or delete. The store appends and answers queries; it does not decide
//! what is worth recording and it does not read the clock, so callers pass times in Unix
//! milliseconds. Writes come in batches because every backend serialises them with the clinical
//! reads and writes on one connection.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::{audit_digest, ChainKey, ChainedRow, PersistError, CHAIN_GENESIS};

pub const MS_PER_DAY: u64 = 86_400_000;

/// SQL value for an unsigned counter or time: clamped to the signed 64-bit range.
pub fn sql_int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// An unsigned counter or time read back from a signed column.
pub fn sql_uint(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// Counters for one caller, one UTC day and one class of request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageCounters {
    /// Requests that were answered, whatever the answer. One turned away by a rate limit or a
    /// quota is counted in `throttled` instead, so `requests + throttled` is every attempt.
    pub requests: u64,
    /// Answers in the 4xx range.
    pub client_errors: u64,
    pub server_errors: u64,
    /// Requests turned away by a rate limit or a quota.
    pub throttled: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// Time spent answering, summed, so a mean is `duration_ms / requests`.
    pub duration_ms: u64,
}

impl UsageCounters {
    pub fn add(&mut self, other: &UsageCounters) {
        self.requests = self.requests.saturating_add(other.requests);
        self.client_errors = self.client_errors.saturating_add(other.client_errors);
        self.server_errors = self.server_errors.saturating_add(other.server_errors);
        self.throttled = self.throttled.saturating_add(other.throttled);
        self.bytes_in = self.bytes_in.saturating_add(other.bytes_in);
        self.bytes_out = self.bytes_out.saturating_add(other.bytes_out);
        self.duration_ms = self.duration_ms.saturating_add(other.duration_ms);
    }

    pub fn is_zero(&self) -> bool {
        *self == UsageCounters::default()
    }
}

/// Which counters a row holds: `day` is the number of whole days since 1970-01-01 UTC.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UsageKey {
    pub day: u32,
    pub principal: String,
    pub class: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageRow {
    pub key: UsageKey,
    pub counters: UsageCounters,
}

#[derive(Clone, Debug, Default)]
pub struct UsageFilter {
    pub principal: Option<String>,
    pub from_day: Option<u32>,
    /// Inclusive.
    pub to_day: Option<u32>,
    pub limit: u32,
}

/// One request, as it was answered. `params` holds the names of the query parameters and never
/// their values, which can be identifiers of patients.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestLogRow {
    /// Set by the store; ignored when appending.
    pub id: u64,
    pub ts_ms: u64,
    pub request_id: String,
    pub principal: String,
    pub remote_ip: Option<String>,
    pub method: String,
    pub path: String,
    pub params: String,
    pub class: String,
    pub status: u16,
    pub duration_ms: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

#[derive(Clone, Debug, Default)]
pub struct RequestLogFilter {
    pub principal: Option<String>,
    pub request_id: Option<String>,
    /// Only responses with at least this status.
    pub min_status: Option<u16>,
    pub since_ms: Option<u64>,
    pub until_ms: Option<u64>,
    /// Only rows older than this id, to continue a listing.
    pub before_id: Option<u64>,
    pub limit: u32,
}

/// One thing that happened to one resource, or one attempt that was turned away. Shaped after
/// FHIR's `AuditEvent` so it can be handed out as one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditRow {
    /// Set by the store; ignored when appending.
    pub id: u64,
    pub ts_ms: u64,
    pub request_id: String,
    /// `http` or `sync`.
    pub source: String,
    /// FHIR `AuditEvent.action`: `C`, `R`, `U`, `D` or `E`.
    pub action: String,
    /// A FHIR restful interaction code such as `read`, `search-type`, `transaction`, or a name
    /// for what has none, such as `oauth-token`.
    pub subtype: String,
    /// FHIR `AuditEvent.outcome`: 0 success, 4 minor failure, 8 serious failure.
    pub outcome: u8,
    /// The HTTP status, or 0 where there was none.
    pub status: u16,
    /// Who asked, as the limiter and the meter know them: `client:<id>` or `ip:<address>`.
    pub principal: String,
    pub client_id: Option<String>,
    pub user: Option<String>,
    pub fhir_user: Option<String>,
    pub remote_ip: Option<String>,
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
    /// The patient whose record this touches, where that could be told.
    pub patient: Option<String>,
    pub query: Option<String>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct AuditFilter {
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
    pub patient: Option<String>,
    pub principal: Option<String>,
    pub user: Option<String>,
    pub action: Option<String>,
    /// `Some(true)`: only attempts that did not succeed. `Some(false)`: only those that did.
    pub failed: Option<bool>,
    pub request_id: Option<String>,
    pub since_ms: Option<u64>,
    pub until_ms: Option<u64>,
    /// Only rows older than this id, to continue a listing.
    pub before_id: Option<u64>,
    pub limit: u32,
}

/// How many rows [`GovernanceStore::prune`] removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    pub request_log: u64,
    pub audit: u64,
}

/// A value to bind into a query built from a filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Param {
    Text(String),
    Int(i64),
}

/// A `WHERE` clause built from a filter. A condition holds one `?` for the value it binds, or none
/// where it is fixed, so a backend can number them for its own dialect and bind
/// [`Where::params`] in order.
#[derive(Debug, Default)]
pub struct Where {
    conditions: Vec<&'static str>,
    pub params: Vec<Param>,
}

impl Where {
    fn text(&mut self, condition: &'static str, value: &Option<String>) {
        if let Some(value) = value {
            self.conditions.push(condition);
            self.params.push(Param::Text(value.clone()));
        }
    }

    fn int(&mut self, condition: &'static str, value: Option<i64>) {
        if let Some(value) = value {
            self.conditions.push(condition);
            self.params.push(Param::Int(value));
        }
    }

    /// ` WHERE a AND b`, or nothing. With `numbered`, placeholders are `$1`, `$2`, … counting
    /// from `first`; without, they stay `?`.
    pub fn sql(&self, numbered: bool, first: usize) -> String {
        if self.conditions.is_empty() {
            return String::new();
        }
        let mut next = first;
        let joined: Vec<String> = self
            .conditions
            .iter()
            .map(|condition| {
                if numbered && condition.contains('?') {
                    let text = condition.replacen('?', &format!("${next}"), 1);
                    next += 1;
                    text
                } else {
                    condition.to_string()
                }
            })
            .collect();
        format!(" WHERE {}", joined.join(" AND "))
    }
}

impl UsageFilter {
    pub fn to_where(&self) -> Where {
        let mut clause = Where::default();
        clause.text("principal = ?", &self.principal);
        clause.int("period_day >= ?", self.from_day.map(i64::from));
        clause.int("period_day <= ?", self.to_day.map(i64::from));
        clause
    }

    pub fn matches(&self, row: &UsageRow) -> bool {
        self.principal
            .as_ref()
            .is_none_or(|principal| *principal == row.key.principal)
            && self.from_day.is_none_or(|day| row.key.day >= day)
            && self.to_day.is_none_or(|day| row.key.day <= day)
    }
}

impl RequestLogFilter {
    pub fn to_where(&self) -> Where {
        let mut clause = Where::default();
        clause.text("principal = ?", &self.principal);
        clause.text("request_id = ?", &self.request_id);
        clause.int("status >= ?", self.min_status.map(i64::from));
        clause.int("ts >= ?", self.since_ms.map(sql_int));
        clause.int("ts < ?", self.until_ms.map(sql_int));
        clause.int("id < ?", self.before_id.map(sql_int));
        clause
    }

    pub fn matches(&self, row: &RequestLogRow) -> bool {
        self.principal
            .as_ref()
            .is_none_or(|principal| *principal == row.principal)
            && self
                .request_id
                .as_ref()
                .is_none_or(|id| *id == row.request_id)
            && self.min_status.is_none_or(|status| row.status >= status)
            && self.since_ms.is_none_or(|ms| row.ts_ms >= ms)
            && self.until_ms.is_none_or(|ms| row.ts_ms < ms)
            && self.before_id.is_none_or(|id| row.id < id)
    }
}

impl AuditFilter {
    pub fn to_where(&self) -> Where {
        let mut clause = Where::default();
        clause.text("resource_type = ?", &self.resource_type);
        clause.text("resource_id = ?", &self.resource_id);
        clause.text("patient = ?", &self.patient);
        clause.text("principal = ?", &self.principal);
        clause.text("user_id = ?", &self.user);
        clause.text("action = ?", &self.action);
        clause.text("request_id = ?", &self.request_id);
        match self.failed {
            Some(true) => clause.conditions.push("outcome <> 0"),
            Some(false) => clause.conditions.push("outcome = 0"),
            None => {}
        }
        clause.int("ts >= ?", self.since_ms.map(sql_int));
        clause.int("ts < ?", self.until_ms.map(sql_int));
        clause.int("id < ?", self.before_id.map(sql_int));
        clause
    }

    pub fn matches(&self, row: &AuditRow) -> bool {
        let same = |want: &Option<String>, have: &Option<String>| {
            want.as_ref().is_none_or(|want| Some(want) == have.as_ref())
        };
        same(&self.resource_type, &row.resource_type)
            && same(&self.resource_id, &row.resource_id)
            && same(&self.patient, &row.patient)
            && self
                .principal
                .as_ref()
                .is_none_or(|principal| *principal == row.principal)
            && same(&self.user, &row.user)
            && self
                .action
                .as_ref()
                .is_none_or(|action| *action == row.action)
            && self
                .request_id
                .as_ref()
                .is_none_or(|id| *id == row.request_id)
            && self
                .failed
                .is_none_or(|failed| failed == (row.outcome != 0))
            && self.since_ms.is_none_or(|ms| row.ts_ms >= ms)
            && self.until_ms.is_none_or(|ms| row.ts_ms < ms)
            && self.before_id.is_none_or(|id| row.id < id)
    }
}

/// Usage counters, the request log and the audit trail.
///
/// **Appending is the only way in.** There is no update, and the only delete is [`prune`], which
/// removes what is older than a retention period the operator chose. Usage counters are added
/// to, never set, so several servers sharing one database each add what they saw.
///
/// [`prune`]: GovernanceStore::prune
pub trait GovernanceStore: Send + Sync {
    /// Add these counters to the rows they name, creating rows that do not exist. One
    /// transaction, so the batch is all in or all out.
    fn usage_add(&self, rows: &[UsageRow]) -> Result<(), PersistError>;

    /// Rows matching the filter, newest day first, then by principal and class.
    fn usage_query(&self, filter: &UsageFilter) -> Result<Vec<UsageRow>, PersistError>;

    fn request_log_append(&self, rows: &[RequestLogRow]) -> Result<(), PersistError>;

    /// Newest first. Each row carries the id the store gave it.
    fn request_log_query(
        &self,
        filter: &RequestLogFilter,
    ) -> Result<Vec<RequestLogRow>, PersistError>;

    /// Append audit rows, each chained to the one before it (see [`audit_digest`]). The chain
    /// is extended inside one transaction that no other writer can interleave with, so that
    /// servers appending at once still make one chain and not several.
    fn audit_append(&self, rows: &[AuditRow], key: Option<&ChainKey>) -> Result<(), PersistError>;

    /// Newest first. Each row carries the id the store gave it.
    fn audit_query(&self, filter: &AuditFilter) -> Result<Vec<AuditRow>, PersistError>;

    /// Up to `limit` rows with an id above `after_id`, oldest first, with the hashes they were
    /// chained with. For [`verify_audit_chain`].
    fn audit_scan(&self, after_id: u64, limit: u32) -> Result<Vec<ChainedRow>, PersistError>;

    /// The id and hash of the newest chained row: what a checkpoint records.
    fn audit_head(&self) -> Result<Option<(u64, Vec<u8>)>, PersistError>;

    /// Say that server `owner` is alive, and learn how many servers sharing this database are:
    /// each one calls this every few seconds, and one that has not for `ttl_ms` is not counted.
    /// This is how a fleet divides a rate limit among itself without sharing a bucket, and it
    /// is called every ten seconds or so, never per request. The count includes the caller.
    ///
    /// A store with a clock every server reads alike, such as a database server's, uses it, so
    /// that servers whose own clocks disagree still agree on who is alive; `now_ms` is the
    /// caller's, for a store that has none.
    fn members_touch(&self, owner: &str, now_ms: u64, ttl_ms: u64) -> Result<u32, PersistError>;

    /// Take `owner` out of the count now, as a server that is stopping does, so that the others
    /// take its share without waiting for it to expire.
    fn members_leave(&self, owner: &str) -> Result<(), PersistError>;

    /// Delete request-log rows older than `request_log_before_ms` and audit rows older than
    /// `audit_before_ms`. `None` keeps everything of that kind.
    ///
    /// Audit rows go only from the start of the trail: those that are both older than the cutoff
    /// and before every row that is not. The servers' clocks differ a little, so rows near the
    /// cutoff are not quite in order of age, and deleting by age alone would take some and
    /// leave their neighbours, which a chain would show as a gap. This can keep a few rows
    /// longer than asked; it can never delete one sooner.
    fn prune(
        &self,
        request_log_before_ms: Option<u64>,
        audit_before_ms: Option<u64>,
    ) -> Result<Pruned, PersistError>;
}

fn clamp_limit(limit: u32) -> usize {
    limit.clamp(1, 10_000) as usize
}

/// [`GovernanceStore`] held in process memory. For tests and for a server run without a
/// database of its own: nothing in it survives the process.
#[derive(Default)]
pub struct MemoryGovernanceStore {
    inner: Mutex<MemoryRows>,
}

#[derive(Default)]
struct MemoryRows {
    usage: BTreeMap<UsageKey, UsageCounters>,
    requests: Vec<RequestLogRow>,
    audit: Vec<ChainedRow>,
    members: BTreeMap<String, u64>,
    next_id: u64,
}

impl MemoryGovernanceStore {
    /// Change a stored audit row without changing its hash, as someone with write access to a
    /// database could. For tests of tamper detection.
    #[doc(hidden)]
    pub fn edit_audit_row(&self, id: u64, edit: impl FnOnce(&mut AuditRow)) {
        let mut inner = self.inner.lock().expect("governance");
        if let Some(stored) = inner.audit.iter_mut().find(|stored| stored.row.id == id) {
            edit(&mut stored.row);
        }
    }

    /// Remove one stored audit row, wherever it is. For tests of tamper detection.
    #[doc(hidden)]
    pub fn remove_audit_row(&self, id: u64) {
        self.inner
            .lock()
            .expect("governance")
            .audit
            .retain(|stored| stored.row.id != id);
    }

    /// Remove every audit row after `id`. For tests of tamper detection.
    #[doc(hidden)]
    pub fn truncate_audit_after(&self, id: u64) {
        self.inner
            .lock()
            .expect("governance")
            .audit
            .retain(|stored| stored.row.id <= id);
    }
}

impl GovernanceStore for MemoryGovernanceStore {
    fn usage_add(&self, rows: &[UsageRow]) -> Result<(), PersistError> {
        let mut inner = self.inner.lock().expect("governance");
        for row in rows {
            inner
                .usage
                .entry(row.key.clone())
                .or_default()
                .add(&row.counters);
        }
        Ok(())
    }

    fn usage_query(&self, filter: &UsageFilter) -> Result<Vec<UsageRow>, PersistError> {
        let inner = self.inner.lock().expect("governance");
        let mut rows: Vec<UsageRow> = inner
            .usage
            .iter()
            .map(|(key, counters)| UsageRow {
                key: key.clone(),
                counters: *counters,
            })
            .filter(|row| filter.matches(row))
            .collect();
        rows.sort_by(|a, b| {
            (std::cmp::Reverse(a.key.day), &a.key.principal, &a.key.class).cmp(&(
                std::cmp::Reverse(b.key.day),
                &b.key.principal,
                &b.key.class,
            ))
        });
        rows.truncate(clamp_limit(filter.limit));
        Ok(rows)
    }

    fn request_log_append(&self, rows: &[RequestLogRow]) -> Result<(), PersistError> {
        let mut inner = self.inner.lock().expect("governance");
        for row in rows {
            inner.next_id += 1;
            let mut row = row.clone();
            row.id = inner.next_id;
            inner.requests.push(row);
        }
        Ok(())
    }

    fn request_log_query(
        &self,
        filter: &RequestLogFilter,
    ) -> Result<Vec<RequestLogRow>, PersistError> {
        let inner = self.inner.lock().expect("governance");
        Ok(inner
            .requests
            .iter()
            .rev()
            .filter(|row| filter.matches(row))
            .take(clamp_limit(filter.limit))
            .cloned()
            .collect())
    }

    fn audit_append(&self, rows: &[AuditRow], key: Option<&ChainKey>) -> Result<(), PersistError> {
        let mut inner = self.inner.lock().expect("governance");
        let mut previous = inner
            .audit
            .last()
            .and_then(|stored| stored.hash.clone())
            .unwrap_or_else(|| CHAIN_GENESIS.to_vec());
        for row in rows {
            inner.next_id += 1;
            let mut row = row.clone();
            row.id = inner.next_id;
            let hash = audit_digest(key, &previous, &row).to_vec();
            inner.audit.push(ChainedRow {
                row,
                prev_hash: Some(previous),
                hash: Some(hash.clone()),
            });
            previous = hash;
        }
        Ok(())
    }

    fn audit_query(&self, filter: &AuditFilter) -> Result<Vec<AuditRow>, PersistError> {
        let inner = self.inner.lock().expect("governance");
        Ok(inner
            .audit
            .iter()
            .rev()
            .map(|stored| &stored.row)
            .filter(|row| filter.matches(row))
            .take(clamp_limit(filter.limit))
            .cloned()
            .collect())
    }

    fn audit_scan(&self, after_id: u64, limit: u32) -> Result<Vec<ChainedRow>, PersistError> {
        let inner = self.inner.lock().expect("governance");
        Ok(inner
            .audit
            .iter()
            .filter(|stored| stored.row.id > after_id)
            .take(limit as usize)
            .cloned()
            .collect())
    }

    fn audit_head(&self) -> Result<Option<(u64, Vec<u8>)>, PersistError> {
        let inner = self.inner.lock().expect("governance");
        Ok(inner
            .audit
            .iter()
            .rev()
            .find_map(|stored| stored.hash.clone().map(|hash| (stored.row.id, hash))))
    }

    fn members_touch(&self, owner: &str, now_ms: u64, ttl_ms: u64) -> Result<u32, PersistError> {
        let mut inner = self.inner.lock().expect("governance");
        inner.members.insert(owner.to_string(), now_ms);
        inner
            .members
            .retain(|_, seen| seen.saturating_add(ttl_ms) >= now_ms);
        Ok(inner.members.len() as u32)
    }

    fn members_leave(&self, owner: &str) -> Result<(), PersistError> {
        self.inner.lock().expect("governance").members.remove(owner);
        Ok(())
    }

    fn prune(
        &self,
        request_log_before_ms: Option<u64>,
        audit_before_ms: Option<u64>,
    ) -> Result<Pruned, PersistError> {
        let mut inner = self.inner.lock().expect("governance");
        let mut pruned = Pruned::default();
        if let Some(before) = request_log_before_ms {
            let kept = inner.requests.len();
            inner.requests.retain(|row| row.ts_ms >= before);
            pruned.request_log = (kept - inner.requests.len()) as u64;
        }
        if let Some(before) = audit_before_ms {
            // From the start only, as far as the first row that is not expired.
            let first_live = inner
                .audit
                .iter()
                .position(|stored| stored.row.ts_ms >= before)
                .unwrap_or(inner.audit.len());
            inner.audit.drain(..first_live);
            pruned.audit = first_live as u64;
        }
        Ok(pruned)
    }
}

/// Shared checks every [`GovernanceStore`] backend must pass.
pub mod governance_checks {
    use super::*;
    use crate::{verify_audit_chain, ChainProblem, CheckpointResult};

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// Everything the trait promises. `tag` keeps this run's rows apart on a database that
    /// already holds others: every principal, request id and resource id here carries it, and
    /// the times are far in the past so that pruning them cannot touch real rows.
    pub fn exercise(store: &dyn GovernanceStore, tag: &str) {
        let principal = |name: &str| format!("client:{tag}-{name}");
        let class = |name: &str| name.to_string();

        // Usage: counters are added to, per (day, principal, class).
        let counters = |requests: u64, bytes_in: u64| UsageCounters {
            requests,
            client_errors: 1,
            server_errors: 2,
            throttled: 3,
            bytes_in,
            bytes_out: bytes_in * 2,
            duration_ms: 40,
        };
        let key = |day: u32, who: &str, what: &str| UsageKey {
            day,
            principal: principal(who),
            class: class(what),
        };
        store.usage_add(&[]).unwrap();
        store
            .usage_add(&[
                UsageRow {
                    key: key(10, "a", "read"),
                    counters: counters(5, 100),
                },
                UsageRow {
                    key: key(10, "a", "write"),
                    counters: counters(1, 10),
                },
                UsageRow {
                    key: key(11, "a", "read"),
                    counters: counters(2, 7),
                },
                UsageRow {
                    key: key(10, "b", "read"),
                    counters: counters(9, 1),
                },
            ])
            .unwrap();
        // The same keys again, in one batch and in the next: both add.
        store
            .usage_add(&[
                UsageRow {
                    key: key(10, "a", "read"),
                    counters: counters(1, 1),
                },
                UsageRow {
                    key: key(10, "a", "read"),
                    counters: counters(1, 1),
                },
            ])
            .unwrap();
        let mine = |who: &str, from: Option<u32>, to: Option<u32>| {
            store
                .usage_query(&UsageFilter {
                    principal: Some(principal(who)),
                    from_day: from,
                    to_day: to,
                    limit: 100,
                })
                .unwrap()
        };
        let all = mine("a", None, None);
        assert_eq!(all.len(), 3, "{all:?}");
        assert_eq!(
            all.iter()
                .map(|row| (row.key.day, row.key.class.as_str()))
                .collect::<Vec<_>>(),
            vec![(11, "read"), (10, "read"), (10, "write")],
            "newest day first, then by class"
        );
        let day10_read = all
            .iter()
            .find(|row| row.key == key(10, "a", "read"))
            .unwrap();
        assert_eq!(
            day10_read.counters,
            UsageCounters {
                requests: 7,
                client_errors: 3,
                server_errors: 6,
                throttled: 9,
                bytes_in: 102,
                bytes_out: 204,
                duration_ms: 120,
            },
            "three additions to one key"
        );
        assert_eq!(mine("a", Some(11), None).len(), 1);
        assert_eq!(mine("a", None, Some(10)).len(), 2);
        assert_eq!(mine("a", Some(10), Some(10)).len(), 2);
        assert!(mine("a", Some(12), None).is_empty());
        assert_eq!(mine("b", None, None).len(), 1);
        assert_eq!(
            store
                .usage_query(&UsageFilter {
                    principal: Some(principal("a")),
                    limit: 1,
                    ..Default::default()
                })
                .unwrap()
                .len(),
            1,
            "limit"
        );

        // Request log: appended, found newest first, filtered, and pruned by age.
        let request = |name: &str, who: &str, ts: u64, status: u16| RequestLogRow {
            id: 0,
            ts_ms: ts,
            request_id: format!("{tag}-{name}"),
            principal: principal(who),
            remote_ip: Some("203.0.113.7".into()),
            method: "GET".into(),
            path: "/Patient".into(),
            params: "name,_count".into(),
            class: "search".into(),
            status,
            duration_ms: 12,
            bytes_in: 0,
            bytes_out: 345,
        };
        store.request_log_append(&[]).unwrap();
        store
            .request_log_append(&[
                request("r1", "a", 1_000, 200),
                request("r2", "a", 2_000, 404),
                request("r3", "b", 3_000, 500),
                request("r4", "a", 4_000, 200),
            ])
            .unwrap();
        let requests = |filter: RequestLogFilter| store.request_log_query(&filter).unwrap();
        let base = |who: &str| RequestLogFilter {
            principal: Some(principal(who)),
            limit: 100,
            ..Default::default()
        };
        let rows = requests(base("a"));
        assert_eq!(
            rows.iter()
                .map(|row| row.request_id.clone())
                .collect::<Vec<_>>(),
            vec![
                format!("{tag}-r4"),
                format!("{tag}-r2"),
                format!("{tag}-r1")
            ],
            "newest first"
        );
        assert!(rows.iter().all(|row| row.id > 0));
        assert!(rows[0].id > rows[1].id && rows[1].id > rows[2].id);
        let first = &rows[2];
        assert_eq!(first.ts_ms, 1_000);
        assert_eq!(first.remote_ip.as_deref(), Some("203.0.113.7"));
        assert_eq!(
            (first.method.as_str(), first.path.as_str()),
            ("GET", "/Patient")
        );
        assert_eq!(
            (first.params.as_str(), first.class.as_str()),
            ("name,_count", "search")
        );
        assert_eq!((first.status, first.duration_ms), (200, 12));
        assert_eq!((first.bytes_in, first.bytes_out), (0, 345));
        assert_eq!(
            requests(RequestLogFilter {
                min_status: Some(400),
                ..base("a")
            })
            .len(),
            1
        );
        assert_eq!(
            requests(RequestLogFilter {
                since_ms: Some(2_000),
                until_ms: Some(4_000),
                ..base("a")
            })
            .len(),
            1,
            "since is inclusive, until is not"
        );
        assert_eq!(
            requests(RequestLogFilter {
                request_id: Some(format!("{tag}-r3")),
                ..base("b")
            })
            .len(),
            1
        );
        let page = requests(RequestLogFilter {
            limit: 1,
            ..base("a")
        });
        assert_eq!(page.len(), 1);
        let next = requests(RequestLogFilter {
            before_id: Some(page[0].id),
            limit: 1,
            ..base("a")
        });
        assert_eq!(
            next[0].request_id,
            format!("{tag}-r2"),
            "continues after the cursor"
        );

        // Audit: appended, filtered by whatever someone may ask, pruned only when told. Times are
        // ten days ago, in order, so that retention sees a trail as a real one is, and the table
        // is emptied first because retention takes the start of it: this is for a database made
        // for tests.
        let ago = now_ms() - 10 * MS_PER_DAY;
        store.prune(None, Some(now_ms() + MS_PER_DAY)).unwrap();
        let event = |name: &str, who: &str, ts: u64, what: (&str, u8)| AuditRow {
            id: 0,
            ts_ms: ts,
            request_id: format!("{tag}-{name}"),
            source: "http".into(),
            action: what.0.into(),
            subtype: "read".into(),
            outcome: what.1,
            status: if what.1 == 0 { 200 } else { 403 },
            principal: principal(who),
            client_id: Some(format!("{tag}-{who}")),
            user: Some(format!("user-{tag}")),
            fhir_user: Some("Practitioner/p1".into()),
            remote_ip: Some("198.51.100.2".into()),
            resource_type: Some("Observation".into()),
            resource_id: Some(format!("{tag}-o-{name}")),
            patient: Some(format!("{tag}-pat-{who}")),
            query: Some("code=8480-6".into()),
            detail: None,
        };
        store.audit_append(&[], None).unwrap();
        store
            .audit_append(
                &[
                    event("e1", "a", ago + 1_000, ("R", 0)),
                    event("e2", "a", ago + 2_000, ("U", 0)),
                    event("e3", "b", ago + 3_000, ("D", 4)),
                    AuditRow {
                        detail: Some("scope does not allow this request".into()),
                        resource_type: None,
                        resource_id: None,
                        patient: None,
                        query: None,
                        ..event("e4", "a", ago + 4_000, ("R", 4))
                    },
                ],
                None,
            )
            .unwrap();
        let audit = |filter: AuditFilter| store.audit_query(&filter).unwrap();
        let by = |who: &str| AuditFilter {
            principal: Some(principal(who)),
            limit: 100,
            ..Default::default()
        };
        let rows = audit(by("a"));
        assert_eq!(
            rows.iter()
                .map(|row| row.request_id.clone())
                .collect::<Vec<_>>(),
            vec![
                format!("{tag}-e4"),
                format!("{tag}-e2"),
                format!("{tag}-e1")
            ],
            "newest first"
        );
        let stored = &rows[2];
        assert_eq!(stored.ts_ms, ago + 1_000);
        assert_eq!(
            (stored.source.as_str(), stored.action.as_str()),
            ("http", "R")
        );
        assert_eq!(
            (stored.subtype.as_str(), stored.outcome, stored.status),
            ("read", 0, 200)
        );
        assert_eq!(stored.client_id, Some(format!("{tag}-a")));
        assert_eq!(stored.user, Some(format!("user-{tag}")));
        assert_eq!(stored.fhir_user.as_deref(), Some("Practitioner/p1"));
        assert_eq!(stored.remote_ip.as_deref(), Some("198.51.100.2"));
        assert_eq!(stored.resource_type.as_deref(), Some("Observation"));
        assert_eq!(stored.query.as_deref(), Some("code=8480-6"));
        assert_eq!(stored.detail, None);
        assert_eq!(
            rows[0].detail.as_deref(),
            Some("scope does not allow this request")
        );
        assert_eq!(rows[0].resource_type, None, "absent stays absent");
        assert_eq!(
            audit(AuditFilter {
                resource_id: Some(format!("{tag}-o-e2")),
                resource_type: Some("Observation".into()),
                ..by("a")
            })
            .len(),
            1
        );
        assert_eq!(
            audit(AuditFilter {
                patient: Some(format!("{tag}-pat-b")),
                limit: 100,
                ..Default::default()
            })
            .len(),
            1,
            "everything that touched one patient"
        );
        assert_eq!(
            audit(AuditFilter {
                action: Some("U".into()),
                ..by("a")
            })
            .len(),
            1
        );
        assert_eq!(
            audit(AuditFilter {
                failed: Some(true),
                ..by("a")
            })
            .len(),
            1
        );
        assert_eq!(
            audit(AuditFilter {
                failed: Some(false),
                ..by("a")
            })
            .len(),
            2
        );
        assert_eq!(
            audit(AuditFilter {
                since_ms: Some(ago + 2_000),
                until_ms: Some(ago + 4_000),
                ..by("a")
            })
            .len(),
            1
        );
        assert_eq!(
            audit(AuditFilter {
                user: Some(format!("user-{tag}")),
                ..by("a")
            })
            .len(),
            3
        );
        assert_eq!(
            audit(AuditFilter {
                request_id: Some(format!("{tag}-e3")),
                limit: 10,
                ..Default::default()
            })
            .len(),
            1
        );
        let page = audit(AuditFilter {
            limit: 2,
            ..by("a")
        });
        assert_eq!(page.len(), 2);
        let rest = audit(AuditFilter {
            before_id: Some(page[1].id),
            ..by("a")
        });
        assert_eq!(rest.len(), 1, "a cursor continues where the page stopped");

        // Pruning removes by age, only the kind asked for, and says how many.
        assert_eq!(store.prune(None, None).unwrap(), Pruned::default());
        let pruned = store.prune(Some(2_500), None).unwrap();
        assert_eq!(pruned.audit, 0, "audit is kept unless asked for");
        assert!(pruned.request_log >= 2, "{pruned:?}");
        assert_eq!(requests(base("a")).len(), 1, "only the newest is left");
        assert_eq!(audit(by("a")).len(), 3);
        let pruned = store.prune(None, Some(ago + 2_500)).unwrap();
        assert!(pruned.audit >= 2 && pruned.request_log == 0, "{pruned:?}");
        assert_eq!(audit(by("a")).len(), 1);
        // Leave nothing behind on a shared database.
        store
            .prune(Some(10_000), Some(now_ms() + MS_PER_DAY))
            .unwrap();
        assert!(audit(by("a")).is_empty() && requests(base("a")).is_empty());
    }

    /// The audit chain: written in batches, read back, verified, and checked against what the
    /// checks promise to catch. This empties the audit trail, so it is for a database made for
    /// tests, as the other conformance checks are.
    pub fn exercise_chain(store: &dyn GovernanceStore, tag: &str) {
        let key = ChainKey::new(format!("a key for the {tag} chain test").into_bytes());
        let clear = || {
            store.prune(None, Some(now_ms() + MS_PER_DAY)).unwrap();
        };
        let rows = |range: std::ops::Range<u32>| -> Vec<AuditRow> {
            range
                .map(|n| AuditRow {
                    ts_ms: now_ms(),
                    request_id: format!("{tag}-{n}"),
                    source: "http".into(),
                    action: "U".into(),
                    subtype: "update".into(),
                    principal: format!("client:{tag}"),
                    resource_type: Some("Patient".into()),
                    resource_id: Some(format!("p{n}")),
                    detail: if n % 2 == 0 {
                        None
                    } else {
                        Some(format!("d{n}"))
                    },
                    ..AuditRow::default()
                })
                .collect()
        };
        clear();
        // Nothing is intact and nothing is wrong.
        let empty = verify_audit_chain(store, Some(&key), None).unwrap();
        assert!(
            empty.intact() && empty.checked == 0 && empty.head.is_none(),
            "{empty:?}"
        );

        // Written in two batches, it is one chain: the second continues from the first.
        store.audit_append(&rows(0..3), Some(&key)).unwrap();
        store.audit_append(&rows(3..5), Some(&key)).unwrap();
        store.audit_append(&[], Some(&key)).unwrap();
        let report = verify_audit_chain(store, Some(&key), None).unwrap();
        assert!(report.intact(), "{report:?}");
        assert_eq!(
            (report.checked, report.unchained, report.breaks_total),
            (5, 0, 0)
        );
        let scanned = store.audit_scan(0, 100).unwrap();
        assert_eq!(scanned.len(), 5);
        assert_eq!(
            scanned[0].prev_hash.as_deref(),
            Some(&CHAIN_GENESIS[..]),
            "it starts at the start"
        );
        for pair in scanned.windows(2) {
            assert_eq!(
                pair[1].prev_hash, pair[0].hash,
                "each follows the one before"
            );
            assert!(pair[1].row.id > pair[0].row.id);
        }
        assert_eq!(
            scanned[3].row.detail.as_deref(),
            Some("d3"),
            "read back as it was written"
        );
        assert_eq!(scanned[2].row.detail, None);
        assert_eq!(
            (report.first_id, report.last_id),
            (Some(scanned[0].row.id), Some(scanned[4].row.id))
        );
        let head = report.head.clone().expect("a head");
        assert_eq!(head, (scanned[4].row.id, scanned[4].hash.clone().unwrap()));
        assert_eq!(store.audit_head().unwrap(), Some(head.clone()));
        // A page starts after the id given and is as long as asked.
        let page = store.audit_scan(scanned[1].row.id, 2).unwrap();
        assert_eq!(
            page.iter().map(|stored| stored.row.id).collect::<Vec<_>>(),
            vec![scanned[2].row.id, scanned[3].row.id]
        );

        // A key that is not the one the chain was made with, or none, finds every row wrong.
        let other = ChainKey::new(b"not the key it was made with".to_vec());
        let wrong = verify_audit_chain(store, Some(&other), None).unwrap();
        assert_eq!((wrong.breaks_total, wrong.intact()), (5, false));
        assert!(wrong
            .breaks
            .iter()
            .all(|found| found.problem == ChainProblem::Edited));
        assert_eq!(
            verify_audit_chain(store, None, None).unwrap().breaks_total,
            5
        );

        // A checkpoint is found, or it is said what is wrong with it.
        let want = (head.0, head.1.as_slice());
        assert_eq!(
            verify_audit_chain(store, Some(&key), Some(want))
                .unwrap()
                .checkpoint,
            Some(CheckpointResult::Matches)
        );
        assert_eq!(
            verify_audit_chain(store, Some(&key), Some((head.0, &[7; 32])))
                .unwrap()
                .checkpoint,
            Some(CheckpointResult::Different)
        );
        let beyond =
            verify_audit_chain(store, Some(&key), Some((head.0 + 1_000, &[7; 32]))).unwrap();
        assert_eq!(
            beyond.checkpoint,
            Some(CheckpointResult::Truncated),
            "the trail ends first"
        );
        assert!(!beyond.intact(), "though every link is sound");

        // Rows made later, with no key, are a chain too, but not one this key can check.
        store.audit_append(&rows(5..6), None).unwrap();
        let mixed = verify_audit_chain(store, Some(&key), None).unwrap();
        assert_eq!((mixed.checked, mixed.breaks_total), (6, 1));
        assert_eq!(mixed.breaks[0].id, store.audit_head().unwrap().unwrap().0);
        clear();
        assert!(store.audit_scan(0, 10).unwrap().is_empty());
    }

    /// Several writers appending at once, each through a connection of its own as separate
    /// servers would have: the result is one chain with every row in it.
    pub fn exercise_concurrent_chain(
        open: &(dyn Fn() -> std::sync::Arc<dyn GovernanceStore> + Sync),
        tag: &str,
    ) {
        const WRITERS: usize = 4;
        const BATCHES: usize = 12;
        const ROWS: usize = 5;
        let key = ChainKey::new(format!("a key for the {tag} concurrency test").into_bytes());
        open().prune(None, Some(now_ms() + MS_PER_DAY)).unwrap();
        std::thread::scope(|scope| {
            for writer in 0..WRITERS {
                let key = key.clone();
                scope.spawn(move || {
                    let store = open();
                    for batch in 0..BATCHES {
                        let rows: Vec<AuditRow> = (0..ROWS)
                            .map(|n| AuditRow {
                                ts_ms: now_ms(),
                                request_id: format!("{tag}-{writer}-{batch}-{n}"),
                                source: "http".into(),
                                action: "R".into(),
                                subtype: "read".into(),
                                principal: format!("client:{tag}"),
                                ..AuditRow::default()
                            })
                            .collect();
                        store.audit_append(&rows, Some(&key)).unwrap();
                    }
                });
            }
        });
        let store = open();
        let report = verify_audit_chain(store.as_ref(), Some(&key), None).unwrap();
        assert!(report.intact(), "{report:?}");
        assert_eq!(
            report.checked as usize,
            WRITERS * BATCHES * ROWS,
            "every row is in it"
        );
        store.prune(None, Some(now_ms() + MS_PER_DAY)).unwrap();
    }

    /// Servers counting one another. The time a member counts for is the caller's, so each
    /// property is checked with the time that makes it independent of how fast the machine is:
    /// a minute for "still counted", where no stall can expire anyone, and a short one with a
    /// longer sleep for "gone", where a stall only makes it more certain. A store with a clock
    /// of its own ignores the `now` passed in, and this holds for it too.
    pub fn exercise_members(store: &dyn GovernanceStore, tag: &str) {
        let now = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
        };
        let sleep = |ms: u64| std::thread::sleep(std::time::Duration::from_millis(ms));
        let (a, b, c, purge) = (
            format!("{tag}-a"),
            format!("{tag}-b"),
            format!("{tag}-c"),
            format!("{tag}-purge"),
        );
        let minute: u64 = 60_000;
        // Whatever an earlier run left behind is older than this, and is not counted by a caller
        // that counts only the last hundred milliseconds. (Only what is older goes: nothing
        // touched just now is lost.)
        sleep(150);
        store.members_touch(&purge, now(), 100).unwrap();
        store.members_leave(&purge).unwrap();

        assert_eq!(store.members_touch(&a, now(), minute).unwrap(), 1, "alone");
        assert_eq!(store.members_touch(&b, now(), minute).unwrap(), 2);
        assert_eq!(store.members_touch(&c, now(), minute).unwrap(), 3);
        assert_eq!(
            store.members_touch(&a, now(), minute).unwrap(),
            3,
            "touching again adds no one"
        );
        // One that says it is going is not counted at once, and saying so twice is no matter.
        store.members_leave(&c).unwrap();
        store.members_leave(&c).unwrap();
        assert_eq!(store.members_touch(&a, now(), minute).unwrap(), 2);
        // One that has gone quiet is counted for as long as the caller says a member counts for.
        assert_eq!(
            store.members_touch(&a, now(), minute).unwrap(),
            2,
            "b is quiet, not gone"
        );
        // And not after: b has not been heard from for more than a second.
        sleep(1_200);
        assert_eq!(
            store.members_touch(&a, now(), 1_000).unwrap(),
            1,
            "b has expired"
        );
        store.members_leave(&a).unwrap();
    }

    #[test]
    fn memory_store() {
        exercise(&MemoryGovernanceStore::default(), "memory");
        exercise_members(&MemoryGovernanceStore::default(), "memory");
        exercise_chain(&MemoryGovernanceStore::default(), "memory");
    }

    #[test]
    fn memory_store_concurrent_chain() {
        let store: std::sync::Arc<dyn GovernanceStore> =
            std::sync::Arc::new(MemoryGovernanceStore::default());
        exercise_concurrent_chain(&|| store.clone(), "memory");
    }

    #[test]
    fn a_trail_that_was_tampered_with_is_found_out() {
        let store = MemoryGovernanceStore::default();
        let key = ChainKey::new(b"a key of some length".to_vec());
        let rows: Vec<AuditRow> = (0..6)
            .map(|n| AuditRow {
                ts_ms: n,
                request_id: format!("r{n}"),
                source: "http".into(),
                action: "R".into(),
                subtype: "read".into(),
                principal: "client:a".into(),
                resource_id: Some(format!("p{n}")),
                ..AuditRow::default()
            })
            .collect();
        let fresh = || {
            let store = MemoryGovernanceStore::default();
            store.audit_append(&rows, Some(&key)).unwrap();
            store
        };
        let found = |store: &MemoryGovernanceStore| {
            let report = verify_audit_chain(store, Some(&key), None).unwrap();
            (
                report.breaks_total,
                report
                    .breaks
                    .iter()
                    .map(|b| (b.id, b.problem))
                    .collect::<Vec<_>>(),
            )
        };
        store.audit_append(&rows, Some(&key)).unwrap();
        assert_eq!(found(&store), (0, vec![]));

        // A row changed.
        let edited = fresh();
        edited.edit_audit_row(3, |row| row.resource_id = Some("someone-else".into()));
        assert_eq!(found(&edited), (1, vec![(3, ChainProblem::Edited)]));
        // A row removed from the middle: the one after it no longer follows.
        let removed = fresh();
        removed.remove_audit_row(3);
        assert_eq!(found(&removed), (1, vec![(4, ChainProblem::Detached)]));
        // A row removed from the start is retention, which the chain cannot tell from an attacker
        // and does not need to: what remains is a chain that starts later.
        let from_start = fresh();
        from_start.remove_audit_row(1);
        from_start.remove_audit_row(2);
        assert_eq!(found(&from_start), (0, vec![]));
        // A row removed from the end leaves a chain that is intact and shorter, which only a
        // checkpoint can show.
        let truncated = fresh();
        let head = truncated.audit_head().unwrap().unwrap();
        truncated.truncate_audit_after(4);
        assert_eq!(found(&truncated), (0, vec![]));
        let report = verify_audit_chain(&truncated, Some(&key), Some((head.0, &head.1))).unwrap();
        assert_eq!(report.checkpoint, Some(CheckpointResult::Truncated));
        assert!(!report.intact());
        // A row put in by hand has no hash, or one that is not right.
        let forged = fresh();
        forged.edit_audit_row(2, |row| row.status = 500);
        forged.edit_audit_row(5, |row| row.status = 500);
        assert_eq!(
            found(&forged),
            (
                2,
                vec![(2, ChainProblem::Edited), (5, ChainProblem::Edited)]
            )
        );
        // The list of breaks is bounded and the count is not.
        let wrecked = MemoryGovernanceStore::default();
        let many: Vec<AuditRow> = (0..60)
            .map(|n| AuditRow {
                ts_ms: n,
                request_id: n.to_string(),
                ..AuditRow::default()
            })
            .collect();
        wrecked.audit_append(&many, Some(&key)).unwrap();
        for id in 1..=60 {
            wrecked.edit_audit_row(id, |row| row.status = 1);
        }
        let report = verify_audit_chain(&wrecked, Some(&key), None).unwrap();
        assert_eq!(
            (report.breaks_total, report.breaks.len()),
            (60, crate::MAX_LISTED_BREAKS)
        );
    }

    #[test]
    fn retention_takes_from_the_start_only_so_that_it_cannot_leave_a_gap() {
        let store = MemoryGovernanceStore::default();
        let row = |n: u64, ts: u64| AuditRow {
            ts_ms: ts,
            request_id: n.to_string(),
            ..AuditRow::default()
        };
        // Two servers a little out of step: the third row is older than the second.
        store
            .audit_append(
                &[row(1, 100), row(2, 1_010), row(3, 990), row(4, 1_100)],
                None,
            )
            .unwrap();
        // Everything before 1,000 is expired, but only the first row is before a live one.
        let pruned = store.prune(None, Some(1_000)).unwrap();
        assert_eq!(pruned.audit, 1);
        let left: Vec<String> = store
            .audit_query(&AuditFilter {
                limit: 10,
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .rev()
            .map(|row| row.request_id)
            .collect();
        assert_eq!(left, ["2", "3", "4"], "no gap in the middle");
        assert_eq!(
            verify_audit_chain(&store, None, None).unwrap().breaks_total,
            0
        );
        // Once the cutoff passes them all, they all go.
        assert_eq!(store.prune(None, Some(5_000)).unwrap().audit, 3);
    }

    #[test]
    fn where_clause_numbers_placeholders_for_postgres() {
        let filter = AuditFilter {
            patient: Some("p".into()),
            failed: Some(true),
            since_ms: Some(5),
            limit: 1,
            ..Default::default()
        };
        let clause = filter.to_where();
        assert_eq!(
            clause.sql(false, 1),
            " WHERE patient = ? AND outcome <> 0 AND ts >= ?"
        );
        assert_eq!(
            clause.sql(true, 1),
            " WHERE patient = $1 AND outcome <> 0 AND ts >= $2"
        );
        assert_eq!(clause.params, vec![Param::Text("p".into()), Param::Int(5)]);
        assert_eq!(AuditFilter::default().to_where().sql(true, 1), "");
    }
}
