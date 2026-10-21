//! Terminology the server carries itself.
//!
//! The FHIR packages a server installs refer to code systems that no package holds: HL7 publishes
//! `ValueSet/currencies` as a whole-system include of ISO 4217, `Money.currency` is bound to it
//! (`required`), and there is no CodeSystem for it in R4 core, in SDC, or in the terminology
//! package. Without one, the value set cannot be expanded, a code cannot be checked against it, and
//! a client that expands every value set a form binds is left with an error.
//!
//! These are installed like a package: stored under their own name and version, so that they are
//! there for a database that already had the R4 packages, and installed once. The value sets that
//! were waiting for them are expanded as they arrive, by the incremental path a code system that
//! is written uses, which reads only those value sets and does not load the catalog again. (Any
//! package, installed before or after, is also followed by a pass over the value sets that have
//! no expansion; see `package::expand_waiting`. This one need not wait for that.)
//! Changing what they hold is a new version, which a database takes in addition to the old one.

use std::sync::RwLock;

use refhir_database::Database;
use refhir_persistence::{ConformanceStore, NewPackage};
use serde_json::Value;

use crate::catalog::Catalog;
use crate::conform::apply_terminology;
use crate::error::FhirError;
use crate::npm::packages_installed;
use crate::package::{announce_terminology, stage_new_package, terminology_row};

/// The name the built-in terminology is stored under, with the version of what it holds.
pub const BUILTIN_PACKAGE: &str = "refhir.builtin.terminology@1.0.0";

/// The resources the package holds. ISO 4217 is the active alphabetic currency codes, with their
/// numeric codes and English names.
const RESOURCES: [&str; 1] = [include_str!("../builtin/CodeSystem-iso4217.json")];

fn builtin_package() -> Result<NewPackage, FhirError> {
    let (name, version) = BUILTIN_PACKAGE
        .split_once('@')
        .expect("the built-in package is name@version");
    let mut terminology = Vec::new();
    for text in RESOURCES {
        let resource: Value = serde_json::from_str(text)
            .map_err(|err| FhirError::Invalid(format!("built-in terminology: {err}")))?;
        terminology.push(terminology_row(&resource)?);
    }
    Ok(NewPackage {
        name: name.to_string(),
        version: version.to_string(),
        // The release these code systems are read for, which is the only one this server serves.
        fhir_version: "4.0.1".to_string(),
        structure_definitions: Vec::new(),
        search_parameters: Vec::new(),
        terminology,
        // A code system has no expansion of its own, and the value sets that include it are in
        // other packages, whose expansions are made when it arrives.
        expansions: Vec::new(),
    })
}

/// Install the built-in terminology, unless this database has it or has no other package. A
/// database that has already expanded every value set it holds gains the expansions that were
/// waiting for it.
pub fn install_builtin_terminology(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
) -> Result<(), FhirError> {
    if packages_installed(store, &[BUILTIN_PACKAGE])? {
        return Ok(());
    }
    // With no other package there is nothing that refers to it, and a replica starts serving once
    // any package is stored: it must not be left with only this. A server that is given its
    // packages later has it at its next start.
    if store.load_packages()?.is_empty() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let package = builtin_package()?;
    let urls: Vec<String> = package
        .terminology
        .iter()
        .map(|row| row.url.clone())
        .collect();
    let (name, version) = (package.name.clone(), package.version.clone());
    // Not published: the catalog is brought up to date below, as it is for a code system that is
    // written, which reads only the value sets that include what arrives and does not load the
    // whole catalog again.
    let pending = stage_new_package(
        store,
        db,
        catalog,
        package,
        started.elapsed(),
        "read",
        false,
    )?;
    // The value sets that were waiting for what this carries are in other packages, and are
    // expanded now, without the catalog being loaded again.
    let package_id = store
        .load_packages()?
        .into_iter()
        .find(|package| package.name == name && package.version == version)
        .map(|package| package.id)
        .ok_or_else(|| FhirError::Storage(format!("{name}@{version} was not stored")))?;
    apply_terminology(store, catalog, package_id, &urls, &[])?;
    if let Some(pending) = pending {
        pending.create(store, db)?;
    }
    announce_terminology(store)
}
