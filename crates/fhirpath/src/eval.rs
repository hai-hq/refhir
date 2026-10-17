use std::collections::BTreeMap;

use regex::Regex;
use rust_decimal::Decimal;
use serde_json::{Map, Value as Json};

use crate::ast::{BinOp, Expr, UnaryOp};
use crate::error::Error;
use crate::host::{civil_from_days, days_from_civil, Host};
use crate::model::{simple_name, Model};
use crate::ucum::{self, Measured};
use crate::value::{
    collections_equal, collections_equivalent, equal_pair, equivalent_pair, format_decimal,
    Temporal, Value,
};

struct Ctx<'a> {
    resource: &'a Json,
    root_resource: &'a Json,
    /// The focus, borrowed: copying the resource for the context, and again for every `where` or
    /// `select` that opens a scope of its own, is most of what evaluating a path costs.
    context: &'a Value,
    model: &'a Model,
    host: &'a dyn Host,
    this_item: Option<Value>,
    index: Option<i64>,
    total: Option<Value>,
    /// R4 invariants apply `as()` to each item. Normative FHIRPath rejects a collection.
    map_cast: bool,
}

pub fn evaluate_expr(
    expr: &Expr,
    root: &Json,
    model: &Model,
    host: &dyn Host,
) -> Result<Vec<Value>, Error> {
    evaluate_at(expr, root, "", root, root, model, host)
}

/// Evaluate `expr` with `focus` as the context node.
///
/// `%context` is the focus. `%resource` is the resource that contains it.
/// `%rootResource` is the outermost resource when the focus sits in a contained
/// resource. `evaluate_expr` passes the same JSON for all three.
pub fn evaluate_at(
    expr: &Expr,
    focus: &Json,
    focus_path: &str,
    resource: &Json,
    root_resource: &Json,
    model: &Model,
    host: &dyn Host,
) -> Result<Vec<Value>, Error> {
    evaluate_focused(
        expr,
        focus,
        focus_path,
        resource,
        root_resource,
        model,
        host,
        false,
    )
}

/// Like `evaluate_at`, but `as()` keeps every item that casts.
///
/// R4 `ElementDefinition.constraint` expressions were written for that behavior.
/// `dom-3` calls `descendants().as(canonical)`.
pub fn evaluate_invariant(
    expr: &Expr,
    focus: &Json,
    focus_path: &str,
    resource: &Json,
    root_resource: &Json,
    model: &Model,
    host: &dyn Host,
) -> Result<Vec<Value>, Error> {
    evaluate_focused(
        expr,
        focus,
        focus_path,
        resource,
        root_resource,
        model,
        host,
        true,
    )
}

// The evaluator is recursive and passes its traversal context positionally; bundling it would
// cost an allocation or a copy on every step.
#[allow(clippy::too_many_arguments)]
fn evaluate_focused(
    expr: &Expr,
    focus: &Json,
    focus_path: &str,
    resource: &Json,
    root_resource: &Json,
    model: &Model,
    host: &dyn Host,
    map_cast: bool,
) -> Result<Vec<Value>, Error> {
    let focus_node = node_from_json(
        focus,
        focus_type(focus, focus_path, model),
        focus_path.to_string(),
        Vec::new(),
    );
    let ctx = Ctx {
        resource,
        root_resource,
        context: &focus_node,
        model,
        host,
        this_item: None,
        index: None,
        total: None,
        map_cast,
    };
    let outcome = ctx.eval(expr, std::slice::from_ref(&focus_node), true)?;
    Ok(outcome.items)
}

fn focus_type(focus: &Json, focus_path: &str, model: &Model) -> Option<String> {
    if focus_path.is_empty() {
        return resource_type(focus);
    }
    model
        .element_types(focus_path)
        .first()
        .cloned()
        .or_else(|| resource_type(focus))
}

struct Outcome {
    items: Vec<Value>,
    ordered: bool,
}

impl Ctx<'_> {
    fn eval(&self, expr: &Expr, focus: &[Value], ordered: bool) -> Result<Outcome, Error> {
        match expr {
            Expr::Empty => Ok(Outcome {
                items: vec![],
                ordered: true,
            }),
            Expr::Bool(value) => Ok(one(Value::system_bool(*value))),
            Expr::Int(value) => Ok(one(Value::Integer(*value))),
            Expr::Decimal(value, scale) => Ok(one(Value::Decimal {
                value: *value,
                scale: *scale,
            })),
            Expr::Str(value) => Ok(one(Value::String(value.clone()))),
            Expr::Date(value) => Ok(one(Value::Date(value.clone()))),
            Expr::DateTime(value) => Ok(one(Value::DateTime(value.clone()))),
            Expr::Time(value) => Ok(one(Value::Time(value.clone()))),
            Expr::Quantity(value, scale, unit) => Ok(one(Value::Quantity {
                value: *value,
                scale: *scale,
                unit: unit.clone(),
            })),
            Expr::This => Ok(self.this_item.clone().map(one).unwrap_or_else(|| Outcome {
                items: focus.to_vec(),
                ordered,
            })),
            Expr::IndexVar => Ok(self
                .index
                .map(|index| one(Value::Integer(index)))
                .unwrap_or(Outcome {
                    items: vec![],
                    ordered: true,
                })),
            Expr::Total => Ok(self.total.clone().map(one).unwrap_or(Outcome {
                items: vec![],
                ordered: true,
            })),
            Expr::Variable(name) => {
                if let Some(items) = self.host.lookup_variable(name)? {
                    return Ok(Outcome {
                        items,
                        ordered: true,
                    });
                }
                Ok(one(self.variable(name)?))
            }
            Expr::TypeName(name) => Ok(one(Value::String(simple_name(name).to_string()))),
            Expr::Ident(name) => self.eval_ident(name, focus, ordered),
            Expr::Member(target, name) => {
                let target = self.eval(target, focus, ordered)?;
                self.members(&target.items, name, target.ordered)
            }
            Expr::Index(target, index) => {
                let target = self.eval(target, focus, ordered)?;
                self.require_ordered(&target)?;
                let index = self.singleton_int(&self.eval(index, focus, ordered)?.items)?;
                Ok(match index {
                    Some(index) if index >= 0 => target
                        .items
                        .get(index as usize)
                        .cloned()
                        .map(one)
                        .unwrap_or(Outcome {
                            items: vec![],
                            ordered: true,
                        }),
                    _ => Outcome {
                        items: vec![],
                        ordered: true,
                    },
                })
            }
            Expr::Unary(op, inner) => self.eval_unary(*op, inner, focus, ordered),
            Expr::Binary(op, left, right) => self.eval_binary(*op, left, right, focus, ordered),
            Expr::Is(inner, name) => {
                let items = self.eval(inner, focus, ordered)?.items;
                Ok(bool_outcome(self.is_type(&items, name)?))
            }
            Expr::As(inner, name) => {
                let items = self.eval(inner, focus, ordered)?.items;
                Ok(Outcome {
                    items: self.cast(&items, name)?,
                    ordered: true,
                })
            }
            Expr::Call {
                focus: call_focus,
                name,
                args,
            } => {
                let input = if let Some(call_focus) = call_focus {
                    self.eval(call_focus, focus, ordered)?
                } else {
                    Outcome {
                        items: focus.to_vec(),
                        ordered,
                    }
                };
                self.call(name, &input, args, focus)
            }
        }
    }

    fn eval_ident(&self, name: &str, focus: &[Value], ordered: bool) -> Result<Outcome, Error> {
        if focus.iter().any(|item| self.model_has_child(item, name)) {
            return self.members(focus, name, ordered);
        }
        let children = self.members(focus, name, ordered)?;
        if !children.items.is_empty() {
            return Ok(children);
        }
        if self.model.is_type(name) || name.chars().next().is_some_and(|ch| ch.is_uppercase()) {
            let items = focus
                .iter()
                .filter(|item| self.value_is(item, name))
                .cloned()
                .collect();
            return Ok(Outcome { items, ordered });
        }
        if self.host.strict() && self.model_loaded(focus) {
            return Err(Error::eval(format!("unknown element {name}")));
        }
        Ok(Outcome {
            items: vec![],
            ordered: true,
        })
    }

    fn members(&self, focus: &[Value], name: &str, ordered: bool) -> Result<Outcome, Error> {
        let mut items = Vec::new();
        for item in focus {
            if let Value::TypeInfo {
                namespace,
                name: type_name,
            } = item
            {
                let text = match name {
                    "namespace" => namespace.clone(),
                    "name" => type_name.clone(),
                    _ => continue,
                };
                items.push(Value::String(text));
                continue;
            }
            items.extend(self.child(item, name)?);
        }
        Ok(Outcome { items, ordered })
    }

    fn child(&self, item: &Value, name: &str) -> Result<Vec<Value>, Error> {
        let Value::Node { json, path, .. } = item else {
            return Ok(vec![]);
        };
        if let Some(choice) = self.model.choice_path(path, name) {
            return Ok(choice_values(json, &choice, path, name));
        }
        if self.model_loaded_path(path)
            && !self.model.known_child(path, name)
            && name != "id"
            && name != "resourceType"
        {
            if self.host.strict() {
                return Err(Error::eval(format!("unknown element {name}")));
            }
            return Ok(vec![]);
        }
        let Some(value) = json.get(name) else {
            return Ok(vec![]);
        };
        let extensions = extension_siblings(json, name);
        let element_path = self
            .model
            .canonical(&if path.is_empty() {
                name.to_string()
            } else {
                format!("{path}.{name}")
            })
            .to_string();
        let types = self.model.element_types(&element_path);
        let fhir_type = types.first().cloned();
        let child_path = match fhir_type.as_deref() {
            Some(fhir_type)
                if !self.model.has_descendants(&element_path)
                    && self.model.has_descendants(fhir_type) =>
            {
                fhir_type.to_string()
            }
            _ => element_path,
        };
        Ok(materialize_list(
            value,
            &extensions,
            fhir_type.as_deref(),
            &child_path,
        ))
    }

    fn model_has_child(&self, item: &Value, name: &str) -> bool {
        let Value::Node { path, .. } = item else {
            return false;
        };
        self.model.known_child(path, name) || self.model.choice_path(path, name).is_some()
    }

    fn model_loaded(&self, focus: &[Value]) -> bool {
        focus.iter().any(|item| match item {
            Value::Node { path, .. } => self.model_loaded_path(path),
            _ => false,
        })
    }

    fn model_loaded_path(&self, path: &str) -> bool {
        !self.model.element_types(path).is_empty() || self.model.is_type(path)
    }

    fn value_is(&self, item: &Value, target: &str) -> bool {
        let (want_namespace, want_name) = split_type(target);
        let (namespace, name) = item.type_name();
        if let Some(want_namespace) = want_namespace {
            if namespace != want_namespace {
                return false;
            }
        }
        if self.model.is_a(name, want_name)
            && (namespace == "FHIR" || want_namespace.is_some() || want_name == name)
        {
            return true;
        }
        let target = want_name;
        if namespace == "System" && system_name(target) == Some(name) {
            return true;
        }
        if let Value::Node {
            json, fhir_type, ..
        } = item
        {
            if json.get("resourceType").and_then(|value| value.as_str()) == Some(target) {
                return true;
            }
            if let Some(fhir_type) = fhir_type {
                return self.model.is_a(fhir_type, target);
            }
        }
        false
    }

    fn is_type(&self, items: &[Value], target: &str) -> Result<Option<bool>, Error> {
        let Some(item) = singleton(items)? else {
            return Ok(None);
        };
        Ok(Some(self.value_is(item, target)))
    }

    fn cast(&self, items: &[Value], target: &str) -> Result<Vec<Value>, Error> {
        if self.map_cast && items.len() > 1 {
            let mut kept = Vec::new();
            for item in items {
                kept.extend(cast_one(item, target));
            }
            return Ok(kept);
        }
        let Some(item) = singleton(items)? else {
            return Ok(vec![]);
        };
        Ok(cast_one(item, target))
    }

    fn eval_unary(
        &self,
        op: UnaryOp,
        inner: &Expr,
        focus: &[Value],
        ordered: bool,
    ) -> Result<Outcome, Error> {
        let items = self.eval(inner, focus, ordered)?.items;
        if items.is_empty() {
            return Ok(Outcome {
                items,
                ordered: true,
            });
        }
        let item =
            singleton(&items)?.ok_or_else(|| Error::eval("unary operator expected one item"))?;
        if let Value::Quantity { value, scale, unit } = item {
            let value = match op {
                UnaryOp::Plus => *value,
                UnaryOp::Minus => -*value,
            };
            return Ok(one(Value::Quantity {
                value,
                scale: *scale,
                unit: unit.clone(),
            }));
        }
        let number =
            number_of(item).ok_or_else(|| Error::eval("unary operator expected a number"))?;
        let value = match op {
            UnaryOp::Plus => number.0,
            UnaryOp::Minus => -number.0,
        };
        Ok(one(
            if matches!(item, Value::Integer(_)) && matches!(op, UnaryOp::Minus | UnaryOp::Plus) {
                Value::Integer(value.try_into().unwrap_or(0))
            } else {
                Value::Decimal {
                    value,
                    scale: number.1,
                }
            },
        ))
    }

    fn eval_binary(
        &self,
        op: BinOp,
        left: &Expr,
        right: &Expr,
        focus: &[Value],
        ordered: bool,
    ) -> Result<Outcome, Error> {
        if matches!(op, BinOp::And | BinOp::Or | BinOp::Xor | BinOp::Implies) {
            return self.eval_logic(op, left, right, focus, ordered);
        }
        let left_out = self.eval(left, focus, ordered)?;
        let right_out = self.eval(right, focus, ordered)?;
        if matches!(op, BinOp::Concat) {
            if left_out.items.len() > 1 || right_out.items.len() > 1 {
                return Ok(Outcome {
                    items: vec![],
                    ordered: true,
                });
            }
            let left = singleton_string(&left_out.items)?.unwrap_or_default();
            let right = singleton_string(&right_out.items)?.unwrap_or_default();
            return Ok(one(Value::String(format!("{left}{right}"))));
        }
        if matches!(op, BinOp::Union) {
            return Ok(Outcome {
                items: union(&left_out.items, &right_out.items),
                ordered: true,
            });
        }
        if matches!(op, BinOp::In | BinOp::Contains) {
            let (needle, haystack) = match op {
                BinOp::In => (&left_out.items, &right_out.items),
                _ => (&right_out.items, &left_out.items),
            };
            if needle.is_empty() {
                return Ok(Outcome {
                    items: vec![],
                    ordered: true,
                });
            }
            if haystack.is_empty() {
                return Ok(one(Value::system_bool(false)));
            }
            return Ok(one(Value::system_bool(membership(needle, haystack))));
        }
        if left_out.items.is_empty() || right_out.items.is_empty() {
            return match op {
                BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq => {
                    Ok(Outcome {
                        items: vec![],
                        ordered: true,
                    })
                }
                BinOp::Equiv => Ok(one(Value::system_bool(
                    left_out.items.is_empty() && right_out.items.is_empty(),
                ))),
                BinOp::NotEquiv => Ok(one(Value::system_bool(
                    !(left_out.items.is_empty() && right_out.items.is_empty()),
                ))),
                _ => Ok(Outcome {
                    items: vec![],
                    ordered: true,
                }),
            };
        }
        match op {
            BinOp::Eq => Ok(bool_outcome(collections_equal(
                &left_out.items,
                &right_out.items,
            ))),
            BinOp::NotEq => Ok(bool_outcome(
                collections_equal(&left_out.items, &right_out.items).map(|value| !value),
            )),
            BinOp::Equiv => Ok(one(Value::system_bool(collections_equivalent(
                &left_out.items,
                &right_out.items,
            )))),
            BinOp::NotEquiv => Ok(one(Value::system_bool(!collections_equivalent(
                &left_out.items,
                &right_out.items,
            )))),
            BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq => {
                let left = singleton(&left_out.items)?
                    .ok_or_else(|| Error::eval("comparison expected one item"))?;
                let right = singleton(&right_out.items)?
                    .ok_or_else(|| Error::eval("comparison expected one item"))?;
                Ok(bool_outcome(compare(left, right, op)))
            }
            _ => {
                let left = singleton(&left_out.items)?
                    .ok_or_else(|| Error::eval("operator expected one item"))?;
                let right = singleton(&right_out.items)?
                    .ok_or_else(|| Error::eval("operator expected one item"))?;
                Ok(self
                    .arithmetic(op, left, right)?
                    .map(one)
                    .unwrap_or(Outcome {
                        items: vec![],
                        ordered: true,
                    }))
            }
        }
    }

    fn eval_logic(
        &self,
        op: BinOp,
        left: &Expr,
        right: &Expr,
        focus: &[Value],
        ordered: bool,
    ) -> Result<Outcome, Error> {
        let left = bool_operand(&self.eval(left, focus, ordered)?.items)?;
        let right = bool_operand(&self.eval(right, focus, ordered)?.items)?;
        let value = match op {
            BinOp::And => match (left, right) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            },
            BinOp::Or => match (left, right) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            },
            BinOp::Xor => match (left, right) {
                (Some(a), Some(b)) => Some(a != b),
                _ => None,
            },
            BinOp::Implies => match (left, right) {
                (Some(false), _) => Some(true),
                (Some(true), Some(value)) => Some(value),
                (Some(true), None) => None,
                (None, Some(true)) => Some(true),
                (None, _) => None,
            },
            _ => None,
        };
        Ok(value
            .map(|value| one(Value::system_bool(value)))
            .unwrap_or(Outcome {
                items: vec![],
                ordered: true,
            }))
    }

    fn arithmetic(&self, op: BinOp, left: &Value, right: &Value) -> Result<Option<Value>, Error> {
        if let Some(result) = date_math(op, left, right)? {
            return Ok(Some(result));
        }
        if matches!(op, BinOp::Add) {
            if let (Some(left), Some(right)) = (concat_text(left), concat_text(right)) {
                return Ok(Some(Value::String(format!("{left}{right}"))));
            }
        }
        if let (Some(left_q), Some(right_q)) = (measured(left), measured(right)) {
            return Ok(quantity_math(
                op,
                &left_q,
                &right_q,
                quantity_scale(left),
                quantity_scale(right),
            ));
        }
        let Some((left_n, left_scale)) = number_of(left) else {
            return Ok(None);
        };
        let Some((right_n, right_scale)) = number_of(right) else {
            return Ok(None);
        };
        let both_int = matches!(left, Value::Integer(_)) && matches!(right, Value::Integer(_));
        match op {
            BinOp::Add => Ok(Some(number_value(
                left_n + right_n,
                left_scale.max(right_scale),
                both_int,
            ))),
            BinOp::Sub => Ok(Some(number_value(
                left_n - right_n,
                left_scale.max(right_scale),
                both_int,
            ))),
            BinOp::Mul => Ok(Some(number_value(
                left_n * right_n,
                left_scale.saturating_add(right_scale),
                both_int,
            ))),
            BinOp::Div => {
                if right_n.is_zero() {
                    return Ok(None);
                }
                let value = left_n / right_n;
                let scale = if value.fract().is_zero() {
                    0
                } else {
                    left_scale.max(right_scale).max(1)
                };
                Ok(Some(Value::Decimal { value, scale }))
            }
            BinOp::TruncDiv => {
                if right_n.is_zero() {
                    return Ok(None);
                }
                Ok(Some(Value::Integer(
                    (left_n / right_n).trunc().try_into().unwrap_or(0),
                )))
            }
            BinOp::Mod => {
                if right_n.is_zero() {
                    return Ok(None);
                }
                Ok(Some(number_value(
                    left_n % right_n,
                    left_scale.max(right_scale),
                    both_int,
                )))
            }
            _ => Ok(None),
        }
    }

    fn call(
        &self,
        name: &str,
        input: &Outcome,
        args: &[Expr],
        outer: &[Value],
    ) -> Result<Outcome, Error> {
        match name {
            "where" | "select" | "repeat" | "all" | "exists" | "aggregate" | "iif" | "trace"
            | "sort" | "is" | "as" => self.call_expr(name, input, args, outer),
            _ => {
                let mut values = Vec::new();
                for arg in args {
                    values.push(self.eval(arg, outer, true)?.items);
                }
                self.call_values(name, input, &values)
            }
        }
    }

    fn call_expr(
        &self,
        name: &str,
        input: &Outcome,
        args: &[Expr],
        outer: &[Value],
    ) -> Result<Outcome, Error> {
        match name {
            "where" => {
                let criteria = args
                    .first()
                    .ok_or_else(|| Error::eval("where() criteria required"))?;
                let mut kept = Vec::new();
                for (index, item) in input.items.iter().enumerate() {
                    if self.criteria(criteria, item, index as i64, outer)? {
                        kept.push(item.clone());
                    }
                }
                Ok(Outcome {
                    items: kept,
                    ordered: input.ordered,
                })
            }
            "select" => {
                let projection = args
                    .first()
                    .ok_or_else(|| Error::eval("select() projection required"))?;
                let mut items = Vec::new();
                for (index, item) in input.items.iter().enumerate() {
                    let child =
                        self.scoped(Some(item.clone()), Some(index as i64), self.total.clone());
                    items.extend(
                        child
                            .eval(projection, std::slice::from_ref(item), input.ordered)?
                            .items,
                    );
                }
                Ok(Outcome {
                    items,
                    ordered: input.ordered,
                })
            }
            "all" => {
                let criteria = args
                    .first()
                    .ok_or_else(|| Error::eval("all() criteria required"))?;
                for (index, item) in input.items.iter().enumerate() {
                    if !self.criteria(criteria, item, index as i64, outer)? {
                        return Ok(one(Value::system_bool(false)));
                    }
                }
                Ok(one(Value::system_bool(true)))
            }
            "exists" => {
                if args.is_empty() {
                    return Ok(one(Value::system_bool(
                        input.items.iter().any(|item| !is_null_node(item)),
                    )));
                }
                let criteria = &args[0];
                for (index, item) in input.items.iter().enumerate() {
                    if self.criteria(criteria, item, index as i64, outer)? {
                        return Ok(one(Value::system_bool(true)));
                    }
                }
                Ok(one(Value::system_bool(false)))
            }
            "repeat" => {
                let projection = args
                    .first()
                    .ok_or_else(|| Error::eval("repeat() projection required"))?;
                let mut pending = input.items.clone();
                let mut items = Vec::new();
                let mut seen = BTreeMap::new();
                let mut guard = 0;
                while !pending.is_empty() && guard < 64 {
                    guard += 1;
                    let mut next = Vec::new();
                    for item in &pending {
                        let child = self.scoped(Some(item.clone()), None, None);
                        for value in child
                            .eval(projection, std::slice::from_ref(item), true)?
                            .items
                        {
                            let key = value.render();
                            if seen.insert(key, ()).is_none() {
                                next.push(value.clone());
                                items.push(value);
                            }
                        }
                    }
                    pending = next;
                }
                Ok(Outcome {
                    items,
                    ordered: true,
                })
            }
            "aggregate" => {
                let projection = args
                    .first()
                    .ok_or_else(|| Error::eval("aggregate() expression required"))?;
                let mut total = if args.len() > 1 {
                    self.eval(&args[1], outer, true)?.items.into_iter().next()
                } else {
                    None
                };
                for (index, item) in input.items.iter().enumerate() {
                    let child = self.scoped(Some(item.clone()), Some(index as i64), total.clone());
                    total = child
                        .eval(projection, std::slice::from_ref(item), true)?
                        .items
                        .into_iter()
                        .next();
                }
                Ok(total.map(one).unwrap_or(Outcome {
                    items: vec![],
                    ordered: true,
                }))
            }
            "iif" => {
                if input.items.len() > 1 {
                    return Err(Error::eval("iif() expected at most one item"));
                }
                let focus = input.items.as_slice();
                let scoped = self.scoped(focus.first().cloned(), self.index, self.total.clone());
                let criterion = iif_criterion(
                    &scoped
                        .eval(
                            args.first()
                                .ok_or_else(|| Error::eval("iif() criterion required"))?,
                            focus,
                            true,
                        )?
                        .items,
                );
                match criterion {
                    Some(true) => scoped.eval(
                        args.get(1)
                            .ok_or_else(|| Error::eval("iif() true result required"))?,
                        focus,
                        true,
                    ),
                    Some(false) => args
                        .get(2)
                        .map(|expr| scoped.eval(expr, focus, true))
                        .unwrap_or(Ok(Outcome {
                            items: vec![],
                            ordered: true,
                        })),
                    None => Ok(Outcome {
                        items: vec![],
                        ordered: true,
                    }),
                }
            }
            "trace" => Ok(Outcome {
                items: input.items.clone(),
                ordered: input.ordered,
            }),
            "sort" => self.sort(input, args),
            "is" => {
                let target = type_arg(args.first());
                Ok(bool_outcome(self.is_type(&input.items, &target)?))
            }
            "as" => {
                let target = type_arg(args.first());
                Ok(Outcome {
                    items: self.cast(&input.items, &target)?,
                    ordered: true,
                })
            }
            _ => Err(Error::eval(format!("unknown function {name}"))),
        }
    }

    fn sort(&self, input: &Outcome, args: &[Expr]) -> Result<Outcome, Error> {
        let criteria: Vec<(bool, &Expr)> = args
            .iter()
            .map(|expr| match expr {
                Expr::Unary(crate::ast::UnaryOp::Minus, inner) => (true, inner.as_ref()),
                other => (false, other),
            })
            .collect();
        let mut items = input.items.clone();
        let mut keys = Vec::new();
        for item in &items {
            if criteria.is_empty() {
                keys.push(vec![(false, vec![item.clone()])]);
                continue;
            }
            let mut item_keys = Vec::new();
            for (descending, expr) in &criteria {
                let child = self.scoped(Some(item.clone()), None, self.total.clone());
                let values = child.eval(expr, std::slice::from_ref(item), true)?.items;
                item_keys.push((*descending, values));
            }
            keys.push(item_keys);
        }
        let mut order: Vec<usize> = (0..items.len()).collect();
        order.sort_by(|left, right| {
            for (left_key, right_key) in keys[*left].iter().zip(&keys[*right]) {
                let mut ordering = cmp_sort_key(&left_key.1, &right_key.1);
                if left_key.0 {
                    ordering = ordering.reverse();
                }
                if ordering != std::cmp::Ordering::Equal {
                    return ordering;
                }
            }
            std::cmp::Ordering::Equal
        });
        let sorted = order
            .into_iter()
            .map(|index| items[index].clone())
            .collect();
        let _ = &mut items;
        Ok(Outcome {
            items: sorted,
            ordered: true,
        })
    }

    fn criteria(
        &self,
        expr: &Expr,
        item: &Value,
        index: i64,
        outer: &[Value],
    ) -> Result<bool, Error> {
        let child = self.scoped(Some(item.clone()), Some(index), self.total.clone());
        let _ = outer;
        Ok(
            bool_operand(&child.eval(expr, std::slice::from_ref(item), true)?.items)?
                .unwrap_or(false),
        )
    }

    fn scoped(
        &self,
        this_item: Option<Value>,
        index: Option<i64>,
        total: Option<Value>,
    ) -> Ctx<'_> {
        Ctx {
            resource: self.resource,
            root_resource: self.root_resource,
            context: self.context,
            model: self.model,
            host: self.host,
            this_item,
            index,
            total,
            map_cast: self.map_cast,
        }
    }

    fn call_values(
        &self,
        name: &str,
        input: &Outcome,
        args: &[Vec<Value>],
    ) -> Result<Outcome, Error> {
        if matches!(name, "skip" | "take" | "first" | "last" | "tail" | "single") {
            self.require_ordered(input)?;
        }
        match name {
            "empty" => Ok(one(Value::system_bool(input.items.is_empty()))),
            "count" => Ok(one(Value::Integer(input.items.len() as i64))),
            "not" => Ok(bool_outcome(not_of(&input.items))),
            "first" => Ok(input.items.first().cloned().map(one).unwrap_or(Outcome {
                items: vec![],
                ordered: true,
            })),
            "last" => Ok(input.items.last().cloned().map(one).unwrap_or(Outcome {
                items: vec![],
                ordered: true,
            })),
            "tail" => Ok(Outcome {
                items: input.items.iter().skip(1).cloned().collect(),
                ordered: true,
            }),
            "single" => {
                if input.items.len() > 1 {
                    return Err(Error::eval("single() expected one item"));
                }
                Ok(Outcome {
                    items: input.items.clone(),
                    ordered: true,
                })
            }
            "skip" => {
                let count = self
                    .singleton_int(args.first().unwrap_or(&vec![]))?
                    .unwrap_or(0)
                    .max(0) as usize;
                Ok(Outcome {
                    items: input.items.iter().skip(count).cloned().collect(),
                    ordered: true,
                })
            }
            "take" => {
                let count = self
                    .singleton_int(args.first().unwrap_or(&vec![]))?
                    .unwrap_or(0)
                    .max(0) as usize;
                Ok(Outcome {
                    items: input.items.iter().take(count).cloned().collect(),
                    ordered: true,
                })
            }
            "distinct" => Ok(Outcome {
                items: union(&input.items, &[]),
                ordered: true,
            }),
            "isDistinct" => Ok(one(Value::system_bool(
                union(&input.items, &[]).len() == input.items.len(),
            ))),
            "combine" => {
                let mut items = input.items.clone();
                if let Some(extra) = args.first() {
                    items.extend(extra.clone());
                }
                Ok(Outcome {
                    items,
                    ordered: true,
                })
            }
            "intersect" => {
                let right = args.first().map(Vec::as_slice).unwrap_or(&[]);
                let mut items = Vec::new();
                for item in &input.items {
                    let kept = right
                        .iter()
                        .any(|other| equal_pair(item, other) == Some(true));
                    let fresh = !items
                        .iter()
                        .any(|existing| equal_pair(item, existing) == Some(true));
                    if kept && fresh {
                        items.push(item.clone());
                    }
                }
                Ok(Outcome {
                    items,
                    ordered: true,
                })
            }
            "exclude" => {
                let right = args.first().map(Vec::as_slice).unwrap_or(&[]);
                Ok(Outcome {
                    items: input
                        .items
                        .iter()
                        .filter(|item| {
                            !right
                                .iter()
                                .any(|other| equal_pair(item, other) == Some(true))
                        })
                        .cloned()
                        .collect(),
                    ordered: true,
                })
            }
            "subsetOf" | "supersetOf" => {
                let right = args.first().map(Vec::as_slice).unwrap_or(&[]);
                let (left, right) = if name == "subsetOf" {
                    (input.items.as_slice(), right)
                } else {
                    (right, input.items.as_slice())
                };
                let value = left
                    .iter()
                    .all(|item| right.iter().any(|other| equivalent_pair(item, other)));
                Ok(one(Value::system_bool(value)))
            }
            "allTrue" => Ok(one(Value::system_bool(
                input
                    .items
                    .iter()
                    .all(|item| item.as_singleton_bool() == Some(true)),
            ))),
            "anyTrue" => Ok(one(Value::system_bool(
                input
                    .items
                    .iter()
                    .any(|item| item.as_singleton_bool() == Some(true)),
            ))),
            "allFalse" => Ok(one(Value::system_bool(
                input
                    .items
                    .iter()
                    .all(|item| item.as_singleton_bool() == Some(false)),
            ))),
            "anyFalse" => Ok(one(Value::system_bool(
                input
                    .items
                    .iter()
                    .any(|item| item.as_singleton_bool() == Some(false)),
            ))),
            "ofType" => {
                let target = arg_type(args.first().unwrap_or(&vec![]));
                Ok(Outcome {
                    items: input
                        .items
                        .iter()
                        .filter(|item| self.exact_type(item, &target))
                        .cloned()
                        .collect(),
                    ordered: input.ordered,
                })
            }
            "extension" => {
                let url = arg_string(args.first().unwrap_or(&vec![])).unwrap_or_default();
                let mut items = Vec::new();
                for item in &input.items {
                    items.extend(extensions_of(item, &url));
                }
                Ok(Outcome {
                    items,
                    ordered: true,
                })
            }
            "hasValue" => Ok(one(Value::system_bool(
                input.items.iter().any(|item| item.has_primitive_value()),
            ))),
            "resolve" => self.resolve(input),
            "memberOf" => self.member_of(input, args),
            "type" => {
                let item = singleton(&input.items)?
                    .ok_or_else(|| Error::eval("type() expected one item"))?;
                let (namespace, name) = item.type_name();
                Ok(one(Value::TypeInfo {
                    namespace: namespace.into(),
                    name: name.into(),
                }))
            }
            "children" => Ok(Outcome {
                items: self.children_of(&input.items),
                ordered: false,
            }),
            "descendants" => Ok(Outcome {
                items: self.descendants_of(&input.items),
                ordered: false,
            }),
            "today" => {
                let mut now = self.host.now();
                now.hour = None;
                now.minute = None;
                now.second = None;
                now.millis = None;
                now.offset = None;
                Ok(one(Value::Date(now)))
            }
            "now" => Ok(one(Value::DateTime(self.host.now()))),
            "timeOfDay" => {
                let now = self.host.now();
                Ok(one(Value::Time(Temporal {
                    year: None,
                    month: None,
                    day: None,
                    ..now
                })))
            }
            "toString" | "toInteger" | "toDecimal" | "toBoolean" | "toDate" | "toDateTime"
            | "toTime" | "toQuantity" | "convertsToString" | "convertsToInteger"
            | "convertsToDecimal" | "convertsToBoolean" | "convertsToDate"
            | "convertsToDateTime" | "convertsToTime" | "convertsToQuantity" => {
                self.convert(name, input)
            }
            "union" => {
                let right = args.first().cloned().unwrap_or_default();
                Ok(Outcome {
                    items: union(&input.items, &right),
                    ordered: true,
                })
            }
            "conformsTo" => {
                let url = arg_string(args.first().unwrap_or(&vec![])).unwrap_or_default();
                let json = input
                    .items
                    .first()
                    .and_then(|item| match item {
                        Value::Node { json, .. } => Some(json),
                        _ => None,
                    })
                    .unwrap_or(self.resource);
                if let Some(answer) = self.host.conforms_to(json, &url)? {
                    return Ok(one(Value::system_bool(answer)));
                }
                if !url.contains("StructureDefinition") {
                    return Ok(Outcome {
                        items: vec![],
                        ordered: true,
                    });
                }
                let expected = url.rsplit('/').next().unwrap_or("");
                let actual = resource_type(json).unwrap_or_default();
                Ok(one(Value::system_bool(actual == expected)))
            }
            "length" | "upper" | "lower" | "toChars" | "indexOf" | "substring" | "startsWith"
            | "endsWith" | "contains" | "replace" | "matches" | "matchesFull"
            | "replaceMatches" => self.strings(name, input, args),
            "abs" | "ceiling" | "floor" | "truncate" | "round" | "sqrt" | "exp" | "ln" | "log"
            | "power" => self.math(name, input, args),
            "lowBoundary" | "highBoundary" | "precision" | "comparable" => {
                self.boundaries(name, input, args)
            }
            other => Err(Error::eval(format!("unsupported function {other}"))),
        }
    }

    fn require_ordered(&self, input: &Outcome) -> Result<(), Error> {
        if self.host.strict() && !input.ordered {
            Err(Error::eval("function requires an ordered collection"))
        } else {
            Ok(())
        }
    }

    fn exact_type(&self, item: &Value, target: &str) -> bool {
        let (want_namespace, target) = split_type(target);
        let (namespace, name) = item.type_name();
        if let Some(want_namespace) = want_namespace {
            if namespace != want_namespace {
                return false;
            }
        }
        name == target || (namespace == "System" && system_name(target) == Some(name))
    }

    fn resolve(&self, input: &Outcome) -> Result<Outcome, Error> {
        let mut items = Vec::new();
        for item in &input.items {
            let reference = reference_text(item);
            let Some(reference) = reference else {
                continue;
            };
            if let Some(id) = reference.strip_prefix('#') {
                if let Some(resource) =
                    contained(self.root_resource, id).or_else(|| contained(self.resource, id))
                {
                    items.push(node_from_json(
                        &resource,
                        resource_type(&resource),
                        String::new(),
                        Vec::new(),
                    ));
                }
                continue;
            }
            if reference.starts_with('#') {
                continue;
            }
            if let Some(resource) = self.host.resolve(&reference)? {
                items.push(node_from_json(
                    &resource,
                    resource_type(&resource),
                    String::new(),
                    Vec::new(),
                ));
            }
        }
        Ok(Outcome {
            items,
            ordered: true,
        })
    }

    fn member_of(&self, input: &Outcome, args: &[Vec<Value>]) -> Result<Outcome, Error> {
        let value_set = arg_string(args.first().unwrap_or(&vec![])).unwrap_or_default();
        let Some(item) = singleton(&input.items)? else {
            return Ok(Outcome {
                items: vec![],
                ordered: true,
            });
        };
        let (system, code) = code_of(item);
        match self.host.member_of(system.as_deref(), &code, &value_set)? {
            Some(value) => Ok(one(Value::system_bool(value))),
            None => Err(Error::unsupported(
                "memberOf() requires a terminology service",
            )),
        }
    }

    fn convert(&self, name: &str, input: &Outcome) -> Result<Outcome, Error> {
        let Some(item) = singleton(&input.items)? else {
            return Ok(if name.starts_with("convertsTo") {
                one(Value::system_bool(false))
            } else {
                Outcome {
                    items: vec![],
                    ordered: true,
                }
            });
        };
        let converted = convert_one(
            name.trim_start_matches("convertsTo")
                .trim_start_matches("to"),
            item,
        );
        if name.starts_with("convertsTo") {
            return Ok(one(Value::system_bool(converted.is_some())));
        }
        Ok(converted.map(one).unwrap_or(Outcome {
            items: vec![],
            ordered: true,
        }))
    }

    fn strings(&self, name: &str, input: &Outcome, args: &[Vec<Value>]) -> Result<Outcome, Error> {
        let Some(item) = singleton(&input.items)? else {
            return Ok(Outcome {
                items: vec![],
                ordered: true,
            });
        };
        let Some(text) = as_text(item) else {
            return Ok(Outcome {
                items: vec![],
                ordered: true,
            });
        };
        match name {
            "length" => Ok(one(Value::Integer(text.chars().count() as i64))),
            "upper" => Ok(one(Value::String(text.to_uppercase()))),
            "lower" => Ok(one(Value::String(text.to_lowercase()))),
            "toChars" => Ok(Outcome {
                items: text
                    .chars()
                    .map(|ch| Value::String(ch.to_string()))
                    .collect(),
                ordered: true,
            }),
            "startsWith" => Ok(one(Value::system_bool(text.starts_with(
                &arg_string(args.first().unwrap_or(&vec![])).unwrap_or_default(),
            )))),
            "endsWith" => Ok(one(Value::system_bool(text.ends_with(
                &arg_string(args.first().unwrap_or(&vec![])).unwrap_or_default(),
            )))),
            "contains" => Ok(one(Value::system_bool(text.contains(
                &arg_string(args.first().unwrap_or(&vec![])).unwrap_or_default(),
            )))),
            "indexOf" => {
                let Some(needle) = arg_string(args.first().unwrap_or(&vec![])) else {
                    return Ok(Outcome {
                        items: vec![],
                        ordered: true,
                    });
                };
                let index = text
                    .find(&needle)
                    .map(|index| text[..index].chars().count() as i64)
                    .unwrap_or(-1);
                Ok(one(Value::Integer(index)))
            }
            "substring" => {
                let Some(start) = self.singleton_int(args.first().unwrap_or(&vec![]))? else {
                    return Ok(Outcome {
                        items: vec![],
                        ordered: true,
                    });
                };
                if start < 0 {
                    return Ok(Outcome {
                        items: vec![],
                        ordered: true,
                    });
                }
                let start = start as usize;
                let chars: Vec<char> = text.chars().collect();
                if start > chars.len() {
                    return Ok(Outcome {
                        items: vec![],
                        ordered: true,
                    });
                }
                let end = if let Some(len) = args.get(1) {
                    start + self.singleton_int(len)?.unwrap_or(0).max(0) as usize
                } else {
                    chars.len()
                };
                Ok(one(Value::String(
                    chars[start..end.min(chars.len())].iter().collect(),
                )))
            }
            "replace" => {
                let Some(pattern) = arg_string(args.first().unwrap_or(&vec![])) else {
                    return Ok(Outcome {
                        items: vec![],
                        ordered: true,
                    });
                };
                let Some(substitution) = arg_string(args.get(1).unwrap_or(&vec![])) else {
                    return Ok(Outcome {
                        items: vec![],
                        ordered: true,
                    });
                };
                if pattern.is_empty() {
                    let mut replaced = substitution.clone();
                    for ch in text.chars() {
                        replaced.push(ch);
                        replaced.push_str(&substitution);
                    }
                    return Ok(one(Value::String(replaced)));
                }
                Ok(one(Value::String(text.replace(&pattern, &substitution))))
            }
            "matches" | "matchesFull" => {
                let Some(pattern) = arg_string(args.first().unwrap_or(&vec![])) else {
                    return Ok(Outcome {
                        items: vec![],
                        ordered: true,
                    });
                };
                let pattern = if name == "matchesFull" {
                    format!("^(?:{pattern})$")
                } else {
                    pattern
                };
                let regex = Regex::new(&format!("(?s){pattern}"))
                    .map_err(|err| Error::eval(err.to_string()))?;
                Ok(one(Value::system_bool(regex.is_match(&text))))
            }
            "replaceMatches" => {
                let Some(pattern) = arg_string(args.first().unwrap_or(&vec![])) else {
                    return Ok(Outcome {
                        items: vec![],
                        ordered: true,
                    });
                };
                let Some(substitution) = arg_string(args.get(1).unwrap_or(&vec![])) else {
                    return Ok(Outcome {
                        items: vec![],
                        ordered: true,
                    });
                };
                if pattern.is_empty() {
                    return Ok(one(Value::String(text)));
                }
                let regex = Regex::new(&format!("(?s){pattern}"))
                    .map_err(|err| Error::eval(err.to_string()))?;
                Ok(one(Value::String(
                    regex.replace_all(&text, substitution.as_str()).into_owned(),
                )))
            }
            _ => Err(Error::eval(name)),
        }
    }

    fn math(&self, name: &str, input: &Outcome, args: &[Vec<Value>]) -> Result<Outcome, Error> {
        let Some(item) = singleton(&input.items)? else {
            return Ok(Outcome {
                items: vec![],
                ordered: true,
            });
        };
        if let Value::Quantity { value, scale, unit } = item {
            if name == "abs" {
                return Ok(one(Value::Quantity {
                    value: value.abs(),
                    scale: *scale,
                    unit: unit.clone(),
                }));
            }
        }
        let Some((value, scale)) = number_of(item) else {
            return Ok(Outcome {
                items: vec![],
                ordered: true,
            });
        };
        let result = match name {
            "abs" => value.abs(),
            "ceiling" => value.ceil(),
            "floor" => value.floor(),
            "truncate" => value.trunc(),
            "round" => {
                let places = args
                    .first()
                    .and_then(|arg| singleton_int_pure(arg))
                    .unwrap_or(0)
                    .max(0) as u32;
                return Ok(one(Value::Decimal {
                    value: value.round_dp(places),
                    scale: places,
                }));
            }
            "sqrt" => {
                if value.is_sign_negative() {
                    return Ok(Outcome {
                        items: vec![],
                        ordered: true,
                    });
                }
                Decimal::from_f64_retain(f64_of(value).sqrt()).unwrap_or(Decimal::ZERO)
            }
            "exp" => Decimal::from_f64_retain(f64_of(value).exp()).unwrap_or(Decimal::ZERO),
            "ln" => Decimal::from_f64_retain(f64_of(value).ln()).unwrap_or(Decimal::ZERO),
            "log" => {
                let base = args
                    .first()
                    .and_then(|arg| arg.first())
                    .and_then(number_of)
                    .map(|number| f64_of(number.0))
                    .unwrap_or(10.0);
                Decimal::from_f64_retain(f64_of(value).log(base)).unwrap_or(Decimal::ZERO)
            }
            "power" => {
                let exp = args
                    .first()
                    .and_then(|arg| arg.first())
                    .and_then(number_of)
                    .map(|number| f64_of(number.0))
                    .unwrap_or(1.0);
                if value.is_sign_negative() && exp.fract() != 0.0 {
                    return Ok(Outcome {
                        items: vec![],
                        ordered: true,
                    });
                }
                Decimal::from_f64_retain(f64_of(value).powf(exp)).unwrap_or(Decimal::ZERO)
            }
            _ => value,
        };
        let _ = scale;
        if matches!(name, "ceiling" | "floor" | "truncate") && matches!(item, Value::Integer(_)) {
            return Ok(one(Value::Integer(result.try_into().unwrap_or(0))));
        }
        Ok(one(Value::Decimal {
            value: result,
            scale: 8,
        }))
    }

    fn boundaries(
        &self,
        name: &str,
        input: &Outcome,
        args: &[Vec<Value>],
    ) -> Result<Outcome, Error> {
        let Some(item) = singleton(&input.items)? else {
            return Ok(Outcome {
                items: vec![],
                ordered: true,
            });
        };
        if name == "precision" {
            return Ok(precision_of(item)
                .map(|value| one(Value::Integer(value)))
                .unwrap_or(Outcome {
                    items: vec![],
                    ordered: true,
                }));
        }
        if name == "comparable" {
            let other = args.first().and_then(|arg| arg.first());
            let comparable = match (measured(item), other.and_then(measured)) {
                (Some(left), Some(right)) => left.unit.dim == right.unit.dim,
                _ => false,
            };
            return Ok(one(Value::system_bool(comparable)));
        }
        let precision = args.first().and_then(|arg| singleton_int_pure(arg));
        boundary(item, name == "lowBoundary", precision)
    }

    fn variable(&self, name: &str) -> Result<Value, Error> {
        let value = match name {
            "sct" => "http://snomed.info/sct",
            "loinc" => "http://loinc.org",
            "ucum" => "http://unitsofmeasure.org",
            "context" => return Ok(self.context.clone()),
            "resource" => {
                return Ok(node_from_json(
                    self.resource,
                    resource_type(self.resource),
                    String::new(),
                    Vec::new(),
                ))
            }
            "rootResource" => {
                return Ok(node_from_json(
                    self.root_resource,
                    resource_type(self.root_resource),
                    String::new(),
                    Vec::new(),
                ))
            }
            other => {
                if let Some(name) = other.strip_prefix("vs-") {
                    return Ok(Value::String(format!(
                        "http://hl7.org/fhir/ValueSet/{name}"
                    )));
                }
                if let Some(name) = other.strip_prefix("ext-") {
                    return Ok(Value::String(format!(
                        "http://hl7.org/fhir/StructureDefinition/{name}"
                    )));
                }
                return Err(Error::eval(format!("unknown variable %{other}")));
            }
        };
        Ok(Value::String(value.into()))
    }

    fn singleton_int(&self, items: &[Value]) -> Result<Option<i64>, Error> {
        Ok(singleton_int_pure(items))
    }
}

fn one(value: Value) -> Outcome {
    Outcome {
        items: vec![value],
        ordered: true,
    }
}

fn bool_outcome(value: Option<bool>) -> Outcome {
    value
        .map(|value| one(Value::system_bool(value)))
        .unwrap_or(Outcome {
            items: vec![],
            ordered: true,
        })
}

fn cast_one(item: &Value, target: &str) -> Vec<Value> {
    let (want_namespace, target) = split_type(target);
    let (namespace, name) = item.type_name();
    if let Some(want_namespace) = want_namespace {
        if namespace != want_namespace {
            return Vec::new();
        }
    }
    if name == target {
        return vec![item.clone()];
    }
    if let Value::Node {
        fhir_type: Some(fhir_type),
        ..
    } = item
    {
        if fhir_type == target {
            return vec![item.clone()];
        }
    }
    Vec::new()
}

fn singleton(items: &[Value]) -> Result<Option<&Value>, Error> {
    match items {
        [] => Ok(None),
        [item] => Ok(Some(item)),
        _ => Err(Error::eval("expected a singleton")),
    }
}

fn bool_operand(items: &[Value]) -> Result<Option<bool>, Error> {
    match items {
        [] => Ok(None),
        [item] => Ok(Some(item.as_singleton_bool().unwrap_or(true))),
        _ => Err(Error::eval("expected a boolean")),
    }
}

fn iif_criterion(items: &[Value]) -> Option<bool> {
    match items {
        [] => Some(false),
        [item] => item.as_singleton_bool(),
        _ => None,
    }
}

fn not_of(items: &[Value]) -> Option<bool> {
    match items {
        [item] => Some(!item.as_singleton_bool().unwrap_or(true)),
        _ => None,
    }
}

fn singleton_int_pure(items: &[Value]) -> Option<i64> {
    match items {
        [Value::Integer(value)] => Some(*value),
        [Value::Decimal { value, .. }] => i64::try_from(*value).ok(),
        _ => None,
    }
}

fn singleton_string(items: &[Value]) -> Result<Option<String>, Error> {
    match singleton(items)? {
        None => Ok(None),
        Some(value) => Ok(as_text(value)),
    }
}

fn arg_string(items: &[Value]) -> Option<String> {
    items.first().and_then(as_text)
}

fn type_arg(expr: Option<&Expr>) -> String {
    match expr {
        Some(Expr::TypeName(name) | Expr::Ident(name)) => name.clone(),
        _ => String::new(),
    }
}

fn split_type(name: &str) -> (Option<&str>, &str) {
    match name.rsplit_once('.') {
        Some((namespace, rest)) if namespace == "System" || namespace.ends_with("System") => {
            (Some("System"), rest)
        }
        Some((namespace, rest)) if namespace == "FHIR" || namespace.ends_with("FHIR") => {
            (Some("FHIR"), rest)
        }
        _ => (None, simple_name(name)),
    }
}

fn arg_type(items: &[Value]) -> String {
    match items.first() {
        Some(Value::String(value)) => simple_name(value).to_string(),
        Some(Value::TypeInfo { name, .. }) => name.clone(),
        _ => String::new(),
    }
}

fn concat_text(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Node {
            json: Json::String(value),
            fhir_type,
            ..
        } => match fhir_type.as_deref() {
            Some(
                "date" | "dateTime" | "instant" | "time" | "boolean" | "integer" | "decimal"
                | "unsignedInt" | "positiveInt",
            ) => None,
            _ => Some(value.clone()),
        },
        _ => None,
    }
}

fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Node {
            json: Json::String(value),
            ..
        } => Some(value.clone()),
        Value::Date(value) => Some(
            crate::value::Value::Date(value.clone())
                .render()
                .trim_start_matches('@')
                .to_string(),
        ),
        Value::DateTime(value) => Some(
            crate::value::Value::DateTime(value.clone())
                .render()
                .trim_start_matches('@')
                .to_string(),
        ),
        Value::Time(value) => Some(
            crate::value::Value::Time(value.clone())
                .render()
                .trim_start_matches("@T")
                .to_string(),
        ),
        Value::Integer(value) => Some(value.to_string()),
        Value::Boolean { value, .. } => Some(value.to_string()),
        Value::Decimal { value, scale } => Some(format_decimal(*value, *scale)),
        Value::Quantity { value, scale, unit } => {
            let number = format_decimal(*value, *scale);
            if is_calendar_word(unit) {
                Some(format!("{number} {unit}"))
            } else {
                Some(format!("{number} '{unit}'"))
            }
        }
        _ => None,
    }
}

fn is_calendar_word(unit: &str) -> bool {
    matches!(
        unit,
        "year"
            | "years"
            | "month"
            | "months"
            | "week"
            | "weeks"
            | "day"
            | "days"
            | "hour"
            | "hours"
            | "minute"
            | "minutes"
            | "second"
            | "seconds"
            | "millisecond"
            | "milliseconds"
    )
}

fn cmp_sort_key(left: &[Value], right: &[Value]) -> std::cmp::Ordering {
    match (left.first(), right.first()) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(left), Some(right)) => compare(left, right, BinOp::Lt)
            .map(|less| {
                if less {
                    std::cmp::Ordering::Less
                } else if compare(left, right, BinOp::Gt) == Some(true) {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .unwrap_or(std::cmp::Ordering::Equal),
    }
}

fn number_of(value: &Value) -> Option<(Decimal, u32)> {
    crate::value::as_number(value)
}

fn number_value(value: Decimal, scale: u32, integer: bool) -> Value {
    if integer && scale == 0 {
        if let Ok(value) = i64::try_from(value) {
            return Value::Integer(value);
        }
    }
    Value::Decimal { value, scale }
}

fn f64_of(value: Decimal) -> f64 {
    value.to_string().parse().unwrap_or(0.0)
}

fn union(left: &[Value], right: &[Value]) -> Vec<Value> {
    let mut items = Vec::new();
    for item in left.iter().chain(right) {
        if !items.iter().any(|existing| {
            equal_pair(existing, item) == Some(true) || equivalent_pair(existing, item)
        }) {
            items.push(item.clone());
        }
    }
    items
}

fn membership(needle: &[Value], haystack: &[Value]) -> bool {
    let Some(needle) = needle.first() else {
        return false;
    };
    if needle_is_collection(needle) {
        return false;
    }
    haystack
        .iter()
        .any(|item| equal_pair(needle, item) == Some(true))
}

fn needle_is_collection(_value: &Value) -> bool {
    false
}

fn compare(left: &Value, right: &Value, op: BinOp) -> Option<bool> {
    let ordering = if let (Some(left_q), Some(right_q)) = (measured(left), measured(right)) {
        if left_q.unit.dim != right_q.unit.dim || left_q.unit.factor.is_zero() {
            None
        } else {
            let right = right_q.value * right_q.unit.factor / left_q.unit.factor;
            Some(left_q.value.cmp(&right))
        }
    } else if let (Some(left), Some(right)) = (number_of(left), number_of(right)) {
        Some(left.0.cmp(&right.0))
    } else if temporal_of(left).is_some() && temporal_of(right).is_some() {
        temporal_values(left, right)
    } else if let (Some(left), Some(right)) = (as_text(left), as_text(right)) {
        Some(left.cmp(&right))
    } else {
        temporal_values(left, right)
    };
    let ordering = ordering?;
    Some(match op {
        BinOp::Lt => ordering.is_lt(),
        BinOp::Gt => ordering.is_gt(),
        BinOp::LtEq => ordering.is_le(),
        BinOp::GtEq => ordering.is_ge(),
        _ => false,
    })
}

fn temporal_values(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    let left = temporal_of(left)?;
    let right = temporal_of(right)?;
    temporal_cmp_values(&left.0, &right.0, left.1, right.1)
}

fn temporal_of(value: &Value) -> Option<(Temporal, crate::value::TimeKind)> {
    match value {
        Value::Date(value) => Some((value.clone(), crate::value::TimeKind::Date)),
        Value::DateTime(value) => Some((value.clone(), crate::value::TimeKind::DateTime)),
        Value::Time(value) => Some((value.clone(), crate::value::TimeKind::Time)),
        Value::Node {
            json: Json::String(text),
            fhir_type,
            ..
        } => parse_temporal_text(text, fhir_type.as_deref()),
        _ => None,
    }
}

fn parse_temporal_text(
    text: &str,
    fhir_type: Option<&str>,
) -> Option<(Temporal, crate::value::TimeKind)> {
    let kind = match fhir_type {
        Some("date") => crate::value::TimeKind::Date,
        Some("time") => crate::value::TimeKind::Time,
        Some("dateTime" | "instant") => crate::value::TimeKind::DateTime,
        _ => {
            if text.contains('T') {
                crate::value::TimeKind::DateTime
            } else if text.len() >= 8 && text.as_bytes().get(4) == Some(&b'-') {
                crate::value::TimeKind::Date
            } else {
                return None;
            }
        }
    };
    let parsed = crate::parse::parse(&format!("@{text}")).ok()?;
    let value = match parsed {
        Expr::Date(value) => value,
        Expr::DateTime(value) => value,
        Expr::Time(value) => value,
        _ => return None,
    };
    Some((value, kind))
}

fn temporal_cmp_values(
    left: &Temporal,
    right: &Temporal,
    left_kind: crate::value::TimeKind,
    right_kind: crate::value::TimeKind,
) -> Option<std::cmp::Ordering> {
    let mixed = matches!(
        (left_kind, right_kind),
        (
            crate::value::TimeKind::Date,
            crate::value::TimeKind::DateTime
        ) | (
            crate::value::TimeKind::DateTime,
            crate::value::TimeKind::Date
        )
    );
    if left_kind != right_kind && !mixed {
        return None;
    }
    if let (Some(left), Some(right)) = (instant_millis(left), instant_millis(right)) {
        return Some(left.cmp(&right));
    }
    let ordering = left
        .year
        .cmp(&right.year)
        .then(cmp_present(left.month, right.month))
        .then(cmp_present(left.day, right.day))
        .then(cmp_present(left.hour, right.hour))
        .then(cmp_present(left.minute, right.minute))
        .then(cmp_present(left.second, right.second))
        .then(cmp_present(left.millis, right.millis));
    if ordering != std::cmp::Ordering::Equal {
        return Some(ordering);
    }
    let left = clear_zero_millis(left);
    let right = clear_zero_millis(right);
    if crate::value::precision_rank(&left, left_kind)
        != crate::value::precision_rank(&right, right_kind)
    {
        return None;
    }
    Some(std::cmp::Ordering::Equal)
}

fn cmp_present<T: Ord>(left: Option<T>, right: Option<T>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        _ => std::cmp::Ordering::Equal,
    }
}

fn instant_millis(value: &Temporal) -> Option<i64> {
    let offset = value.offset?;
    let year = value.year?;
    let month = value.month?;
    let day = value.day?;
    let hour = value.hour.unwrap_or(0) as i64;
    let minute = value.minute.unwrap_or(0) as i64;
    let second = value.second.unwrap_or(0) as i64;
    let millis = value.millis.unwrap_or(0) as i64;
    let local = days_from_civil(year, month, day) * 86_400_000
        + hour * 3_600_000
        + minute * 60_000
        + second * 1000
        + millis;
    Some(local - offset as i64 * 60_000)
}

fn clear_zero_millis(value: &Temporal) -> Temporal {
    let mut value = value.clone();
    if value.millis == Some(0) {
        value.millis = None;
    }
    value
}

fn date_math(op: BinOp, left: &Value, right: &Value) -> Result<Option<Value>, Error> {
    let (temporal, quantity, negate) = match op {
        BinOp::Add => match (temporal_of(left), measured(right)) {
            (Some(temporal), Some(quantity)) => (temporal, quantity, false),
            _ => return Ok(None),
        },
        BinOp::Sub => match (temporal_of(left), measured(right)) {
            (Some(temporal), Some(quantity)) => (temporal, quantity, true),
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    if quantity.unit.indefinite {
        return Ok(None);
    }
    let mut amount = quantity.value;
    if negate {
        amount = -amount;
    }
    let whole = amount.trunc();
    if whole != amount && quantity.unit.label != "s" && quantity.unit.label != "ms" {
        // Fractional calendar units truncate toward zero.
    }
    let units = i64::try_from(whole).unwrap_or(0);
    let mut temporal = temporal.0;
    let kind = temporal_kind_of(left);
    if !add_units(&mut temporal, &quantity.unit.label, units) {
        return Ok(None);
    }
    Ok(Some(match kind {
        crate::value::TimeKind::Date => Value::Date(temporal),
        crate::value::TimeKind::Time => Value::Time(temporal),
        crate::value::TimeKind::DateTime => Value::DateTime(temporal),
    }))
}

fn temporal_kind_of(value: &Value) -> crate::value::TimeKind {
    match value {
        Value::Time(_) => crate::value::TimeKind::Time,
        Value::DateTime(_) => crate::value::TimeKind::DateTime,
        Value::Node {
            fhir_type: Some(name),
            ..
        } if name == "dateTime" || name == "instant" => crate::value::TimeKind::DateTime,
        Value::Node {
            json: Json::String(text),
            ..
        } if text.contains('T') => crate::value::TimeKind::DateTime,
        _ => crate::value::TimeKind::Date,
    }
}

fn add_units(temporal: &mut Temporal, unit: &str, amount: i64) -> bool {
    match unit {
        "a" | "mo" => false,
        "year" | "years" => add_years(temporal, amount),
        "month" | "months" => add_months(temporal, amount),
        "wk" | "week" | "weeks" => add_days(temporal, amount * 7),
        "d" | "day" | "days" => add_days(temporal, amount),
        "h" | "hour" | "hours" => add_seconds(temporal, amount * 3600),
        "min" | "minute" | "minutes" => add_seconds(temporal, amount * 60),
        "s" | "second" | "seconds" => add_seconds(temporal, amount),
        "ms" | "millisecond" | "milliseconds" => add_millis(temporal, amount),
        _ => false,
    }
}

fn add_months(temporal: &mut Temporal, months: i64) -> bool {
    let (Some(year), Some(month)) = (temporal.year, temporal.month) else {
        return false;
    };
    let total = year as i64 * 12 + (month as i64 - 1) + months;
    let year = total.div_euclid(12) as i32;
    let month = (total.rem_euclid(12) + 1) as u8;
    temporal.year = Some(year);
    temporal.month = Some(month);
    if let Some(day) = temporal.day {
        temporal.day = Some(day.min(days_in_month(year, month)));
    }
    true
}

fn add_years(temporal: &mut Temporal, years: i64) -> bool {
    let Some(year) = temporal.year else {
        return false;
    };
    let year = year as i64 + years;
    if year < i32::MIN as i64 || year > i32::MAX as i64 {
        return false;
    }
    let year = year as i32;
    temporal.year = Some(year);
    if let (Some(month), Some(day)) = (temporal.month, temporal.day) {
        temporal.day = Some(day.min(days_in_month(year, month)));
    }
    true
}

/// Days in `month` (1 to 12) of `year`.
pub fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

fn add_days(temporal: &mut Temporal, days: i64) -> bool {
    let (Some(year), Some(month), Some(day)) = (temporal.year, temporal.month, temporal.day) else {
        return false;
    };
    let (year, month, day) = civil_from_days(days_from_civil(year, month, day) + days);
    temporal.year = Some(year);
    temporal.month = Some(month);
    temporal.day = Some(day);
    true
}

fn add_seconds(temporal: &mut Temporal, seconds: i64) -> bool {
    let Some(hour) = temporal.hour else {
        return false;
    };
    let current = hour as i64 * 3600
        + temporal.minute.unwrap_or(0) as i64 * 60
        + temporal.second.unwrap_or(0) as i64
        + seconds;
    let day_delta = current.div_euclid(86_400);
    let within = current.rem_euclid(86_400);
    if day_delta != 0 && !add_days(temporal, day_delta) && temporal.year.is_some() {
        return false;
    }
    temporal.hour = Some((within / 3600) as u8);
    if temporal.minute.is_some() || within % 3600 != 0 {
        temporal.minute = Some(((within % 3600) / 60) as u8);
    }
    if temporal.second.is_some() || within % 60 != 0 {
        temporal.second = Some((within % 60) as u8);
    }
    true
}

fn add_millis(temporal: &mut Temporal, millis: i64) -> bool {
    let current = temporal.millis.unwrap_or(0) as i64 + millis;
    let extra = current.div_euclid(1000);
    temporal.millis = Some(current.rem_euclid(1000) as u32);
    if temporal.second.is_none() {
        temporal.second = Some(0);
    }
    add_seconds(temporal, extra)
}

fn measured(value: &Value) -> Option<Measured> {
    match value {
        Value::Quantity { value, unit, .. } => ucum::parse(unit).map(|unit| Measured {
            value: *value,
            unit,
        }),
        Value::Node {
            json, fhir_type, ..
        } if matches!(fhir_type.as_deref(), Some("Quantity" | "Age" | "Duration")) => {
            let text = json_number_text(json.get("value")?)?;
            let unit = json
                .get("code")
                .and_then(|item| item.as_str())
                .unwrap_or("1");
            let Ok(magnitude) = text.parse() else {
                return None;
            };
            ucum::parse(unit).map(|unit| Measured {
                value: magnitude,
                unit,
            })
        }
        _ => None,
    }
}

fn json_number_text(value: &Json) -> Option<String> {
    match value {
        Json::String(value) => Some(value.clone()),
        Json::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn quantity_scale(value: &Value) -> u32 {
    match value {
        Value::Quantity { scale, .. } => *scale,
        _ => 0,
    }
}

fn quantity_math(
    op: BinOp,
    left: &Measured,
    right: &Measured,
    left_scale: u32,
    right_scale: u32,
) -> Option<Value> {
    match op {
        BinOp::Mul => {
            let mut dim = left.unit.dim.clone();
            for (name, exp) in &right.unit.dim {
                *dim.entry(name.clone()).or_insert(0) += *exp;
            }
            dim.retain(|_, exp| *exp != 0);
            let value = left.value * left.unit.factor * right.value * right.unit.factor;
            let scale = left_scale.saturating_add(right_scale).saturating_add(2);
            Some(Value::Quantity {
                value,
                scale,
                unit: ucum::format_dim(&dim),
            })
        }
        BinOp::Div => {
            if right.value.is_zero() {
                return None;
            }
            let mut dim = left.unit.dim.clone();
            for (name, exp) in &right.unit.dim {
                *dim.entry(name.clone()).or_insert(0) -= *exp;
            }
            dim.retain(|_, exp| *exp != 0);
            let value = (left.value * left.unit.factor) / (right.value * right.unit.factor);
            Some(Value::Quantity {
                value,
                scale: 0,
                unit: ucum::format_dim(&dim),
            })
        }
        BinOp::Add | BinOp::Sub if left.unit.dim == right.unit.dim => {
            let right_value = right.value * right.unit.factor / left.unit.factor;
            let value = if matches!(op, BinOp::Add) {
                left.value + right_value
            } else {
                left.value - right_value
            };
            Some(Value::Quantity {
                value,
                scale: left_scale.max(right_scale),
                unit: left.unit.label.clone(),
            })
        }
        _ => None,
    }
}

fn convert_one(kind: &str, value: &Value) -> Option<Value> {
    let text = as_text(value)?;
    match kind {
        "String" | "string" => Some(Value::String(text)),
        "Integer" | "integer" => match value {
            Value::Boolean { value: true, .. } => Some(Value::Integer(1)),
            Value::Boolean { value: false, .. } => Some(Value::Integer(0)),
            _ => text.parse::<i64>().ok().map(Value::Integer),
        },
        "Decimal" | "decimal" => {
            if let Value::Boolean { value, .. } = value {
                return Some(Value::Decimal {
                    value: Decimal::from(if *value { 1 } else { 0 }),
                    scale: 0,
                });
            }
            let scale = text
                .split_once('.')
                .map(|(_, fraction)| fraction.len() as u32)
                .unwrap_or(0);
            Decimal::from_str_exact(&text)
                .ok()
                .map(|value| Value::Decimal { value, scale })
        }
        "Boolean" | "boolean" => match value {
            Value::Integer(1) => Some(Value::system_bool(true)),
            Value::Integer(0) => Some(Value::system_bool(false)),
            Value::Decimal { value, .. } if *value == Decimal::ONE || *value == Decimal::ZERO => {
                Some(Value::system_bool(*value == Decimal::ONE))
            }
            _ => match text.to_ascii_lowercase().as_str() {
                "true" | "t" | "yes" | "y" | "1" | "1.0" => Some(Value::system_bool(true)),
                "false" | "f" | "no" | "n" | "0" | "0.0" => Some(Value::system_bool(false)),
                _ => None,
            },
        },
        "Date" | "date" => {
            parse_temporal_text(&text, Some("date")).map(|(value, _)| Value::Date(value))
        }
        "DateTime" | "dateTime" => {
            parse_temporal_text(&text, Some("dateTime")).map(|(value, _)| Value::DateTime(value))
        }
        "Time" | "time" => {
            let literal = if text.starts_with("@T") {
                text
            } else if text.starts_with('T') {
                format!("@{text}")
            } else {
                format!("@T{text}")
            };
            parse_temporal_text(literal.trim_start_matches('@'), Some("time"))
                .map(|(value, _)| Value::Time(value))
        }
        "Quantity" | "quantity" => {
            match value {
                Value::Integer(number) => {
                    return Some(Value::Quantity {
                        value: Decimal::from(*number),
                        scale: 0,
                        unit: "1".into(),
                    });
                }
                Value::Decimal { value, scale } => {
                    return Some(Value::Quantity {
                        value: *value,
                        scale: *scale,
                        unit: "1".into(),
                    });
                }
                Value::Boolean { value, .. } => {
                    return Some(Value::Quantity {
                        value: Decimal::from(if *value { 1 } else { 0 }),
                        scale: 0,
                        unit: "1".into(),
                    });
                }
                Value::Quantity { .. } => return Some(value.clone()),
                _ => {}
            }
            if let Ok(number) = Decimal::from_str_exact(text.trim()) {
                let scale = text
                    .trim()
                    .split_once('.')
                    .map(|(_, fraction)| fraction.len() as u32)
                    .unwrap_or(0);
                return Some(Value::Quantity {
                    value: number,
                    scale,
                    unit: "1".into(),
                });
            }
            let (number, unit) = text.split_once(' ')?;
            if !unit.contains('\'') && !is_calendar_word(unit.trim_matches('\'')) {
                return None;
            }
            let scale = number
                .split_once('.')
                .map(|(_, fraction)| fraction.len() as u32)
                .unwrap_or(0);
            Some(Value::Quantity {
                value: Decimal::from_str_exact(number).ok()?,
                scale,
                unit: unit.trim_matches('\'').to_string(),
            })
        }
        _ => None,
    }
}

fn system_name(target: &str) -> Option<&'static str> {
    Some(match target {
        "Boolean" | "boolean" => "Boolean",
        "Integer" | "integer" => "Integer",
        "Decimal" | "decimal" => "Decimal",
        "String" | "string" => "String",
        "Date" | "date" => "Date",
        "DateTime" | "dateTime" => "DateTime",
        "Time" | "time" => "Time",
        "Quantity" => "Quantity",
        _ => return None,
    })
}

fn is_null_node(value: &Value) -> bool {
    matches!(
        value,
        Value::Node {
            json: Json::Null,
            ..
        }
    )
}

fn resource_type(json: &Json) -> Option<String> {
    json.get("resourceType")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

/// The values a plain path of members (`Slot.start`, `Appointment.participant.actor`) reads out of
/// a resource, as the JSON the evaluator would have given for each, without evaluating anything.
///
/// Evaluating is for the general case: it copies the whole resource into a node, and every member
/// it reads into another, and a search does that for each row it checks and each sort key it needs.
/// A path that is only members is a walk down the JSON. This gives what `evaluate` gives for it, or
/// `None` where it cannot be sure of that, and the caller evaluates:
///
/// - the expression is not a bare path, or its first name is not the resource's own type (a
///   supertype such as `Resource` is the evaluator's to read);
/// - a member is a choice element (`value[x]`), is not an element the model holds at that point
///   (the evaluator answers an unknown element with nothing or, for a strict host, an error), or is
///   `id` or `resourceType`;
/// - a member the model types as a boolean or a number is written as a string, which the evaluator
///   turns into a boolean or a number.
///
/// An element that is absent is not in doubt: it reads as nothing, as evaluating gives.
pub fn member_values<'a>(expr: &Expr, root: &'a Json, model: &Model) -> Option<Vec<&'a Json>> {
    let (root_name, members) = expr.member_path()?;
    if root.get("resourceType")?.as_str()? != root_name {
        return None;
    }
    // Where the values read so far sit in the model, and a buffer for the path of the next element,
    // so a step allocates nothing of its own.
    let mut path = String::from(root_name);
    let mut key = String::new();
    let mut current = vec![root];
    for name in members {
        if name == "id" || name == "resourceType" {
            return None;
        }
        // The same steps `child` takes, in the same order.
        if model.choice_path(&path, name).is_some() || !model.known_child(&path, name) {
            return None;
        }
        key.clear();
        key.push_str(&path);
        key.push('.');
        key.push_str(name);
        let element_path = model.canonical(&key);
        let fhir_type = model.element_types(element_path).first();
        let coerced = matches!(
            fhir_type.map(String::as_str),
            Some("boolean" | "integer" | "unsignedInt" | "positiveInt" | "decimal")
        );
        let mut next = Vec::new();
        for node in current {
            match node.get(name) {
                Some(Json::Array(items)) => next.extend(items.iter()),
                Some(value) => next.push(value),
                None => {}
            }
        }
        if coerced && next.iter().any(|value| value.is_string()) {
            return None;
        }
        let child_path = match fhir_type {
            Some(fhir_type)
                if !model.has_descendants(element_path) && model.has_descendants(fhir_type) =>
            {
                fhir_type.as_str()
            }
            _ => element_path,
        };
        path.clear();
        path.push_str(child_path);
        current = next;
    }
    Some(current)
}

fn node_from_json(
    json: &Json,
    fhir_type: Option<String>,
    path: String,
    extensions: Vec<Json>,
) -> Value {
    let path = if path.is_empty() {
        fhir_type.clone().unwrap_or_default()
    } else {
        path
    };
    Value::Node {
        json: json.clone(),
        fhir_type: fhir_type.or_else(|| resource_type(json)),
        path,
        extensions,
    }
}

fn extension_siblings(json: &Json, name: &str) -> Vec<Json> {
    let underscore = json.get(format!("_{name}")).cloned().unwrap_or(Json::Null);
    match underscore {
        Json::Array(items) => items,
        Json::Object(_) => vec![underscore],
        _ => Vec::new(),
    }
}

fn materialize_list(
    value: &Json,
    extensions: &[Json],
    fhir_type: Option<&str>,
    path: &str,
) -> Vec<Value> {
    match value {
        Json::Array(items) => items
            .iter()
            .enumerate()
            .map(|(index, item)| materialize_one(item, extensions.get(index), fhir_type, path))
            .collect(),
        other => vec![materialize_one(other, extensions.first(), fhir_type, path)],
    }
}

fn materialize_one(
    value: &Json,
    extension: Option<&Json>,
    fhir_type: Option<&str>,
    path: &str,
) -> Value {
    let extensions = extension
        .and_then(|value| value.get("extension"))
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let mut json = value.clone();
    if let Some(fhir_type) = fhir_type {
        json = coerce_primitive(&json, fhir_type);
    }
    Value::Node {
        json,
        fhir_type: fhir_type.map(|value| value.to_string()),
        path: path.to_string(),
        extensions,
    }
}

fn coerce_primitive(value: &Json, fhir_type: &str) -> Json {
    let Some(text) = value.as_str() else {
        return value.clone();
    };
    match fhir_type {
        "boolean" => Json::Bool(text == "true"),
        "integer" | "unsignedInt" | "positiveInt" => text
            .parse::<i64>()
            .map(|number| Json::Number(number.into()))
            .unwrap_or_else(|_| value.clone()),
        "decimal" => serde_json::Number::from_f64(text.parse().unwrap_or(0.0))
            .map(Json::Number)
            .unwrap_or_else(|| value.clone()),
        _ => value.clone(),
    }
}

fn choice_values(json: &Json, _choice_path: &str, path: &str, name: &str) -> Vec<Value> {
    let Some(object) = json.as_object() else {
        return vec![];
    };
    let mut items = Vec::new();
    for (key, value) in object {
        let Some(suffix) = key.strip_prefix(name) else {
            continue;
        };
        if suffix.is_empty() || !suffix.chars().next().is_some_and(|ch| ch.is_uppercase()) {
            continue;
        }
        let fhir_type = choice_type_name(suffix);
        let child_path = if fhir_type.chars().next().is_some_and(|ch| ch.is_uppercase()) {
            fhir_type.clone()
        } else if path.is_empty() {
            name.to_string()
        } else {
            format!("{path}.{name}")
        };
        items.extend(materialize_list(value, &[], Some(&fhir_type), &child_path));
    }
    items
}

fn choice_type_name(suffix: &str) -> String {
    let mut chars = suffix.chars();
    match chars.next() {
        Some(first) => {
            let rest: String = chars.collect();
            let lowered = format!("{}{rest}", first.to_lowercase());
            if lowered.chars().next().is_some_and(|ch| ch.is_lowercase())
                && suffix_is_primitive(&lowered)
            {
                lowered
            } else {
                suffix.to_string()
            }
        }
        None => suffix.to_string(),
    }
}

fn suffix_is_primitive(name: &str) -> bool {
    matches!(
        name,
        "boolean"
            | "integer"
            | "string"
            | "decimal"
            | "uri"
            | "url"
            | "canonical"
            | "base64Binary"
            | "instant"
            | "date"
            | "dateTime"
            | "time"
            | "code"
            | "oid"
            | "id"
            | "markdown"
            | "unsignedInt"
            | "positiveInt"
            | "uuid"
            | "xhtml"
    )
}

fn extensions_of(value: &Value, url: &str) -> Vec<Value> {
    let extensions = match value {
        Value::Node {
            extensions, json, ..
        } => {
            let mut all = extensions.clone();
            if let Some(extra) = json.get("extension").and_then(|value| value.as_array()) {
                all.extend(extra.clone());
            }
            all
        }
        _ => return vec![],
    };
    extensions
        .into_iter()
        .filter(|item| item.get("url").and_then(|value| value.as_str()) == Some(url))
        .map(|json| Value::Node {
            fhir_type: Some("Extension".into()),
            path: "Extension".into(),
            extensions: vec![],
            json,
        })
        .collect()
}

impl Ctx<'_> {
    fn children_of(&self, items: &[Value]) -> Vec<Value> {
        let mut children = Vec::new();
        for item in items {
            let Value::Node { json, path, .. } = item else {
                continue;
            };
            let Some(object) = json.as_object() else {
                continue;
            };
            for (key, value) in object {
                if key.starts_with('_') || key == "resourceType" {
                    continue;
                }
                let element_path = self
                    .model
                    .canonical(&if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    })
                    .to_string();
                let types = self.model.element_types(&element_path);
                let fhir_type = types.first().cloned();
                let child_path = match fhir_type.as_deref() {
                    Some(fhir_type)
                        if !self.model.has_descendants(&element_path)
                            && self.model.has_descendants(fhir_type) =>
                    {
                        fhir_type.to_string()
                    }
                    _ => element_path,
                };
                children.extend(materialize_list(
                    value,
                    &extension_siblings(json, key),
                    fhir_type.as_deref(),
                    &child_path,
                ));
            }
        }
        children
    }

    fn descendants_of(&self, items: &[Value]) -> Vec<Value> {
        let mut all = Vec::new();
        let mut pending = self.children_of(items);
        let mut guard = 0;
        while !pending.is_empty() && guard < 64 {
            guard += 1;
            let next = self.children_of(&pending);
            all.extend(pending);
            pending = next;
        }
        all
    }
}

fn reference_text(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Node { json, .. } => json
            .get("reference")
            .and_then(|item| item.as_str())
            .or_else(|| json.as_str())
            .map(|value| value.to_string()),
        _ => None,
    }
}

fn contained(root: &Json, id: &str) -> Option<Json> {
    root.get("contained")?
        .as_array()?
        .iter()
        .find(|item| item.get("id").and_then(|value| value.as_str()) == Some(id))
        .cloned()
}

fn code_of(value: &Value) -> (Option<String>, String) {
    match value {
        Value::Node { json, .. } => {
            if let Some(code) = json.get("code").and_then(|item| item.as_str()) {
                return (
                    json.get("system")
                        .and_then(|item| item.as_str())
                        .map(|value| value.to_string()),
                    code.to_string(),
                );
            }
            (None, json.as_str().unwrap_or("").to_string())
        }
        other => (None, as_text(other).unwrap_or_default()),
    }
}

fn precision_of(value: &Value) -> Option<i64> {
    if matches!(value, Value::Node { .. }) {
        return precision_of(&temporal_value(value)?);
    }
    match value {
        Value::Decimal { scale, .. } => Some(*scale as i64),
        Value::Integer(_) => Some(0),
        Value::Date(value) => Some(value.precision_digits(crate::value::TimeKind::Date) as i64),
        Value::DateTime(value) => {
            Some(value.precision_digits(crate::value::TimeKind::DateTime) as i64)
        }
        Value::Time(value) => Some(value.precision_digits(crate::value::TimeKind::Time) as i64),
        _ => None,
    }
}

fn temporal_value(value: &Value) -> Option<Value> {
    match value {
        Value::Date(_) | Value::DateTime(_) | Value::Time(_) => Some(value.clone()),
        Value::Node {
            json: Json::String(text),
            fhir_type,
            ..
        } => {
            let (temporal, kind) = parse_temporal_text(text, fhir_type.as_deref())?;
            Some(match kind {
                crate::value::TimeKind::Date => Value::Date(temporal),
                crate::value::TimeKind::DateTime => Value::DateTime(temporal),
                crate::value::TimeKind::Time => Value::Time(temporal),
            })
        }
        _ => None,
    }
}

fn boundary(value: &Value, low: bool, precision: Option<i64>) -> Result<Outcome, Error> {
    let owned;
    let value = if let Some(temporal) = temporal_value(value) {
        owned = temporal;
        &owned
    } else {
        value
    };
    let precision = precision.unwrap_or(match value {
        Value::Date(_) | Value::DateTime(_) => 17,
        Value::Time(_) => 9,
        _ => 8,
    });
    let empty = Outcome {
        items: vec![],
        ordered: true,
    };
    match value {
        Value::Integer(number) => {
            if !(0..=31).contains(&precision) {
                return Ok(empty);
            }
            Ok(one(decimal_from_boundary(&boundary_decimal(
                &number.to_string(),
                precision,
                low,
            ))))
        }
        Value::Decimal { value, scale } => {
            if !(0..=31).contains(&precision) {
                return Ok(empty);
            }
            Ok(one(decimal_from_boundary(&boundary_decimal(
                &format_decimal(*value, *scale),
                precision,
                low,
            ))))
        }
        Value::Quantity { value, scale, unit } => {
            if !(0..=31).contains(&precision) {
                return Ok(empty);
            }
            match decimal_from_boundary(&boundary_decimal(
                &format_decimal(*value, *scale),
                precision,
                low,
            )) {
                Value::Decimal { value, scale } => Ok(one(Value::Quantity {
                    value,
                    scale,
                    unit: unit.clone(),
                })),
                other => Ok(one(other)),
            }
        }
        Value::Date(temporal) | Value::DateTime(temporal) | Value::Time(temporal) => Ok(
            temporal_boundary(temporal, temporal_kind_of(value), low, precision as u32)
                .unwrap_or(empty),
        ),
        _ => Ok(empty),
    }
}

fn decimal_from_boundary(text: &str) -> Value {
    let scale = text
        .split_once('.')
        .map(|(_, fraction)| fraction.len() as u32)
        .unwrap_or(0);
    let numeric: String = text.chars().filter(|ch| *ch != '-').collect();
    let zero = numeric.chars().all(|ch| ch == '0' || ch == '.');
    if text.starts_with('-') && zero {
        return Value::String(text.to_string());
    }
    let value = Decimal::from_str_exact(text).unwrap_or(Decimal::ZERO);
    Value::Decimal { value, scale }
}

fn boundary_decimal(value: &str, precision: i64, low: bool) -> String {
    if low {
        low_boundary_decimal(value, precision)
    } else {
        high_boundary_decimal(value, precision)
    }
}

fn low_boundary_decimal(value: &str, precision: i64) -> String {
    let (number, exponent) = split_exponent(value);
    if decimal_is_zero(number) {
        return apply_precision("-0.5000000000000000000000000", precision, true);
    }
    if let Some(rest) = number.strip_prefix('-') {
        return format!(
            "-{}{}",
            high_boundary_decimal(rest, precision),
            exponent.unwrap_or("")
        );
    }
    let expanded = if number.contains('.') {
        format!("{}50000000000000000000000000000", minus_one(number))
    } else {
        format!("{}.50000000000000000000000000000", minus_one(number))
    };
    format!(
        "{}{}",
        apply_precision(&expanded, precision, true),
        exponent.unwrap_or("")
    )
}

fn high_boundary_decimal(value: &str, precision: i64) -> String {
    let (number, exponent) = split_exponent(value);
    if decimal_is_zero(number) {
        return apply_precision("0.50000000000000000000000000000", precision, false);
    }
    if let Some(rest) = number.strip_prefix('-') {
        return format!(
            "-{}{}",
            low_boundary_decimal(rest, precision),
            exponent.unwrap_or("")
        );
    }
    let expanded = if number.contains('.') {
        format!("{number}50000000000000000000000000000")
    } else {
        format!("{number}.50000000000000000000000000000")
    };
    format!(
        "{}{}",
        apply_precision(&expanded, precision, false),
        exponent.unwrap_or("")
    )
}

fn split_exponent(value: &str) -> (&str, Option<&str>) {
    match value.find(['e', 'E']) {
        Some(index) => (&value[..index], Some(&value[index..])),
        None => (value, None),
    }
}

fn decimal_is_zero(value: &str) -> bool {
    value.chars().all(|ch| matches!(ch, '0' | '.' | '-' | '+'))
}

fn apply_precision(value: &str, precision: i64, down: bool) -> String {
    let mut digits = value.to_string();
    let dot = digits.find('.');
    if let Some(dot) = dot {
        digits.remove(dot);
    }
    let current = decimal_places(value);
    let delta = precision - current as i64;
    let mut body = if delta == 0 {
        digits
    } else if delta > 0 {
        format!("{digits}{}", "0".repeat(delta as usize))
    } else {
        let mut cut = value.len() as i64 + delta;
        if dot.is_some() {
            cut -= 1;
        }
        let cut = cut.max(0) as usize;
        if !down && digits.as_bytes().get(cut).is_some_and(|ch| *ch >= b'5') && cut > 0 {
            let previous = digits.as_bytes()[cut - 1] + 1;
            format!("{}{}", &digits[..cut - 1], previous as char)
        } else {
            digits[..cut.min(digits.len())].to_string()
        }
    };
    if body.ends_with('.') {
        body.pop();
    }
    match dot {
        Some(dot) if dot < body.len() => format!("{}.{}", &body[..dot], &body[dot..]),
        _ => body,
    }
}

fn decimal_places(value: &str) -> usize {
    let value = value.split(['e', 'E']).next().unwrap_or(value);
    value
        .split_once('.')
        .map(|(_, fraction)| fraction.len())
        .unwrap_or(0)
}

fn minus_one(value: &str) -> String {
    let mut chars: Vec<char> = value.chars().collect();
    for ch in chars.iter_mut().rev() {
        if *ch == '0' {
            *ch = '9';
        } else if *ch != '.' {
            *ch = char::from_u32(*ch as u32 - 1).unwrap_or(*ch);
            break;
        }
    }
    chars.into_iter().collect()
}

fn temporal_boundary(
    temporal: &Temporal,
    kind: crate::value::TimeKind,
    low: bool,
    precision: u32,
) -> Option<Outcome> {
    let allowed: &[u32] = match kind {
        crate::value::TimeKind::Time => &[2, 4, 6, 9],
        crate::value::TimeKind::Date | crate::value::TimeKind::DateTime => {
            &[4, 6, 8, 10, 12, 14, 17]
        }
    };
    if !allowed.contains(&precision) {
        return None;
    }
    let mut temporal = temporal.clone();
    if kind == crate::value::TimeKind::Time {
        fill_time(&mut temporal, low, precision);
        return Some(one(Value::Time(temporal)));
    }
    fill_date(&mut temporal, low, precision);
    let value = if temporal.hour.is_some() {
        Value::DateTime(temporal)
    } else {
        Value::Date(temporal)
    };
    Some(one(value))
}

fn fill_time(temporal: &mut Temporal, low: bool, precision: u32) {
    if precision < 4 {
        temporal.minute = None;
        temporal.second = None;
        temporal.millis = None;
        return;
    }
    if temporal.minute.is_none() {
        temporal.minute = Some(if low { 0 } else { 59 });
    }
    if precision < 6 {
        temporal.second = None;
        temporal.millis = None;
        return;
    }
    if temporal.second.is_none() {
        temporal.second = Some(if low { 0 } else { 59 });
    }
    if precision < 9 {
        temporal.millis = None;
        return;
    }
    if temporal.millis.is_none() {
        temporal.millis = Some(if low { 0 } else { 999 });
    }
}

fn fill_date(temporal: &mut Temporal, low: bool, precision: u32) {
    if precision < 6 {
        temporal.month = None;
    } else if temporal.month.is_none() {
        temporal.month = Some(if low { 1 } else { 12 });
    }
    if precision < 8 {
        temporal.day = None;
    } else if temporal.day.is_none() {
        let year = temporal.year.unwrap_or(1970);
        let month = temporal.month.unwrap_or(1);
        temporal.day = Some(if low { 1 } else { days_in_month(year, month) });
    }
    if precision < 10 {
        temporal.hour = None;
        temporal.minute = None;
        temporal.second = None;
        temporal.millis = None;
        temporal.offset = None;
        return;
    }
    if temporal.hour.is_none() {
        temporal.hour = Some(if low { 0 } else { 23 });
    }
    if precision < 12 {
        temporal.minute = None;
        temporal.second = None;
        temporal.millis = None;
        return;
    }
    if temporal.minute.is_none() {
        temporal.minute = Some(if !low && precision < 17 { 59 } else { 0 });
    }
    if precision < 14 {
        temporal.second = None;
        temporal.millis = None;
        return;
    }
    if temporal.second.is_none() {
        temporal.second = Some(if low { 0 } else { 59 });
    }
    if precision < 17 {
        temporal.millis = None;
        return;
    }
    if temporal.millis.is_none() {
        temporal.millis = Some(if low { 0 } else { 999 });
    }
    if temporal.offset.is_none() {
        temporal.offset = Some(if low { 14 * 60 } else { -12 * 60 });
    }
}

#[allow(dead_code)]
fn _map() -> BTreeMap<String, Json> {
    Map::new().into_iter().collect()
}
