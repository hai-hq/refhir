use serde_json::Value as Json;

use crate::error::Error;
use crate::value::{Temporal, Value};

pub trait Host {
    fn resolve(&self, reference: &str) -> Result<Option<Json>, Error> {
        let _ = reference;
        Ok(None)
    }

    /// `None` means no terminology service is configured.
    fn member_of(
        &self,
        system: Option<&str>,
        code: &str,
        value_set: &str,
    ) -> Result<Option<bool>, Error> {
        let _ = (system, code, value_set);
        Ok(None)
    }

    fn now(&self) -> Temporal {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        utc_temporal(secs)
    }

    fn strict(&self) -> bool {
        false
    }

    /// `Some` when this host can check a profile. `None` leaves the type-name fallback.
    fn conforms_to(&self, _resource: &Json, _url: &str) -> Result<Option<bool>, Error> {
        Ok(None)
    }

    /// Extra `%name` bindings. `None` falls through to the built-in variables.
    /// `Some` is the collection, including an empty one.
    fn lookup_variable(&self, name: &str) -> Result<Option<Vec<Value>>, Error> {
        let _ = name;
        Ok(None)
    }
}

/// The FHIRPath functions whose result can depend on the [`Host`]: they ask it for a referenced
/// resource, the clock, a profile or a value set. An expression that calls none of them, and uses
/// no `%variable` (which the host may bind), gives the same answer for the same resource wherever
/// and whenever it is evaluated, so values computed from it earlier are still right.
///
/// Keep this in step with the functions in `eval.rs` that call `self.host`.
pub const HOST_FUNCTIONS: &[&str] = &[
    "resolve",
    "conformsTo",
    "memberOf",
    "now",
    "today",
    "timeOfDay",
];

pub struct EmptyHost;

impl Host for EmptyHost {}

fn utc_temporal(secs: u64) -> Temporal {
    let days = (secs / 86_400) as i64;
    let time = (secs % 86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    let hour = (time / 3600) as u8;
    let minute = ((time % 3600) / 60) as u8;
    let second = (time % 60) as u8;
    Temporal {
        year: Some(year),
        month: Some(month),
        day: Some(day),
        hour: Some(hour),
        minute: Some(minute),
        second: Some(second),
        millis: Some(0),
        offset: Some(0),
    }
}

pub fn civil_from_days(days: i64) -> (i32, u8, u8) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u8;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

pub fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = (year - era * 400) as u64;
    let mp = if month > 2 {
        month as u64 - 3
    } else {
        month as u64 + 9
    };
    let doy = (153 * mp + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era as i64 * 146_097 + doe as i64 - 719_468
}

#[cfg(test)]
mod tests {
    use super::HOST_FUNCTIONS;
    use crate::parse;

    fn depends(expression: &str) -> bool {
        parse(expression).expect(expression).depends_on_host()
    }

    #[test]
    fn host_dependence_is_found_wherever_it_sits_in_an_expression() {
        for function in HOST_FUNCTIONS {
            assert!(depends(&format!("Patient.{function}()")), "{function}");
            assert!(
                depends(&format!("Patient.name.where({function}() = 1)")),
                "inside an argument: {function}"
            );
            assert!(
                depends(&format!("(Patient.a | Patient.b.{function}()).first()")),
                "inside a union: {function}"
            );
        }
        // A variable may be bound by the host.
        assert!(depends("Patient.name.where(use = %thing)"));
        assert!(depends("%resource.id"));

        // Everything else is the same wherever and whenever it runs.
        for expression in [
            "Patient.name.family",
            "Patient.name.where(use = 'official').given.first()",
            "Patient.identifier.where(system = 'x').value",
            "Patient.birthDate",
            "Patient.extension.where(url = 'u').value",
        ] {
            assert!(!depends(expression), "{expression}");
        }
    }

    /// `HOST_FUNCTIONS` and the `%variable` rule stand for the methods of `Host` that the
    /// evaluator calls. If it starts calling another, this fails: decide whether the functions
    /// that use it belong in `HOST_FUNCTIONS` before changing the list below.
    #[test]
    fn the_evaluator_calls_no_host_method_that_the_list_leaves_out() {
        let source = include_str!("eval.rs");
        let mut called: Vec<&str> = source
            .match_indices("host.")
            .filter_map(|(at, _)| {
                let rest = &source[at + "host.".len()..];
                let name = &rest[..rest.find('(')?];
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_')
                    .then_some(name)
            })
            .collect();
        called.sort_unstable();
        called.dedup();
        assert_eq!(
            called,
            [
                "conforms_to",     // conformsTo
                "lookup_variable", // any %variable
                "member_of",       // memberOf
                "now",             // now, today, timeOfDay
                "resolve",         // resolve
                "strict",          // a mode for the whole evaluation, not a function
            ]
        );
    }
}
