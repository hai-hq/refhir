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
pub use eval::days_in_month;
pub use host::{days_from_civil, EmptyHost, Host};
pub use model::{Model, ModelBuilder};
pub use parse::parse;
pub use value::{Temporal, Value};

use serde_json::Value as Json;

use crate::eval::evaluate_expr;

mod eval;

pub fn evaluate(
    expr: &Expr,
    root: &Json,
    model: &Model,
    host: &dyn Host,
) -> Result<Vec<Value>, Error> {
    evaluate_expr(expr, root, model, host)
}

pub fn evaluate_at(
    expr: &Expr,
    focus: &Json,
    focus_path: &str,
    resource: &Json,
    root_resource: &Json,
    model: &Model,
    host: &dyn Host,
) -> Result<Vec<Value>, Error> {
    crate::eval::evaluate_at(
        expr,
        focus,
        focus_path,
        resource,
        root_resource,
        model,
        host,
    )
}

pub fn evaluate_invariant(
    expr: &Expr,
    focus: &Json,
    focus_path: &str,
    resource: &Json,
    root_resource: &Json,
    model: &Model,
    host: &dyn Host,
) -> Result<Vec<Value>, Error> {
    crate::eval::evaluate_invariant(
        expr,
        focus,
        focus_path,
        resource,
        root_resource,
        model,
        host,
    )
}

pub fn evaluate_owned(expression: &str, root: &Json) -> Result<Vec<Value>, Error> {
    let expr = parse(expression)?;
    evaluate(&expr, root, &Model::new(), &EmptyHost)
}

#[cfg(test)]
mod context_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn evaluate_at_keeps_resource_distinct_from_focus() {
        let resource = json!({
            "resourceType": "Patient",
            "id": "p1",
            "contact": [{"name": {"family": "Ann"}}]
        });
        let focus = &resource["contact"][0];
        let expr = parse("name.exists() and %resource.id = 'p1' and %context.name.family = 'Ann'")
            .unwrap();
        let items = evaluate_at(
            &expr,
            focus,
            "Patient.contact",
            &resource,
            &resource,
            &Model::new(),
            &EmptyHost,
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].as_singleton_bool().unwrap());
    }

    #[test]
    fn invariant_as_keeps_each_matching_item() {
        let expr = parse("('a' | 'b').as(System.String)").unwrap();
        let root = json!({});
        let strict = evaluate(&expr, &root, &Model::new(), &EmptyHost);
        assert!(strict.is_err());
        let items =
            evaluate_invariant(&expr, &root, "", &root, &root, &Model::new(), &EmptyHost).unwrap();
        let text: Vec<_> = items.iter().map(Value::render).collect();
        assert_eq!(text, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn membership_is_false_when_the_collection_is_empty() {
        let root = json!({});
        let missing = parse("'#1' in {}").unwrap();
        let items = evaluate(&missing, &root, &Model::new(), &EmptyHost).unwrap();
        assert_eq!(items[0].as_singleton_bool(), Some(false));
        let empty_left = parse("{} in ('#1')").unwrap();
        let items = evaluate(&empty_left, &root, &Model::new(), &EmptyHost).unwrap();
        assert!(items.is_empty());
    }
}
