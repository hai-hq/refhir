//! Packages prepared ahead of time, so that a first start does not download, parse and expand
//! them.
//!
//! Installing a package reads its files, keeps its definitions and terminology, and expands every
//! value set: seconds of work, and the most memory the server ever asks for. None of it depends
//! on the database the package goes into, so it can be done once, when ReFHIR is built or
//! released, and the rows it comes to written to one file. A server that finds the file stores
//! those rows as they are.
//!
//! The rows are what [`read_package`] makes, so a package installed from the file and one
//! installed from the registry are stored the same. The file is a gzip stream of JSON values, one
//! after another: a header that says which packages follow, in the order to install them, and
//! then one value for each package.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Instant;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use refhir_database::Database;
use refhir_persistence::{ConformanceStore, NewPackage};
use serde::{Deserialize, Serialize};

use crate::catalog::Catalog;
use crate::error::FhirError;
use crate::npm::{
    compare_versions, materialize, parse_package_spec, source_digest, Node, PackageFetcher,
    Resolver,
};
use crate::package::{
    finish_install, package_identity, read_package, run_backfills, stage_new_package,
    PendingBackfill,
};

/// The name a prepared file is given when nothing else names it, and the one looked for next to
/// the server.
pub const BUNDLE_FILE: &str = "refhir-packages.bundle";

/// Bumped when what the file holds changes in a way an older server cannot read. The bytes are
/// the serde form of [`NewPackage`] and its rows, which is why a test below freezes them: a file
/// made by one build is read by another (a release's archive beside a newer binary, an image
/// layer), and a field added to those types without a bump would make every such file unreadable.
const FORMAT: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Header {
    format: u32,
    /// The version of ReFHIR that wrote the file.
    refhir: String,
    /// `name@version` of each package that follows, in the order to install them: a package
    /// comes after the ones it depends on.
    packages: Vec<String>,
    /// The SHA-256 of the tarball each package was downloaded from, for the packages that came
    /// from a registry: what it served when the file was made, kept so that what a release ships
    /// can be checked afterwards.
    #[serde(default)]
    sources: Vec<Source>,
}

#[derive(Serialize, Deserialize)]
struct Source {
    package: String,
    sha256: String,
}

/// Prepare `specs` and everything they depend on, and write them to `out`. Each spec is a
/// package spec as `$install` takes it (`name@version`, a `.tgz`, or a directory). Returns the
/// `name@version` of each package written, in the order they are stored.
///
/// The packages are chosen as installing them one after another into an empty database would
/// choose them, and each is read one at a time, so the memory this takes is that of the largest.
/// The file is written under a name of its own and moved into place when it is whole, so a file
/// that is there is complete; when writing fails nothing is left behind.
pub fn write_prepared(
    out: &Path,
    specs: &[String],
    fetcher: &dyn PackageFetcher,
    cache: &Path,
) -> Result<Vec<String>, FhirError> {
    let mut installed: Vec<(String, String)> = Vec::new();
    let mut order = Vec::new();
    for spec in specs {
        let root = materialize(&parse_package_spec(spec)?, fetcher, cache)?;
        let (name, version) = package_identity(&root.dir)?;
        if installed.contains(&(name.clone(), version.clone())) {
            continue;
        }
        let nodes =
            Resolver::new(installed.clone(), fetcher, cache).resolve(root, name, version)?;
        for node in nodes {
            installed.push((node.name.clone(), node.version.clone()));
            order.push(node);
        }
    }
    let names: Vec<String> = order.iter().map(label).collect();
    let header = Header {
        format: FORMAT,
        refhir: env!("CARGO_PKG_VERSION").to_string(),
        packages: names.clone(),
        sources: order
            .iter()
            .filter_map(|node| {
                Some(Source {
                    package: label(node),
                    sha256: source_digest(&node.staged.dir)?,
                })
            })
            .collect(),
    };
    let failed = |err: &dyn std::fmt::Display| {
        FhirError::Storage(format!("prepared packages {}: {err}", out.display()))
    };
    if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|err| failed(&err))?;
    }
    let mut partial = out.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    let written = write_file(&partial, &header, &order)
        .and_then(|()| fs::rename(&partial, out).map_err(|err| failed(&err)));
    if written.is_err() {
        let _ = fs::remove_file(&partial);
    }
    written?;
    Ok(names)
}

fn label(node: &Node) -> String {
    format!("{}@{}", node.name, node.version)
}

/// The header, then each of `order` read and written in turn, to `path`, which is flushed to the
/// disk before this returns.
fn write_file(path: &Path, header: &Header, order: &[Node]) -> Result<(), FhirError> {
    let failed = |err: &dyn std::fmt::Display| {
        FhirError::Storage(format!("prepared packages {}: {err}", path.display()))
    };
    let mut writer = GzEncoder::new(
        BufWriter::new(File::create(path).map_err(|err| failed(&err))?),
        Compression::new(6),
    );
    write_value(&mut writer, header).map_err(|err| failed(&err))?;
    for node in order {
        let started = Instant::now();
        let package = read_package(&node.staged.dir)?;
        write_value(&mut writer, &package).map_err(|err| failed(&err))?;
        tracing::info!(
            target: "refhir_engine",
            "prepared {}@{}: {} definitions, {} search parameters, {} terminology resources, {} expansions in {:.1?}{}",
            node.name,
            node.version,
            package.structure_definitions.len(),
            package.search_parameters.len(),
            package.terminology.len(),
            package.expansions.len(),
            started.elapsed(),
            source_digest(&node.staged.dir)
                .map(|digest| format!(" (source sha256 {digest})"))
                .unwrap_or_default(),
        );
    }
    let file = writer
        .finish()
        .and_then(|buffered| buffered.into_inner().map_err(|err| err.into_error()))
        .map_err(|err| failed(&err))?;
    file.sync_all().map_err(|err| failed(&err))
}

/// One value and a line break.
fn write_value(writer: &mut impl Write, value: &impl Serialize) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")
}

type Packages = serde_json::StreamDeserializer<
    'static,
    serde_json::de::IoRead<BufReader<GzDecoder<File>>>,
    NewPackage,
>;

/// A prepared file being read. It is an iterator over its packages, one at a time, so reading
/// holds a package and not the file.
pub struct PreparedBundle {
    names: Vec<String>,
    packages: Packages,
    origin: String,
}

impl PreparedBundle {
    pub fn open(path: &Path) -> Result<Self, FhirError> {
        let origin = path.display().to_string();
        let invalid =
            |what: String| FhirError::Invalid(format!("prepared packages {origin}: {what}"));
        let file = File::open(path).map_err(|err| invalid(err.to_string()))?;
        let mut values =
            serde_json::Deserializer::from_reader(BufReader::new(GzDecoder::new(file)));
        let header = Header::deserialize(&mut values).map_err(|err| invalid(err.to_string()))?;
        if header.format != FORMAT {
            return Err(invalid(format!(
                "format {} was written by ReFHIR {}, and this one reads format {FORMAT}",
                header.format, header.refhir
            )));
        }
        Ok(Self {
            names: header.packages,
            packages: values.into_iter(),
            origin,
        })
    }

    /// `name@version` of each package in the file, in the order to install them.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Whether every one of `specs` (`name@version`) is in the file.
    pub fn covers(&self, specs: &[&str]) -> bool {
        specs
            .iter()
            .all(|spec| self.names.iter().any(|name| name == spec))
    }
}

impl Iterator for PreparedBundle {
    type Item = Result<NewPackage, FhirError>;

    fn next(&mut self) -> Option<Self::Item> {
        let next = self.packages.next()?;
        Some(
            next.map_err(|err| {
                FhirError::Invalid(format!("prepared packages {}: {err}", self.origin))
            }),
        )
    }
}

/// Install the packages in the prepared file at `path` that the database does not have, and
/// publish them. `specs` (`name@version`) are the packages that were asked for: a file that does
/// not hold all of them is not used, and `None` comes back; what to do then is the caller's (the
/// server stops, it does not go and download). Otherwise the number of packages installed comes
/// back, which is none when the database has them all.
///
/// A package is stored as installing it from the registry would store it, and a version at least
/// as new as the one in the file that is already installed is used as it is. A file that holds
/// a different number of packages than its header lists is damaged and is refused. If the file
/// turns out to be damaged part of the way through, the packages before the damage are installed
/// and published, and the error comes back.
pub fn install_prepared(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    path: &Path,
    specs: &[&str],
) -> Result<Option<usize>, FhirError> {
    let mut bundle = PreparedBundle::open(path)?;
    if !bundle.covers(specs) {
        return Ok(None);
    }
    let mut newest: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for package in store.load_packages().map_err(FhirError::from)? {
        let newer = newest
            .get(&package.name)
            .is_none_or(|current| compare_versions(&package.version, current).is_gt());
        if newer {
            newest.insert(package.name, package.version);
        }
    }
    let expected = bundle.names.len();
    let mut seen = 0;
    let mut staged = 0;
    let mut pending: Vec<PendingBackfill> = Vec::new();
    let mut result = Ok(());
    loop {
        let started = Instant::now();
        let package = match bundle.next() {
            None => break,
            Some(Ok(package)) => package,
            Some(Err(err)) => {
                result = Err(err);
                break;
            }
        };
        seen += 1;
        if newest
            .get(&package.name)
            .is_some_and(|have| compare_versions(have, &package.version).is_ge())
        {
            continue;
        }
        staged += 1;
        match stage_new_package(
            store,
            db,
            catalog,
            package,
            started.elapsed(),
            "load",
            false,
        ) {
            Ok(backfill) => pending.extend(backfill),
            Err(err) => {
                result = Err(err);
                break;
            }
        }
    }
    // A stream that ends before the header's list does, or goes on past it, is not the file that
    // was made: the packages it was made to carry may not be in it.
    if result.is_ok() && seen != expected {
        result = Err(FhirError::Invalid(format!(
            "prepared packages {}: its header lists {expected} packages and it holds {seen}",
            bundle.origin
        )));
    }
    if staged == 0 {
        // Nothing new, but an earlier attempt may have left backfills behind.
        run_backfills(store, db, catalog)?;
        return result.map(|()| Some(0));
    }
    finish_install(store, db, catalog, &pending, result)?;
    Ok(Some(staged))
}

#[cfg(test)]
mod tests {
    use refhir_persistence::{
        NewExpansion, NewSearchParameter, NewStructureDefinition, NewTerminology, ParamStatus,
    };

    use super::*;

    /// A registry that must not be reached.
    struct Refuses;

    impl PackageFetcher for Refuses {
        fn fetch(&self, name: &str, version: &str, _dest: &Path) -> Result<(), FhirError> {
            Err(FhirError::Invalid(format!("{name}@{version} was fetched")))
        }
    }

    /// What a prepared file holds is part of what ships, so it is frozen here. If this fails, the
    /// bytes changed, and a file made before the change cannot be read as it stands: bump
    /// [`FORMAT`], and either teach the reader the old form or accept that old files are refused.
    #[test]
    fn what_a_prepared_file_holds_is_frozen() {
        let package = NewPackage {
            name: "example.pkg".into(),
            version: "1.0.0".into(),
            fhir_version: "4.0.1".into(),
            structure_definitions: vec![NewStructureDefinition {
                url: "http://example.org/sd".into(),
                type_name: "Thing".into(),
                kind: "resource".into(),
                is_abstract: false,
                json: "{}".into(),
            }],
            search_parameters: vec![NewSearchParameter {
                url: "http://example.org/sp".into(),
                base: "Thing".into(),
                code: "x".into(),
                param_type: "token".into(),
                expression: Some("Thing.x".into()),
                status: ParamStatus::Pending,
                json: "{}".into(),
            }],
            terminology: vec![NewTerminology {
                resource_type: "CodeSystem".into(),
                url: "http://example.org/cs".into(),
                json: "{}".into(),
            }],
            expansions: vec![NewExpansion {
                url: "http://example.org/vs".into(),
                json: "{}".into(),
            }],
        };
        let frozen = r#"{"name":"example.pkg","version":"1.0.0","fhir_version":"4.0.1","structure_definitions":[{"url":"http://example.org/sd","type_name":"Thing","kind":"resource","is_abstract":false,"json":"{}"}],"search_parameters":[{"url":"http://example.org/sp","base":"Thing","code":"x","param_type":"token","expression":"Thing.x","status":"pending","json":"{}"}],"terminology":[{"resource_type":"CodeSystem","url":"http://example.org/cs","json":"{}"}],"expansions":[{"url":"http://example.org/vs","json":"{}"}]}"#;
        assert_eq!(serde_json::to_string(&package).unwrap(), frozen);
        let read: NewPackage = serde_json::from_str(frozen).unwrap();
        assert_eq!(read.search_parameters[0].status, ParamStatus::Pending);
        assert_eq!(read.expansions[0].url, "http://example.org/vs");

        let header = Header {
            format: FORMAT,
            refhir: "0.0.1".into(),
            packages: vec!["example.pkg@1.0.0".into()],
            sources: vec![Source {
                package: "example.pkg@1.0.0".into(),
                sha256: "ab".repeat(32),
            }],
        };
        assert_eq!(
            serde_json::to_string(&header).unwrap(),
            format!(
                r#"{{"format":1,"refhir":"0.0.1","packages":["example.pkg@1.0.0"],"sources":[{{"package":"example.pkg@1.0.0","sha256":"{}"}}]}}"#,
                "ab".repeat(32)
            )
        );
        // The header written for a package that came from a registry carries the digest of the
        // tarball it was downloaded from.
        let scratch =
            std::env::temp_dir().join(format!("refhir-prepared-{}", uuid::Uuid::new_v4()));
        let cached = scratch.join("cache").join("example.pkg").join("1.0.0");
        fs::create_dir_all(&cached).unwrap();
        fs::write(
            cached.join("package.json"),
            r#"{"name":"example.pkg","version":"1.0.0","fhirVersion":"4.0.1"}"#,
        )
        .unwrap();
        fs::write(cached.join(".complete"), "ab".repeat(32)).unwrap();
        let out = scratch.join("out.bundle");
        write_prepared(
            &out,
            &["example.pkg@1.0.0".to_string()],
            &Refuses,
            &scratch.join("cache"),
        )
        .unwrap();
        let mut values = serde_json::Deserializer::from_reader(BufReader::new(GzDecoder::new(
            File::open(&out).unwrap(),
        )));
        let written = Header::deserialize(&mut values).unwrap();
        assert_eq!(written.sources.len(), 1);
        assert_eq!(written.sources[0].package, "example.pkg@1.0.0");
        assert_eq!(written.sources[0].sha256, "ab".repeat(32));
        let _ = fs::remove_dir_all(scratch);

        // A header written without the digests reads, and says it has none.
        let old: Header =
            serde_json::from_str(r#"{"format":1,"refhir":"0.0.1","packages":[]}"#).unwrap();
        assert!(old.sources.is_empty());
    }
}
