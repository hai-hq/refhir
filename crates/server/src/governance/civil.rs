//! Calendar arithmetic for usage periods and audit timestamps.
//!
//! Quotas run on UTC days and months, and an `AuditEvent` carries an ISO-8601 instant. Nothing
//! else in the workspace needs a date library, so this is the handful of conversions those two
//! need (Howard Hinnant's `days_from_civil` and `civil_from_days`, which are exact for every
//! date in the proleptic Gregorian calendar).

use refhir_persistence::MS_PER_DAY;

/// `(year, month 1..=12, day 1..=31)` for a count of days since 1970-01-01.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    let year = year_of_era + era * 400;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Days since 1970-01-01 for a calendar date.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = (i64::from(month) + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The UTC day a Unix-millisecond time falls on.
pub fn day_of(ms: u64) -> u32 {
    u32::try_from(ms / MS_PER_DAY).unwrap_or(u32::MAX)
}

/// The first day of the month that `day` is in.
pub fn month_start(day: u32) -> u32 {
    let (year, month, _) = civil_from_days(i64::from(day));
    days_from_civil(year, month, 1) as u32
}

/// The first day of the month after the one `day` is in.
pub fn next_month_start(day: u32) -> u32 {
    let (year, month, _) = civil_from_days(i64::from(day));
    let (year, month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    days_from_civil(year, month, 1) as u32
}

/// Midnight UTC at the start of `day`, in Unix milliseconds.
pub fn day_start_ms(day: u32) -> u64 {
    u64::from(day) * MS_PER_DAY
}

/// `2026-10-12` for a day number.
pub fn date_string(day: u32) -> String {
    let (year, month, date) = civil_from_days(i64::from(day));
    format!("{year:04}-{month:02}-{date:02}")
}

/// `2026-10-12T03:17:00.123Z` for a Unix-millisecond time.
pub fn instant_string(ms: u64) -> String {
    let (year, month, date) = civil_from_days(i64::from(day_of(ms)));
    let in_day = ms % MS_PER_DAY;
    format!(
        "{year:04}-{month:02}-{date:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1_000 % 60,
        in_day % 1_000
    )
}

/// A date, or a date and time, as Unix milliseconds. Accepts `2026-10-12` (midnight UTC) and
/// `2026-10-12T03:17:00Z`, with optional fractional seconds and a `+hh:mm` or `-hh:mm` offset
/// in place of the `Z`. A time with no zone is taken as UTC.
pub fn parse_instant(text: &str) -> Option<u64> {
    let (date, time) = match text.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (text, None),
    };
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    // The date must exist: 2026-02-30 round-trips to a different one.
    if civil_from_days(days) != (year, month, day) {
        return None;
    }
    let mut ms = i128::from(days) * i128::from(MS_PER_DAY);
    if let Some(time) = time {
        let (clock, offset_minutes) = split_zone(time)?;
        let mut fields = clock.split(':');
        let hours: i128 = fields.next()?.parse().ok()?;
        let minutes: i128 = fields.next()?.parse().ok()?;
        let seconds = fields.next().unwrap_or("0");
        if fields.next().is_some() {
            return None;
        }
        let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
        let whole: i128 = whole.parse().ok()?;
        let millis: i128 = if fraction.is_empty() {
            0
        } else {
            let digits: String = fraction.chars().chain("000".chars()).take(3).collect();
            digits.parse().ok()?
        };
        if !(0..24).contains(&hours) || !(0..60).contains(&minutes) || !(0..61).contains(&whole) {
            return None;
        }
        ms += ((hours * 60 + minutes) * 60 + whole) * 1_000 + millis;
        ms -= i128::from(offset_minutes) * 60_000;
    }
    u64::try_from(ms).ok()
}

/// A clock time with its zone taken off, and the zone as minutes east of UTC.
fn split_zone(time: &str) -> Option<(&str, i32)> {
    if let Some(clock) = time.strip_suffix('Z') {
        return Some((clock, 0));
    }
    if let Some(index) = time.rfind(['+', '-']) {
        let (clock, zone) = time.split_at(index);
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let (hours, minutes) = zone[1..].split_once(':')?;
        let minutes = hours.parse::<i32>().ok()? * 60 + minutes.parse::<i32>().ok()?;
        return Some((clock, sign * minutes));
    }
    Some((time, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_numbers_and_dates_agree_across_centuries() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_373), (2025, 10, 12));
        for days in (-800_000..800_000).step_by(37) {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(
                days_from_civil(year, month, day),
                days,
                "{year}-{month}-{day}"
            );
        }
    }

    #[test]
    fn month_boundaries_follow_the_calendar() {
        let day = |text: &str| (parse_instant(text).unwrap() / MS_PER_DAY) as u32;
        assert_eq!(month_start(day("2026-10-12")), day("2026-10-01"));
        assert_eq!(month_start(day("2026-10-01")), day("2026-10-01"));
        assert_eq!(next_month_start(day("2026-10-12")), day("2026-11-01"));
        assert_eq!(next_month_start(day("2026-12-31")), day("2027-01-01"));
        // February, in a leap year and not.
        assert_eq!(next_month_start(day("2024-02-10")), day("2024-03-01"));
        assert_eq!(next_month_start(day("2024-02-10")) - day("2024-02-01"), 29);
        assert_eq!(next_month_start(day("2026-02-10")) - day("2026-02-01"), 28);
    }

    #[test]
    fn instants_print_as_utc_with_milliseconds() {
        assert_eq!(instant_string(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            instant_string(1_700_000_000_123),
            "2023-11-14T22:13:20.123Z"
        );
        assert_eq!(date_string(20_373), "2025-10-12");
        assert_eq!(day_start_ms(1), MS_PER_DAY);
    }

    #[test]
    fn instants_parse_with_offsets_and_reject_nonsense() {
        assert_eq!(parse_instant("1970-01-01"), Some(0));
        assert_eq!(parse_instant("1970-01-02"), Some(MS_PER_DAY));
        assert_eq!(
            parse_instant("2023-11-14T22:13:20Z"),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            parse_instant("2023-11-14T22:13:20.123Z"),
            Some(1_700_000_000_123)
        );
        assert_eq!(
            parse_instant("2023-11-14T22:13:20"),
            Some(1_700_000_000_000)
        );
        // 23:13:20+01:00 is the same instant as 22:13:20Z.
        assert_eq!(
            parse_instant("2023-11-14T23:13:20+01:00"),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            parse_instant("2023-11-14T17:13:20-05:00"),
            Some(1_700_000_000_000)
        );
        for bad in [
            "",
            "yesterday",
            "2026-13-01",
            "2026-02-30",
            "2026-10",
            "2026-10-12T25:00:00Z",
            "2026-10-12T10:61:00Z",
            "2026-10-12T10",
        ] {
            assert_eq!(parse_instant(bad), None, "{bad}");
        }
    }
}
