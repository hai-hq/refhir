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
mod xhtml;

pub use ast::Expr;
pub use error::{Error, ErrorKind};
pub use eval::{days_in_month, member_values};
pub use host::{civil_from_days, days_from_civil, EmptyHost, Host};
pub use model::{Model, ModelBuilder};
pub use parse::parse;
pub use value::{Temporal, Value};
pub use xhtml::html_checks;

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

    fn answer(expression: &str, invariant: bool) -> Option<bool> {
        let expr = parse(expression).unwrap();
        let root = json!({});
        let items = if invariant {
            evaluate_invariant(&expr, &root, "", &root, &root, &Model::new(), &EmptyHost)
        } else {
            evaluate(&expr, &root, &Model::new(), &EmptyHost)
        }
        .unwrap();
        assert!(items.len() <= 1, "{expression}");
        items.first().map(|item| item.as_singleton_bool().unwrap())
    }

    #[test]
    fn implies_follows_the_fhirpath_truth_table() {
        for invariant in [false, true] {
            for right in ["true", "false", "{}"] {
                assert_eq!(
                    answer(&format!("false implies {right}"), invariant),
                    Some(true)
                );
            }
            assert_eq!(answer("true implies true", invariant), Some(true));
            assert_eq!(answer("true implies false", invariant), Some(false));
            assert_eq!(answer("true implies {}", invariant), None);
            assert_eq!(answer("{} implies true", invariant), Some(true));
            assert_eq!(answer("{} implies false", invariant), None);
            assert_eq!(answer("{} implies {}", invariant), None);
        }
    }

    /// Normative FHIRPath answers empty for `{} is T`. As an operand of a boolean operator in an
    /// R4 invariant it is false, so a guard such as ras-2's
    /// `probability is decimal implies (probability as decimal) <= 100` holds when there is no
    /// probability, as the HL7 reference validator has it. Anywhere else it stays empty.
    #[test]
    fn an_empty_collection_is_no_type_in_an_invariant_guard() {
        for invariant in [false, true] {
            for expression in ["{} is System.Decimal", "{}.is(System.Decimal)"] {
                assert_eq!(answer(expression, invariant), None, "{expression}");
                let tested = format!("({expression}).empty()");
                assert_eq!(answer(&tested, invariant), Some(true), "{tested}");
            }
        }
        for guard in [
            "{} is System.Decimal implies ({} as System.Decimal) <= 100",
            "{}.is(System.Decimal) implies ({} as System.Decimal) <= 100",
            "({} is System.Decimal).not() or false",
        ] {
            let expected = if guard.contains("not()") {
                None
            } else {
                Some(true)
            };
            assert_eq!(answer(guard, false), None, "{guard}");
            assert_eq!(answer(guard, true), expected, "{guard}");
        }
        assert_eq!(answer("({} is System.Decimal) or true", true), Some(true));
        assert_eq!(answer("({} is System.Decimal) and true", true), Some(false));
        let present = "150.0 is System.Decimal implies (150.0 as System.Decimal) <= 100";
        assert_eq!(answer(present, true), Some(false));
        let within = "50.0 is System.Decimal implies (50.0 as System.Decimal) <= 100";
        assert_eq!(answer(within, true), Some(true));
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
