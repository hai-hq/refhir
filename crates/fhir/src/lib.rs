//! FHIR resource catalog and operation handlers.
//!
//! Convex runs user functions in V8. HaiFHIR handlers are Rust functions on
//! `Transaction`. Operation names look like `Patient/read`.

mod bundle;
mod catalog;
mod error;
mod history;
mod ops;
mod run;
mod search;

#[cfg(test)]
mod tests;

pub use bundle::transaction;
pub use catalog::{capability_statement, ResourceType};
pub use error::{outcome_code, FhirError};
pub use run::{dispatch, run, stamp_stored_json, RunError, RunResult, MAX_OCC_FAILURES};
