//! Enough UCUM to compare and combine the quantities in the FHIRPath suite.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

#[derive(Clone, Debug, PartialEq)]
pub struct Unit {
    pub factor: Decimal,
    pub dim: BTreeMap<String, i32>,
    pub indefinite: bool,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct Measured {
    pub value: Decimal,
    pub unit: Unit,
}

impl Measured {
    pub fn equal_exact(&self, other: &Self) -> Option<bool> {
        if self.unit.dim != other.unit.dim || self.unit.indefinite || other.unit.indefinite {
            return None;
        }
        Some(self.value * self.unit.factor == other.value * other.unit.factor)
    }

    pub fn equivalent(&self, other: &Self, scale: u32) -> bool {
        if self.unit.dim != other.unit.dim || self.unit.factor.is_zero() {
            return false;
        }
        let converted = other.value * other.unit.factor / self.unit.factor;
        converted.round_dp(scale) == self.value.round_dp(scale)
    }
}

pub fn parse(raw: &str) -> Option<Unit> {
    if let Some(unit) = named_calendar(raw) {
        return Some(unit);
    }
    let text = calendar_unit(raw).unwrap_or(raw);
    if text == "1" {
        return Some(bare("1", Decimal::ONE, BTreeMap::new(), false));
    }
    if text == "[lb_av]" {
        return Some(bare(text, Decimal::new(45359237, 5), single("g"), false));
    }
    if text == "[in_i]" {
        return Some(bare(text, Decimal::new(254, 4), single("m"), false));
    }
    if text == "wk" {
        return Some(bare(
            text,
            Decimal::from(7 * 24 * 60 * 60),
            single("s"),
            false,
        ));
    }
    let indefinite = text == "mo" || text == "a";
    parse_product(text, indefinite)
}

fn bare(label: &str, factor: Decimal, dim: BTreeMap<String, i32>, indefinite: bool) -> Unit {
    Unit {
        factor,
        dim,
        indefinite,
        label: label.into(),
    }
}

fn named_calendar(raw: &str) -> Option<Unit> {
    let (label, dim) = match raw {
        "year" | "years" => ("year", "year"),
        "month" | "months" => ("month", "month"),
        _ => return None,
    };
    Some(bare(label, Decimal::ONE, single(dim), false))
}

fn calendar_unit(raw: &str) -> Option<&str> {
    Some(match raw {
        "week" | "weeks" => "wk",
        "day" | "days" => "d",
        "hour" | "hours" => "h",
        "minute" | "minutes" => "min",
        "second" | "seconds" => "s",
        "millisecond" | "milliseconds" => "ms",
        _ => return None,
    })
}

fn parse_product(text: &str, indefinite: bool) -> Option<Unit> {
    let (num, den) = text.split_once('/').unwrap_or((text, ""));
    let mut factor = Decimal::ONE;
    let mut dim = BTreeMap::new();
    apply_side(num, 1, &mut factor, &mut dim)?;
    if !den.is_empty() {
        apply_side(den, -1, &mut factor, &mut dim)?;
    }
    dim.retain(|_, exp| *exp != 0);
    Some(Unit {
        factor,
        dim,
        indefinite,
        label: text.into(),
    })
}

fn apply_side(
    text: &str,
    sign: i32,
    factor: &mut Decimal,
    dim: &mut BTreeMap<String, i32>,
) -> Option<()> {
    let mut rest = text;
    while !rest.is_empty() {
        if rest.starts_with('.') {
            rest = &rest[1..];
        }
        let (atom, next) = take_atom(rest)?;
        let (prefix, base) = split_prefix(atom);
        let (exp, next) = take_exp(next);
        rest = next;
        *factor *= prefix;
        let canonical = canonical_base(base)?;
        *factor *= canonical.1;
        if canonical.0 != "1" {
            *dim.entry(canonical.0.into()).or_insert(0) += sign * exp;
        }
    }
    Some(())
}

fn canonical_base(base: &str) -> Option<(&str, Decimal)> {
    Some(match base {
        "g" => ("g", Decimal::ONE),
        "m" => ("m", Decimal::ONE),
        "s" => ("s", Decimal::ONE),
        "l" | "L" => ("l", Decimal::ONE),
        "ms" => ("s", Decimal::new(1, 3)),
        "min" => ("s", Decimal::from(60)),
        "h" => ("s", Decimal::from(3600)),
        "d" => ("s", Decimal::from(86400)),
        "mo" => ("mo", Decimal::ONE),
        "a" => ("a", Decimal::ONE),
        "1" => ("1", Decimal::ONE),
        _ => return None,
    })
}

fn split_prefix(atom: &str) -> (Decimal, &str) {
    const PREFIXES: &[(&str, i32)] = &[
        ("da", 1),
        ("Y", 24),
        ("Z", 21),
        ("E", 18),
        ("P", 15),
        ("T", 12),
        ("G", 9),
        ("M", 6),
        ("k", 3),
        ("h", 2),
        ("d", -1),
        ("c", -2),
        ("m", -3),
        ("u", -6),
        ("n", -9),
        ("p", -12),
        ("f", -15),
        ("a", -18),
        ("z", -21),
        ("y", -24),
    ];
    for (name, exp) in PREFIXES {
        if atom.starts_with(name) && atom.len() > name.len() {
            let rest = &atom[name.len()..];
            if canonical_base(rest).is_some() {
                return (pow10(*exp), rest);
            }
        }
    }
    (Decimal::ONE, atom)
}

fn pow10(exp: i32) -> Decimal {
    if exp >= 0 {
        Decimal::from(10i64.pow(exp as u32))
    } else {
        Decimal::new(1, (-exp) as u32)
    }
}

fn take_atom(text: &str) -> Option<(&str, &str)> {
    if text.starts_with('[') {
        let end = text.find(']')?;
        return Some((&text[..=end], &text[end + 1..]));
    }
    let len = text
        .chars()
        .take_while(|ch| ch.is_ascii_alphabetic())
        .map(|ch| ch.len_utf8())
        .sum();
    if len == 0 {
        return None;
    }
    Some((&text[..len], &text[len..]))
}

fn take_exp(text: &str) -> (i32, &str) {
    let bytes = text.as_bytes();
    let mut index = 0;
    let sign = if bytes.first() == Some(&b'-') {
        index = 1;
        -1
    } else if bytes.first() == Some(&b'+') {
        index = 1;
        1
    } else {
        1
    };
    let start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index == start {
        return (1, text);
    }
    let exp: i32 = text[start..index].parse().unwrap_or(1);
    (sign * exp, &text[index..])
}

fn single(name: &str) -> BTreeMap<String, i32> {
    let mut dim = BTreeMap::new();
    dim.insert(name.into(), 1);
    dim
}

pub fn format_dim(dim: &BTreeMap<String, i32>) -> String {
    if dim.is_empty() {
        return "1".into();
    }
    let mut num = Vec::new();
    let mut den = Vec::new();
    for (name, exp) in dim {
        if *exp > 0 {
            num.push(format_exp(name, *exp));
        } else {
            den.push(format_exp(name, -*exp));
        }
    }
    match (num.is_empty(), den.is_empty()) {
        (true, true) => "1".into(),
        (false, true) => num.join("."),
        (true, false) => format!("1/{}", den.join(".")),
        (false, false) => format!("{}/{}", num.join("."), den.join(".")),
    }
}

fn format_exp(name: &str, exp: i32) -> String {
    if exp == 1 {
        name.into()
    } else {
        format!("{name}{exp}")
    }
}
