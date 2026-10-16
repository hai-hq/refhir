//! Optional `refhir.toml` — CLI flags override file values when both are set.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use refhir_server::{ClientRate, GovernanceConfig, QuotaLimits};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FileConfig {
    #[serde(default)]
    pub fhir: FhirSection,
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub database: DatabaseSection,
    #[serde(default)]
    pub conformance: ConformanceSection,
    #[serde(default)]
    pub smart: SmartSection,
    #[serde(default)]
    pub export: ExportSection,
    #[serde(default)]
    pub terminology: TerminologySection,
    #[serde(default)]
    pub features: FeaturesSection,
    /// Present means on: a section with numbers in it that is not switched on would leave an
    /// operator believing they are protected.
    pub rate_limit: Option<RateLimitSection>,
    pub quota: Option<QuotaSection>,
    pub audit: Option<AuditSection>,
    pub request_log: Option<RequestLogSection>,
    pub metering: Option<MeteringSection>,
    /// Sections nothing here reads. A misspelt `[rate_limt]` would otherwise leave the server
    /// without the limit its operator thinks it has, so they are named at start-up.
    #[serde(flatten)]
    pub unknown: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub cluster: ClusterSection,
}

/// Running several processes against one database. See `Role`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClusterSection {
    /// `primary` (default), `standby`, or `replica`.
    pub role: Option<String>,
    /// Where a replica sends writes and reads that need a newer snapshot than it has.
    pub primary_url: Option<String>,
    /// Most Postgres connections to open (default 8). On SQLite, the read connections opened
    /// next to the one writer (default 4).
    pub db_pool: Option<usize>,
}

/// What this process does in a deployment with more than one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The one process that writes. Refuses to start while another primary holds the lease.
    Primary,
    /// Waits for the lease and becomes the primary when the current one is gone.
    Standby,
    /// Serves reads from the database the primary writes; sends everything else to it.
    Replica,
}

impl Role {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "primary" => Ok(Role::Primary),
            "standby" => Ok(Role::Standby),
            "replica" => Ok(Role::Replica),
            other => Err(format!(
                "unknown role `{other}`: use primary, standby or replica"
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Role::Primary => "primary",
            Role::Standby => "standby",
            Role::Replica => "replica",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerSection {
    pub bind: Option<String>,
    pub public_url: Option<String>,
    /// Reverse proxies in front of this server. See `[server] trusted_proxy_hops` in
    /// `refhir.toml.example`.
    pub trusted_proxy_hops: Option<u8>,
    /// The networks those proxies connect from, such as `["10.0.0.0/8"]`. See
    /// `[server] trusted_proxy_peers` in `refhir.toml.example`.
    pub trusted_proxy_peers: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DatabaseSection {
    /// `sqlite` (default), `memory`, or `postgres`.
    pub kind: Option<String>,
    pub sqlite_path: Option<String>,
    pub postgres_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FhirSection {
    /// Bundled core for this release (`r4` today). Future: `r5`.
    pub version: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConformanceSection {
    /// Optional implementation guides for `[fhir].version`, installed after release defaults.
    pub packages: Option<Vec<String>>,
    pub reference_policy: Option<String>,
    /// Let `PUT [type]/[id]` create a resource under an id the client names. Off by default: the
    /// server assigns ids and a PUT to an unknown id is a 405.
    pub update_as_create: Option<bool>,
    pub package_registry: Option<String>,
    pub package_cache: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SmartSection {
    pub enabled: Option<bool>,
    pub patient: Option<String>,
    pub encounter: Option<String>,
    pub fhir_user: Option<String>,
    pub dcr_token: Option<String>,
    pub confidential_secret: Option<String>,
    /// Accept any username with no credential check. Testing only.
    pub demo_login: Option<bool>,
    /// Sign users in through an OpenID Connect provider.
    pub oidc: Option<OidcSection>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct OidcSection {
    pub issuer: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub fhir_user_claim: Option<String>,
    pub redirect_uri: Option<String>,
    /// Only let users whose ID token carries one of these values for each named claim sign in,
    /// e.g. `allowed_claims = { organization_id = ["org_..."] }`.
    pub allowed_claims: Option<std::collections::BTreeMap<String, Vec<String>>>,
    /// Only let users with a verified email in one of these domains sign in.
    pub allowed_email_domains: Option<Vec<String>>,
    /// Bind a person to a FHIR resource by search when the ID token has no `fhir_user_claim`.
    pub fhir_user_lookup: Option<FhirUserLookupSection>,
    /// Use the signed-in user's own `Patient` as the launch patient instead of the picker.
    pub patient_from_fhir_user: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FhirUserLookupSection {
    /// `Practitioner` (default), `Patient`, `RelatedPerson` or `Person`.
    pub resource_type: Option<String>,
    /// ID token claim to search for: `sub` (default), `email`, or any other claim.
    pub claim: Option<String>,
    /// Search parameter to match it against. Default `identifier`.
    pub search_param: Option<String>,
    /// Identifier system for `system|value`.
    pub system: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ExportSection {
    pub dir: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TerminologySection {
    pub urls: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FeaturesSection {
    pub trace_engine: Option<bool>,
    pub sdc_archiver: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RateLimitSection {
    pub enabled: Option<bool>,
    pub requests_per_second: Option<f64>,
    pub burst: Option<u32>,
    pub anonymous_requests_per_second: Option<f64>,
    pub anonymous_burst: Option<u32>,
    pub auth_failures_per_minute: Option<u32>,
    pub max_sync_connections: Option<u32>,
    pub max_live_queries: Option<u32>,
    pub servers: Option<u32>,
    pub cost: Option<CostSection>,
    #[serde(default)]
    pub clients: BTreeMap<String, ClientRateSection>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CostSection {
    pub read: Option<u32>,
    pub search: Option<u32>,
    pub write: Option<u32>,
    pub bundle: Option<u32>,
    pub operation: Option<u32>,
    pub export: Option<u32>,
    pub sync: Option<u32>,
    pub auth: Option<u32>,
    pub discovery: Option<u32>,
    pub admin: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ClientRateSection {
    pub requests_per_second: Option<f64>,
    pub burst: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct QuotaLimitsSection {
    pub requests_per_day: Option<u64>,
    pub writes_per_day: Option<u64>,
    pub bytes_per_day: Option<u64>,
    pub requests_per_month: Option<u64>,
    pub writes_per_month: Option<u64>,
    pub bytes_per_month: Option<u64>,
}

impl From<&QuotaLimitsSection> for QuotaLimits {
    fn from(section: &QuotaLimitsSection) -> Self {
        QuotaLimits {
            requests_per_day: section.requests_per_day,
            writes_per_day: section.writes_per_day,
            bytes_per_day: section.bytes_per_day,
            requests_per_month: section.requests_per_month,
            writes_per_month: section.writes_per_month,
            bytes_per_month: section.bytes_per_month,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct QuotaSection {
    pub enabled: Option<bool>,
    pub requests_per_day: Option<u64>,
    pub writes_per_day: Option<u64>,
    pub bytes_per_day: Option<u64>,
    pub requests_per_month: Option<u64>,
    pub writes_per_month: Option<u64>,
    pub bytes_per_month: Option<u64>,
    /// How often the totals are reread from the database, to see other servers' counts.
    pub refresh_seconds: Option<u64>,
    #[serde(default)]
    pub clients: BTreeMap<String, QuotaLimitsSection>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AuditSection {
    pub enabled: Option<bool>,
    pub reads: Option<bool>,
    pub include_query: Option<bool>,
    pub retention_days: Option<u32>,
    pub unattributed_per_second: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RequestLogSection {
    pub enabled: Option<bool>,
    pub store: Option<bool>,
    pub retention_hours: Option<u32>,
    pub refused_per_second: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MeteringSection {
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FhirRelease {
    R4,
}

/// FHIR release defaults plus optional `[conformance].packages` / `--package` IGs.
#[derive(Debug, Clone)]
pub enum PackagePlan {
    Bundled(FhirRelease, Vec<String>),
}

/// Resolved settings used to start the server (file + env + CLI).
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub sqlite: Option<String>,
    pub memory: bool,
    pub postgres: Option<String>,
    pub packages: PackagePlan,
    pub bind: String,
    pub references: String,
    pub update_as_create: bool,
    pub auth: bool,
    pub public_url: Option<String>,
    pub smart_patient: Option<String>,
    pub smart_encounter: Option<String>,
    pub smart_fhir_user: Option<String>,
    pub smart_dcr_token: Option<String>,
    pub smart_confidential_secret: Option<String>,
    pub smart_demo_login: bool,
    pub oidc: Option<refhir_server::OidcConfig>,
    pub export_dir: String,
    pub terminology_urls: Vec<String>,
    pub trace_engine: bool,
    pub sdc_archiver: bool,
    /// Observing by default: usage, the request log and the audit trail are on, and nothing is
    /// limited until an operator sizes a limit for their traffic.
    pub governance: GovernanceConfig,
    pub role: Role,
    pub primary_url: Option<String>,
    /// Database connections to open at most; `None` means the default.
    pub db_pool: Option<usize>,
}

impl ResolvedConfig {
    pub fn defaults() -> Self {
        Self {
            sqlite: None,
            memory: false,
            postgres: None,
            packages: PackagePlan::Bundled(FhirRelease::R4, Vec::new()),
            bind: "127.0.0.1:8080".to_string(),
            references: "strict".to_string(),
            update_as_create: false,
            auth: false,
            public_url: None,
            smart_patient: None,
            smart_encounter: None,
            smart_fhir_user: None,
            smart_dcr_token: None,
            smart_confidential_secret: None,
            smart_demo_login: false,
            oidc: None,
            export_dir: "export".to_string(),
            terminology_urls: Vec::new(),
            trace_engine: false,
            sdc_archiver: false,
            governance: GovernanceConfig::observing(),
            role: Role::Primary,
            primary_url: None,
            db_pool: None,
        }
    }

    pub fn apply_file(&mut self, file: &FileConfig) -> Result<(), String> {
        if let Some(role) = &file.cluster.role {
            self.role = Role::parse(role)?;
        }
        if file.cluster.primary_url.is_some() {
            self.primary_url = file.cluster.primary_url.clone();
        }
        if let Some(size) = file.cluster.db_pool {
            if size == 0 {
                return Err("cluster.db_pool must be a positive number".into());
            }
            self.db_pool = Some(size);
        }
        if let Some(v) = &file.server.bind {
            self.bind = v.clone();
        }
        if file.server.public_url.is_some() {
            self.public_url = file.server.public_url.clone();
        }

        match file.database.kind.as_deref() {
            Some("memory") => {
                self.memory = true;
                self.sqlite = None;
                self.postgres = None;
            }
            Some("postgres") => {
                if let Some(url) = &file.database.postgres_url {
                    self.postgres = Some(url.clone());
                    self.memory = false;
                    self.sqlite = None;
                } else if env_value("REFHIR_POSTGRES_URL").is_none() {
                    // Falling back to a local SQLite file would put clinical data on the wrong disk.
                    return Err(
                        "[database] kind = \"postgres\" needs postgres_url or REFHIR_POSTGRES_URL"
                            .into(),
                    );
                }
            }
            Some("sqlite") | None => {
                if let Some(path) = &file.database.sqlite_path {
                    self.sqlite = Some(path.clone());
                    self.memory = false;
                    self.postgres = None;
                }
            }
            Some(other) => {
                return Err(format!(
                    "unknown [database] kind `{other}`: use sqlite, memory or postgres"
                ));
            }
        }
        if file.database.postgres_url.is_some()
            && file.database.kind.as_deref() != Some("sqlite")
            && (file.database.kind.as_deref() == Some("postgres") || file.database.kind.is_none())
        {
            self.postgres = file.database.postgres_url.clone();
            self.memory = false;
            self.sqlite = None;
        }

        self.packages = package_plan_from_file(&file.fhir, &file.conformance)?;

        if let Some(v) = &file.conformance.reference_policy {
            self.references = v.clone();
        }
        if let Some(v) = file.conformance.update_as_create {
            self.update_as_create = v;
        }
        // The environment wins over the file (file < env < CLI), so only fill gaps.
        if let Some(v) = &file.conformance.package_registry {
            if env_value("REFHIR_PACKAGE_REGISTRY").is_none() {
                std::env::set_var("REFHIR_PACKAGE_REGISTRY", v);
            }
        }
        if let Some(v) = &file.conformance.package_cache {
            if env_value("REFHIR_PACKAGE_CACHE").is_none() {
                std::env::set_var("REFHIR_PACKAGE_CACHE", v);
            }
        }

        if let Some(v) = file.smart.enabled {
            self.auth = v;
        }
        if file.smart.patient.is_some() {
            self.smart_patient = file.smart.patient.clone();
        }
        if file.smart.encounter.is_some() {
            self.smart_encounter = file.smart.encounter.clone();
        }
        if file.smart.fhir_user.is_some() {
            self.smart_fhir_user = file.smart.fhir_user.clone();
        }
        if file.smart.dcr_token.is_some() {
            self.smart_dcr_token = file.smart.dcr_token.clone();
        }
        if file.smart.confidential_secret.is_some() {
            self.smart_confidential_secret = file.smart.confidential_secret.clone();
        }
        if let Some(v) = file.smart.demo_login {
            self.smart_demo_login = v;
        }
        if let Some(section) = &file.smart.oidc {
            let oidc = self
                .oidc
                .get_or_insert_with(|| refhir_server::OidcConfig::new("", ""));
            if let Some(v) = &section.issuer {
                oidc.issuer = v.clone();
            }
            if let Some(v) = &section.client_id {
                oidc.client_id = v.clone();
            }
            if section.client_secret.is_some() {
                oidc.client_secret = section.client_secret.clone();
            }
            if let Some(v) = &section.scopes {
                oidc.scopes = v.clone();
            }
            if let Some(v) = &section.fhir_user_claim {
                oidc.fhir_user_claim = v.clone();
            }
            if section.redirect_uri.is_some() {
                oidc.redirect_uri = section.redirect_uri.clone();
            }
            if let Some(v) = &section.allowed_claims {
                oidc.allowed_claims = v.clone();
            }
            if let Some(v) = &section.allowed_email_domains {
                oidc.allowed_email_domains = v.clone();
            }
            if let Some(v) = section.patient_from_fhir_user {
                oidc.patient_from_fhir_user = v;
            }
            if let Some(lookup) = &section.fhir_user_lookup {
                let mut found = refhir_server::FhirUserLookup::new(
                    lookup.resource_type.as_deref().unwrap_or("Practitioner"),
                );
                if let Some(v) = &lookup.claim {
                    found.claim = v.clone();
                }
                if let Some(v) = &lookup.search_param {
                    found.search_param = v.clone();
                }
                found.system = lookup.system.clone();
                oidc.fhir_user_lookup = Some(found);
            }
        }
        if let Some(v) = &file.export.dir {
            self.export_dir = v.clone();
        }
        if let Some(urls) = &file.terminology.urls {
            self.terminology_urls = urls.clone();
        }
        if let Some(v) = file.features.trace_engine {
            self.trace_engine = v;
        }
        if let Some(v) = file.features.sdc_archiver {
            self.sdc_archiver = v;
        }
        self.apply_governance(file)
    }

    fn apply_governance(&mut self, file: &FileConfig) -> Result<(), String> {
        let governance = &mut self.governance;
        if let Some(hops) = file.server.trusted_proxy_hops {
            governance.proxy.trusted_hops = hops;
        }
        if let Some(peers) = &file.server.trusted_proxy_peers {
            governance.proxy.trusted_peers = peers
                .iter()
                .map(|text| {
                    refhir_server::Cidr::parse(text)
                        .map_err(|err| format!("[server] trusted_proxy_peers: {err}"))
                })
                .collect::<Result<_, _>>()?;
        }
        if let Some(section) = &file.metering {
            if let Some(v) = section.enabled {
                governance.metering = v;
            }
        }
        if let Some(section) = &file.request_log {
            let log = &mut governance.request_log;
            log.enabled = section.enabled.unwrap_or(true);
            if let Some(v) = section.store {
                log.store = v;
            }
            if let Some(v) = section.retention_hours {
                log.retention_hours = v;
            }
            if let Some(v) = section.refused_per_second {
                log.refused_per_second = v;
            }
        }
        if let Some(section) = &file.audit {
            let audit = &mut governance.audit;
            audit.enabled = section.enabled.unwrap_or(true);
            if let Some(v) = section.reads {
                audit.reads = v;
            }
            if let Some(v) = section.include_query {
                audit.include_query = v;
            }
            if section.retention_days.is_some() {
                audit.retention_days = section.retention_days;
            }
            if let Some(v) = section.unattributed_per_second {
                audit.unattributed_per_second = v;
            }
        }
        if let Some(section) = &file.rate_limit {
            let rate = &mut governance.rate_limit;
            rate.enabled = section.enabled.unwrap_or(true);
            if let Some(v) = section.requests_per_second {
                rate.requests_per_second = v;
            }
            if let Some(v) = section.burst {
                rate.burst = v;
            }
            if section.anonymous_requests_per_second.is_some() {
                rate.anonymous_requests_per_second = section.anonymous_requests_per_second;
            }
            if section.anonymous_burst.is_some() {
                rate.anonymous_burst = section.anonymous_burst;
            }
            if let Some(v) = section.auth_failures_per_minute {
                rate.auth_failures_per_minute = v;
            }
            if let Some(v) = section.max_sync_connections {
                rate.max_sync_connections = v;
            }
            if let Some(v) = section.max_live_queries {
                rate.max_live_queries = v;
            }
            if section.servers.is_some() {
                rate.servers = section.servers;
            }
            if let Some(cost) = &section.cost {
                let costs = &mut rate.costs;
                for (slot, value) in [
                    (&mut costs.read, cost.read),
                    (&mut costs.search, cost.search),
                    (&mut costs.write, cost.write),
                    (&mut costs.bundle, cost.bundle),
                    (&mut costs.operation, cost.operation),
                    (&mut costs.export, cost.export),
                    (&mut costs.sync, cost.sync),
                    (&mut costs.auth, cost.auth),
                    (&mut costs.discovery, cost.discovery),
                    (&mut costs.admin, cost.admin),
                ] {
                    if let Some(value) = value {
                        *slot = value;
                    }
                }
            }
            for (client, over) in &section.clients {
                rate.clients.insert(
                    client.clone(),
                    ClientRate {
                        requests_per_second: over.requests_per_second,
                        burst: over.burst,
                    },
                );
            }
        }
        if let Some(section) = &file.quota {
            let quota = &mut governance.quota;
            quota.enabled = section.enabled.unwrap_or(true);
            quota.limits = QuotaLimits {
                requests_per_day: section.requests_per_day,
                writes_per_day: section.writes_per_day,
                bytes_per_day: section.bytes_per_day,
                requests_per_month: section.requests_per_month,
                writes_per_month: section.writes_per_month,
                bytes_per_month: section.bytes_per_month,
            };
            if let Some(seconds) = section.refresh_seconds {
                quota.refresh = Duration::from_secs(seconds.max(1));
            }
            for (client, limits) in &section.clients {
                quota.clients.insert(client.clone(), limits.into());
            }
        }
        Ok(())
    }

    /// A one-line account of what is on, for the log at start-up.
    pub fn describe_governance(&self) -> String {
        let governance = &self.governance;
        let mut parts = Vec::new();
        parts.push(
            if governance.metering {
                "metering on"
            } else {
                "metering off"
            }
            .to_string(),
        );
        parts.push(if governance.audit.enabled {
            format!(
                "audit on ({}{}{})",
                if governance.audit.reads {
                    "reads and changes"
                } else {
                    "changes only"
                },
                match governance.audit.retention_days {
                    Some(days) => format!(", kept {days} days"),
                    None => ", kept forever".to_string(),
                },
                if governance.audit.chain_key.is_some() {
                    ", chained with a key"
                } else {
                    ", chained with no key"
                }
            )
        } else {
            "audit OFF".to_string()
        });
        parts.push(if governance.request_log.enabled {
            if governance.request_log.store {
                format!(
                    "request log on (kept {}h)",
                    governance.request_log.retention_hours
                )
            } else {
                "request log on (log lines only)".to_string()
            }
        } else {
            "request log off".to_string()
        });
        let rate = &governance.rate_limit;
        parts.push(if rate.enabled {
            format!(
                "rate limit {}/s burst {}",
                rate.requests_per_second, rate.burst
            )
        } else {
            "rate limit off".to_string()
        });
        parts.push(if governance.quota.enabled {
            "quotas on".to_string()
        } else {
            "quotas off".to_string()
        });
        parts.join(", ")
    }
}

pub fn parse_fhir_release(version: &str) -> Result<FhirRelease, String> {
    match version.trim().to_ascii_lowercase().as_str() {
        "r4" | "4.0.1" | "4.0" => Ok(FhirRelease::R4),
        other => Err(format!(
            "unsupported [fhir].version `{other}` (only r4 is supported in this build)"
        )),
    }
}

pub fn resolve_fhir_release(fhir: &FhirSection) -> Result<FhirRelease, String> {
    parse_fhir_release(fhir.version.as_deref().unwrap_or("r4"))
}

/// `fhirVersion` recorded in the conformance store for packages in this release.
pub fn fhir_version_for_release(release: FhirRelease) -> &'static str {
    match release {
        FhirRelease::R4 => "4.0.1",
    }
}

pub fn release_name(release: FhirRelease) -> &'static str {
    match release {
        FhirRelease::R4 => "r4",
    }
}

pub fn release_from_plan(plan: &PackagePlan) -> FhirRelease {
    let PackagePlan::Bundled(release, _) = plan;
    *release
}

/// Refuse starting with a different `[fhir].version` than the database was created with.
pub fn check_release_matches_database(
    release: FhirRelease,
    store: &dyn refhir_persistence::ConformanceStore,
) -> Result<(), String> {
    check_stored_release(stored_fhir_version(store)?.as_deref(), release)
}

fn stored_fhir_version(
    store: &dyn refhir_persistence::ConformanceStore,
) -> Result<Option<String>, String> {
    let packages = store.load_packages().map_err(|err| err.to_string())?;
    canonical_stored_fhir_version(
        packages
            .iter()
            .map(|package| package.fhir_version.as_str())
            .collect(),
    )
}

/// One `fhir_version` across all installed packages, if any.
pub fn canonical_stored_fhir_version(versions: Vec<&str>) -> Result<Option<String>, String> {
    if versions.is_empty() {
        return Ok(None);
    }
    let first = versions[0];
    for other in &versions[1..] {
        if *other != first {
            return Err(format!(
                "conformance database has mixed FHIR versions ({first} vs {other}); use a new database"
            ));
        }
    }
    Ok(Some(first.to_string()))
}

pub fn check_stored_release(stored: Option<&str>, release: FhirRelease) -> Result<(), String> {
    let expected = fhir_version_for_release(release);
    let Some(stored) = stored else {
        return Ok(());
    };
    if stored == expected {
        return Ok(());
    }
    Err(format!(
        "this database was created for FHIR {stored}; [fhir].version = \"{}\" ({expected}) is not allowed — use a new database to change release",
        release_name(release),
    ))
}

fn package_plan_from_file(
    fhir: &FhirSection,
    section: &ConformanceSection,
) -> Result<PackagePlan, String> {
    let release = resolve_fhir_release(fhir)?;
    let igs = section.packages.clone().unwrap_or_default();
    Ok(PackagePlan::Bundled(release, igs))
}

/// Environment overrides for secrets and URLs. Applied after the config file and before the CLI
/// flags, so precedence is file < env < CLI.
pub fn apply_env(cfg: &mut ResolvedConfig) -> Result<(), String> {
    if let Some(role) = env_value("REFHIR_ROLE") {
        cfg.role = Role::parse(&role)?;
    }
    if let Some(url) = env_value("REFHIR_PRIMARY_URL") {
        cfg.primary_url = Some(url);
    }
    if let Some(size) = env_value("REFHIR_DB_POOL") {
        cfg.db_pool = Some(
            size.parse::<usize>()
                .ok()
                .filter(|size| *size > 0)
                .ok_or_else(|| format!("REFHIR_DB_POOL must be a positive number, not `{size}`"))?,
        );
    }
    if let Some(url) = env_value("REFHIR_POSTGRES_URL") {
        cfg.postgres = Some(url);
        cfg.memory = false;
        cfg.sqlite = None;
    }
    if let Some(url) = env_value("REFHIR_PUBLIC_URL") {
        cfg.public_url = Some(url);
    }
    if let Some(value) = env_value("REFHIR_UPDATE_AS_CREATE") {
        cfg.update_as_create = matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
    }
    // The key to the audit chain is a secret and stays out of the config file; the database it
    // protects must not be where it lives.
    if let Some(key) = env_value("REFHIR_AUDIT_KEY") {
        cfg.governance.audit.chain_key = Some(refhir_persistence::ChainKey::new(key.into_bytes()));
    }
    if let Some(token) = env_value("REFHIR_SMART_DCR_TOKEN") {
        cfg.smart_dcr_token = Some(token);
    }
    if let Some(secret) = env_value("REFHIR_SMART_CONFIDENTIAL_SECRET") {
        cfg.smart_confidential_secret = Some(secret);
    }
    let issuer = env_value("REFHIR_OIDC_ISSUER");
    let client_id = env_value("REFHIR_OIDC_CLIENT_ID");
    let client_secret = env_value("REFHIR_OIDC_CLIENT_SECRET");
    if issuer.is_some() || client_id.is_some() || client_secret.is_some() {
        let oidc = cfg
            .oidc
            .get_or_insert_with(|| refhir_server::OidcConfig::new("", ""));
        if let Some(v) = issuer {
            oidc.issuer = v;
        }
        if let Some(v) = client_id {
            oidc.client_id = v;
        }
        if client_secret.is_some() {
            oidc.client_secret = client_secret;
        }
    }
    Ok(())
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The value after `--sqlite` or `--postgres`. A word that starts with `--` is the next flag,
/// which the first was left without a value to take, and not a database named `--memory`.
pub fn database_value(flag: &str, value: String) -> Result<String, String> {
    if value.starts_with("--") {
        return Err(format!("{flag} needs a value, not the flag {value}"));
    }
    Ok(value)
}

pub fn discover_config_path(cli_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = cli_path {
        return Some(p.to_path_buf());
    }
    if let Ok(p) = std::env::var("REFHIR_CONFIG") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    for candidate in ["refhir.toml", "/etc/refhir/refhir.toml"] {
        let path = Path::new(candidate);
        if path.is_file() {
            return Some(path.to_path_buf());
        }
    }
    None
}

pub fn load_file(path: &Path) -> Result<FileConfig, Box<dyn std::error::Error>> {
    let text = fs::read_to_string(path)?;
    let cfg: FileConfig = toml::from_str(&text)?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that set or read process environment variables take turns.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn env_overrides_file_values() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = ResolvedConfig::defaults();
        cfg.postgres = Some("postgres://CHANGE_ME".into());
        cfg.smart_confidential_secret = Some("from-file".into());
        std::env::set_var("REFHIR_POSTGRES_URL", "postgres://real");
        std::env::set_var("REFHIR_SMART_CONFIDENTIAL_SECRET", "from-env");
        std::env::set_var("REFHIR_OIDC_ISSUER", "https://idp.example");
        std::env::set_var("REFHIR_OIDC_CLIENT_ID", "refhir");
        std::env::set_var("REFHIR_OIDC_CLIENT_SECRET", "idp-secret");
        apply_env(&mut cfg).unwrap();
        for name in [
            "REFHIR_OIDC_ISSUER",
            "REFHIR_OIDC_CLIENT_ID",
            "REFHIR_OIDC_CLIENT_SECRET",
        ] {
            std::env::remove_var(name);
        }
        let oidc = cfg.oidc.clone().expect("oidc from env");
        assert_eq!(oidc.issuer, "https://idp.example");
        assert_eq!(oidc.client_id, "refhir");
        assert_eq!(oidc.client_secret.as_deref(), Some("idp-secret"));
        assert!(oidc.validate().is_ok());
        std::env::remove_var("REFHIR_POSTGRES_URL");
        std::env::remove_var("REFHIR_SMART_CONFIDENTIAL_SECRET");
        assert_eq!(cfg.postgres.as_deref(), Some("postgres://real"));
        assert_eq!(cfg.smart_confidential_secret.as_deref(), Some("from-env"));
    }

    #[test]
    fn oidc_allowlist_and_fhir_user_binding_come_from_the_file() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let file: FileConfig = toml::from_str(
            r#"
            [smart.oidc]
            issuer = "https://idp.example"
            client_id = "refhir"
            allowed_claims = { organization_id = ["org_a", "org_b"] }
            allowed_email_domains = ["example.com"]
            patient_from_fhir_user = true
            [smart.oidc.fhir_user_lookup]
            system = "https://idp.example/sub"
            "#,
        )
        .unwrap();
        let mut cfg = ResolvedConfig::defaults();
        cfg.apply_file(&file).unwrap();
        let oidc = cfg.oidc.expect("oidc");
        assert_eq!(oidc.allowed_claims["organization_id"], ["org_a", "org_b"]);
        assert_eq!(oidc.allowed_email_domains, ["example.com"]);
        assert!(oidc.patient_from_fhir_user);
        let lookup = oidc.fhir_user_lookup.clone().expect("lookup");
        assert_eq!(lookup.resource_type, "Practitioner");
        assert_eq!(lookup.claim, "sub");
        assert_eq!(lookup.search_param, "identifier");
        assert_eq!(lookup.system.as_deref(), Some("https://idp.example/sub"));
        assert!(oidc.validate().is_ok());
    }

    #[test]
    fn oidc_stays_open_and_unbound_unless_configured() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let file: FileConfig = toml::from_str(
            "[smart.oidc]\nissuer = \"https://idp.example\"\nclient_id = \"refhir\"\n",
        )
        .unwrap();
        let mut cfg = ResolvedConfig::defaults();
        cfg.apply_file(&file).unwrap();
        let oidc = cfg.oidc.expect("oidc");
        assert!(oidc.allowed_claims.is_empty());
        assert!(oidc.allowed_email_domains.is_empty());
        assert!(oidc.fhir_user_lookup.is_none());
        assert!(!oidc.patient_from_fhir_user);
    }

    #[test]
    fn an_unknown_or_incomplete_database_kind_is_rejected() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = ResolvedConfig::defaults();
        let typo = FileConfig {
            database: DatabaseSection {
                kind: Some("postgresql".into()),
                postgres_url: Some("postgres://x".into()),
                ..DatabaseSection::default()
            },
            ..FileConfig::default()
        };
        assert!(cfg.apply_file(&typo).unwrap_err().contains("postgresql"));
        std::env::remove_var("REFHIR_POSTGRES_URL");
        let no_url = FileConfig {
            database: DatabaseSection {
                kind: Some("postgres".into()),
                ..DatabaseSection::default()
            },
            ..FileConfig::default()
        };
        assert!(cfg.apply_file(&no_url).is_err());
    }

    fn resolved(text: &str) -> ResolvedConfig {
        let file: FileConfig = toml::from_str(text).expect("parses");
        let mut cfg = ResolvedConfig::defaults();
        cfg.apply_file(&file).expect("applies");
        cfg
    }

    #[test]
    fn a_server_started_with_no_file_records_everything_and_limits_nothing() {
        let governance = ResolvedConfig::defaults().governance;
        assert!(governance.metering && governance.audit.enabled && governance.request_log.enabled);
        assert!(governance.audit.reads && governance.audit.include_query);
        assert_eq!(
            governance.audit.retention_days, None,
            "audit is kept until someone says otherwise"
        );
        assert!(!governance.rate_limit.enabled && !governance.quota.enabled);
        assert_eq!(governance.proxy.trusted_hops, 0);
    }

    #[test]
    fn the_governance_sections_are_read_from_the_file() {
        let cfg = resolved(
            r#"
            [server]
            trusted_proxy_hops = 2
            trusted_proxy_peers = ["10.0.0.0/8", "fd00::/8"]

            [metering]
            enabled = true

            [request_log]
            store = false

            [audit]
            reads = false
            include_query = false
            retention_days = 2190

            [rate_limit]
            requests_per_second = 20
            burst = 40
            anonymous_requests_per_second = 2.5
            anonymous_burst = 10
            auth_failures_per_minute = 8
            max_sync_connections = 3
            max_live_queries = 12

            [rate_limit.cost]
            export = 40
            read = 2

            [rate_limit.clients.batch]
            requests_per_second = 300
            burst = 600

            [rate_limit.clients.trusted]
            requests_per_second = 0

            [quota]
            requests_per_day = 100000
            writes_per_month = 50000
            refresh_seconds = 10

            [quota.clients.acme]
            requests_per_day = 1000000
            bytes_per_day = 0
            "#,
        );
        let g = &cfg.governance;
        assert_eq!(g.proxy.trusted_hops, 2);
        assert_eq!(g.proxy.trusted_peers.len(), 2);
        assert!(g.proxy.trusted_peers[0].contains("10.1.2.3".parse().unwrap()));
        assert!(!g.proxy.trusted_peers[0].contains("11.0.0.1".parse().unwrap()));
        assert!(!g.request_log.store && g.request_log.enabled);
        assert!(g.audit.enabled && !g.audit.reads && !g.audit.include_query);
        assert_eq!(g.audit.retention_days, Some(2190));
        let rate = &g.rate_limit;
        assert!(rate.enabled);
        assert_eq!((rate.requests_per_second, rate.burst), (20.0, 40));
        assert_eq!(rate.anonymous_requests_per_second, Some(2.5));
        assert_eq!(rate.anonymous_burst, Some(10));
        assert_eq!(
            (
                rate.auth_failures_per_minute,
                rate.max_sync_connections,
                rate.max_live_queries
            ),
            (8, 3, 12)
        );
        assert_eq!(
            (rate.costs.export, rate.costs.read, rate.costs.search),
            (40, 2, 2),
            "the rest keep their default"
        );
        assert_eq!(rate.for_client("batch"), (300.0, 600));
        assert_eq!(rate.for_client("trusted").0, 0.0, "exempt");
        let quota = &g.quota;
        assert!(quota.enabled);
        assert_eq!(quota.refresh, Duration::from_secs(10));
        assert_eq!(quota.for_client("anyone").requests_per_day, Some(100_000));
        assert_eq!(quota.for_client("anyone").writes_per_month, Some(50_000));
        assert_eq!(quota.for_client("acme").requests_per_day, Some(1_000_000));
        // A client's own section overrides field by field: what it leaves out is inherited,
        // and a zero lifts a limit the default sets.
        assert_eq!(
            quota.for_client("acme").writes_per_month,
            Some(50_000),
            "inherited"
        );
        assert_eq!(
            quota.for_client("acme").bytes_per_day,
            None,
            "lifted by a zero"
        );
    }

    #[test]
    fn a_proxy_network_that_is_not_one_is_refused() {
        let file: FileConfig =
            toml::from_str("[server]\ntrusted_proxy_peers = [\"10.0.0.0/40\"]\n").unwrap();
        let err = ResolvedConfig::defaults().apply_file(&file).unwrap_err();
        assert!(err.contains("trusted_proxy_peers"), "{err}");
    }

    #[test]
    fn a_section_that_is_present_is_on_unless_it_says_otherwise() {
        let on = resolved("[rate_limit]\nrequests_per_second = 5\nburst = 20\n");
        assert!(
            on.governance.rate_limit.enabled,
            "numbers without enabled = true still limit"
        );
        let off = resolved("[rate_limit]\nenabled = false\nrequests_per_second = 5\n");
        assert!(!off.governance.rate_limit.enabled);
        assert!(
            resolved("[quota]\nrequests_per_day = 1\n")
                .governance
                .quota
                .enabled
        );
        assert!(
            !resolved("[quota]\nenabled = false\n")
                .governance
                .quota
                .enabled
        );
        assert!(
            !resolved("[audit]\nenabled = false\n")
                .governance
                .audit
                .enabled
        );
        assert!(
            resolved("[audit]\nreads = false\n")
                .governance
                .audit
                .enabled
        );
        assert!(
            !resolved("[metering]\nenabled = false\n")
                .governance
                .metering
        );
        assert!(
            !resolved("[request_log]\nenabled = false\n")
                .governance
                .request_log
                .enabled
        );
    }

    #[test]
    fn a_misspelt_key_in_a_governance_section_is_an_error_and_an_unknown_section_is_named() {
        for typo in [
            "[rate_limit]\nrequests_per_secnd = 5\n",
            "[rate_limit.cost]\nexports = 5\n",
            "[rate_limit.clients.a]\nburts = 5\n",
            "[quota]\nrequest_per_day = 5\n",
            "[audit]\nread = false\n",
        ] {
            assert!(toml::from_str::<FileConfig>(typo).is_err(), "{typo}");
        }
        let file: FileConfig = toml::from_str("[rate_limt]\nenabled = true\n").unwrap();
        assert_eq!(file.unknown.keys().collect::<Vec<_>>(), ["rate_limt"]);
        let file: FileConfig = toml::from_str("[rate_limit]\nenabled = true\n").unwrap();
        assert!(file.unknown.is_empty());
    }

    #[test]
    fn the_configurations_that_ship_with_the_server_parse_and_pass_validation() {
        for (name, text) in [
            (
                "refhir.toml.example",
                include_str!("../../../refhir.toml.example"),
            ),
            (
                "deploy/refhir.production.toml",
                include_str!("../../../deploy/refhir.production.toml"),
            ),
        ] {
            let file: FileConfig =
                toml::from_str(text).unwrap_or_else(|err| panic!("{name}: {err}"));
            assert!(
                file.unknown.is_empty(),
                "{name}: {:?}",
                file.unknown.keys().collect::<Vec<_>>()
            );
            let mut cfg = ResolvedConfig::defaults();
            cfg.apply_file(&file)
                .unwrap_or_else(|err| panic!("{name}: {err}"));
            cfg.governance
                .validate(cfg.auth)
                .unwrap_or_else(|err| panic!("{name}: {err}"));
        }
    }

    #[test]
    fn the_audit_key_comes_from_the_environment_and_is_never_printed() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = ResolvedConfig::defaults();
        assert!(cfg.governance.audit.chain_key.is_none());
        assert!(cfg.describe_governance().contains("chained with no key"));
        std::env::set_var("REFHIR_AUDIT_KEY", "0123456789abcdef0123456789abcdef");
        apply_env(&mut cfg).unwrap();
        std::env::remove_var("REFHIR_AUDIT_KEY");
        assert_eq!(
            cfg.governance.audit.chain_key.as_ref().map(|key| key.len()),
            Some(32)
        );
        assert!(cfg.describe_governance().contains("chained with a key"));
        assert!(
            !format!("{cfg:?}").contains("0123456789abcdef"),
            "a key is not in Debug output"
        );
        assert!(cfg.governance.validate(false).is_ok());
        // Too short to be a secret is refused when the server starts.
        std::env::set_var("REFHIR_AUDIT_KEY", "short");
        apply_env(&mut cfg).unwrap();
        std::env::remove_var("REFHIR_AUDIT_KEY");
        assert!(cfg.governance.validate(false).is_err());
    }

    #[test]
    fn what_is_on_is_described_in_one_line() {
        let mut cfg = resolved(
            "[rate_limit]\nrequests_per_second = 20\nburst = 40\n[audit]\nretention_days = 30\n",
        );
        let line = cfg.describe_governance();
        assert!(line.contains("rate limit 20/s burst 40"), "{line}");
        assert!(
            line.contains("audit on (reads and changes, kept 30 days, chained with no key)"),
            "{line}"
        );
        assert!(line.contains("quotas off"), "{line}");
        cfg.governance.audit.enabled = false;
        assert!(cfg.describe_governance().contains("audit OFF"));
    }

    #[test]
    fn fhir_version_parses_r4() {
        assert_eq!(parse_fhir_release("r4").unwrap(), FhirRelease::R4);
        assert_eq!(parse_fhir_release("4.0.1").unwrap(), FhirRelease::R4);
        assert!(parse_fhir_release("r5").is_err());
    }

    #[test]
    fn release_cannot_change_on_existing_database() {
        assert!(check_stored_release(Some("4.0.1"), FhirRelease::R4).is_ok());
        assert!(check_stored_release(Some("5.0.0"), FhirRelease::R4).is_err());
        assert!(check_stored_release(None, FhirRelease::R4).is_ok());
    }

    #[test]
    fn canonical_stored_fhir_version_rejects_mixed_packages() {
        assert!(canonical_stored_fhir_version(vec![]).unwrap().is_none());
        assert_eq!(
            canonical_stored_fhir_version(vec!["4.0.1", "4.0.1"]).unwrap(),
            Some("4.0.1".into())
        );
        assert!(canonical_stored_fhir_version(vec!["4.0.1", "5.0.0"]).is_err());
    }

    #[test]
    fn packages_install_after_release_defaults() {
        let file = FileConfig {
            fhir: FhirSection {
                version: Some("r4".into()),
            },
            conformance: ConformanceSection {
                packages: Some(vec!["hl7.fhir.us.core@6.1.0".into()]),
                ..ConformanceSection::default()
            },
            ..FileConfig::default()
        };
        let plan = package_plan_from_file(&file.fhir, &file.conformance).unwrap();
        assert!(matches!(
            plan,
            PackagePlan::Bundled(FhirRelease::R4, extras) if extras.len() == 1
        ));
    }

    #[test]
    fn invalid_fhir_version_in_file_is_rejected() {
        let file = FileConfig {
            fhir: FhirSection {
                version: Some("r5".into()),
            },
            ..FileConfig::default()
        };
        assert!(package_plan_from_file(&file.fhir, &file.conformance).is_err());
    }
}
