//! FHIRPath values. System literals and FHIR nodes share one enum so operators
//! can follow the spec's collection and equality rules.

use rust_decimal::Decimal;
use serde_json::Value as Json;

use crate::ucum::{self, Measured};

#[derive(Clone, Debug)]
pub enum Value {
    Boolean {
        value: bool,
        system: bool,
    },
    Integer(i64),
    Decimal {
        value: Decimal,
        scale: u32,
    },
    String(String),
    Date(Temporal),
    DateTime(Temporal),
    Time(Temporal),
    Quantity {
        value: Decimal,
        scale: u32,
        unit: String,
    },
    /// A FHIR JSON node. `fhir_type` is the concrete type name (`code`, `HumanName`, `Age`).
    Node {
        json: Json,
        fhir_type: Option<String>,
        path: String,
        extensions: Vec<Json>,
    },
    TypeInfo {
        namespace: String,
        name: String,
    },
}

/// A partial date, dateTime, or time. Missing fields are unspecified precision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Temporal {
    pub year: Option<i32>,
    pub month: Option<u8>,
    pub day: Option<u8>,
    pub hour: Option<u8>,
    pub minute: Option<u8>,
    pub second: Option<u8>,
    pub millis: Option<u32>,
    /// Offset in minutes. `None` means the timezone was not specified.
    pub offset: Option<i16>,
}

impl Temporal {
    pub fn date(year: i32, month: Option<u8>, day: Option<u8>) -> Self {
        Self {
            year: Some(year),
            month,
            day,
            hour: None,
            minute: None,
            second: None,
            millis: None,
            offset: None,
        }
    }

    pub fn precision_digits(&self, kind: TimeKind) -> u32 {
        let mut digits = 0;
        if kind != TimeKind::Time {
            if let Some(year) = self.year {
                digits += digit_count(year.max(0) as u64).max(4);
            }
            if self.month.is_some() {
                digits += 2;
            }
            if self.day.is_some() {
                digits += 2;
            }
        }
        if kind != TimeKind::Date {
            if self.hour.is_some() {
                digits += 2;
            }
            if self.minute.is_some() {
                digits += 2;
            }
            if self.second.is_some() {
                digits += 2;
            }
            if let Some(ms) = self.millis {
                digits += digit_count(ms as u64).max(3);
            }
        }
        digits
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeKind {
    Date,
    DateTime,
    Time,
}

fn digit_count(mut value: u64) -> u32 {
    if value == 0 {
        return 1;
    }
    let mut count = 0;
    while value > 0 {
        value /= 10;
        count += 1;
    }
    count
}

impl Value {
    pub fn system_bool(value: bool) -> Self {
        Self::Boolean {
            value,
            system: true,
        }
    }

    pub fn fhir_bool(value: bool) -> Self {
        Self::Boolean {
            value,
            system: false,
        }
    }

    pub fn type_name(&self) -> (&str, &str) {
        match self {
            Self::Boolean { system: true, .. } => ("System", "Boolean"),
            Self::Boolean { system: false, .. } => ("FHIR", "boolean"),
            Self::Integer(_) => ("System", "Integer"),
            Self::Decimal { .. } => ("System", "Decimal"),
            Self::String(_) => ("System", "String"),
            Self::Date(_) => ("System", "Date"),
            Self::DateTime(_) => ("System", "DateTime"),
            Self::Time(_) => ("System", "Time"),
            Self::Quantity { .. } => ("System", "Quantity"),
            Self::TypeInfo { .. } => ("System", "TypeInfo"),
            Self::Node { fhir_type, .. } => ("FHIR", fhir_type.as_deref().unwrap_or("Element")),
        }
    }

    pub fn has_primitive_value(&self) -> bool {
        match self {
            Self::Node { json, .. } => !json.is_null() && !json.is_object() && !json.is_array(),
            Self::TypeInfo { .. } => false,
            _ => true,
        }
    }

    pub fn as_singleton_bool(&self) -> Option<bool> {
        match self {
            Self::Boolean { value, .. } => Some(*value),
            Self::Node {
                json: Json::Bool(value),
                ..
            } => Some(*value),
            _ => None,
        }
    }

    /// Render in the form the HL7 suite uses for an output element.
    pub fn render(&self) -> String {
        match self {
            Self::Boolean { value, .. } => value.to_string(),
            Self::Integer(value) => value.to_string(),
            Self::Decimal { value, scale } => format_decimal(*value, *scale),
            Self::String(value) => value.clone(),
            Self::Date(value) => format!("@{}", format_date(value)),
            Self::DateTime(value) => format!("@{}", format_datetime(value)),
            Self::Time(value) => format!("@T{}", format_time(value)),
            Self::Quantity { value, scale, unit } => {
                format!("{} '{unit}'", format_decimal(*value, *scale))
            }
            Self::Node {
                json, fhir_type, ..
            } => match (fhir_type.as_deref(), json) {
                (Some("date" | "dateTime" | "instant"), Json::String(value)) => format!("@{value}"),
                (Some("time"), Json::String(value)) => format!("@T{value}"),
                (_, Json::String(value)) => value.clone(),
                (_, Json::Bool(value)) => value.to_string(),
                (_, Json::Number(value)) => value.to_string(),
                (_, Json::Null) => String::new(),
                (_, other) => other.to_string(),
            },
            Self::TypeInfo { name, .. } => name.clone(),
        }
    }
}

pub fn format_decimal(value: Decimal, scale: u32) -> String {
    let scaled = value.round_dp(scale);
    if scaled.is_zero() {
        let digits = if scale == 0 {
            "0".to_string()
        } else {
            format!("0.{:0<width$}", "", width = scale as usize)
        };
        return digits;
    }
    if scale == 0 {
        return scaled.trunc().to_string();
    }
    format!("{scaled:.prec$}", prec = scale as usize)
}

fn format_date(value: &Temporal) -> String {
    let year = value.year.unwrap_or(0);
    match (value.month, value.day) {
        (Some(month), Some(day)) => format!("{year:04}-{month:02}-{day:02}"),
        (Some(month), None) => format!("{year:04}-{month:02}"),
        _ => format!("{year:04}"),
    }
}

fn format_time(value: &Temporal) -> String {
    let hour = value.hour.unwrap_or(0);
    match (value.minute, value.second, value.millis) {
        (Some(minute), Some(second), Some(millis)) => {
            format!("{hour:02}:{minute:02}:{second:02}.{millis:03}")
        }
        (Some(minute), Some(second), None) => format!("{hour:02}:{minute:02}:{second:02}"),
        (Some(minute), None, _) => format!("{hour:02}:{minute:02}"),
        _ => format!("{hour:02}"),
    }
}

fn format_datetime(value: &Temporal) -> String {
    let mut text = format_date(value);
    if value.hour.is_some() {
        text.push('T');
        text.push_str(&format_time(value));
        if let Some(offset) = value.offset {
            text.push_str(&format_offset(offset));
        }
    }
    text
}

pub fn format_offset(minutes: i16) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let minutes = minutes.unsigned_abs();
    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

pub fn equal_pair(left: &Value, right: &Value) -> Option<bool> {
    match (left, right) {
        (Value::Boolean { value: a, .. }, Value::Boolean { value: b, .. }) => Some(a == b),
        (
            Value::Boolean { value: a, .. },
            Value::Node {
                json: Json::Bool(b),
                ..
            },
        )
        | (
            Value::Node {
                json: Json::Bool(a),
                ..
            },
            Value::Boolean { value: b, .. },
        ) => Some(a == b),
        (Value::Integer(a), Value::Integer(b)) => Some(a == b),
        (Value::Integer(a), Value::Decimal { value: b, .. })
        | (Value::Decimal { value: b, .. }, Value::Integer(a)) => Some(Decimal::from(*a) == *b),
        (Value::Decimal { value: a, .. }, Value::Decimal { value: b, .. }) => Some(a == b),
        (Value::String(a), Value::String(b)) => Some(a == b),
        (
            Value::String(a),
            Value::Node {
                json: Json::String(b),
                ..
            },
        )
        | (
            Value::Node {
                json: Json::String(a),
                ..
            },
            Value::String(b),
        ) => Some(a == b),
        (Value::Node { json: a, .. }, Value::Node { json: b, .. }) => Some(a == b),
        (Value::Date(a), Value::Date(b)) => same_precision_date(a, b, TimeKind::Date),
        (
            Value::Date(a),
            Value::Node {
                json: Json::String(text),
                ..
            },
        )
        | (
            Value::Node {
                json: Json::String(text),
                ..
            },
            Value::Date(a),
        ) => temporal_text_equal(a, text, TimeKind::Date),
        (Value::DateTime(a), Value::DateTime(b)) => same_precision_date(a, b, TimeKind::DateTime),
        (
            Value::DateTime(a),
            Value::Node {
                json: Json::String(text),
                ..
            },
        )
        | (
            Value::Node {
                json: Json::String(text),
                ..
            },
            Value::DateTime(a),
        ) => temporal_text_equal(a, text, TimeKind::DateTime),
        (Value::Time(a), Value::Time(b)) => same_precision_date(a, b, TimeKind::Time),
        (
            Value::Time(a),
            Value::Node {
                json: Json::String(text),
                fhir_type,
                ..
            },
        )
        | (
            Value::Node {
                json: Json::String(text),
                fhir_type,
                ..
            },
            Value::Time(a),
        ) => match fhir_type.as_deref() {
            Some("time") => temporal_text_equal(a, text, TimeKind::Time),
            Some("date" | "dateTime" | "instant") => Some(false),
            _ => temporal_text_equal(a, text, TimeKind::Time),
        },
        (Value::Quantity { .. }, _) | (_, Value::Quantity { .. }) => quantity_equal(left, right),
        _ => {
            match (temporal_kind(left), temporal_kind(right)) {
                (Some(TimeKind::Time), Some(other)) | (Some(other), Some(TimeKind::Time))
                    if other != TimeKind::Time =>
                {
                    return Some(false)
                }
                (Some(_), Some(_)) => return None,
                _ => {}
            }
            if let (Some(left_q), Some(right_q)) = (as_quantity(left), as_quantity(right)) {
                return left_q.equal_exact(&right_q);
            }
            None
        }
    }
}

fn temporal_kind(value: &Value) -> Option<TimeKind> {
    match value {
        Value::Date(_) => Some(TimeKind::Date),
        Value::DateTime(_) => Some(TimeKind::DateTime),
        Value::Time(_) => Some(TimeKind::Time),
        Value::Node {
            json: Json::String(_),
            fhir_type,
            ..
        } => match fhir_type.as_deref() {
            Some("date") => Some(TimeKind::Date),
            Some("dateTime" | "instant") => Some(TimeKind::DateTime),
            Some("time") => Some(TimeKind::Time),
            _ => None,
        },
        _ => None,
    }
}

fn temporal_text_equal(literal: &Temporal, text: &str, kind: TimeKind) -> Option<bool> {
    let parsed = crate::parse::parse(&format!("@{text}")).ok()?;
    let other = match parsed {
        crate::ast::Expr::Date(value)
        | crate::ast::Expr::DateTime(value)
        | crate::ast::Expr::Time(value) => value,
        _ => return None,
    };
    same_precision_date(literal, &other, kind)
}

fn same_precision_date(left: &Temporal, right: &Temporal, kind: TimeKind) -> Option<bool> {
    let left = drop_zero_millis(left);
    let right = drop_zero_millis(right);
    if left.offset.is_some() != right.offset.is_some() {
        return None;
    }
    if precision_rank(&left, kind) != precision_rank(&right, kind) {
        if kind == TimeKind::DateTime && only_millis_differs(&left, &right) {
            return Some(false);
        }
        return None;
    }
    if let (Some(left_instant), Some(right_instant)) = (utc_millis(&left), utc_millis(&right)) {
        return Some(left_instant == right_instant);
    }
    Some(left == right)
}

fn drop_zero_millis(value: &Temporal) -> Temporal {
    let mut value = value.clone();
    if value.millis == Some(0) {
        value.millis = None;
    }
    value
}

fn only_millis_differs(left: &Temporal, right: &Temporal) -> bool {
    left.year == right.year
        && left.month == right.month
        && left.day == right.day
        && left.hour == right.hour
        && left.minute == right.minute
        && left.second == right.second
        && left.offset == right.offset
}

fn utc_millis(value: &Temporal) -> Option<i64> {
    let offset = value.offset?;
    let year = value.year?;
    let month = value.month?;
    let day = value.day?;
    let local = crate::host::days_from_civil(year, month, day) * 86_400_000
        + value.hour.unwrap_or(0) as i64 * 3_600_000
        + value.minute.unwrap_or(0) as i64 * 60_000
        + value.second.unwrap_or(0) as i64 * 1000
        + value.millis.unwrap_or(0) as i64;
    Some(local - offset as i64 * 60_000)
}

pub fn precision_rank(value: &Temporal, kind: TimeKind) -> u8 {
    match kind {
        TimeKind::Date => match (value.month, value.day) {
            (None, _) => 1,
            (Some(_), None) => 2,
            _ => 3,
        },
        TimeKind::Time => match (value.minute, value.second, value.millis) {
            (None, _, _) => 1,
            (Some(_), None, _) => 2,
            (Some(_), Some(_), None) => 3,
            _ => 4,
        },
        TimeKind::DateTime => {
            precision_rank(value, TimeKind::Date) * 10 + precision_rank(value, TimeKind::Time)
        }
    }
}

pub fn equivalent_pair(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::String(a), Value::String(b))
        | (
            Value::String(a),
            Value::Node {
                json: Json::String(b),
                ..
            },
        )
        | (
            Value::Node {
                json: Json::String(a),
                ..
            },
            Value::String(b),
        )
        | (
            Value::Node {
                json: Json::String(a),
                ..
            },
            Value::Node {
                json: Json::String(b),
                ..
            },
        ) => a.eq_ignore_ascii_case(b),
        (Value::Decimal { .. }, _)
        | (_, Value::Decimal { .. })
        | (Value::Integer(_), Value::Integer(_)) => number_equivalent(left, right),
        (Value::Quantity { .. }, _) | (_, Value::Quantity { .. }) => {
            quantity_equivalent(left, right)
        }
        (Value::Date(a), Value::Date(b)) => same_precision_date(a, b, TimeKind::Date) == Some(true),
        (Value::DateTime(a), Value::DateTime(b)) => {
            same_precision_date(a, b, TimeKind::DateTime) == Some(true)
        }
        (Value::Time(a), Value::Time(b)) => same_precision_date(a, b, TimeKind::Time) == Some(true),
        (Value::Date(_), Value::DateTime(_)) | (Value::DateTime(_), Value::Date(_)) => false,
        _ => equal_pair(left, right).unwrap_or(false),
    }
}

fn number_equivalent(left: &Value, right: &Value) -> bool {
    let Some((left_value, left_scale)) = as_number(left) else {
        return false;
    };
    let Some((right_value, right_scale)) = as_number(right) else {
        return false;
    };
    let scale = left_scale.min(right_scale);
    left_value.round_dp(scale) == right_value.round_dp(scale)
}

pub fn as_number(value: &Value) -> Option<(Decimal, u32)> {
    match value {
        Value::Integer(value) => Some((Decimal::from(*value), 0)),
        Value::Decimal { value, scale } => Some((*value, *scale)),
        Value::Node {
            json: Json::Number(number),
            ..
        } => {
            let text = number.to_string();
            let scale = text
                .split_once('.')
                .map(|(_, fraction)| fraction.len() as u32)
                .unwrap_or(0);
            Decimal::from_str_exact(&text)
                .ok()
                .map(|value| (value, scale))
        }
        _ => None,
    }
}

fn as_quantity(value: &Value) -> Option<Measured> {
    match value {
        Value::Quantity { value, unit, .. } => ucum::parse(unit).map(|unit| Measured {
            value: *value,
            unit,
        }),
        Value::Node {
            json, fhir_type, ..
        } if is_quantity_type(fhir_type.as_deref()) => {
            let magnitude = json
                .get("value")
                .and_then(|item| item.as_str())
                .or_else(|| json.get("value").and_then(|item| item.as_f64()).map(|_| ""));
            let text = if let Some(text) = json.get("value").and_then(|item| item.as_str()) {
                text.to_string()
            } else if let Some(number) = json.get("value").and_then(|item| item.as_f64()) {
                number.to_string()
            } else if let Some(number) = json.get("value").and_then(|item| item.as_i64()) {
                number.to_string()
            } else {
                return None;
            };
            let _ = magnitude;
            let unit = json
                .get("code")
                .and_then(|item| item.as_str())
                .or_else(|| json.get("unit").and_then(|item| item.as_str()))
                .unwrap_or("1");
            let decimal = Decimal::from_str_exact(&text).ok()?;
            ucum::parse(unit).map(|unit| Measured {
                value: decimal,
                unit,
            })
        }
        _ => None,
    }
}

fn is_quantity_type(name: Option<&str>) -> bool {
    matches!(
        name,
        Some(
            "Quantity"
                | "Age"
                | "Duration"
                | "SimpleQuantity"
                | "MoneyQuantity"
                | "Count"
                | "Distance"
        )
    )
}

fn quantity_equal(left: &Value, right: &Value) -> Option<bool> {
    let left = as_quantity(left)?;
    let right = as_quantity(right)?;
    left.equal_exact(&right)
}

fn quantity_equivalent(left: &Value, right: &Value) -> bool {
    let (Some(left_q), Some(right_q)) = (as_quantity(left), as_quantity(right)) else {
        return false;
    };
    let (Some((left_scale, _)), Some((right_scale, _))) =
        (quantity_scale(left), quantity_scale(right))
    else {
        return false;
    };
    left_q.equivalent(&right_q, left_scale.min(right_scale))
}

fn quantity_scale(value: &Value) -> Option<(u32, ())> {
    match value {
        Value::Quantity { scale, .. } => Some((*scale, ())),
        Value::Node { json, .. } => {
            let text = json
                .get("value")
                .map(|item| item.to_string())
                .unwrap_or_default();
            let text = text.trim_matches('"');
            let scale = text
                .split_once('.')
                .map(|(_, fraction)| fraction.len() as u32)
                .unwrap_or(0);
            Some((scale, ()))
        }
        _ => None,
    }
}

pub fn collections_equal(left: &[Value], right: &[Value]) -> Option<bool> {
    if left.is_empty() || right.is_empty() {
        return None;
    }
    if left.len() != right.len() {
        return Some(false);
    }
    let mut same = true;
    for (left, right) in left.iter().zip(right) {
        match equal_pair(left, right) {
            Some(false) => return Some(false),
            None => return None,
            Some(true) => {}
        }
        same = true;
    }
    Some(same)
}

pub fn collections_equivalent(left: &[Value], right: &[Value]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut used = vec![false; right.len()];
    for item in left {
        let Some(index) = right
            .iter()
            .enumerate()
            .position(|(index, candidate)| !used[index] && equivalent_pair(item, candidate))
        else {
            return false;
        };
        used[index] = true;
    }
    true
}
