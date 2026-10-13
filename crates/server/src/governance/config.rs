//! What the operator can tune. Nothing here is read from a file: `refhir` builds it from
//! `refhir.toml` and the server only consumes it.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

use super::classify::Class;

/// The slowest refill a limit may have: a token every eleven days. Below this a limit is a
/// typo (an exponent, a misplaced decimal) and not a policy.
const MIN_RATE: f64 = 1e-6;

/// Rate limits, quotas, metering, request logs and the audit trail.
///
/// [`Default`] turns every feature off, so a server that embeds this crate and passes nothing
/// behaves as it always did. [`GovernanceConfig::observing`] is what the `refhir` binary starts
/// from: it records everything and refuses nothing, because a limit is a policy the operator
/// has to size for their own traffic.
#[derive(Clone, Debug)]
pub struct GovernanceConfig {
    /// Count what each caller uses, per day and per class of request.
    pub metering: bool,
    pub request_log: RequestLogConfig,
    pub audit: AuditConfig,
    pub rate_limit: RateLimitConfig,
    pub quota: QuotaConfig,
    pub proxy: ProxyConfig,
    /// How often buffered usage, request-log and audit rows are written. A crash loses at most
    /// this much; a clean shutdown loses nothing.
    pub flush_interval: Duration,
}

impl Default for GovernanceConfig {
    fn default() -> Self {
        Self {
            metering: false,
            request_log: RequestLogConfig::default(),
            audit: AuditConfig::default(),
            rate_limit: RateLimitConfig::default(),
            quota: QuotaConfig::default(),
            proxy: ProxyConfig::default(),
            flush_interval: Duration::from_secs(2),
        }
    }
}

impl GovernanceConfig {
    /// Record everything, limit nothing.
    pub fn observing() -> Self {
        Self {
            metering: true,
            request_log: RequestLogConfig {
                enabled: true,
                ..RequestLogConfig::default()
            },
            audit: AuditConfig {
                enabled: true,
                ..AuditConfig::default()
            },
            ..Self::default()
        }
    }

    /// Whether any feature is on. When none is, requests take the path they always did.
    pub fn active(&self) -> bool {
        self.metering
            || self.request_log.enabled
            || self.audit.enabled
            || self.rate_limit.enabled
            || self.quota.enabled
    }

    /// Check what can be checked before the server starts, so that a typo is a refused start and
    /// not a limit that silently does nothing. `auth` is whether SMART is on, which decides the
    /// default for callers with no token.
    pub fn validate(&self, auth: bool) -> Result<(), String> {
        if let Some(key) = &self.audit.chain_key {
            if key.len() < MIN_CHAIN_KEY_BYTES {
                return Err(format!(
                    "the audit chain key (REFHIR_AUDIT_KEY) is {} bytes; it must be at least \
                     {MIN_CHAIN_KEY_BYTES}, such as the output of `openssl rand -hex 32`",
                    key.len()
                ));
            }
        }
        for (what, value) in [
            (
                "[audit] unattributed_per_second",
                self.audit.unattributed_per_second,
            ),
            (
                "[request_log] refused_per_second",
                self.request_log.refused_per_second,
            ),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(format!("{what} must be 0 (keep every row) or more"));
            }
        }
        // Quotas and per-client rates are by client, and without SMART no request has one.
        // Refuse to start rather than run with a limit that never applies.
        if !auth {
            if self.quota.enabled && (self.quota.limits.any() || !self.quota.clients.is_empty()) {
                return Err(
                    "[quota] needs SMART (--smart): quotas are per client, and without it no \
                     request has a client, so no quota would ever apply. Use [rate_limit] to \
                     limit by address"
                        .into(),
                );
            }
            if self.rate_limit.enabled && !self.rate_limit.clients.is_empty() {
                return Err(
                    "[rate_limit.clients] needs SMART (--smart): a per-client rate is chosen by \
                     the client id in the token, and without SMART there is none"
                        .into(),
                );
            }
        }
        let rate = &self.rate_limit;
        if !rate.enabled {
            return Ok(());
        }
        let dearest = |classes: &[Class]| {
            classes
                .iter()
                .map(|class| rate.costs.of(*class))
                .max()
                .unwrap_or(0)
        };
        let check = |what: &str, per_second: f64, burst: u32, largest: u32| -> Result<(), String> {
            if !per_second.is_finite() || per_second < 0.0 {
                return Err(format!("{what}: requests_per_second must not be negative"));
            }
            if per_second > 0.0 && per_second < MIN_RATE {
                return Err(format!(
                    "{what}: requests_per_second ({per_second}) is below {MIN_RATE}, a token \
                     every eleven days. Use 0 to exempt the caller"
                ));
            }
            // Zero is how a caller is exempted, so it needs no burst.
            if per_second > 0.0 && burst < largest {
                return Err(format!(
                    "{what}: burst ({burst}) is smaller than the cost of the dearest request \
                     it can make ({largest}), which could then never be served. Raise burst or \
                     lower [rate_limit.cost]"
                ));
            }
            Ok(())
        };
        let everything = dearest(&Class::ALL);
        check(
            "[rate_limit]",
            rate.requests_per_second,
            rate.burst,
            everything,
        )?;
        // With SMART on, a caller with no token can only reach discovery and the OAuth
        // endpoints; without it, that caller is everyone.
        let (anonymous_rate, anonymous_burst) = rate.anonymous(auth);
        let anonymous_reach = if auth {
            dearest(&[Class::Discovery, Class::Auth])
        } else {
            everything
        };
        check(
            "[rate_limit] anonymous",
            anonymous_rate,
            anonymous_burst,
            anonymous_reach,
        )?;
        for (client, over) in &rate.clients {
            check(
                &format!("[rate_limit.clients.{client}]"),
                over.requests_per_second.unwrap_or(rate.requests_per_second),
                over.burst.unwrap_or(rate.burst),
                everything,
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RequestLogConfig {
    /// Log every request as one structured line with its request id and caller.
    pub enabled: bool,
    /// Also keep recent requests in the database, where `/_requests` reads them.
    pub store: bool,
    /// How long stored requests are kept.
    pub retention_hours: u32,
    /// How many rows a second may be kept for requests the server turned away (throttled) or
    /// that failed with no caller known. Past that they are counted, not stored: turning a
    /// request away must cost less than serving it, and a database write per refusal does not.
    /// `0` keeps every one.
    pub refused_per_second: f64,
}

impl Default for RequestLogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            store: true,
            retention_hours: 24,
            refused_per_second: 20.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AuditConfig {
    pub enabled: bool,
    /// Record reads and searches. Turn off to record only changes, refusals and administrative
    /// access, which is much less volume.
    pub reads: bool,
    /// Record the search parameters of a search. They can name a patient by identifier, so a
    /// deployment that cannot hold that in the audit trail turns this off.
    pub include_query: bool,
    /// How long audit rows are kept. `None` keeps them forever.
    pub retention_days: Option<u32>,
    /// How many rows a second may be kept for requests refused with no caller known (no token,
    /// a bad one). Past that they are counted, and one summary row per interval says how many,
    /// so the gap is itself on record. What an identified caller does is never sampled. `0`
    /// keeps every one.
    pub unattributed_per_second: f64,
    /// The secret that keys the hash chain over the audit trail. With it, someone who can write
    /// the database but does not have it cannot rewrite the trail without it showing; without
    /// it the chain only catches accidents and clumsy edits, since anyone who can write the
    /// table can recompute it. Keep it out of the database and out of the config file: the
    /// `refhir` binary reads it from `REFHIR_AUDIT_KEY`.
    pub chain_key: Option<refhir_persistence::ChainKey>,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            reads: true,
            include_query: true,
            retention_days: None,
            unattributed_per_second: 20.0,
            chain_key: None,
        }
    }
}

/// The shortest key the chain accepts: shorter is guessable.
pub const MIN_CHAIN_KEY_BYTES: usize = 16;

/// What each class of request costs a caller's token bucket. A cost of zero is never limited.
#[derive(Clone, Debug)]
pub struct Costs {
    pub read: u32,
    pub search: u32,
    pub write: u32,
    pub bundle: u32,
    pub operation: u32,
    pub export: u32,
    pub sync: u32,
    pub auth: u32,
    pub discovery: u32,
    pub admin: u32,
}

impl Default for Costs {
    fn default() -> Self {
        Self {
            read: 1,
            search: 2,
            write: 2,
            bundle: 5,
            operation: 5,
            export: 20,
            sync: 5,
            auth: 1,
            discovery: 1,
            admin: 1,
        }
    }
}

impl Costs {
    pub fn of(&self, class: Class) -> u32 {
        match class {
            Class::Read => self.read,
            Class::Search => self.search,
            Class::Write => self.write,
            Class::Bundle => self.bundle,
            Class::Operation => self.operation,
            Class::Export => self.export,
            Class::Sync => self.sync,
            Class::Auth => self.auth,
            Class::Discovery => self.discovery,
            Class::Admin => self.admin,
            // Not a request: the server re-running a live query is not the caller's to pay for.
            Class::Push => 0,
        }
    }
}

/// A caller's own rate, in place of the default. `requests_per_second = 0` exempts the caller.
#[derive(Clone, Debug, Default)]
pub struct ClientRate {
    pub requests_per_second: Option<f64>,
    pub burst: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct RateLimitConfig {
    pub enabled: bool,
    /// How fast a caller's bucket refills: the sustained rate, in cost units per second.
    pub requests_per_second: f64,
    /// How large the bucket is: the most a caller can spend at once.
    pub burst: u32,
    /// For a caller with no token, who is told apart by address. Unset: a tenth of the above
    /// where SMART is on (an anonymous caller there can only reach discovery and the OAuth
    /// endpoints), and the same as the above where it is off.
    pub anonymous_requests_per_second: Option<f64>,
    pub anonymous_burst: Option<u32>,
    /// How many requests an address may get a 401 for in a minute before its requests that
    /// carry a credential are turned away without the credential being looked up. This is
    /// what stands between a guessed token and the database.
    pub auth_failures_per_minute: u32,
    pub costs: Costs,
    /// Per client id (the `client_id` of the token).
    pub clients: BTreeMap<String, ClientRate>,
    /// `/sync` connections one caller may hold open at once. `0` is no limit.
    pub max_sync_connections: u32,
    /// Live queries one `/sync` connection may hold. Each one is re-run whenever a write
    /// overlaps it, so this bounds the work a caller can make the server do by itself.
    /// `0` is no limit.
    pub max_live_queries: u32,
    /// How many servers share this limit between them, each taking an equal part. Unset, a
    /// server counts the others that share its database, by a heartbeat every ten seconds that
    /// is nowhere near a request, and so follows a fleet as it grows and shrinks. Set it to
    /// pin the number, or to 1 where a balancer sends each caller to one server only.
    pub servers: Option<u32>,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            requests_per_second: 50.0,
            burst: 100,
            anonymous_requests_per_second: None,
            anonymous_burst: None,
            auth_failures_per_minute: 20,
            costs: Costs::default(),
            clients: BTreeMap::new(),
            max_sync_connections: 10,
            max_live_queries: 100,
            servers: None,
        }
    }
}

impl RateLimitConfig {
    /// The rate and bucket size for callers with no token.
    pub fn anonymous(&self, auth: bool) -> (f64, u32) {
        let (rate, burst) = if auth {
            (
                (self.requests_per_second / 10.0).max(1.0),
                (self.burst / 5).max(1),
            )
        } else {
            (self.requests_per_second, self.burst)
        };
        (
            self.anonymous_requests_per_second.unwrap_or(rate),
            self.anonymous_burst.unwrap_or(burst),
        )
    }

    /// A limit's part for one of `servers` servers sharing it. The rate divides exactly. The
    /// bucket divides too, but never below `dearest`, the cost of the dearest request a caller
    /// of this kind can make, which it could otherwise never afford.
    pub fn divided(&self, rate: f64, burst: u32, servers: u32, dearest: u32) -> (f64, u32) {
        if servers <= 1 {
            return (rate, burst);
        }
        (
            rate / f64::from(servers),
            burst.div_ceil(servers).max(dearest).max(1),
        )
    }

    /// The cost of the dearest of `classes`.
    pub fn dearest(&self, classes: &[Class]) -> u32 {
        classes
            .iter()
            .map(|class| self.costs.of(*class))
            .max()
            .unwrap_or(0)
    }

    /// The rate and bucket size for a client id. A rate of zero means the client is exempt.
    pub fn for_client(&self, client_id: &str) -> (f64, u32) {
        match self.clients.get(client_id) {
            Some(over) => (
                over.requests_per_second.unwrap_or(self.requests_per_second),
                over.burst.unwrap_or(self.burst),
            ),
            None => (self.requests_per_second, self.burst),
        }
    }
}

/// Totals a caller may not pass in a UTC day or month. Each is a count of requests that count
/// toward quotas (not discovery, the OAuth endpoints or the admin endpoints). `None` is no limit
/// and, in a per-client override, falls back to the default; `Some(0)` in an override lifts a
/// limit the default sets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuotaLimits {
    pub requests_per_day: Option<u64>,
    /// Creates, updates, patches, deletes and transactions.
    pub writes_per_day: Option<u64>,
    /// Bytes sent plus bytes received.
    pub bytes_per_day: Option<u64>,
    pub requests_per_month: Option<u64>,
    pub writes_per_month: Option<u64>,
    pub bytes_per_month: Option<u64>,
}

impl QuotaLimits {
    pub fn any(&self) -> bool {
        [
            self.requests_per_day,
            self.writes_per_day,
            self.bytes_per_day,
            self.requests_per_month,
            self.writes_per_month,
            self.bytes_per_month,
        ]
        .iter()
        .any(|limit| limit.is_some_and(|limit| limit > 0))
    }

    /// These limits where they are set, the defaults where they are not. A zero lifts a limit.
    pub fn over(&self, defaults: &QuotaLimits) -> QuotaLimits {
        let pick = |own: Option<u64>, base: Option<u64>| match own {
            Some(0) => None,
            Some(limit) => Some(limit),
            None => base.filter(|limit| *limit > 0),
        };
        QuotaLimits {
            requests_per_day: pick(self.requests_per_day, defaults.requests_per_day),
            writes_per_day: pick(self.writes_per_day, defaults.writes_per_day),
            bytes_per_day: pick(self.bytes_per_day, defaults.bytes_per_day),
            requests_per_month: pick(self.requests_per_month, defaults.requests_per_month),
            writes_per_month: pick(self.writes_per_month, defaults.writes_per_month),
            bytes_per_month: pick(self.bytes_per_month, defaults.bytes_per_month),
        }
    }
}

#[derive(Clone, Debug)]
pub struct QuotaConfig {
    pub enabled: bool,
    pub limits: QuotaLimits,
    /// Per client id.
    pub clients: BTreeMap<String, QuotaLimits>,
    /// How often a server rereads a caller's totals from the database, to see what the other
    /// servers sharing it have counted. Within this time a quota is enforced on what this
    /// server knows.
    pub refresh: Duration,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            limits: QuotaLimits::default(),
            clients: BTreeMap::new(),
            refresh: Duration::from_secs(30),
        }
    }
}

impl QuotaConfig {
    pub fn for_client(&self, client_id: &str) -> QuotaLimits {
        match self.clients.get(client_id) {
            Some(own) => own.over(&self.limits),
            None => self.limits.over(&QuotaLimits::default()),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProxyConfig {
    /// How many reverse proxies stand between clients and this server. `X-Forwarded-For` is read
    /// only when this is above zero, and then the entry that many from the right is the client:
    /// each proxy appends the address it saw, so the entries to the left of that one are
    /// whatever the client claimed. Zero reads the address of the connection itself.
    pub trusted_hops: u8,
    /// The networks the proxies connect from. When there are any, `X-Forwarded-For` is believed
    /// only on a connection from one of them: a client that reaches the server directly, past
    /// the proxies, could otherwise write the header itself and choose, or frame, an address.
    /// Empty believes it on every connection.
    pub trusted_peers: Vec<Cidr>,
}

/// A network, as `10.0.0.0/8` or `fd00::/8`; a bare address is that one host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    bits: u8,
}

impl Cidr {
    pub fn parse(text: &str) -> Result<Cidr, String> {
        let (address, bits) = match text.trim().split_once('/') {
            Some((address, bits)) => (address, Some(bits)),
            None => (text.trim(), None),
        };
        let addr: IpAddr = address.parse().map_err(|_| {
            format!("{text:?} is not an IP address or a network such as 10.0.0.0/8")
        })?;
        let longest = if addr.is_ipv4() { 32 } else { 128 };
        let bits = match bits {
            None => longest,
            Some(bits) => bits
                .parse::<u8>()
                .ok()
                .filter(|bits| *bits <= longest)
                .ok_or_else(|| format!("{text:?}: the prefix must be 0 to {longest}"))?,
        };
        Ok(Cidr { addr, bits })
    }

    /// Whether `ip` is in this network. An address of the other family never is.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                same_prefix(&net.octets(), &ip.octets(), self.bits)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                same_prefix(&net.octets(), &ip.octets(), self.bits)
            }
            _ => false,
        }
    }
}

fn same_prefix(a: &[u8], b: &[u8], bits: u8) -> bool {
    let whole = usize::from(bits / 8);
    let rest = bits % 8;
    a[..whole] == b[..whole] && (rest == 0 || (a[whole] ^ b[whole]) >> (8 - rest) == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_server_does_nothing_and_an_observing_one_refuses_nothing() {
        assert!(!GovernanceConfig::default().active());
        let observing = GovernanceConfig::observing();
        assert!(observing.active());
        assert!(!observing.rate_limit.enabled && !observing.quota.enabled);
        assert!(observing.audit.enabled && observing.metering && observing.request_log.enabled);
    }

    #[test]
    fn anonymous_limits_follow_whether_smart_is_on() {
        let rate = RateLimitConfig::default();
        assert_eq!(rate.anonymous(true), (5.0, 20));
        assert_eq!(rate.anonymous(false), (50.0, 100));
        let set = RateLimitConfig {
            anonymous_requests_per_second: Some(3.0),
            anonymous_burst: Some(30),
            ..RateLimitConfig::default()
        };
        assert_eq!(set.anonymous(true), (3.0, 30));
        assert_eq!(set.anonymous(false), (3.0, 30));
    }

    #[test]
    fn a_client_override_falls_back_field_by_field() {
        let mut rate = RateLimitConfig::default();
        rate.clients.insert(
            "batch".into(),
            ClientRate {
                requests_per_second: Some(500.0),
                burst: None,
            },
        );
        assert_eq!(rate.for_client("batch"), (500.0, 100));
        assert_eq!(rate.for_client("other"), (50.0, 100));
    }

    #[test]
    fn quota_overrides_fall_back_and_a_zero_lifts_the_default() {
        let config = QuotaConfig {
            enabled: true,
            limits: QuotaLimits {
                requests_per_day: Some(1_000),
                writes_per_day: Some(100),
                ..QuotaLimits::default()
            },
            clients: BTreeMap::from([(
                "big".to_string(),
                QuotaLimits {
                    requests_per_day: Some(50_000),
                    writes_per_day: Some(0),
                    bytes_per_month: Some(9),
                    ..QuotaLimits::default()
                },
            )]),
            refresh: Duration::from_secs(30),
        };
        let small = config.for_client("anyone");
        assert_eq!(
            (small.requests_per_day, small.writes_per_day),
            (Some(1_000), Some(100))
        );
        let big = config.for_client("big");
        assert_eq!(big.requests_per_day, Some(50_000));
        assert_eq!(big.writes_per_day, None, "lifted");
        assert_eq!(big.bytes_per_month, Some(9));
        assert!(!QuotaLimits::default().any());
        assert!(big.any());
        assert!(!QuotaLimits {
            requests_per_day: Some(0),
            ..Default::default()
        }
        .any());
    }

    #[test]
    fn a_limit_is_divided_among_the_servers_that_share_it() {
        let rate = RateLimitConfig::default();
        let everything = rate.dearest(&Class::ALL);
        assert_eq!(everything, 20, "the export");
        // One server keeps the whole of it; the rate always divides exactly.
        assert_eq!(rate.divided(50.0, 100, 1, everything), (50.0, 100));
        assert_eq!(rate.divided(50.0, 100, 4, everything), (12.5, 25));
        // A bucket never shrinks below what the dearest request costs.
        assert_eq!(rate.divided(50.0, 100, 10, everything), (5.0, 20));
        assert_eq!(rate.divided(50.0, 100, 100, everything), (0.5, 20));
        // Callers who can only make cheap requests may have a smaller one.
        let cheap = rate.dearest(&[Class::Discovery, Class::Auth]);
        assert_eq!(rate.divided(5.0, 20, 10, cheap), (0.5, 2));
        // An uneven bucket rounds up, so that no server is left with less than its part.
        assert_eq!(rate.divided(10.0, 7, 2, 1), (5.0, 4));
        // Zero servers is not a thing a count can be.
        assert_eq!(rate.divided(50.0, 100, 0, everything), (50.0, 100));
    }

    #[test]
    fn a_network_contains_the_addresses_that_share_its_prefix() {
        let net = |text: &str| Cidr::parse(text).unwrap();
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();
        assert!(net("10.0.0.0/8").contains(ip("10.255.0.1")));
        assert!(!net("10.0.0.0/8").contains(ip("11.0.0.1")));
        assert!(net("192.168.4.0/22").contains(ip("192.168.7.9")));
        assert!(!net("192.168.4.0/22").contains(ip("192.168.8.9")));
        assert!(net("0.0.0.0/0").contains(ip("203.0.113.9")));
        assert!(net("203.0.113.9").contains(ip("203.0.113.9")), "a host");
        assert!(!net("203.0.113.9").contains(ip("203.0.113.8")));
        assert!(net("fd00::/8").contains(ip("fd12:3456::1")));
        assert!(!net("fd00::/8").contains(ip("fe80::1")));
        assert!(
            !net("10.0.0.0/8").contains(ip("::ffff:10.0.0.1")),
            "another family"
        );
        for bad in ["", "ten", "10.0.0.0/33", "10.0.0.0/x", "::/129"] {
            assert!(Cidr::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_chain_key_too_short_to_be_secret_is_refused() {
        let mut config = GovernanceConfig::observing();
        assert!(config.validate(false).is_ok(), "none is allowed: unkeyed");
        config.audit.chain_key = Some(refhir_persistence::ChainKey::new(b"short".to_vec()));
        let err = config.validate(false).unwrap_err();
        assert!(
            err.contains("REFHIR_AUDIT_KEY") && err.contains("5 bytes"),
            "{err}"
        );
        config.audit.chain_key = Some(refhir_persistence::ChainKey::new(vec![b'k'; 16]));
        assert!(config.validate(false).is_ok());
    }

    #[test]
    fn a_negative_row_budget_is_refused() {
        let mut config = GovernanceConfig::observing();
        assert!(config.validate(false).is_ok());
        config.audit.unattributed_per_second = -1.0;
        assert!(config
            .validate(false)
            .unwrap_err()
            .contains("unattributed_per_second"));
        config.audit.unattributed_per_second = 0.0;
        config.request_log.refused_per_second = f64::NAN;
        assert!(config
            .validate(false)
            .unwrap_err()
            .contains("refused_per_second"));
    }

    #[test]
    fn a_limit_that_could_never_apply_is_refused_at_start() {
        let quota = |enabled| GovernanceConfig {
            quota: QuotaConfig {
                enabled,
                limits: QuotaLimits {
                    requests_per_day: Some(10),
                    ..QuotaLimits::default()
                },
                ..QuotaConfig::default()
            },
            ..GovernanceConfig::default()
        };
        let err = quota(true).validate(false).unwrap_err();
        assert!(err.contains("[quota] needs SMART"), "{err}");
        assert!(quota(true).validate(true).is_ok());
        assert!(
            quota(false).validate(false).is_ok(),
            "switched off is nothing to refuse"
        );
        let mut per_client = GovernanceConfig::default();
        per_client.rate_limit.enabled = true;
        per_client
            .rate_limit
            .clients
            .insert("a".into(), ClientRate::default());
        assert!(per_client
            .validate(false)
            .unwrap_err()
            .contains("[rate_limit.clients]"));
        assert!(per_client.validate(true).is_ok());
    }

    #[test]
    fn a_bucket_smaller_than_the_dearest_request_is_refused_at_start() {
        let mut config = GovernanceConfig::default();
        config.rate_limit.enabled = true;
        assert!(config.validate(false).is_ok());
        config.rate_limit.burst = 10;
        let err = config.validate(false).unwrap_err();
        assert!(err.contains("burst (10)") && err.contains("(20)"), "{err}");
        config.rate_limit.costs.export = 10;
        assert!(config.validate(false).is_ok());
        // An exempt client needs no bucket; one that is limited does.
        config.rate_limit.clients.insert(
            "trusted".into(),
            ClientRate {
                requests_per_second: Some(0.0),
                burst: Some(1),
            },
        );
        assert!(config.validate(true).is_ok());
        config.rate_limit.clients.insert(
            "tiny".into(),
            ClientRate {
                requests_per_second: Some(5.0),
                burst: Some(1),
            },
        );
        assert!(config.validate(true).unwrap_err().contains("tiny"));
        config.rate_limit.clients.remove("tiny");
        config.rate_limit.requests_per_second = -1.0;
        assert!(config.validate(true).is_err());
        config.rate_limit.requests_per_second = 1e-20;
        assert!(config.validate(true).unwrap_err().contains("below"));
        // Off means unchecked.
        config.rate_limit.enabled = false;
        assert!(config.validate(true).is_ok());
    }

    #[test]
    fn the_anonymous_bucket_is_checked_against_what_an_anonymous_caller_can_reach() {
        let mut config = GovernanceConfig::default();
        config.rate_limit.enabled = true;
        config.rate_limit.burst = 20;
        // With SMART on the default anonymous bucket is a fifth of this, and a caller with no
        // token can only make cheap requests: that is fine.
        config.rate_limit.costs.export = 20;
        assert_eq!(config.rate_limit.anonymous(true).1, 4);
        assert!(config.validate(true).is_ok());
        // Without SMART, the anonymous caller is everyone, and the export does not fit.
        config.rate_limit.anonymous_burst = Some(4);
        let err = config.validate(false).unwrap_err();
        assert!(err.contains("anonymous") && err.contains("(4)"), "{err}");
        // And a cheap-only bucket that is still too small for discovery is refused too.
        config.rate_limit.anonymous_burst = Some(0);
        assert!(config.validate(true).unwrap_err().contains("anonymous"));
    }
}
