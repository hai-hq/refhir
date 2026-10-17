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
//! then, for each package, a value that names it and values that hold its rows a few megabytes
//! at a time. A package is stored as the file is read, so installing the largest of them takes
//! the memory of one of those values and not of the package: the default packages' largest is
//! over a hundred megabytes of JSON.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Instant;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use refhir_database::Database;
use refhir_persistence::{
    ConformanceStore, InstallError, NewExpansion, NewPackage, NewSearchParameter,
    NewStructureDefinition, NewTerminology, PackageRows, PersistError,
};
use serde::{Deserialize, Serialize};

use crate::catalog::Catalog;
use crate::error::FhirError;
use crate::npm::{
    compare_versions, materialize, parse_package_spec, source_digest, Node, PackageFetcher,
    Resolver,
};
use crate::package::{
    finish_install, package_identity, read_package, run_backfills, stage_package_in_pieces,
    PackageHead, PendingBackfill,
};

/// The name a prepared file is given when nothing else names it, and the one looked for next to
/// the server.
pub const BUNDLE_FILE: &str = "refhir-packages.bundle";

/// Bumped when what the file holds changes in a way an older server cannot read. The bytes are
/// the serde form of the header, of [`PieceRef`] and of the row types in `refhir-persistence`,
/// which is why a test below freezes them: a file made by one build is read by another (a
/// release's archive beside a newer binary, an image layer), and a field added to those without a
/// bump would make every such file unreadable.
const FORMAT: u32 = 2;

/// About how much JSON text one value of rows holds. It bounds what installing a package from the
/// file holds at once; a row larger than that is a value of its own.
const PIECE_BYTES: usize = 4 * 1024 * 1024;

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

/// A value of the file after its header, as it is read. A `Package` says which package the rows
/// that follow belong to, up to the next `Package`.
#[derive(Deserialize)]
enum Piece {
    Package {
        name: String,
        version: String,
        fhir_version: String,
    },
    StructureDefinitions(Vec<NewStructureDefinition>),
    SearchParameters(Vec<NewSearchParameter>),
    Terminology(Vec<NewTerminology>),
    Expansions(Vec<NewExpansion>),
}

/// [`Piece`] as it is written, over rows that are borrowed. The two are the same in the file.
#[derive(Serialize)]
enum PieceRef<'a> {
    Package {
        name: &'a str,
        version: &'a str,
        fhir_version: &'a str,
    },
    StructureDefinitions(&'a [NewStructureDefinition]),
    SearchParameters(&'a [NewSearchParameter]),
    Terminology(&'a [NewTerminology]),
    Expansions(&'a [NewExpansion]),
}

/// `rows` in runs of about `piece_bytes` of text, at least one row each.
fn pieces_of<T>(rows: &[T], text_len: impl Fn(&T) -> usize, piece_bytes: usize) -> Vec<&[T]> {
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < rows.len() {
        let mut end = start;
        let mut bytes = 0;
        while end < rows.len() {
            let next = text_len(&rows[end]);
            if end > start && bytes + next > piece_bytes {
                break;
            }
            bytes += next;
            end += 1;
        }
        pieces.push(&rows[start..end]);
        start = end;
    }
    pieces
}

/// The values a package is written as: it is named, and then its rows follow, a few megabytes at
/// a time, in the order they are stored.
fn pieces_of_package(package: &NewPackage, piece_bytes: usize) -> Vec<PieceRef<'_>> {
    let mut pieces = vec![PieceRef::Package {
        name: &package.name,
        version: &package.version,
        fhir_version: &package.fhir_version,
    }];
    pieces.extend(
        pieces_of(
            &package.structure_definitions,
            |row| row.json.len(),
            piece_bytes,
        )
        .into_iter()
        .map(PieceRef::StructureDefinitions),
    );
    pieces.extend(
        pieces_of(
            &package.search_parameters,
            |row| row.json.len(),
            piece_bytes,
        )
        .into_iter()
        .map(PieceRef::SearchParameters),
    );
    pieces.extend(
        pieces_of(&package.terminology, |row| row.json.len(), piece_bytes)
            .into_iter()
            .map(PieceRef::Terminology),
    );
    pieces.extend(
        pieces_of(&package.expansions, |row| row.json.len(), piece_bytes)
            .into_iter()
            .map(PieceRef::Expansions),
    );
    pieces
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
    write_prepared_in_pieces(out, specs, fetcher, cache, PIECE_BYTES)
}

/// [`write_prepared`], with the rows of a package cut into values of about `piece_bytes` of
/// text.
pub(crate) fn write_prepared_in_pieces(
    out: &Path,
    specs: &[String],
    fetcher: &dyn PackageFetcher,
    cache: &Path,
    piece_bytes: usize,
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
    let written = write_file(&partial, &header, &order, piece_bytes)
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
fn write_file(
    path: &Path,
    header: &Header,
    order: &[Node],
    piece_bytes: usize,
) -> Result<(), FhirError> {
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
        for piece in pieces_of_package(&package, piece_bytes) {
            write_value(&mut writer, &piece).map_err(|err| failed(&err))?;
        }
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

type Pieces = serde_json::StreamDeserializer<
    'static,
    serde_json::de::IoRead<BufReader<GzDecoder<File>>>,
    Piece,
>;

/// A prepared file being read. It is read a value at a time, so reading holds a few megabytes of
/// rows and not the file. Iterating gives each package whole; [`install_prepared`] stores them a
/// few rows at a time instead.
pub struct PreparedBundle {
    names: Vec<String>,
    /// What each package was made from, as the header lists it.
    sources: Vec<Source>,
    pieces: Pieces,
    origin: String,
    /// The value that was read to find where the last package ended, which starts the next.
    ahead: Option<Piece>,
    /// Why the last read failed, when it did: the file is damaged or cannot be read.
    failure: Option<FhirError>,
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
            sources: header.sources,
            pieces: values.into_iter(),
            origin,
            ahead: None,
            failure: None,
        })
    }

    /// `name@version` of each package in the file, in the order to install them.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// A digest of what the package `name@version` was made from and the format it was prepared
    /// in, when the file says. It identifies the package's content: two files that give the same
    /// digest for a package were made from the same source in the same format, so a store may
    /// keep one copy for both. A new format is a new identity, so what a release prepares
    /// differently is not served from a copy an older release prepared.
    pub(crate) fn content_of(&self, name: &str, version: &str) -> Option<String> {
        let spec = format!("{name}@{version}");
        self.sources
            .iter()
            .find(|source| source.package == spec)
            .map(|source| format!("{}:{FORMAT}", source.sha256))
    }

    /// Whether every one of `specs` (`name@version`) is in the file.
    pub fn covers(&self, specs: &[&str]) -> bool {
        specs
            .iter()
            .all(|spec| self.names.iter().any(|name| name == spec))
    }

    fn damaged(&self, what: impl std::fmt::Display) -> FhirError {
        FhirError::Invalid(format!("prepared packages {}: {what}", self.origin))
    }

    /// The next value of the file, or `None` at its end.
    fn next_piece(&mut self) -> Result<Option<Piece>, FhirError> {
        if let Some(piece) = self.ahead.take() {
            return Ok(Some(piece));
        }
        match self.pieces.next() {
            None => Ok(None),
            Some(Ok(piece)) => Ok(Some(piece)),
            Some(Err(err)) => Err(self.damaged(err)),
        }
    }

    /// The package that comes next, or `None` when there is none. What is left of the last one
    /// must have been taken by [`Self::feed`] or [`Self::skip_package`] first.
    pub(crate) fn next_package(&mut self) -> Result<Option<PackageHead>, FhirError> {
        match self.next_piece()? {
            None => Ok(None),
            Some(Piece::Package {
                name,
                version,
                fhir_version,
            }) => Ok(Some(PackageHead {
                name,
                version,
                fhir_version,
            })),
            Some(_) => Err(self.damaged("rows come before the package they belong to")),
        }
    }

    /// Hand the rows of the package just named to `rows`, up to where the next package starts.
    /// A file that cannot be read fails the install it is feeding, and the reason is kept for
    /// [`Self::take_failure`], which says it better than the install can.
    pub(crate) fn feed(&mut self, rows: &mut dyn PackageRows) -> Result<(), InstallError> {
        loop {
            let piece = match self.next_piece() {
                Ok(piece) => piece,
                Err(err) => {
                    let reason = err.to_string();
                    self.failure = Some(err);
                    return Err(InstallError::Persist(PersistError::Definite(reason)));
                }
            };
            match piece {
                None => return Ok(()),
                Some(piece @ Piece::Package { .. }) => {
                    self.ahead = Some(piece);
                    return Ok(());
                }
                Some(Piece::StructureDefinitions(batch)) => rows.structure_definitions(&batch)?,
                Some(Piece::SearchParameters(batch)) => rows.search_parameters(&batch)?,
                Some(Piece::Terminology(batch)) => rows.terminology(&batch)?,
                Some(Piece::Expansions(batch)) => rows.expansions(&batch)?,
            }
        }
    }

    /// Pass over the rows of the package just named.
    pub(crate) fn skip_package(&mut self) -> Result<(), FhirError> {
        loop {
            match self.next_piece()? {
                None => return Ok(()),
                Some(piece @ Piece::Package { .. }) => {
                    self.ahead = Some(piece);
                    return Ok(());
                }
                Some(_) => {}
            }
        }
    }

    /// Why the last [`Self::feed`] could not read the file, if that is what failed it.
    pub(crate) fn take_failure(&mut self) -> Option<FhirError> {
        self.failure.take()
    }
}

/// Each package of the file, whole.
impl Iterator for PreparedBundle {
    type Item = Result<NewPackage, FhirError>;

    fn next(&mut self) -> Option<Self::Item> {
        let head = match self.next_package() {
            Ok(head) => head?,
            Err(err) => return Some(Err(err)),
        };
        let mut package = NewPackage {
            name: head.name,
            version: head.version,
            fhir_version: head.fhir_version,
            structure_definitions: Vec::new(),
            search_parameters: Vec::new(),
            terminology: Vec::new(),
            expansions: Vec::new(),
        };
        match self.feed(&mut package) {
            Ok(()) => Some(Ok(package)),
            Err(err) => Some(Err(self.take_failure().unwrap_or_else(|| {
                FhirError::Invalid(format!("prepared packages {}: {err}", self.origin))
            }))),
        }
    }
}

/// Install the packages in the prepared file at `path` that the database does not have, and
/// publish them. `specs` (`name@version`) are the packages that were asked for: a file that does
/// not hold all of them is not used, and `None` comes back; what to do then is the caller's (the
/// server stops, it does not go and download). Otherwise the number of packages installed comes
/// back, which is none when the database has them all.
///
/// A package is stored as installing it from the registry would store it, and a version at least
/// as new as the one in the file that is already installed is used as it is. A file that holds a
/// different number of packages than its header lists is damaged and is refused. If the file
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
        let head = match bundle.next_package() {
            Ok(None) => break,
            Ok(Some(head)) => head,
            Err(err) => {
                result = Err(err);
                break;
            }
        };
        seen += 1;
        if newest
            .get(&head.name)
            .is_some_and(|have| compare_versions(have, &head.version).is_ge())
        {
            if let Err(err) = bundle.skip_package() {
                result = Err(err);
                break;
            }
            continue;
        }
        staged += 1;
        let content = bundle.content_of(&head.name, &head.version);
        // A store that gives the package from a copy it already holds does not ask for its rows.
        let mut read = false;
        let stored = stage_package_in_pieces(
            store,
            db,
            catalog,
            &head,
            content.as_deref(),
            &mut |rows| {
                read = true;
                bundle.feed(rows)
            },
            false,
        );
        match stored {
            Ok(backfill) => {
                pending.extend(backfill);
                // The next package starts where this one ends, so what was not read is passed over.
                if !read {
                    if let Err(err) = bundle.skip_package() {
                        result = Err(err);
                        break;
                    }
                }
            }
            Err(err) => {
                // A file that cannot be read says why better than the install it failed.
                result = Err(bundle.take_failure().unwrap_or(err));
                break;
            }
        }
    }
    // A stream that ends before the header's list does, or goes on past it, is not the file that
    // was made: the packages it was made to carry may not be in it.
    if result.is_ok() && seen != expected {
        result = Err(bundle.damaged(format!(
            "its header lists {expected} packages and it holds {seen}"
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
    use refhir_persistence::ParamStatus;

    use super::*;

    fn text(len: usize) -> NewTerminology {
        NewTerminology {
            resource_type: "CodeSystem".into(),
            url: "http://example.org/s".into(),
            json: "x".repeat(len),
        }
    }

    #[test]
    fn rows_are_cut_by_the_text_they_hold_and_every_row_goes_somewhere() {
        let lens = |pieces: Vec<&[NewTerminology]>| -> Vec<Vec<usize>> {
            pieces
                .into_iter()
                .map(|piece| piece.iter().map(|row| row.json.len()).collect())
                .collect()
        };
        let rows: Vec<NewTerminology> = [3, 4, 5, 20, 1, 1].into_iter().map(text).collect();
        let cut = |bytes| lens(pieces_of(&rows, |row| row.json.len(), bytes));
        // A value takes rows while they fit, a row that does not fit starts the next, and one
        // larger than the limit is a value of its own.
        assert_eq!(cut(8), vec![vec![3, 4], vec![5], vec![20], vec![1, 1]]);
        assert_eq!(
            cut(1),
            vec![vec![3], vec![4], vec![5], vec![20], vec![1], vec![1]]
        );
        assert_eq!(cut(1000), vec![vec![3, 4, 5, 20, 1, 1]]);
        assert!(pieces_of::<NewTerminology>(&[], |row| row.json.len(), 8).is_empty());
        // Whatever the limit, the rows come back in order.
        for bytes in [1, 7, 9, 29, 34] {
            assert_eq!(cut(bytes).concat(), vec![3, 4, 5, 20, 1, 1]);
        }
    }

    #[test]
    fn a_package_is_named_first_and_its_rows_follow_in_the_order_they_are_stored() {
        let package = NewPackage {
            name: "example.pkg".into(),
            version: "1.0.0".into(),
            fhir_version: "4.0.1".into(),
            structure_definitions: Vec::new(),
            search_parameters: Vec::new(),
            terminology: (0..6).map(|_| text(100)).collect(),
            expansions: vec![NewExpansion {
                url: "http://example.org/vs".into(),
                json: "{}".into(),
            }],
        };
        let kinds = |bytes| -> Vec<&'static str> {
            pieces_of_package(&package, bytes)
                .iter()
                .map(|piece| match piece {
                    PieceRef::Package { .. } => "package",
                    PieceRef::StructureDefinitions(_) => "definitions",
                    PieceRef::SearchParameters(_) => "parameters",
                    PieceRef::Terminology(_) => "terminology",
                    PieceRef::Expansions(_) => "expansions",
                })
                .collect()
        };
        assert_eq!(kinds(PIECE_BYTES), ["package", "terminology", "expansions"]);
        assert_eq!(
            kinds(250),
            [
                "package",
                "terminology",
                "terminology",
                "terminology",
                "expansions"
            ]
        );
    }

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
        let frozen = [
            r#"{"Package":{"name":"example.pkg","version":"1.0.0","fhir_version":"4.0.1"}}"#,
            r#"{"StructureDefinitions":[{"url":"http://example.org/sd","type_name":"Thing","kind":"resource","is_abstract":false,"json":"{}"}]}"#,
            r#"{"SearchParameters":[{"url":"http://example.org/sp","base":"Thing","code":"x","param_type":"token","expression":"Thing.x","status":"pending","json":"{}"}]}"#,
            r#"{"Terminology":[{"resource_type":"CodeSystem","url":"http://example.org/cs","json":"{}"}]}"#,
            r#"{"Expansions":[{"url":"http://example.org/vs","json":"{}"}]}"#,
        ];
        let written: Vec<String> = pieces_of_package(&package, PIECE_BYTES)
            .iter()
            .map(|piece| serde_json::to_string(piece).unwrap())
            .collect();
        assert_eq!(written, frozen);
        // And what was written reads back as the rows it was made from.
        let read: Vec<Piece> = frozen
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(matches!(&read[0], Piece::Package { name, .. } if name == "example.pkg"));
        assert!(
            matches!(&read[2], Piece::SearchParameters(rows) if rows[0].status == ParamStatus::Pending)
        );
        assert!(
            matches!(&read[4], Piece::Expansions(rows) if rows[0].url == "http://example.org/vs")
        );

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
                r#"{{"format":2,"refhir":"0.0.1","packages":["example.pkg@1.0.0"],"sources":[{{"package":"example.pkg@1.0.0","sha256":"{}"}}]}}"#,
                "ab".repeat(32)
            )
        );
        // A header written without the digests reads, and says it has none.
        let old: Header =
            serde_json::from_str(r#"{"format":2,"refhir":"0.0.1","packages":[]}"#).unwrap();
        assert!(old.sources.is_empty());
    }

    /// The header written for a package that came from a registry carries the digest of the
    /// tarball it was downloaded from.
    #[test]
    fn the_header_records_the_digest_of_a_downloaded_package() {
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
        // What a store keeps one copy by: the source and the format it was prepared in, so a
        // format a later release writes is not served from a copy this one made.
        let bundle = PreparedBundle::open(&out).unwrap();
        assert_eq!(
            bundle.content_of("example.pkg", "1.0.0"),
            Some(format!("{}:{FORMAT}", "ab".repeat(32)))
        );
        assert_eq!(bundle.content_of("example.pkg", "2.0.0"), None);
        let _ = fs::remove_dir_all(scratch);
    }
}
