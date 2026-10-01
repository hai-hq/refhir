use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value as Json;

use crate::schema::Schema;

struct Pattern {
    code: &'static str,
    regex: &'static str,
}

const PATTERNS: &[Pattern] = &[
    Pattern { code: "string", regex: r#"^[\s\S]+$"# },
    Pattern { code: "markdown", regex: r#"^[\s\S]+$"# },
    Pattern { code: "xhtml", regex: r#"^[\s\S]+$"# },
    Pattern { code: "id", regex: r"^[A-Za-z0-9\-\.]{1,64}$" },
    Pattern { code: "code", regex: r"^[^\s]+( [^\s]+)*$" },
    Pattern { code: "uri", regex: r"^\S*$" },
    Pattern { code: "url", regex: r"^\S*$" },
    Pattern { code: "canonical", regex: r"^\S*$" },
    Pattern { code: "oid", regex: r"^urn:oid:[0-2](\.(0|[1-9][0-9]*))+$" },
    Pattern {
        code: "uuid",
        regex: r"(?i)^urn:uuid:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
    },
    Pattern { code: "base64Binary", regex: r"^[ \r\n\tA-Za-z0-9+/=]*$" },
    Pattern {
        code: "date",
        regex: r"^-?[0-9]{4}(-(0[1-9]|1[0-2])(-(0[0-9]|[1-2][0-9]|3[0-1]))?)?$",
    },
    Pattern {
        code: "dateTime",
        regex: r"^-?[0-9]{4}(-(0[1-9]|1[0-2])(-(0[0-9]|[1-2][0-9]|3[0-1])(T([01][0-9]|2[0-3]):[0-5][0-9]:([0-5][0-9]|60)(\.[0-9]+)?(Z|(\+|-)((0[0-9]|1[0-3]):[0-5][0-9]|14:00))?)?)?)?$",
    },
    Pattern {
        code: "instant",
        regex: r"^-?[0-9]{4}-(0[1-9]|1[0-2])-(0[0-9]|[1-2][0-9]|3[0-1])T([01][0-9]|2[0-3]):[0-5][0-9]:([0-5][0-9]|60)(\.[0-9]+)?(Z|(\+|-)((0[0-9]|1[0-3]):[0-5][0-9]|14:00))$",
    },
    Pattern {
        code: "time",
        regex: r"^([01][0-9]|2[0-3]):[0-5][0-9]:([0-5][0-9]|60)(\.[0-9]+)?$",
    },
];

pub fn fhir_type(code: &str) -> &str {
    match code {
        "http://hl7.org/fhirpath/System.String" => "string",
        "http://hl7.org/fhirpath/System.Boolean" => "boolean",
        "http://hl7.org/fhirpath/System.Integer" => "integer",
        "http://hl7.org/fhirpath/System.Decimal" => "decimal",
        "http://hl7.org/fhirpath/System.DateTime" => "dateTime",
        "http://hl7.org/fhirpath/System.Time" => "time",
        other => other,
    }
}

pub fn is_primitive(code: &str) -> bool {
    let code = fhir_type(code);
    matches!(
        code,
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

pub fn type_suffix(code: &str) -> String {
    let mut chars = code.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// `None` means the value matches the primitive type.
pub fn check_primitive(schema: &Schema, code: &str, value: &Json) -> Option<String> {
    let code = fhir_type(code);
    match code {
        "boolean" => {
            if value.is_boolean() {
                None
            } else {
                Some("expected boolean".into())
            }
        }
        "integer" => integer_in(value, i64::MIN, i64::MAX, "integer"),
        "unsignedInt" => integer_in(value, 0, i32::MAX as i64, "unsignedInt"),
        "positiveInt" => integer_in(value, 1, i32::MAX as i64, "positiveInt"),
        "decimal" => {
            if value.is_number() {
                None
            } else {
                Some("expected decimal".into())
            }
        }
        other => {
            let Some(text) = value.as_str() else {
                return Some(format!("expected {other}"));
            };
            if text.is_empty() && other != "uri" && other != "url" && other != "canonical" {
                return Some(format!("empty {other}"));
            }
            let pattern = schema
                .primitive_regex(other)
                .unwrap_or_else(|| builtin(other).unwrap_or(r#"^[\s\S]*$"#));
            if regex_match(pattern, text) {
                None
            } else {
                Some(format!("invalid {other}"))
            }
        }
    }
}

fn integer_in(value: &Json, min: i64, max: i64, name: &str) -> Option<String> {
    let Some(number) = value.as_i64() else {
        return Some(format!("expected {name}"));
    };
    if number < min || number > max {
        Some(format!("{name} out of range"))
    } else {
        None
    }
}

fn builtin(code: &str) -> Option<&'static str> {
    PATTERNS.iter().find(|pattern| pattern.code == code).map(|pattern| pattern.regex)
}

fn regex_match(pattern: &str, text: &str) -> bool {
    Regex::new(pattern).map(|regex| regex.is_match(text)).unwrap_or(true)
}

pub fn compiled_regex_ok(pattern: &str) -> bool {
    static _LOCK: OnceLock<()> = OnceLock::new();
    let _ = _LOCK;
    Regex::new(pattern).is_ok()
}
