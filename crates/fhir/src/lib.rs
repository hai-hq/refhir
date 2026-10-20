//! FHIR resource catalog and operation handlers.
//!
//! Handlers are Rust functions on `Transaction`. Operation names look like `Patient/read`.

mod builtin;
mod bundle;
mod cards;
mod catalog;
mod compartment;
mod conform;
mod document;
mod error;
mod extract;
mod history;
mod instant;
mod lazy;
mod memory;
mod meta;
mod npm;
mod ops;
mod package;
mod patch;
mod patient_match;
mod prepared;
mod resolve;
mod run;
mod s3;
mod sdc;
mod search;
mod summary;
mod terms;

#[cfg(test)]
mod tests;

pub use builtin::{install_builtin_terminology, BUILTIN_PACKAGE};
pub use bundle::transaction;
pub use catalog::{Catalog, CatalogClock, CatalogStamp, ReferencePolicy};
pub use conform::{commit_operation, repair_conformance};
pub use error::{outcome_code, FhirError};
pub use memory::release_unused_memory;
pub use npm::{
    install_package, install_with, package_downloads_allowed, packages_installed,
    parse_package_spec, unexpandable_value_sets, NpmRegistry, PackageFetcher, PackageSpec,
    DOWNLOAD_VARIABLE,
};
pub use package::{
    cards_dir, cross_version_dir, default_r4_registry_specs, expression_clash_dir,
    extension_copies_dir, extra_dir, install_package_dir, ips_package, minir4_dir,
    newer_terminology_dir, patient_match_dir, r4_dir, r4_examples_dir, reload_catalog,
    run_backfills, sdc_dir, stage_package_dir, version_clash_dir, watch_catalog, CatalogWatcher,
    R4_CORE_PACKAGE, SDC_PACKAGE,
};
pub use prepared::{install_prepared, write_prepared, PreparedBundle, BUNDLE_FILE};
pub use refhir_persistence::ConformanceStore;
pub use run::{
    dispatch, import_resource, run, stamp_stored_json, RunError, RunResult, CONTROL_ARGS,
    MAX_OCC_FAILURES,
};
pub use search::{
    add_param, join_repeated, resource_in_compartment, AND, DEFAULT_COUNT, HANDLING,
    PATIENT_COMPARTMENT,
};
pub use terms::INSTANCE;
