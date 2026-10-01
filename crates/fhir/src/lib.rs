//! FHIR resource catalog and operation handlers.
//!
//! Convex runs user functions in V8. ReFHIR handlers are Rust functions on
//! `Transaction`. Operation names look like `Patient/read`.

mod bundle;
mod catalog;
mod compartment;
mod conform;
mod error;
mod extract;
mod history;
mod meta;
mod ops;
mod package;
mod patch;
mod resolve;
mod run;
mod search;
mod terms;

#[cfg(test)]
mod tests;

pub use bundle::transaction;
pub use catalog::{Catalog, ReferencePolicy};
pub use conform::{commit_operation, repair_conformance};
pub use error::{outcome_code, FhirError};
pub use package::{
    expression_clash_dir, extra_dir, install_package_dir, minir4_dir, r4_dir, run_backfills,
    stage_package_dir, version_clash_dir,
};
pub use refhir_persistence::ConformanceStore;
pub use run::{dispatch, run, stamp_stored_json, RunError, RunResult, MAX_OCC_FAILURES};
