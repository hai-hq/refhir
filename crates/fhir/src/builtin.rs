//! Terminology the server carries itself.
//!
//! The FHIR packages a server installs refer to code systems that no package holds: HL7 publishes
//! `ValueSet/currencies` as a whole-system include of ISO 4217, `Money.currency` is bound to it
//! (`required`), and there is no CodeSystem for it in R4 core, in SDC, or in the terminology
//! package. Without one, the value set cannot be expanded, a code cannot be checked against it, and
//! a client that expands every value set a form binds is left with an error.
//!
//! These are installed like a package of two files: stored under their own name and version, so
//! that they are there for a database that already had the R4 packages, and installed once.
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

/// ISO 4217: the active alphabetic currency codes, with their numeric codes and English names.
const ISO_4217: &str = include_str!("../builtin/CodeSystem-iso4217.json");

fn builtin_package() -> Result<NewPackage, FhirError> {
    let (name, version) = BUILTIN_PACKAGE
        .split_once('@')
        .expect("the built-in package is name@version");
    let mut terminology = Vec::new();
    for text in [ISO_4217] {
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

/// Install the built-in terminology, unless this database has it. A database that has already
/// expanded every value set it holds gains the expansions that were waiting for it.
pub fn install_builtin_terminology(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
) -> Result<(), FhirError> {
    if packages_installed(store, &[BUILTIN_PACKAGE])? {
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
    stage_new_package(store, db, catalog, package, started.elapsed(), "read", true)?;
    // Installing a package stores the expansions of its own value sets. The ones that were
    // waiting for what it carries are in other packages, and are expanded now, as they are when a
    // code system is written: by reading only the value sets that include it.
    let package_id = store
        .load_packages()?
        .into_iter()
        .find(|package| package.name == name && package.version == version)
        .map(|package| package.id)
        .ok_or_else(|| FhirError::Storage(format!("{name}@{version} was not stored")))?;
    apply_terminology(store, catalog, package_id, &urls, &[])?;
    announce_terminology(store)
}
