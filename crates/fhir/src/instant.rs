//! Instants: the time a resource version was written, as FHIR writes it, and the times a request
//! names. A time here is milliseconds since 1970-01-01T00:00:00Z.

use std::time::{SystemTime, UNIX_EPOCH};

/// Now.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// A time as a FHIR instant in UTC: `2026-10-18T04:45:36.123Z`.
pub(crate) fn format(ms: i64) -> String {
    let seconds = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let (year, month, day) = refhir_fhirpath::civil_from_days(seconds.div_euclid(86_400));
    let time = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

/// The moment a FHIR date or dateTime names: `2026`, `2026-10`, `2026-10-18`, or a date with a time
/// (`2026-10-18T04:45`, with seconds, a fraction of a second, and a zone, `Z` or `+01:00`). A
/// date, or a time that stops short, is the start of what it names, and a time with no zone is UTC.
/// Anything else is `None`.
pub(crate) fn parse(text: &str) -> Option<i64> {
    let (date, time) = match text.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (text, None),
    };
    let mut parts = date.split('-');
    let year: i32 = number(parts.next()?, 4)?;
    let month: u8 = parts.next().map_or(Some(1), |part| number(part, 2))?;
    let day: u8 = parts.next().map_or(Some(1), |part| number(part, 2))?;
    if parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    if day < 1 || day > refhir_fhirpath::days_in_month(year, month) {
        return None;
    }
    let mut ms = refhir_fhirpath::days_from_civil(year, month, day) * 86_400_000;
    let Some(time) = time else {
        return Some(ms);
    };
    // A time needs a date down to the day.
    if !date.contains('-') || date.matches('-').count() != 2 {
        return None;
    }
    let (clock, offset_minutes) = split_zone(time)?;
    let mut fields = clock.split(':');
    let hour: i64 = number(fields.next()?, 2)?;
    let minute: i64 = number(fields.next()?, 2)?;
    let (second, millis) = match fields.next() {
        None => (0, 0),
        Some(field) => {
            let (whole, fraction) = match field.split_once('.') {
                Some((whole, fraction)) => (whole, Some(fraction)),
                None => (field, None),
            };
            let millis = match fraction {
                None => 0,
                Some(digits) => {
                    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                        return None;
                    }
                    let padded = format!("{digits:0<3}");
                    padded[..3].parse::<i64>().ok()?
                }
            };
            (number::<i64>(whole, 2)?, millis)
        }
    };
    if fields.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    ms += ((hour * 60 + minute) * 60 + second) * 1000 + millis;
    Some(ms - offset_minutes * 60_000)
}

/// A number of exactly `digits` digits.
fn number<T: std::str::FromStr>(text: &str, digits: usize) -> Option<T> {
    if text.len() != digits || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The clock part of a time, and its zone's offset from UTC in minutes.
fn split_zone(time: &str) -> Option<(&str, i64)> {
    if let Some(clock) = time.strip_suffix('Z') {
        return Some((clock, 0));
    }
    if let Some(at) = time.rfind(['+', '-']) {
        let (clock, zone) = time.split_at(at);
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let (hours, minutes) = zone[1..].split_once(':')?;
        let (hours, minutes): (i64, i64) = (number(hours, 2)?, number(minutes, 2)?);
        if hours > 23 || minutes > 59 {
            return None;
        }
        return Some((clock, sign * (hours * 60 + minutes)));
    }
    Some((time, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_is_written_as_a_utc_instant_with_milliseconds() {
        assert_eq!(format(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(
            format(-1),
            "1969-12-31T23:59:59.999Z",
            "a time before 1970 still reads right"
        );
        let now = format(now_ms());
        assert_eq!(now.len(), 24, "{now}");
        assert!(now.ends_with('Z'));
    }

    #[test]
    fn what_is_written_is_read_back() {
        for ms in [
            0,
            1,
            999,
            1_700_000_000_123,
            4_102_444_800_000,
            951_782_400_000,
            -86_400_001,
        ] {
            assert_eq!(parse(&format(ms)), Some(ms), "{ms}");
        }
    }

    #[test]
    fn a_date_names_where_it_starts() {
        assert_eq!(parse("1970"), Some(0));
        assert_eq!(parse("1970-01"), Some(0));
        assert_eq!(parse("1970-01-02"), Some(86_400_000));
        assert_eq!(parse("2024-02-29"), Some(1_709_164_800_000), "a leap day");
    }

    #[test]
    fn a_time_has_a_zone_or_is_utc_and_a_fraction_or_not() {
        let base = parse("2026-10-18T04:45:36Z").unwrap();
        assert_eq!(parse("2026-10-18T04:45:36"), Some(base), "no zone is UTC");
        assert_eq!(parse("2026-10-18T04:45:36.500Z"), Some(base + 500));
        assert_eq!(parse("2026-10-18T04:45:36.5Z"), Some(base + 500));
        assert_eq!(
            parse("2026-10-18T04:45:36.123456Z"),
            Some(base + 123),
            "finer than a millisecond is cut"
        );
        assert_eq!(
            parse("2026-10-18T04:45Z"),
            Some(base - 36_000),
            "seconds may be left out"
        );
        assert_eq!(
            parse("2026-10-18T05:45:36+01:00"),
            Some(base),
            "an offset is taken off"
        );
        assert_eq!(parse("2026-10-17T23:45:36-05:00"), Some(base));
    }

    #[test]
    fn what_is_not_a_time_is_refused() {
        for bad in [
            "",
            "garbage",
            "2026-13-01",
            "2026-00-10",
            "2026-02-30",
            "2023-02-29",
            "2026-10-32",
            "2026-10-18T24:00:00Z",
            "2026-10-18T04:60:00Z",
            "2026-10-18T04:45:60Z",
            "2026-10-18T04",
            "2026-10-18T04:45:36.Z",
            "2026-10-18T04:45:36+25:00",
            "2026-10-18T04:45:36+0100",
            "2026-10T04:45:36Z",
            "2026-10-18T04:45:36Zjunk",
            "26-10-18",
            "2026-10-18-01",
            "1",
            "12345",
            "2026-1-18",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }
}
