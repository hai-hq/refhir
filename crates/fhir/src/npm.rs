//! Fetch a FHIR NPM package and install it the same way a local directory is
//! installed. Network I/O stays outside the OCC transaction.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock, TryLockError};
use std::time::Duration;

use flate2::read::GzDecoder;
use refhir_database::{Database, ReadSet};
use refhir_persistence::ConformanceStore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::catalog::Catalog;
use crate::error::FhirError;
use crate::package::{install_package_dir, package_identity, package_root, read_manifest};
use crate::run::RunResult;

const ENGINE: &str = "refhir_engine";
const DEFAULT_REGISTRY: &str = "https://packages.fhir.org";
const MAX_TARBALL: u64 = 100 * 1024 * 1024;
const MAX_UNCOMPRESSED: u64 = 512 * 1024 * 1024;
const MAX_ENTRIES: u64 = 20_000;
const COMPLETE: &str = ".complete";

fn install_lock() -> &'static Mutex<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    &LOCK
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageSpec {
    Dir(PathBuf),
    Tarball(PathBuf),
    Npm { name: String, version: String },
}

pub trait PackageFetcher {
    fn fetch(&self, name: &str, version: &str, dest: &Path) -> Result<(), FhirError>;
}

#[derive(Clone, Debug)]
pub struct NpmRegistry {
    pub base_url: String,
    pub cache_dir: PathBuf,
}

impl NpmRegistry {
    pub fn from_env() -> Self {
        let base_url = std::env::var("REFHIR_PACKAGE_REGISTRY")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_REGISTRY.to_string());
        let cache_dir = std::env::var("REFHIR_PACKAGE_CACHE")
            .ok()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".refhir/packages"));
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            cache_dir,
        }
    }
}

impl PackageFetcher for NpmRegistry {
    fn fetch(&self, name: &str, version: &str, dest: &Path) -> Result<(), FhirError> {
        let url = format!("{}/{name}/{version}", self.base_url.trim_end_matches('/'));
        tracing::info!(target: ENGINE, name, version, url = url.as_str(), "downloading FHIR package");
        let bytes = download_tarball(&url)?;
        extract_tarball(&bytes, dest)?;
        write_complete(dest, &sha256_hex(&bytes))
    }
}

/// Install from a directory, `.tgz`, or `name@version` / `name#version`.
pub fn install_package(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    spec: &str,
) -> Result<(), FhirError> {
    let registry = NpmRegistry::from_env();
    install_with(store, db, catalog, spec, &registry, &registry.cache_dir)
}

pub fn install_with(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    spec: &str,
    fetcher: &dyn PackageFetcher,
    cache: &Path,
) -> Result<(), FhirError> {
    let parsed = parse_package_spec(spec)?;
    let mut stack = BTreeSet::new();
    install_tree(store, db, catalog, parsed, fetcher, cache, &mut stack)?;
    Ok(())
}

pub fn install_operation(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    args: &Value,
) -> Result<RunResult, FhirError> {
    let _guard = match install_lock().try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => {
            return Err(FhirError::Conflict(
                "another package install is running".into(),
            ));
        }
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
    };
    let (name, version) = npm_spec_from_args(args)?;
    let spec = format!("{name}@{version}");
    let fetcher = NpmRegistry::from_env();
    install_with(store, db, catalog, &spec, &fetcher, &fetcher.cache_dir)?;
    Ok(RunResult {
        value: json!({
            "resourceType": "OperationOutcome",
            "issue": [{
                "severity": "information",
                "code": "informational",
                "diagnostics": format!("installed {spec}")
            }]
        }),
        ts: db.latest_ts(),
        reads: ReadSet::default(),
        wrote: false,
        writes: Vec::new(),
        status: 200,
    })
}

pub fn parse_package_spec(text: &str) -> Result<PackageSpec, FhirError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(FhirError::Invalid("package is required".into()));
    }
    let path = PathBuf::from(text);
    if is_tarball(text) {
        return Ok(PackageSpec::Tarball(path));
    }
    if path.is_dir() {
        return Ok(PackageSpec::Dir(path));
    }
    if let Some((name, version)) = split_npm(text) {
        return Ok(PackageSpec::Npm { name, version });
    }
    if looks_like_path(text) {
        return Ok(PackageSpec::Dir(path));
    }
    Err(FhirError::Invalid(format!(
        "package {text} needs a version (name@version) or a local directory"
    )))
}

fn npm_spec_from_args(args: &Value) -> Result<(String, String), FhirError> {
    if optional(args, &["registry"]).is_some() {
        return Err(FhirError::Invalid(
            "$install does not accept a registry; set REFHIR_PACKAGE_REGISTRY".into(),
        ));
    }
    let (name, version) = if let Some(package) = optional(args, &["package", "npm"]) {
        split_npm(package).ok_or_else(|| {
            FhirError::Invalid("$install only accepts name@version from the package registry".into())
        })?
    } else {
        match (optional(args, &["name"]), optional(args, &["version"])) {
            (Some(name), Some(version)) => (name.to_string(), version.to_string()),
            (Some(_), None) => return Err(FhirError::Invalid("version is required".into())),
            (None, Some(_)) => return Err(FhirError::Invalid("name is required".into())),
            (None, None) => {
                return Err(FhirError::Invalid(
                    "package or name and version is required".into(),
                ));
            }
        }
    };
    if looks_like_path(&name) || looks_like_path(&version) {
        return Err(FhirError::Invalid(
            "$install only accepts name@version from the package registry".into(),
        ));
    }
    Ok((safe_segment(&name)?, safe_segment(&version)?))
}

fn install_tree(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    spec: PackageSpec,
    fetcher: &dyn PackageFetcher,
    cache: &Path,
    stack: &mut BTreeSet<(String, String)>,
) -> Result<(), FhirError> {
    if let PackageSpec::Npm { name, version } = &spec {
        if already_installed(store, name, version)? {
            return Ok(());
        }
        if !stack.insert((name.clone(), version.clone())) {
            return Err(FhirError::Invalid(format!(
                "package {name}@{version} depends on itself"
            )));
        }
    }
    let staged = materialize(&spec, fetcher, cache)?;
    let (name, version) = package_identity(&staged.dir)?;
    if already_installed(store, &name, &version)? {
        return Ok(());
    }
    if matches!(spec, PackageSpec::Dir(_) | PackageSpec::Tarball(_))
        && !stack.insert((name.clone(), version.clone()))
    {
        return Err(FhirError::Invalid(format!(
            "package {name}@{version} depends on itself"
        )));
    }
    for (dep_name, dep_version) in dependencies_of(&staged.dir)? {
        install_tree(
            store,
            db,
            catalog,
            PackageSpec::Npm {
                name: dep_name,
                version: dep_version,
            },
            fetcher,
            cache,
            stack,
        )?;
    }
    install_package_dir(store, db, catalog, &staged.dir)?;
    stack.remove(&(name, version));
    Ok(())
}

struct Staged {
    dir: PathBuf,
    _scratch: Option<PathBuf>,
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(path) = &self._scratch {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn materialize(
    spec: &PackageSpec,
    fetcher: &dyn PackageFetcher,
    cache: &Path,
) -> Result<Staged, FhirError> {
    match spec {
        PackageSpec::Dir(path) => Ok(Staged {
            dir: path.clone(),
            _scratch: None,
        }),
        PackageSpec::Tarball(path) => {
            let scratch = scratch_dir()?;
            let bytes = read_tarball(path)?;
            extract_tarball(&bytes, &scratch)?;
            Ok(Staged {
                dir: scratch.clone(),
                _scratch: Some(scratch),
            })
        }
        PackageSpec::Npm { name, version } => {
            let dest = cache.join(safe_segment(name)?).join(safe_segment(version)?);
            if cache_complete(&dest) {
                return Ok(Staged {
                    dir: dest,
                    _scratch: None,
                });
            }
            if dest.exists() {
                let _ = fs::remove_dir_all(&dest);
            }
            fs::create_dir_all(&dest).map_err(|err| {
                FhirError::Storage(format!("package cache {}: {err}", dest.display()))
            })?;
            match fetcher.fetch(name, version, &dest) {
                Ok(()) => {
                    if package_root(&dest).is_err() {
                        let _ = fs::remove_dir_all(&dest);
                        return Err(FhirError::Invalid(format!(
                            "downloaded {name}@{version} is not a FHIR package"
                        )));
                    }
                    if !dest.join(COMPLETE).is_file() {
                        write_complete(&dest, "ok")?;
                    }
                    Ok(Staged {
                        dir: dest,
                        _scratch: None,
                    })
                }
                Err(err) => {
                    let _ = fs::remove_dir_all(&dest);
                    Err(err)
                }
            }
        }
    }
}

fn dependencies_of(path: &Path) -> Result<Vec<(String, String)>, FhirError> {
    let root = package_root(path)?;
    let manifest = read_manifest(&root)?;
    let Some(deps) = manifest.get("dependencies").and_then(|value| value.as_object()) else {
        return Ok(Vec::new());
    };
    let mut rows = Vec::new();
    for (name, version) in deps {
        let version = version.as_str().ok_or_else(|| {
            FhirError::Invalid(format!("dependency {name} version must be a string"))
        })?;
        if !exact_version(version) {
            return Err(FhirError::Invalid(format!(
                "dependency {name} version {version} must be an exact version"
            )));
        }
        rows.push((name.clone(), version.to_string()));
    }
    Ok(rows)
}

fn already_installed(
    store: &dyn ConformanceStore,
    name: &str,
    version: &str,
) -> Result<bool, FhirError> {
    Ok(store
        .load_packages()
        .map_err(FhirError::from)?
        .iter()
        .any(|package| package.name == name && package.version == version))
}

fn scratch_dir() -> Result<PathBuf, FhirError> {
    let path = std::env::temp_dir().join(format!("refhir-pkg-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&path)
        .map_err(|err| FhirError::Storage(format!("temp package dir: {err}")))?;
    Ok(path)
}

fn cache_complete(dest: &Path) -> bool {
    dest.join(COMPLETE).is_file() && package_root(dest).is_ok()
}

fn write_complete(dest: &Path, token: &str) -> Result<(), FhirError> {
    fs::write(dest.join(COMPLETE), token)
        .map_err(|err| FhirError::Storage(format!("package cache marker: {err}")))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn read_tarball(path: &Path) -> Result<Vec<u8>, FhirError> {
    let file = File::open(path)
        .map_err(|err| FhirError::Invalid(format!("{}: {err}", path.display())))?;
    let mut bytes = Vec::new();
    file.take(MAX_TARBALL + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| FhirError::Invalid(format!("{}: {err}", path.display())))?;
    if bytes.len() as u64 > MAX_TARBALL {
        return Err(FhirError::Invalid(format!(
            "{} is larger than {MAX_TARBALL} bytes",
            path.display()
        )));
    }
    Ok(bytes)
}

fn download_tarball(url: &str) -> Result<Vec<u8>, FhirError> {
    let mut last = FhirError::Storage(format!("download {url} failed"));
    for _ in 0..3 {
        match download_once(url) {
            Ok(bytes) => return Ok(bytes),
            Err(err) if retries_download(&err) => last = err,
            Err(err) => return Err(err),
        }
    }
    Err(last)
}

fn retries_download(err: &FhirError) -> bool {
    matches!(err, FhirError::Storage(_))
}

fn download_once(url: &str) -> Result<Vec<u8>, FhirError> {
    let response = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(120))
        .build()
        .get(url)
        .set("User-Agent", "refhir/0.1")
        .call()
        .map_err(|err| match err {
            ureq::Error::Status(404, _) => {
                FhirError::NotFound(format!("package not found at {url}"))
            }
            ureq::Error::Status(code, _) if (400..500).contains(&code) => {
                FhirError::Invalid(format!("download {url}: HTTP {code}"))
            }
            other => FhirError::Storage(format!("download {url}: {other}")),
        })?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_TARBALL + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| FhirError::Storage(format!("download {url}: {err}")))?;
    if bytes.len() as u64 > MAX_TARBALL {
        return Err(FhirError::Invalid(format!(
            "package at {url} is larger than {MAX_TARBALL} bytes"
        )));
    }
    Ok(bytes)
}

struct BoundedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe) {
                Ok(0) => Ok(0),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "package tarball unpacks larger than the size limit",
                )),
                Err(err) => Err(err),
            };
        }
        let take = buf.len().min(self.remaining.min(usize::MAX as u64) as usize);
        let n = self.inner.read(&mut buf[..take])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

fn extract_tarball(bytes: &[u8], dest: &Path) -> Result<(), FhirError> {
    extract_tarball_limited(bytes, dest, MAX_UNCOMPRESSED, MAX_ENTRIES)
}

fn extract_tarball_limited(
    bytes: &[u8],
    dest: &Path,
    max_uncompressed: u64,
    max_entries: u64,
) -> Result<(), FhirError> {
    fs::create_dir_all(dest)
        .map_err(|err| FhirError::Storage(format!("extract {}: {err}", dest.display())))?;
    let decoder = BoundedReader {
        inner: GzDecoder::new(bytes),
        remaining: max_uncompressed,
    };
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|err| FhirError::Invalid(format!("invalid package tarball: {err}")))?;
    let mut count = 0u64;
    for entry in entries {
        let mut entry = entry
            .map_err(|err| FhirError::Invalid(format!("invalid package tarball: {err}")))?;
        let kind = entry.header().entry_type();
        if kind.is_symlink()
            || kind.is_hard_link()
            || kind.is_fifo()
            || kind.is_character_special()
            || kind.is_block_special()
        {
            return Err(FhirError::Invalid(
                "package tarball has a link or special file".into(),
            ));
        }
        let path = entry
            .path()
            .map_err(|err| FhirError::Invalid(format!("invalid package tarball: {err}")))?;
        if path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(FhirError::Invalid(
                "package tarball has an unsafe path".into(),
            ));
        }
        count += 1;
        if count > max_entries {
            return Err(FhirError::Invalid(
                "package tarball has too many files".into(),
            ));
        }
        let unpacked = entry
            .unpack_in(dest)
            .map_err(|err| FhirError::Invalid(format!("invalid package tarball: {err}")))?;
        if !unpacked {
            return Err(FhirError::Invalid(
                "package tarball has an unsafe path".into(),
            ));
        }
    }
    Ok(())
}

fn split_npm(text: &str) -> Option<(String, String)> {
    let (name, version) = text
        .rsplit_once('@')
        .or_else(|| text.rsplit_once('#'))
        .filter(|(name, version)| !name.is_empty() && !version.is_empty())?;
    if looks_like_path(name) {
        return None;
    }
    Some((name.to_string(), version.to_string()))
}

fn looks_like_path(text: &str) -> bool {
    text.starts_with('.')
        || text.starts_with('/')
        || text.contains('/')
        || text.contains('\\')
        || Path::new(text).is_dir()
}

fn is_tarball(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.ends_with(".tgz") || lower.ends_with(".tar.gz")
}

fn exact_version(version: &str) -> bool {
    !version.is_empty()
        && version != "latest"
        && !version.starts_with(['^', '~', '>', '<', '*'])
        && !version.contains(' ')
}

fn safe_segment(text: &str) -> Result<String, FhirError> {
    if text.is_empty()
        || text.contains("..")
        || !text
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
    {
        return Err(FhirError::Invalid(format!("invalid package id {text}")));
    }
    Ok(text.to_string())
}

fn optional<'a>(args: &'a Value, names: &[&str]) -> Option<&'a str> {
    for name in names {
        if let Some(text) = args.get(*name).and_then(|value| value.as_str()) {
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    let parameters = args
        .get("parameter")
        .and_then(|value| value.as_array())
        .or_else(|| {
            args.pointer("/resource/parameter")
                .and_then(|value| value.as_array())
        })?;
    for name in names {
        for parameter in parameters {
            if parameter.get("name").and_then(|value| value.as_str()) != Some(*name) {
                continue;
            }
            for key in [
                "valueUri",
                "valueUrl",
                "valueCanonical",
                "valueString",
                "valueCode",
            ] {
                if let Some(text) = parameter.get(key).and_then(|value| value.as_str()) {
                    if !text.is_empty() {
                        return Some(text);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_name_version_and_local_dir() {
        assert_eq!(
            parse_package_spec("hl7.fhir.us.core@6.1.0").unwrap(),
            PackageSpec::Npm {
                name: "hl7.fhir.us.core".into(),
                version: "6.1.0".into()
            }
        );
        assert_eq!(
            parse_package_spec("hl7.fhir.us.core#6.1.0").unwrap(),
            PackageSpec::Npm {
                name: "hl7.fhir.us.core".into(),
                version: "6.1.0".into()
            }
        );
        assert!(matches!(
            parse_package_spec("pkg.tgz").unwrap(),
            PackageSpec::Tarball(_)
        ));
        assert!(parse_package_spec("hl7.fhir.us.core").is_err());
        let extra = crate::package::extra_dir();
        assert_eq!(
            parse_package_spec(extra.to_str().unwrap()).unwrap(),
            PackageSpec::Dir(extra.to_path_buf())
        );
    }

    #[test]
    fn install_operation_only_accepts_registry_name_version() {
        let (name, version) = npm_spec_from_args(&json!({
            "resourceType": "Parameters",
            "parameter": [
                {"name": "name", "valueString": "hl7.fhir.us.core"},
                {"name": "version", "valueString": "6.1.0"}
            ]
        }))
        .unwrap();
        assert_eq!(name, "hl7.fhir.us.core");
        assert_eq!(version, "6.1.0");
        assert_eq!(
            npm_spec_from_args(&json!({"package": "hl7.fhir.us.core@6.1.0"})).unwrap(),
            ("hl7.fhir.us.core".into(), "6.1.0".into())
        );
        assert!(npm_spec_from_args(&json!({"package": "/tmp/ig"})).is_err());
        assert!(npm_spec_from_args(&json!({"package": "pkg.tgz"})).is_err());
        assert!(npm_spec_from_args(&json!({
            "name": "hl7.fhir.us.core",
            "version": "6.1.0",
            "registry": "http://127.0.0.1"
        }))
        .is_err());
    }

    #[test]
    fn extract_rejects_parent_paths_and_symlinks() {
        let dest = std::env::temp_dir().join(format!("refhir-tar-{}", uuid::Uuid::new_v4()));
        let parent = gzipped_tar(|builder| {
            let mut header = tar::Header::new_gnu();
            {
                let gnu = header.as_gnu_mut().unwrap();
                let name = b"../evil.json";
                gnu.name[..name.len()].copy_from_slice(name);
            }
            header.set_size(2);
            header.set_cksum();
            builder.append(&header, b"{}".as_ref()).unwrap();
        });
        assert!(extract_tarball(&parent, &dest).is_err());
        let _ = fs::remove_dir_all(&dest);

        let dest = std::env::temp_dir().join(format!("refhir-tar-{}", uuid::Uuid::new_v4()));
        let link = gzipped_tar(|builder| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_cksum();
            builder.append_link(&mut header, "link", "/etc/passwd").unwrap();
        });
        assert!(extract_tarball(&link, &dest).is_err());
        let _ = fs::remove_dir_all(&dest);
    }

    #[test]
    fn extract_stops_on_decompressed_bytes_not_tar_headers() {
        let dest = std::env::temp_dir().join(format!("refhir-tar-{}", uuid::Uuid::new_v4()));
        let bytes = gzipped_tar(|builder| {
            let data = vec![b'x'; 2000];
            let mut header = tar::Header::new_gnu();
            header.set_path("package.json").unwrap();
            header.set_size(data.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, "package.json", data.as_slice())
                .unwrap();
        });
        let err = extract_tarball_limited(&bytes, &dest, 100, MAX_ENTRIES).unwrap_err();
        assert!(matches!(err, FhirError::Invalid(_)), "{err}");
        let _ = fs::remove_dir_all(&dest);
    }

    fn gzipped_tar(write: impl FnOnce(&mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>)) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        write(&mut builder);
        builder.into_inner().unwrap().finish().unwrap()
    }
}
