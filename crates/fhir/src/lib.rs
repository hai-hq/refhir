//! FHIR resource catalog and operation handlers.
//!
//! Handlers are Rust functions on `Transaction`. Operation names look like `Patient/read`.

mod bundle;
mod catalog;
mod compartment;
mod conform;
mod error;
mod extract;
mod history;
mod meta;
mod npm;
mod ops;
mod package;
mod patch;
mod resolve;
mod run;
mod s3;
mod sdc;
mod search;
mod terms;

#[cfg(test)]
mod tests;

pub use bundle::transaction;
pub use catalog::{Catalog, CatalogClock, ReferencePolicy};
pub use conform::{commit_operation, repair_conformance};
pub use error::{outcome_code, FhirError};
pub use npm::{
    install_package, install_with, packages_installed, parse_package_spec, unexpandable_value_sets,
    NpmRegistry, PackageFetcher, PackageSpec,
};
pub use package::{
    cross_version_dir, default_r4_registry_specs, expression_clash_dir, extension_copies_dir,
    extra_dir, install_package_dir, minir4_dir, newer_terminology_dir, r4_dir, reload_catalog,
    run_backfills, sdc_dir, stage_package_dir, version_clash_dir, watch_catalog, CatalogWatcher,
    R4_CORE_PACKAGE, SDC_PACKAGE,
};
pub use refhir_persistence::ConformanceStore;
pub use run::{
    dispatch, import_resource, run, stamp_stored_json, RunError, RunResult, CONTROL_ARGS,
    MAX_OCC_FAILURES,
};
pub use search::{
    add_param, join_repeated, resource_in_compartment, AND, HANDLING, PATIENT_COMPARTMENT,
};
