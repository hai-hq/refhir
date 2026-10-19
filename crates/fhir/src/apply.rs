//! `ChargeItemDefinition/[id]/$apply`: what a ChargeItem costs under a definition.
//!
//! A `ChargeItemDefinition` says when it applies (`applicability`, FHIRPath conditions over the
//! ChargeItem) and what the price is made of (`propertyGroup.priceComponent`: a base price, and
//! surcharges, deductions, discounts and taxes that apply under conditions of their own). R4 gives
//! the operation two inputs, `chargeItem` and `account`, and says its result is "Any" resource,
//! with a description copied from `ActivityDefinition/$apply` that does not fit. This is what is
//! made of that:
//!
//! - The result is a draft `Invoice`, not stored. It is the resource FHIR means these prices for
//!   ("acquire pricing details for creating an Invoice", as the resource page says), its line item
//!   has price components of the same shape as the definition's, and `account` is its `account`.
//!   The caller keeps it, changes it or throws it away.
//! - `applicability` conditions are FHIRPath with the ChargeItem as `%context`, as the element
//!   says. Every condition of the definition must be true, or the definition does not apply to this
//!   ChargeItem (`422`, `business-rule`). A property group is used when every condition of its own is
//!   true, and a group with none always is.
//! - The amounts are the definition's, in the definition's currency, with no rounding and no
//!   conversion. A component with a `factor` and no `amount` is that fraction of the base price.
//!   The ChargeItem's `quantity` is not applied: the definition does not say what its prices are
//!   for.
//! - `totalNet` is the base price plus surcharges, less deductions and discounts (taken as they are
//!   written, positive or negative); `totalGross` adds tax. `informational` components are shown and
//!   never added. With no base price, nothing is added up and there is no total: no price.

use std::str::FromStr;

use refhir_database::Transaction;
use refhir_fhirpath::Value as PathValue;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde_json::{json, Value};

use crate::catalog::Catalog;
use crate::error::FhirError;
use crate::ops;
use crate::resolve::TxHost;

const TYPE: &str = "ChargeItemDefinition";

/// The component types of `ChargeItemDefinition.propertyGroup.priceComponent.type`
/// (`http://hl7.org/fhir/ValueSet/invoice-priceComponentType`).
const COMPONENT_TYPES: [&str; 6] = [
    "base",
    "surcharge",
    "deduction",
    "discount",
    "tax",
    "informational",
];

/// Say in a CapabilityStatement that a definition can be applied.
pub(crate) fn annotate_capability(statement: &mut Value) {
    let Some(resources) = statement
        .pointer_mut("/rest/0/resource")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for resource in resources {
        if resource.get("type").and_then(Value::as_str) != Some(TYPE) {
            continue;
        }
        let operation = json!({
            "name": "apply",
            "definition": "http://hl7.org/fhir/OperationDefinition/ChargeItemDefinition-apply"
        });
        match resource.get_mut("operation").and_then(Value::as_array_mut) {
            Some(operations) => operations.push(operation),
            None => resource["operation"] = json!([operation]),
        }
    }
}

/// The types and interactions a request to apply reads besides the definition: the ChargeItem,
/// and the Account if one is named. The path of the request names only the definition.
pub fn apply_reads(body: &Value) -> Vec<(String, &'static str)> {
    let mut reads = vec![("ChargeItem".to_string(), "read")];
    if parameter(body, "account").is_some() {
        reads.push(("Account".to_string(), "read"));
    }
    reads
}

pub(crate) fn operation(
    tx: &mut Transaction,
    catalog: &Catalog,
    type_name: &str,
    args: &Value,
) -> Result<Value, FhirError> {
    if type_name != TYPE {
        return Err(FhirError::Unsupported(format!(
            "unsupported operation {type_name}/$apply"
        )));
    }
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| FhirError::Invalid("a ChargeItemDefinition id is required".into()))?;
    let body = args
        .get("resource")
        .ok_or_else(|| FhirError::Invalid("Parameters with a chargeItem are required".into()))?;
    let charge_item_reference = reference_parameter(body, "chargeItem", "ChargeItem")?
        .ok_or_else(|| FhirError::Invalid("the parameter chargeItem is required".into()))?;
    let account_reference = reference_parameter(body, "account", "Account")?;

    // The definition is what the URL names, so it is a 404 when it is not there. What the
    // parameters name is not in the URL: a request that names something that does not exist is
    // well formed and cannot be acted on, a 422.
    let definition = ops::read(tx, catalog.get(TYPE)?, id)?;
    let charge_item = read_named(tx, catalog, "ChargeItem", &charge_item_reference)?;
    if let Some(account) = &account_reference {
        read_named(tx, catalog, "Account", account)?;
    }

    // The definition as a whole.
    for (position, condition) in conditions(&definition).iter().enumerate() {
        let path = format!("ChargeItemDefinition.applicability[{position}]");
        if !holds(tx, catalog, condition, &charge_item, &path)? {
            return Err(FhirError::Unprocessable(outcome(
                "business-rule",
                &format!(
                    "ChargeItemDefinition/{id} does not apply to {charge_item_reference}: {}",
                    describe(condition)
                ),
                &path,
            )));
        }
    }

    // The price components of the groups that apply.
    let mut components = Vec::new();
    for (group_position, group) in definition
        .get("propertyGroup")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let mut applies = true;
        for (position, condition) in conditions(group).iter().enumerate() {
            let path = format!(
                "ChargeItemDefinition.propertyGroup[{group_position}].applicability[{position}]"
            );
            if !holds(tx, catalog, condition, &charge_item, &path)? {
                applies = false;
                break;
            }
        }
        if !applies {
            continue;
        }
        for (position, component) in group
            .get("priceComponent")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let path = format!(
                "ChargeItemDefinition.propertyGroup[{group_position}].priceComponent[{position}]"
            );
            components.push((component.clone(), path));
        }
    }
    let priced = price(&components)?;

    let mut line = json!({
        "sequence": 1,
        "chargeItemReference": {"reference": charge_item_reference}
    });
    if !priced.components.is_empty() {
        line["priceComponent"] = Value::Array(priced.components);
    }
    let mut invoice = json!({
        "resourceType": "Invoice",
        "status": "draft",
        "lineItem": [line]
    });
    if let Some(subject) = charge_item.get("subject") {
        invoice["subject"] = subject.clone();
    }
    if let Some(account) = account_reference {
        invoice["account"] = json!({"reference": account});
    }
    if let Some((net, gross, currency)) = priced.totals {
        invoice["totalNet"] = money(net, &currency);
        invoice["totalGross"] = money(gross, &currency);
    }
    Ok(invoice)
}

/// The value of the parameter called `name` of `Parameters`, if there is one.
fn parameter<'a>(body: &'a Value, name: &str) -> Option<&'a Value> {
    body.get("parameter")
        .and_then(Value::as_array)?
        .iter()
        .find(|item| item.get("name").and_then(Value::as_str) == Some(name))
}

/// The relative reference (`Type/id`) a Reference parameter holds, for the type it must be of.
fn reference_parameter(
    body: &Value,
    name: &str,
    type_name: &str,
) -> Result<Option<String>, FhirError> {
    if body.get("resourceType").and_then(Value::as_str) != Some("Parameters") {
        return Err(FhirError::Invalid(
            "the body must be a Parameters resource".into(),
        ));
    }
    let Some(found) = parameter(body, name) else {
        return Ok(None);
    };
    let reference = found
        .pointer("/valueReference/reference")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            FhirError::Invalid(format!("the parameter {name} must be a valueReference"))
        })?;
    let wanted = format!("{type_name}/");
    match reference.strip_prefix(&wanted) {
        Some(id) if refhir_types::valid_id(id) => Ok(Some(reference.to_string())),
        _ => Err(FhirError::Invalid(format!(
            "the parameter {name} must refer to a {type_name} as {wanted}[id], not {reference:?}"
        ))),
    }
}

/// The resource a parameter names, or a 422 that says it is not there.
fn read_named(
    tx: &mut Transaction,
    catalog: &Catalog,
    type_name: &str,
    reference: &str,
) -> Result<Value, FhirError> {
    let id = reference.rsplit('/').next().unwrap_or("");
    let model = catalog.get(type_name)?;
    match ops::read(tx, model, id) {
        Err(FhirError::NotFound(_)) => Err(FhirError::Unprocessable(outcome(
            "not-found",
            &format!("{reference} does not exist"),
            type_name,
        ))),
        other => other,
    }
}

fn conditions(of: &Value) -> Vec<&Value> {
    of.get("applicability")
        .and_then(Value::as_array)
        .map(|items| items.iter().collect())
        .unwrap_or_default()
}

fn describe(condition: &Value) -> String {
    match condition.get("description").and_then(Value::as_str) {
        Some(text) => format!("{text:?} is not true of it"),
        None => format!(
            "{:?} is not true of it",
            condition
                .get("expression")
                .and_then(Value::as_str)
                .unwrap_or("")
        ),
    }
}

/// Whether one `applicability` condition is true of the ChargeItem. A condition that cannot be
/// decided (in a language the server does not evaluate, with no expression, or that does not
/// parse, run or give true or false) is a 422 that says which, and is never taken as true: a price
/// given for a condition nobody checked would be a wrong price.
fn holds(
    tx: &mut Transaction,
    catalog: &Catalog,
    condition: &Value,
    charge_item: &Value,
    path: &str,
) -> Result<bool, FhirError> {
    let cannot = |why: String| FhirError::Unprocessable(outcome("not-supported", &why, path));
    let language = condition
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("text/fhirpath");
    let media_type = language.split(';').next().unwrap_or("").trim();
    if !media_type.eq_ignore_ascii_case("text/fhirpath") {
        return Err(cannot(format!(
            "the language {language:?} is not supported: only text/fhirpath is evaluated"
        )));
    }
    let Some(text) = condition.get("expression").and_then(Value::as_str) else {
        return Err(cannot(
            "the condition has no expression to evaluate".to_string(),
        ));
    };
    let expression = refhir_fhirpath::parse(text)
        .map_err(|err| cannot(format!("the expression {text:?} cannot be read: {err}")))?;
    let host = TxHost::new(tx, catalog);
    let result = refhir_fhirpath::evaluate(&expression, charge_item, catalog.path_model(), &host)
        .map_err(|err| {
        cannot(format!(
            "the expression {text:?} cannot be evaluated: {err}"
        ))
    })?;
    match result.as_slice() {
        [] => Ok(false),
        [PathValue::Boolean { value, .. }] => Ok(*value),
        _ => Err(cannot(format!(
            "the expression {text:?} must give true or false"
        ))),
    }
}

/// The price components that apply, as the Invoice has them, and what they add up to.
struct Priced {
    components: Vec<Value>,
    /// Net, gross and the currency they are in, when there is a base price.
    totals: Option<(Decimal, Decimal, String)>,
}

fn price(components: &[(Value, String)]) -> Result<Priced, FhirError> {
    let invalid =
        |path: &str, why: String| FhirError::Unprocessable(outcome("business-rule", &why, path));
    // The base price comes first: the others may be a fraction of it.
    let mut base: Option<Decimal> = None;
    let mut currency: Option<String> = None;
    for (component, path) in components {
        if component.get("type").and_then(Value::as_str) != Some("base") {
            continue;
        }
        if let Some((amount, unit)) = amount_of(component, path)? {
            *base.get_or_insert(Decimal::ZERO) += amount;
            agree(&mut currency, &unit, path)?;
        }
    }
    let mut net = base.unwrap_or(Decimal::ZERO);
    let mut tax = Decimal::ZERO;
    let mut shown = Vec::new();
    for (component, path) in components {
        let kind = component
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !COMPONENT_TYPES.contains(&kind) {
            return Err(invalid(
                path,
                format!(
                    "the price component type {kind:?} is not one of {}",
                    COMPONENT_TYPES.join(", ")
                ),
            ));
        }
        let mut out = json!({"type": kind});
        for field in ["code", "factor"] {
            if let Some(value) = component.get(field) {
                out[field] = value.clone();
            }
        }
        let amount =
            match amount_of(component, path)? {
                Some((amount, unit)) => {
                    // What is only shown is never added, so it may be in another currency.
                    if kind != "informational" {
                        agree(&mut currency, &unit, path)?;
                    }
                    out["amount"] = component["amount"].clone();
                    Some(amount)
                }
                None => match (factor_of(component, path)?, base) {
                    (Some(factor), Some(base)) => {
                        let amount = base * factor;
                        out["amount"] = money(amount, currency.as_deref().unwrap_or_default());
                        Some(amount)
                    }
                    (Some(_), None) => return Err(invalid(
                        path,
                        "the price component is a fraction of the base price, and there is none"
                            .into(),
                    )),
                    (None, _) => None,
                },
            };
        match (kind, amount) {
            ("surcharge", Some(amount)) => net += amount,
            ("deduction" | "discount", Some(amount)) => net -= amount.abs(),
            ("tax", Some(amount)) => tax += amount,
            ("base" | "informational", _) => {}
            (_, None) => {
                return Err(invalid(
                    path,
                    format!("the {kind} price component has neither an amount nor a factor"),
                ))
            }
            _ => {}
        }
        shown.push(out);
    }
    let totals = match (base, currency) {
        (Some(_), Some(currency)) => Some((net, net + tax, currency)),
        _ => None,
    };
    Ok(Priced {
        components: shown,
        totals,
    })
}

/// The amount of a component, and its currency, if it has one.
fn amount_of(component: &Value, path: &str) -> Result<Option<(Decimal, String)>, FhirError> {
    let Some(amount) = component.get("amount") else {
        return Ok(None);
    };
    let bad = |why: &str| {
        FhirError::Unprocessable(outcome("business-rule", why, &format!("{path}.amount")))
    };
    let value = amount
        .get("value")
        .and_then(Value::as_number)
        .ok_or_else(|| bad("the amount has no value"))?;
    let currency = amount
        .get("currency")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            bad("the amount has no currency, and a price without one cannot be added")
        })?;
    let value = Decimal::from_str(&value.to_string())
        .or_else(|_| Decimal::from_scientific(&value.to_string()))
        .map_err(|_| bad("the amount is not a number this can add"))?;
    Ok(Some((value, currency.to_string())))
}

fn factor_of(component: &Value, path: &str) -> Result<Option<Decimal>, FhirError> {
    let Some(factor) = component.get("factor").and_then(Value::as_number) else {
        return Ok(None);
    };
    Decimal::from_str(&factor.to_string())
        .or_else(|_| Decimal::from_scientific(&factor.to_string()))
        .map(Some)
        .map_err(|_| {
            FhirError::Unprocessable(outcome(
                "business-rule",
                "the factor is not a number this can multiply",
                &format!("{path}.factor"),
            ))
        })
}

/// Prices in different currencies are never added: there is no rate to convert them at.
fn agree(currency: &mut Option<String>, unit: &str, path: &str) -> Result<(), FhirError> {
    match currency {
        Some(held) if held != unit => Err(FhirError::Unprocessable(outcome(
            "business-rule",
            &format!("the price is in {held} and this amount in {unit}: they cannot be added"),
            path,
        ))),
        Some(_) => Ok(()),
        None => {
            *currency = Some(unit.to_string());
            Ok(())
        }
    }
}

/// `Money`, with a whole amount written as a whole number.
fn money(value: Decimal, currency: &str) -> Value {
    let value = value.normalize();
    let number = if value.fract().is_zero() {
        value.to_i64().map(serde_json::Number::from)
    } else {
        None
    }
    .or_else(|| value.to_f64().and_then(serde_json::Number::from_f64))
    .unwrap_or_else(|| serde_json::Number::from(0));
    json!({"value": number, "currency": currency})
}

fn outcome(code: &str, diagnostics: &str, expression: &str) -> Value {
    json!({
        "resourceType": "OperationOutcome",
        "issue": [{
            "severity": "error",
            "code": code,
            "diagnostics": diagnostics,
            "expression": [expression]
        }]
    })
}
