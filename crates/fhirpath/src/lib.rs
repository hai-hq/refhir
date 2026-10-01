//! FHIRPath 2.0.0 (normative N1).
//!
//! The crate evaluates expressions against FHIR JSON. Search indexing and
//! reference lookup stay in the host: this crate does not open a database.

mod ast;
mod error;
mod host;
mod model;
mod parse;
mod ucum;
mod value;

pub use ast::Expr;
pub use error::{Error, ErrorKind};
pub use host::{days_from_civil, Host};
pub use model::Model;
pub use parse::parse;
pub use value::Value;

use host::EmptyHost;
use serde_json::Value as Json;

use crate::eval::evaluate_expr;

mod eval;

pub fn evaluate(expr: &Expr, root: &Json, model: &Model, host: &dyn Host) -> Result<Vec<Value>, Error> {
    evaluate_expr(expr, root, model, host)
}

pub fn evaluate_owned(expression: &str, root: &Json) -> Result<Vec<Value>, Error> {
    let expr = parse(expression)?;
    evaluate(&expr, root, &Model::new(), &EmptyHost)
}
