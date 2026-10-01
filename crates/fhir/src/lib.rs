//! FHIR resource catalog and operation handlers.
//!
//! Convex runs user functions in V8. HaiFHIR handlers are Rust functions on
//! `Transaction`. Operation names look like `Patient/read`.

mod bundle;
mod catalog;
mod error;
mod extract;
mod history;
mod ops;
mod package;
mod resolve;
mod run;
mod search;

#[cfg(test)]
mod tests;

pub use bundle::transaction;
pub use catalog::Catalog;
pub use error::{outcome_code, FhirError};
pub use package::{
    expression_clash_dir, extra_dir, install_package_dir, minir4_dir, r4_dir, run_backfills,
    stage_package_dir, version_clash_dir,
};
pub use run::{dispatch, run, stamp_stored_json, RunError, RunResult, MAX_OCC_FAILURES};
