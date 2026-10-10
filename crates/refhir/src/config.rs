//! Optional `refhir.toml` — CLI flags override file values when both are set.

use std::fs;
use std::path::{Path, PathBuf};

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
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerSection {
    pub bind: Option<String>,
    pub public_url: Option<String>,
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
        }
    }

    pub fn apply_file(&mut self, file: &FileConfig) -> Result<(), String> {
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
        if file.database.postgres_url.is_some() && file.database.kind.as_deref() != Some("sqlite") {
            if file.database.kind.as_deref() == Some("postgres") || file.database.kind.is_none() {
                self.postgres = file.database.postgres_url.clone();
                self.memory = false;
                self.sqlite = None;
            }
        }

        self.packages = package_plan_from_file(&file.fhir, &file.conformance)?;

        if let Some(v) = &file.conformance.reference_policy {
            self.references = v.clone();
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
        Ok(())
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
pub fn apply_env(cfg: &mut ResolvedConfig) {
    if let Some(url) = env_value("REFHIR_POSTGRES_URL") {
        cfg.postgres = Some(url);
        cfg.memory = false;
        cfg.sqlite = None;
    }
    if let Some(url) = env_value("REFHIR_PUBLIC_URL") {
        cfg.public_url = Some(url);
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
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
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
        apply_env(&mut cfg);
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
