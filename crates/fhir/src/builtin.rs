//! Terminology that FHIR binds to and its packages do not carry.
//!
//! Some bindings are to code systems that belong to someone else and are not distributed with the
//! specification: `Money.currency` is bound to ISO 4217, whose codes the R4 core package never
//! includes. Without them the value set `http://hl7.org/fhir/ValueSet/currencies` cannot be
//! expanded, and a currency that is not one is only a warning. The code systems here are small,
//! and are stored as a package of their own, so they are one more version of a code system among
//! the others (a package a deployment installs with a newer list is used in preference).

use std::sync::RwLock;
use std::time::Duration;

use refhir_database::Database;
use refhir_persistence::{ConformanceStore, NewPackage};
use refhir_validate::TerminologyBuilder;
use serde_json::Value;

use crate::catalog::Catalog;
use crate::error::FhirError;
use crate::package::{expansion_rows, stage_new_package, terminology_row};

/// ISO 4217's alphabetic codes, as SIX Group (its maintenance agency) published them on the date
/// the code system's `version` gives.
const ISO_4217: &str = include_str!("../terminology/CodeSystem-iso4217.json");

/// R4's value set of currencies, which the core package defines and cannot expand for want of the
/// code system above. Stored beside it, the package can: a value set that two packages define is
/// answered by the one that has an expansion.
const CURRENCIES: &str = include_str!("../terminology/ValueSet-currencies.json");

/// The package the built-in code systems are stored as. Its version changes when a code system in
/// it does, and the server then stores the new one next to the old.
const PACKAGE: &str = "refhir.terminology";
const VERSION: &str = "2026.9.17";
/// The FHIR release the code systems are for: the one this server serves.
const RELEASE: &str = "4.0.1";

/// The package of built-in code systems, with the version the server would store.
fn package() -> Result<NewPackage, FhirError> {
    let mut terminology = Vec::new();
    let mut expanded = TerminologyBuilder::for_package(None);
    for (name, text) in [("ISO 4217", ISO_4217), ("currencies", CURRENCIES)] {
        let resource: Value = serde_json::from_str(text)
            .map_err(|err| FhirError::Invalid(format!("built-in {name}: {err}")))?;
        terminology.push(terminology_row(&resource)?);
        expanded.add_resource(0, resource);
    }
    Ok(NewPackage {
        name: PACKAGE.into(),
        version: VERSION.into(),
        fhir_version: RELEASE.into(),
        structure_definitions: Vec::new(),
        search_parameters: Vec::new(),
        terminology,
        expansions: expansion_rows(expanded, RELEASE),
    })
}

/// Store the built-in code systems unless this version of them is stored already, and publish
/// them. It says whether it stored them.
///
/// It is cheap to call on every start: the answer is one read of the list of packages. Publishing
/// rebuilds the catalog, which is the cost of it, so a database that is about to be given the
/// packages of its release uses [`stage_builtin_terminology`] and lets their publish include
/// these.
pub fn install_builtin_terminology(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
) -> Result<bool, FhirError> {
    store_builtin(store, db, catalog, true)
}

/// [`install_builtin_terminology`] without the publish: stored, and in the catalog at the next
/// publish, which is the one that ends installing the packages of the release.
pub fn stage_builtin_terminology(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
) -> Result<bool, FhirError> {
    store_builtin(store, db, catalog, false)
}

fn store_builtin(
    store: &dyn ConformanceStore,
    db: &Database,
    catalog: &RwLock<Catalog>,
    publish_now: bool,
) -> Result<bool, FhirError> {
    let installed = store.load_packages().map_err(FhirError::from)?;
    if installed
        .iter()
        .any(|package| package.name == PACKAGE && package.version == VERSION)
    {
        return Ok(false);
    }
    // The package has no search parameters, so there is no backfill to queue for it.
    stage_new_package(
        store,
        db,
        catalog,
        package()?,
        Duration::ZERO,
        "built in",
        publish_now,
    )?;
    Ok(true)
}
