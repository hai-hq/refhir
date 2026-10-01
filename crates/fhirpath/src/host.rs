use serde_json::Value as Json;

use crate::error::Error;
use crate::value::Temporal;

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
}

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
