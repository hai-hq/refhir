//! A `date` search compares moments, not the text a moment was written as.
//!
//! A time written with an offset is a point on the world's clock: `2026-10-31T23:30:00+01:00` and
//! `2026-10-31T22:30:00Z` are the same one. These tests store the same moments written in several
//! zones and at several precisions, then ask for them with every prefix, with the value written in
//! several zones too. Each expected answer is worked out by hand from the moments, in the comments
//! beside the data, and none is copied from what a search returned.

use std::sync::OnceLock;

use super::*;

/// One moment, 2026-10-31T22:30:00Z, written in six zones. Every search that finds one must find
/// all of them.
const SAME: &[(&str, &str)] = &[
    ("same-z", "2026-10-31T22:30:00Z"),
    ("same-plus01", "2026-10-31T23:30:00+01:00"),
    ("same-minus05", "2026-10-31T17:30:00-05:00"),
    ("same-plus0530", "2026-11-01T04:00:00+05:30"),
    ("same-plus14", "2026-11-01T12:30:00+14:00"),
    ("same-minus12", "2026-10-31T10:30:00-12:00"),
];

/// Moments at and around the ends of the window 2026-09-30T23:00:00Z up to 2026-10-31T23:00:00Z,
/// written so that the clock in the text points the wrong way about where the moment is.
const EDGES: &[(&str, &str)] = &[
    // 2026-09-30T23:00:00Z: exactly where the window opens.
    ("edge-open", "2026-10-01T00:00:00+01:00"),
    // 2026-09-30T22:59:59Z: a second before it, though its clock reads the 1st of October.
    ("edge-before-open", "2026-10-01T00:59:59+02:00"),
    // 2026-10-31T22:59:59Z: a second before it closes, though its clock reads the 1st of November.
    ("edge-before-close", "2026-11-01T00:59:59+02:00"),
    // 2026-10-31T23:00:00Z: exactly where it closes.
    ("edge-close", "2026-11-01T00:00:00+01:00"),
    // 2026-11-01T03:30:00Z: after the window, though its clock reads 22:30 on the 31st.
    ("trap-late", "2026-10-31T22:30:00-05:00"),
    // 2026-09-30T10:30:00Z: before the window, though its clock reads the 1st of October.
    ("trap-early", "2026-10-01T00:30:00+14:00"),
];

/// A period: 2026-10-31T22:30:00Z up to 2026-11-01T00:30:00Z, written in +01:00.
const STAY_START: &str = "2026-10-31T23:30:00+01:00";
const STAY_END: &str = "2026-11-01T01:30:00+01:00";

/// Encounters that started and have no end, which R4 reads as going on. Their ids start with
/// `open-`: `found` leaves them out, so that what goes on past a window does not change the sets
/// the moments above are checked against, and `found_open` reads only them.
const OPEN: &[(&str, &str)] = &[
    // 2026-10-31T22:30:00Z: the first Encounter of the report that started this.
    ("open-z", "2026-10-31T22:30:00Z"),
    // The same moment, written at +01:00: the second.
    ("open-plus01", "2026-10-31T23:30:00+01:00"),
    // 2026-12-01T08:00:00Z.
    ("open-dec", "2026-12-01T09:00:00+01:00"),
];

/// Periods of an EpisodeOfCare, whose `date` is its `period`: a start and an end, either of which
/// may be missing. Each comment is what it stands for in UTC, up to the first moment after it.
#[allow(clippy::type_complexity)]
const EPISODES: &[(&str, Option<&str>, Option<&str>)] = &[
    // 2026-10-29T08:00:00Z up to 2026-10-31T12:00:01Z: the end is the second it names.
    (
        "ep-closed",
        Some("2026-10-29T08:00:00Z"),
        Some("2026-10-31T12:00:00Z"),
    ),
    // The same two moments, written at +01:00.
    (
        "ep-zones",
        Some("2026-10-29T09:00:00+01:00"),
        Some("2026-10-31T13:00:00+01:00"),
    ),
    // From 2026-10-29T08:00:00Z, and not ended.
    ("ep-open-end", Some("2026-10-29T08:00:00Z"), None),
    // Until 2026-10-31T12:00:01Z, with no start.
    ("ep-open-start", None, Some("2026-10-31T12:00:00Z")),
    // 2026-10-29T00:00:00Z up to 2026-11-01T00:00:00Z: the start of one day to the end of another.
    ("ep-days", Some("2026-10-29"), Some("2026-10-31")),
    // 2026-01-01 up to 2027-01-01.
    ("ep-year", Some("2026"), Some("2026")),
];

/// The window 2026-09-30T23:00:00Z (the first moment in it) up to 2026-10-31T23:00:00Z (the first
/// moment after it), written in six zones: each pair is the same two moments.
const WINDOWS: &[(&str, &str)] = &[
    ("2026-09-30T23:00:00Z", "2026-10-31T23:00:00Z"),
    ("2026-10-01T00:00:00+01:00", "2026-11-01T00:00:00+01:00"),
    ("2026-09-30T18:00:00-05:00", "2026-10-31T18:00:00-05:00"),
    ("2026-10-01T04:30:00+05:30", "2026-11-01T04:30:00+05:30"),
    ("2026-10-01T13:00:00+14:00", "2026-11-01T13:00:00+14:00"),
    ("2026-09-30T11:00:00-12:00", "2026-10-31T11:00:00-12:00"),
];

/// Observations told apart by how much of a date they say. The comment is the part of the clock it
/// stands for, in UTC, up to the first moment after it.
const PRECISIONS: &[(&str, &str)] = &[
    ("year", "2026"),                               // 2026-01-01 up to 2027-01-01
    ("month", "2026-10"),                           // 2026-10-01 up to 2026-11-01
    ("day", "2026-10-31"),                          // 2026-10-31 up to 2026-11-01
    ("second", "2026-10-31T22:30:00Z"),             // 22:30:00 up to 22:30:01
    ("second-plus01", "2026-10-31T23:30:00+01:00"), // the same second
    ("tenth", "2026-10-31T22:30:00.1Z"),            // 22:30:00.100 up to 22:30:00.200
    ("milli", "2026-10-31T22:30:00.123Z"),          // 22:30:00.123 up to 22:30:00.124
    ("prev-second", "2026-10-31T22:29:59Z"),        // 22:29:59 up to 22:30:00
    ("next-second", "2026-10-31T22:30:01Z"),        // 22:30:01 up to 22:30:02
    ("next-day", "2026-11-01"),                     // 2026-11-01 up to 2026-11-02
    ("next-month", "2026-11"),                      // 2026-11-01 up to 2026-12-01
    ("next-year", "2027"),                          // 2027-01-01 up to 2028-01-01
];

/// `created` of a Claim, a ClaimResponse and a PaymentReconciliation.
const CREATED: &[(&str, &str)] = &[
    ("z", "2026-10-31T22:30:00Z"),
    ("plus01", "2026-10-31T23:30:00+01:00"),
    // 2026-11-01T03:30:00Z.
    ("trap-late", "2026-10-31T22:30:00-05:00"),
    // 2026-09-30T10:30:00Z.
    ("trap-early", "2026-10-01T00:30:00+14:00"),
];

/// The kinds that carry `created`, and what their ids start with.
const BILLING: &[(&str, &str)] = &[
    ("Claim", "claim"),
    ("ClaimResponse", "response"),
    ("PaymentReconciliation", "recon"),
];

fn encounter(id: &str, period: Value) -> Value {
    json!({
        "resourceType": "Encounter", "id": id, "status": "finished",
        "class": {"system": "http://terminology.hl7.org/CodeSystem/v3-ActCode", "code": "AMB"},
        "period": period
    })
}

fn effective(id: &str, written: &str) -> Value {
    json!({
        "resourceType": "Observation", "id": id, "status": "final",
        "code": {"text": "vitals"}, "effectiveDateTime": written
    })
}

fn billing(type_name: &str, id: &str, created: &str) -> Value {
    let claim_type = json!({"coding": [{
        "system": "http://terminology.hl7.org/CodeSystem/claim-type", "code": "institutional"}]});
    match type_name {
        "Claim" => json!({
            "resourceType": "Claim", "id": id, "status": "active", "type": claim_type,
            "use": "claim", "patient": {"reference": "Patient/p1"}, "created": created,
            "provider": {"reference": "Organization/o1"},
            "priority": {"coding": [{
                "system": "http://terminology.hl7.org/CodeSystem/processpriority",
                "code": "normal"}]},
            "insurance": [{"sequence": 1, "focal": true,
                "coverage": {"reference": "Coverage/cov1"}}]
        }),
        "ClaimResponse" => json!({
            "resourceType": "ClaimResponse", "id": id, "status": "active", "type": claim_type,
            "use": "claim", "patient": {"reference": "Patient/p1"}, "created": created,
            "insurer": {"reference": "Organization/o1"}, "outcome": "complete"
        }),
        _ => json!({
            "resourceType": "PaymentReconciliation", "id": id, "status": "active",
            "created": created, "paymentDate": "2026-10-31",
            "paymentAmount": {"value": 10, "currency": "USD"}
        }),
    }
}

fn store(harness: &Harness, resource: Value) {
    let path = format!("{}/create", resource["resourceType"].as_str().unwrap());
    apply(harness, &path, json!({"resource": resource}));
}

/// Every resource the tests search, written once and only read after that.
fn fixture() -> &'static Harness {
    static SHARED: OnceLock<Harness> = OnceLock::new();
    SHARED.get_or_init(|| {
        let harness = r4_ready();
        for (id, start) in SAME.iter().chain(EDGES) {
            // A moment is a period that starts and ends in it.
            store(
                &harness,
                encounter(id, json!({"start": start, "end": start})),
            );
        }
        store(
            &harness,
            encounter("stay", json!({"start": STAY_START, "end": STAY_END})),
        );
        for (id, start) in OPEN {
            store(&harness, encounter(id, json!({"start": start})));
        }
        for (id, written) in PRECISIONS {
            store(&harness, effective(&format!("obs-{id}"), written));
        }
        store(&harness, json!({"resourceType": "Patient", "id": "p1"}));
        for (id, start, end) in EPISODES {
            let mut period = serde_json::Map::new();
            for (name, value) in [("start", start), ("end", end)] {
                if let Some(value) = value {
                    period.insert(name.to_string(), json!(value));
                }
            }
            store(
                &harness,
                json!({"resourceType": "EpisodeOfCare", "id": id, "status": "active",
                    "patient": {"reference": "Patient/p1"}, "period": period}),
            );
        }
        store(
            &harness,
            json!({"resourceType": "Organization", "id": "o1", "name": "Insurer"}),
        );
        store(
            &harness,
            json!({"resourceType": "Coverage", "id": "cov1", "status": "active",
                "beneficiary": {"reference": "Patient/p1"},
                "payor": [{"reference": "Organization/o1"}]}),
        );
        for (type_name, prefix) in BILLING {
            for (id, created) in CREATED {
                store(
                    &harness,
                    billing(type_name, &format!("{prefix}-{id}"), created),
                );
            }
        }
        harness
    })
}

/// The ids a search finds, in order, whatever they are. A parameter given more than once is AND.
fn found_all(type_name: &str, param: &str, values: &[&str]) -> Vec<String> {
    let bundle = apply(
        fixture(),
        &format!("{type_name}/search"),
        json!({param: values.join("\u{1f}")}),
    );
    matched_ids(&bundle)
}

/// `found_all`, without the Encounters that have no end.
fn found(type_name: &str, param: &str, values: &[&str]) -> Vec<String> {
    found_all(type_name, param, values)
        .into_iter()
        .filter(|id| !id.starts_with("open-"))
        .collect()
}

/// What a search finds among the Encounters that have no end.
fn found_open(values: &[&str]) -> Vec<String> {
    only(found_all("Encounter", "date", values), "open-")
}

fn sorted(ids: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<String> {
    let mut ids: Vec<String> = ids.into_iter().map(|id| id.as_ref().to_string()).collect();
    ids.sort();
    ids
}

fn same_ids() -> Vec<String> {
    sorted(SAME.iter().map(|(id, _)| id))
}

/// The ids that start with `prefix`, so a search over a set that holds other moments can be read
/// for the one set it is about.
fn only(ids: Vec<String>, prefix: &str) -> Vec<String> {
    ids.into_iter()
        .filter(|id| id.starts_with(prefix))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The same moment, written in different zones

/// The report that started this: two Encounters for the same moment, and a window written in UTC
/// found only the one written in UTC.
#[test]
fn a_window_finds_the_same_moment_whatever_zone_it_was_written_in() {
    // Inside the window: the six writings of 22:30Z on the 31st, the edge that is exactly at the
    // opening, the one a second before the close, and the stay that starts in it.
    let mut inside = same_ids();
    inside.extend(["edge-open", "edge-before-close", "stay"].map(String::from));
    let inside = sorted(inside);
    for (open, close) in WINDOWS {
        let ids = found(
            "Encounter",
            "date",
            &[&format!("ge{open}"), &format!("lt{close}")],
        );
        assert_eq!(ids, inside, "ge{open} lt{close}");
        for pair in ["same-z", "same-plus01"] {
            assert!(
                ids.iter().any(|id| id == pair),
                "{pair} is not in ge{open} lt{close}"
            );
        }
    }
    // The ends of a window are written in different zones, too.
    for (open, close) in [
        (WINDOWS[0].0, WINDOWS[1].1),
        (WINDOWS[1].0, WINDOWS[0].1),
        (WINDOWS[2].0, WINDOWS[3].1),
        (WINDOWS[5].0, WINDOWS[4].1),
    ] {
        let ids = found(
            "Encounter",
            "date",
            &[&format!("ge{open}"), &format!("lt{close}")],
        );
        assert_eq!(ids, inside, "ge{open} lt{close}");
    }
}

#[test]
fn a_window_keeps_what_its_clock_says_and_its_moment_does_not() {
    // `gt` leaves out a moment exactly at the opening, and `le` keeps one exactly at the close.
    let mut after_open = same_ids();
    after_open.extend(["edge-before-close", "edge-close", "stay"].map(String::from));
    let after_open = sorted(after_open);
    for (open, close) in WINDOWS {
        let ids = found(
            "Encounter",
            "date",
            &[&format!("gt{open}"), &format!("le{close}")],
        );
        assert_eq!(ids, after_open, "gt{open} le{close}");
    }
    // `sa` and `eb` take only what lies wholly beyond a value: the second before the close is
    // wholly before it, the one at the close is not, and one at the opening is not after it. The
    // stay starts inside the window and ends at 2026-11-01T00:30:01Z, so it is not wholly before the
    // close.
    let mut wholly = same_ids();
    wholly.extend(["edge-before-close"].map(String::from));
    let wholly = sorted(wholly);
    for (open, close) in WINDOWS {
        let ids = found(
            "Encounter",
            "date",
            &[&format!("sa{open}"), &format!("eb{close}")],
        );
        assert_eq!(ids, wholly, "sa{open} eb{close}");
    }
}

/// Which of the answers a prefix gives for a value a second before the moment, at it, and a second
/// after it, for a stored value of a second. A second is the period it stands for, so `gt` at the
/// moment finds nothing: the stored second does not reach above it.
const PREFIXES: &[(&str, [bool; 3])] = &[
    ("eq", [false, true, false]),
    ("ne", [true, false, true]),
    ("gt", [true, false, false]),
    ("lt", [false, false, true]),
    ("ge", [true, true, false]),
    ("le", [false, true, true]),
    ("sa", [true, false, false]),
    ("eb", [false, false, true]),
];

/// A second before the moment, the moment, and a second after, in four zones.
const AROUND: &[[&str; 3]] = &[
    [
        "2026-10-31T22:29:59Z",
        "2026-10-31T22:30:00Z",
        "2026-10-31T22:30:01Z",
    ],
    [
        "2026-10-31T23:29:59+01:00",
        "2026-10-31T23:30:00+01:00",
        "2026-10-31T23:30:01+01:00",
    ],
    [
        "2026-10-31T17:29:59-05:00",
        "2026-10-31T17:30:00-05:00",
        "2026-10-31T17:30:01-05:00",
    ],
    [
        "2026-11-01T03:59:59+05:30",
        "2026-11-01T04:00:00+05:30",
        "2026-11-01T04:00:01+05:30",
    ],
];

#[test]
fn every_prefix_answers_alike_for_the_same_moment_in_any_zone() {
    for (prefix, answers) in PREFIXES {
        for zone in AROUND {
            for (value, found_it) in zone.iter().zip(answers) {
                let ids = only(
                    found("Encounter", "date", &[&format!("{prefix}{value}")]),
                    "same-",
                );
                let expected = if *found_it { same_ids() } else { Vec::new() };
                assert_eq!(ids, expected, "{prefix}{value}");
            }
        }
    }
    // No prefix is `eq`.
    for zone in AROUND {
        let ids = only(found("Encounter", "date", &[zone[1]]), "same-");
        assert_eq!(ids, same_ids(), "{}", zone[1]);
        assert!(only(found("Encounter", "date", &[zone[0]]), "same-").is_empty());
    }
}

/// A period runs between its ends, each read in the zone it was written in.
#[test]
fn the_ends_of_a_period_are_read_in_their_own_zones() {
    let has_stay = |value: &str| {
        found("Encounter", "date", &[value])
            .iter()
            .any(|id| id == "stay")
    };
    // The stay ends at 2026-11-01T00:30:00Z. A time at or before that is reached by it.
    for value in [
        "ge2026-11-01T00:00:00Z",
        "ge2026-11-01T01:00:00+01:00",
        "ge2026-10-31T19:00:00-05:00",
        "ge2026-11-01T05:30:00+05:30",
    ] {
        assert!(has_stay(value), "{value}");
    }
    // One after it is not.
    for value in [
        "ge2026-11-01T01:00:00Z",
        "ge2026-11-01T02:00:00+01:00",
        "ge2026-10-31T20:00:00-05:00",
        "ge2026-11-01T06:30:00+05:30",
    ] {
        assert!(!has_stay(value), "{value}");
    }
    // The stay starts at 2026-10-31T22:30:00Z.
    for value in [
        "lt2026-10-31T22:45:00Z",
        "lt2026-10-31T23:45:00+01:00",
        "lt2026-10-31T17:45:00-05:00",
    ] {
        assert!(has_stay(value), "{value}");
    }
    for value in [
        "lt2026-10-31T22:00:00Z",
        "lt2026-10-31T23:00:00+01:00",
        "lt2026-10-31T17:00:00-05:00",
    ] {
        assert!(!has_stay(value), "{value}");
    }
}

// ---------------------------------------------------------------------------------------------
// A time with no zone

/// The errors an `Encounter` with this `period.start` is refused for, as the elements they name.
fn start_errors(start: &str) -> Vec<String> {
    let harness = fixture();
    let catalog = harness.catalog.read().expect("catalog");
    let resource = encounter("check", json!({"start": start}));
    let report = run(
        &harness.db,
        &catalog,
        "Encounter/$validate",
        &json!({"resource": resource}),
    )
    .expect("validate");
    report.value["issue"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|issue| issue["severity"] == "error")
        .flat_map(|issue| issue["expression"].as_array().cloned().unwrap_or_default())
        .map(|expression| expression.as_str().unwrap_or_default().to_string())
        .collect()
}

/// R4 writes a `dateTime` as a date to the year, the month or the day, or as a day with a time, and
/// a time has seconds and a zone. A time with no zone names no moment, so it is refused, rather
/// than read as some zone the writer did not say.
#[test]
fn a_stored_time_must_have_a_zone_and_a_date_need_not() {
    for refused in [
        "2026-10-31T22:30:00",
        "2026-10-31T22:30:00.5",
        "2026-10-31T22:30",
        "2026-10-31T22:30Z",
        "2026-10-31T22",
        "2026-10-31T22:30:00+0100",
        "2026-10-31T22:30:00+15:00",
        "2026-10-31 22:30:00Z",
    ] {
        assert_eq!(
            start_errors(refused),
            ["Encounter.period.start"],
            "{refused:?} should be refused"
        );
    }
    for accepted in [
        "2026",
        "2026-10",
        "2026-10-31",
        "2026-10-31T22:30:00Z",
        "2026-10-31T22:30:00.123456Z",
        "2026-10-31T22:30:00-05:00",
        "2026-10-31T22:30:00+14:00",
    ] {
        assert!(
            start_errors(accepted).is_empty(),
            "{accepted:?} should be accepted"
        );
    }
}

/// The elements a resource is refused for, as the elements they name.
fn errors_of(resource: Value) -> Vec<String> {
    let harness = fixture();
    let catalog = harness.catalog.read().expect("catalog");
    let path = format!("{}/$validate", resource["resourceType"].as_str().unwrap());
    let report =
        run(&harness.db, &catalog, &path, &json!({"resource": resource})).expect("validate");
    report.value["issue"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|issue| issue["severity"] == "error")
        .flat_map(|issue| issue["expression"].as_array().cloned().unwrap_or_default())
        .map(|expression| expression.as_str().unwrap_or_default().to_string())
        .collect()
}

/// A `date` of a Patient, a `dateTime` of an Encounter's period and an `instant` of an
/// Observation, each with `value`: the errors it is refused for.
fn errors_for(kind: &str, value: &str) -> Vec<String> {
    match kind {
        "date" => errors_of(json!({"resourceType": "Patient", "birthDate": value})),
        "dateTime" => errors_of(encounter("check", json!({"start": value}))),
        _ => errors_of(json!({
            "resourceType": "Observation", "status": "final", "code": {"text": "vitals"},
            "issued": value})),
    }
}

/// A date that cannot be searched for, or that does not exist, is refused when it is written, in
/// each of the three kinds that hold one. The day and the month start at 1, the year at 0001 (R4
/// has no year 0 and no year before it), and the day has to be one of the month in that year.
#[test]
fn a_stored_date_must_be_one_that_exists() {
    let element = |kind: &str| match kind {
        "date" => "Patient.birthDate",
        "dateTime" => "Encounter.period.start",
        _ => "Observation.issued",
    };
    // A day and a clock, for the kinds that need both; the kinds that may stop at a day or at a
    // month are also asked with less.
    for (kind, shorter) in [("date", true), ("dateTime", true), ("instant", false)] {
        let at = |day: &str| {
            if kind == "date" {
                day.to_string()
            } else {
                format!("{day}T10:00:00Z")
            }
        };
        for day in [
            "2026-10-00",
            "2026-00-10",
            "2026-13-01",
            "0000-01-01",
            "-0001-01-01",
            "2026-02-30",
            "2026-04-31",
            "2023-02-29",
            "1900-02-29",
            "2100-02-29",
            "2026-02-32",
        ] {
            assert_eq!(
                errors_for(kind, &at(day)),
                [element(kind)],
                "{kind} {day} should be refused"
            );
        }
        for day in [
            "2024-02-29",
            "2000-02-29",
            "2026-02-28",
            "2026-04-30",
            "2026-10-31",
            "0001-01-01",
            "9999-12-31",
        ] {
            assert!(
                errors_for(kind, &at(day)).is_empty(),
                "{kind} {day} should be accepted"
            );
        }
        if shorter {
            for text in ["0000", "0000-01", "-2026", "-2026-10", "2026-00", "2026-13"] {
                assert_eq!(
                    errors_for(kind, text),
                    [element(kind)],
                    "{kind} {text} should be refused"
                );
            }
            for text in ["0001", "9999", "2026", "2026-02", "2026-12"] {
                assert!(
                    errors_for(kind, text).is_empty(),
                    "{kind} {text} should be accepted"
                );
            }
        }
    }
}

/// What can be written as a `date` is what can be searched for: one grammar for the two. A date
/// that is stored and that no search can read is a record nothing finds, and a value that is asked
/// for and could never be stored is a search that cannot find anything.
#[test]
fn a_date_that_can_be_stored_is_one_that_can_be_asked_for() {
    let birthdate = crate::extract::compile_parameter(
        &json!({"code": "birthdate", "type": "date", "expression": "Patient.birthDate"}),
        &BTreeMap::new(),
    )
    .expect("a date parameter");
    for text in [
        "2026",
        "2026-10",
        "2026-10-31",
        "2024-02-29",
        "2000-02-29",
        "0001",
        "0001-01-01",
        "9999",
        "9999-12-31",
        "2026-10-00",
        "2026-00",
        "2026-13",
        "2026-02-30",
        "2026-04-31",
        "2023-02-29",
        "1900-02-29",
        "0000",
        "0000-01",
        "0000-01-01",
        "-2026",
        "-0001-01-01",
        "20261031",
        "2026-1-5",
        "26-10-31",
        "2026-10-31-01",
        "yesterday",
        "",
    ] {
        assert_eq!(
            errors_for("date", text).is_empty(),
            birthdate.check_value(None, text).is_ok(),
            "{text:?}: the validator and the search disagree"
        );
    }
}

#[test]
fn a_write_of_a_time_with_no_zone_is_refused_and_stores_nothing() {
    let harness = ready();
    apply(
        &harness,
        "Patient/create",
        json!({"resource": patient("p1", true, "Ann")}),
    );
    let observation = |written: &str| {
        json!({"resource": {
            "resourceType": "Observation", "status": "final", "code": {"text": "vitals"},
            "subject": {"reference": "Patient/p1"}, "effectiveDateTime": written}})
    };
    // A resource that fails validation is a 400 with an OperationOutcome.
    let Err(RunError::Fhir(FhirError::Outcome(outcome))) = run_op(
        &harness,
        "Observation/create",
        &observation("2026-10-31T22:30:00"),
    ) else {
        panic!("a time with no zone should be refused");
    };
    assert!(
        outcome
            .to_string()
            .contains("Observation.effectiveDateTime"),
        "the outcome should name the element: {outcome}"
    );
    run_op(
        &harness,
        "Observation/create",
        &observation("2026-10-31T22:30:00Z"),
    )
    .expect("with a zone");
    run_op(&harness, "Observation/create", &observation("2026-10-31")).expect("a date");
    assert_eq!(apply(&harness, "Observation/search", json!({}))["total"], 2);
}

/// A value in a search that has no zone is read as UTC, as a time in a search is everywhere else in
/// ReFHIR, and it finds what the same time with a `Z` finds.
#[test]
fn a_search_value_with_no_zone_is_utc() {
    for (prefix, _) in PREFIXES {
        for value in AROUND[0] {
            let bare = value.trim_end_matches('Z');
            assert_ne!(bare, value);
            assert_eq!(
                found("Encounter", "date", &[&format!("{prefix}{bare}")]),
                found("Encounter", "date", &[&format!("{prefix}{value}")]),
                "{prefix}{bare}"
            );
        }
    }
    let ids = only(
        found(
            "Encounter",
            "date",
            &[&format!("ge{}", AROUND[0][1].trim_end_matches('Z'))],
        ),
        "same-",
    );
    assert_eq!(ids, same_ids(), "22:30:00 with no zone is 22:30:00Z");
    assert!(
        only(
            found("Encounter", "date", &["gt2026-10-31T22:30:00"]),
            "same-"
        )
        .is_empty(),
        "22:30:00 with no zone is not read in the zone of a stored value"
    );
}

// ---------------------------------------------------------------------------------------------
// Precision

/// What a search finds among the Observations of `PRECISIONS`, by the names they have there.
fn by_name(query: &str) -> Vec<String> {
    sorted(
        found("Observation", "date", &[query])
            .iter()
            .map(|id| id.trim_start_matches("obs-")),
    )
}

fn all_names() -> Vec<&'static str> {
    PRECISIONS.iter().map(|(name, _)| *name).collect()
}

/// What the search must find among the Observations, and nothing else.
fn exact(query: &str, expected: &[&str]) {
    assert_eq!(by_name(query), sorted(expected), "{query}");
}

fn all_but(left_out: &[&str]) -> Vec<&'static str> {
    all_names()
        .into_iter()
        .filter(|name| !left_out.contains(name))
        .collect()
}

/// `eq` is what the search value holds, whole: R4 asks that the range of the search value fully
/// contains the range of the stored value, so a month or a year, which reaches beyond the day, is
/// not `eq` to it, and it is `ne`.
#[test]
fn a_value_to_the_day_is_the_day_in_utc() {
    // 2026-10-31 up to 2026-11-01.
    let narrower = [
        "day",
        "second",
        "second-plus01",
        "tenth",
        "milli",
        "prev-second",
        "next-second",
    ];
    exact("eq2026-10-31", &narrower);
    exact("2026-10-31", &narrower);
    exact(
        "ne2026-10-31",
        &["year", "month", "next-day", "next-month", "next-year"],
    );
    exact(
        "gt2026-10-31",
        &["year", "next-day", "next-month", "next-year"],
    );
    exact("lt2026-10-31", &["year", "month"]);
    // The month ends where the day does: it does not reach above the day, and the day does not
    // hold it, so it is not `ge` the day.
    exact("ge2026-10-31", &all_but(&["month"]));
    exact(
        "le2026-10-31",
        &[
            "year",
            "month",
            "day",
            "second",
            "second-plus01",
            "tenth",
            "milli",
            "prev-second",
            "next-second",
        ],
    );
    exact("sa2026-10-31", &["next-day", "next-month", "next-year"]);
    exact("eb2026-10-31", &[]);
}

#[test]
fn a_value_to_the_minute_is_the_minute() {
    // 22:30:00 up to 22:31:00.
    let q = "2026-10-31T22:30Z";
    let inside = ["second", "second-plus01", "tenth", "milli", "next-second"];
    exact(&format!("eq{q}"), &inside);
    exact(
        &format!("ne{q}"),
        &[
            "year",
            "month",
            "day",
            "prev-second",
            "next-day",
            "next-month",
            "next-year",
        ],
    );
    exact(
        &format!("gt{q}"),
        &[
            "year",
            "month",
            "day",
            "next-day",
            "next-month",
            "next-year",
        ],
    );
    exact(&format!("lt{q}"), &["year", "month", "day", "prev-second"]);
    exact(&format!("ge{q}"), &all_but(&["prev-second"]));
    exact(
        &format!("le{q}"),
        &[
            "year",
            "month",
            "day",
            "second",
            "second-plus01",
            "tenth",
            "milli",
            "prev-second",
            "next-second",
        ],
    );
    exact(&format!("sa{q}"), &["next-day", "next-month", "next-year"]);
    exact(&format!("eb{q}"), &["prev-second"]);
    // The same minute in another zone.
    exact("eq2026-10-31T23:30+01:00", &inside);
    exact("eq2026-10-31T17:30-05:00", &inside);
}

#[test]
fn a_value_to_the_second_is_the_second() {
    // 22:30:00 up to 22:30:01.
    let q = "2026-10-31T22:30:00Z";
    exact(
        &format!("eq{q}"),
        &["second", "second-plus01", "tenth", "milli"],
    );
    exact(
        &format!("ne{q}"),
        &[
            "year",
            "month",
            "day",
            "prev-second",
            "next-second",
            "next-day",
            "next-month",
            "next-year",
        ],
    );
    exact(
        &format!("gt{q}"),
        &[
            "year",
            "month",
            "day",
            "next-second",
            "next-day",
            "next-month",
            "next-year",
        ],
    );
    exact(&format!("lt{q}"), &["year", "month", "day", "prev-second"]);
    exact(&format!("ge{q}"), &all_but(&["prev-second"]));
    exact(
        &format!("le{q}"),
        &[
            "year",
            "month",
            "day",
            "second",
            "second-plus01",
            "tenth",
            "milli",
            "prev-second",
        ],
    );
    exact(
        &format!("sa{q}"),
        &["next-second", "next-day", "next-month", "next-year"],
    );
    exact(&format!("eb{q}"), &["prev-second"]);
    exact(
        "eq2026-10-31T23:30:00+01:00",
        &["second", "second-plus01", "tenth", "milli"],
    );
}

#[test]
fn a_value_to_the_millisecond_is_the_millisecond() {
    // 22:30:00.123 up to 22:30:00.124.
    let q = "2026-10-31T22:30:00.123Z";
    exact(&format!("eq{q}"), &["milli"]);
    exact(&format!("ne{q}"), &all_but(&["milli"]));
    exact(
        &format!("gt{q}"),
        &[
            "year",
            "month",
            "day",
            "second",
            "second-plus01",
            "tenth",
            "next-second",
            "next-day",
            "next-month",
            "next-year",
        ],
    );
    exact(
        &format!("lt{q}"),
        &[
            "year",
            "month",
            "day",
            "second",
            "second-plus01",
            "tenth",
            "prev-second",
        ],
    );
    exact(&format!("ge{q}"), &all_but(&["prev-second"]));
    exact(
        &format!("le{q}"),
        &[
            "year",
            "month",
            "day",
            "second",
            "second-plus01",
            "tenth",
            "milli",
            "prev-second",
        ],
    );
    exact(
        &format!("sa{q}"),
        &["next-second", "next-day", "next-month", "next-year"],
    );
    exact(&format!("eb{q}"), &["prev-second"]);
    exact("eq2026-10-31T23:30:00.123+01:00", &["milli"]);
    // A fraction is a tenth, a hundredth or a thousandth, however many digits are written.
    exact("eq2026-10-31T22:30:00.1Z", &["tenth", "milli"]);
    exact("eq2026-10-31T22:30:00.12Z", &["milli"]);
    exact("eq2026-10-31T22:30:00.123456Z", &["milli"]);
}

// ---------------------------------------------------------------------------------------------
// The other kinds that are searched by a date

/// The claims window of Boolbyte: `created` between two instants, for each kind that has one.
#[test]
fn created_is_compared_as_a_moment_for_claims_responses_and_payments() {
    for (type_name, prefix) in BILLING {
        let id = |suffix: &str| format!("{prefix}-{suffix}");
        let both = sorted([id("z"), id("plus01")]);
        for (open, close) in WINDOWS {
            let ids = found(
                type_name,
                "created",
                &[&format!("ge{open}"), &format!("lt{close}")],
            );
            assert_eq!(ids, both, "{type_name} ge{open} lt{close}");
        }
        // The clinic's day, the 1st of November at +01:00, which is 2026-10-31T23:00:00Z up to
        // 2026-11-01T23:00:00Z: it holds the moment whose clock reads 22:30 on the 31st at -05:00,
        // and neither of the moments at 22:30Z.
        for (open, close) in [
            ("2026-11-01T00:00:00+01:00", "2026-11-02T00:00:00+01:00"),
            ("2026-10-31T23:00:00Z", "2026-11-01T23:00:00Z"),
        ] {
            let ids = found(
                type_name,
                "created",
                &[&format!("ge{open}"), &format!("lt{close}")],
            );
            assert_eq!(ids, [id("trap-late")], "{type_name} ge{open} lt{close}");
        }
        // The 1st of October by the clock of the one at +14:00 is the 30th of September in UTC.
        let ids = found(
            type_name,
            "created",
            &["ge2026-10-01T00:00:00Z", "lt2026-10-02T00:00:00Z"],
        );
        assert!(ids.is_empty(), "{type_name}: {ids:?}");
        // Every prefix, for the moment written two ways.
        for (op, answers) in PREFIXES {
            for zone in AROUND {
                for (value, found_it) in zone.iter().zip(answers) {
                    let ids = found(type_name, "created", &[&format!("{op}{value}")]);
                    let has = |suffix: &str| ids.contains(&id(suffix));
                    assert_eq!(has("z"), *found_it, "{type_name} {op}{value}");
                    assert_eq!(has("plus01"), *found_it, "{type_name} {op}{value}");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Periods

/// What a search finds among the EpisodeOfCare periods.
fn episodes(values: &[&str]) -> Vec<String> {
    found("EpisodeOfCare", "date", values)
}

fn ids(names: &[&str]) -> Vec<String> {
    sorted(names)
}

/// A period is the range from where its start begins to where its end stops, and the prefixes
/// compare that range with the one the value names. Each expected answer below is worked out from
/// `EPISODES`: the closed ones run 2026-10-29T08:00:00Z up to 2026-10-31T12:00:01Z (`ep-closed`
/// and `ep-zones`), `ep-days` is 2026-10-29 up to 2026-11-01, `ep-year` is 2026, `ep-open-end` has
/// no end and `ep-open-start` no start.
#[test]
fn a_period_is_compared_as_the_range_between_its_ends() {
    let all = ids(&[
        "ep-closed",
        "ep-zones",
        "ep-open-end",
        "ep-open-start",
        "ep-days",
        "ep-year",
    ]);
    // `eq` is a period that lies wholly inside the value.
    assert_eq!(episodes(&["eq2026-10-30"]), ids(&[]));
    assert_eq!(episodes(&["ne2026-10-30"]), all);
    assert_eq!(
        episodes(&["eq2026-10"]),
        ids(&["ep-closed", "ep-zones", "ep-days"])
    );
    assert_eq!(
        episodes(&["ne2026-10"]),
        ids(&["ep-open-end", "ep-open-start", "ep-year"])
    );
    assert_eq!(
        episodes(&["eq2026"]),
        ids(&["ep-closed", "ep-zones", "ep-days", "ep-year"])
    );
    assert_eq!(
        episodes(&["ne2026"]),
        ids(&["ep-open-end", "ep-open-start"])
    );
    assert_eq!(episodes(&["eq2026-10-31T12:00:00Z"]), ids(&[]));
    // `gt` is a period that reaches above the value, `lt` one that reaches below it.
    assert_eq!(
        episodes(&["gt2026-10-31"]),
        ids(&["ep-open-end", "ep-year"])
    );
    assert_eq!(episodes(&["lt2026-10-30"]), all);
    assert_eq!(
        episodes(&["lt2026-10-29"]),
        ids(&["ep-open-start", "ep-year"])
    );
    // `ge` and `le` add the period that lies wholly inside the value. None does here: one that
    // ends inside the day does not reach above it, and one that starts inside the day does not
    // reach below it.
    assert_eq!(
        episodes(&["ge2026-10-31"]),
        ids(&["ep-open-end", "ep-year"])
    );
    assert_eq!(
        episodes(&["le2026-10-29"]),
        ids(&["ep-open-start", "ep-year"])
    );
    // `sa` is a period wholly after the value, `eb` one wholly before it.
    assert_eq!(
        episodes(&["sa2026-10-28"]),
        ids(&["ep-closed", "ep-zones", "ep-open-end", "ep-days"])
    );
    assert_eq!(episodes(&["sa2026-10-31"]), ids(&[]));
    assert_eq!(
        episodes(&["eb2026-11-01"]),
        ids(&["ep-closed", "ep-zones", "ep-days", "ep-open-start"])
    );
    assert_eq!(episodes(&["eb2026-10-29"]), ids(&[]));
}

/// A window of two values, which is how a period is asked for between two times: it finds every
/// period that is going on at some time in it.
#[test]
fn a_window_finds_the_periods_that_are_going_on_in_it() {
    let all = ids(&[
        "ep-closed",
        "ep-zones",
        "ep-open-end",
        "ep-open-start",
        "ep-days",
        "ep-year",
    ]);
    // Every one of them is going on during 2026-10-30, written in UTC and at +01:00.
    for window in [
        ["ge2026-10-30T00:00:00Z", "lt2026-10-31T00:00:00Z"],
        ["ge2026-10-30T01:00:00+01:00", "lt2026-10-31T01:00:00+01:00"],
        ["ge2026-10-29T19:00:00-05:00", "lt2026-10-30T19:00:00-05:00"],
    ] {
        assert_eq!(episodes(&window), all, "{window:?}");
    }
    // Only what has not ended, and the year, are going on in the middle of November.
    for window in [
        ["ge2026-11-02T00:00:00Z", "lt2026-11-03T00:00:00Z"],
        ["ge2026-11-02T01:00:00+01:00", "lt2026-11-03T01:00:00+01:00"],
    ] {
        assert_eq!(
            episodes(&window),
            ids(&["ep-open-end", "ep-year"]),
            "{window:?}"
        );
    }
    // Before any of them started, what has no start and the year are going on.
    assert_eq!(
        episodes(&["ge2026-09-01T00:00:00Z", "lt2026-09-02T00:00:00Z"]),
        ids(&["ep-open-start", "ep-year"])
    );
    // Far from every date they hold, only what has no start is below 2020, and only what has no end
    // is above 2030: they are found by being open, not by being near.
    assert_eq!(episodes(&["lt2020-01-01"]), ids(&["ep-open-start"]));
    assert_eq!(episodes(&["gt2030-01-01"]), ids(&["ep-open-end"]));
    // And in January 2027 only what has no end, since the year ends at the start of it.
    assert_eq!(
        episodes(&["ge2027-01-01T00:00:00Z", "lt2027-02-01T00:00:00Z"]),
        ids(&["ep-open-end"])
    );
}

/// The same two moments written at +01:00 are the same period, whichever way it is asked for.
#[test]
fn a_period_written_in_another_zone_is_the_same_period() {
    let mut values: Vec<String> = Vec::new();
    for prefix in ["eq", "ne", "gt", "lt", "ge", "le", "sa", "eb"] {
        for value in [
            "2026",
            "2026-10",
            "2026-10-29",
            "2026-10-31",
            "2026-11-01",
            "2026-10-29T08:00:00Z",
            "2026-10-29T09:00:00+01:00",
            "2026-10-29T07:59:59Z",
            "2026-10-29T08:00:01Z",
            "2026-10-31T12:00:00Z",
            "2026-10-31T07:00:00-05:00",
            "2026-10-31T11:59:59Z",
            "2026-10-31T12:00:01Z",
        ] {
            values.push(format!("{prefix}{value}"));
        }
    }
    for value in &values {
        let ids = episodes(&[value]);
        assert_eq!(
            ids.contains(&"ep-closed".to_string()),
            ids.contains(&"ep-zones".to_string()),
            "{value}: {ids:?}"
        );
    }
}

/// An Encounter with a start and no end has not ended: R4 reads its period as going on, so it is
/// found by a window after the one it started in as well as by the one it started in, and it is
/// `ne` any time it is not wholly inside.
#[test]
fn an_encounter_with_no_end_is_going_on() {
    let both = ids(&["open-plus01", "open-z"]);
    // The request that started this: the one at 22:30Z and the same moment at +01:00 are both in
    // the window, in UTC and in other zones, and the one that starts in December is not.
    for (open, close) in WINDOWS {
        assert_eq!(
            found_open(&[&format!("ge{open}"), &format!("lt{close}")]),
            both,
            "ge{open} lt{close}"
        );
    }
    // A window in the middle of November: both are still going on, and the December one has not
    // started.
    assert_eq!(
        found_open(&["ge2026-11-15T00:00:00Z", "lt2026-11-16T00:00:00Z"]),
        both
    );
    // A window in 2027: all three.
    assert_eq!(
        found_open(&["ge2027-01-01T00:00:00Z", "lt2027-01-02T00:00:00Z"]),
        ids(&["open-dec", "open-plus01", "open-z"])
    );
    // A window that closes before any of them started holds none.
    assert!(found_open(&["ge2026-10-01T00:00:00Z", "lt2026-10-02T00:00:00Z"]).is_empty());
    assert!(found_open(&["lt2026-10-31T22:15:00Z"]).is_empty());
    assert_eq!(found_open(&["lt2026-10-31T22:45:00Z"]), both);
    assert_eq!(found_open(&["lt2026-10-31T23:45:00+01:00"]), both);
    // They reach above anything, and lie wholly after what is before them.
    assert_eq!(
        found_open(&["gt2030-01-01"]),
        ids(&["open-dec", "open-plus01", "open-z"])
    );
    assert_eq!(
        found_open(&["sa2026-10-31T22:00:00Z"]),
        ids(&["open-dec", "open-plus01", "open-z"])
    );
    // They are never inside a value and never wholly before one.
    assert!(found_open(&["eq2026-10-31"]).is_empty());
    assert!(found_open(&["eb2030-01-01"]).is_empty());
    assert_eq!(
        found_open(&["ne2026-10-31"]),
        ids(&["open-dec", "open-plus01", "open-z"])
    );
}

// ---------------------------------------------------------------------------------------------
// Order

/// The ids of a search in the order the bundle holds them, which `matched_ids` does not keep.
fn in_order(sort: &str) -> Vec<String> {
    let bundle = apply(
        fixture(),
        "Encounter/search",
        json!({"_sort": sort, "_count": "50"}),
    );
    bundle["entry"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["search"]["mode"] == "match")
        .map(|entry| entry["resource"]["id"].as_str().unwrap().to_string())
        .filter(|id| !id.starts_with("open-"))
        .collect()
}

/// The six writings of the one moment and the stay that starts at it, by id.
fn same_and_stay() -> Vec<String> {
    let mut ids = same_ids();
    ids.push("stay".to_string());
    ids.sort();
    ids
}

/// `_sort=date` puts moments in the order of the clock of the world, not of the clock in the text.
/// The text of `trap-late` reads 22:30 on the 31st and sorts among the moments of that day, but it
/// is the last moment of them all.
#[test]
fn sorting_by_date_orders_moments_and_not_the_text_of_them() {
    // Oldest first. The moments that are the same are in the order of their ids.
    let mut earliest_first: Vec<String> = [
        "trap-early",       // 2026-09-30T10:30:00Z
        "edge-before-open", // 2026-09-30T22:59:59Z
        "edge-open",        // 2026-09-30T23:00:00Z
    ]
    .map(String::from)
    .to_vec();
    // 2026-10-31T22:30:00Z, six writings of it and the stay that starts then.
    earliest_first.extend(same_and_stay());
    earliest_first.extend(
        [
            "edge-before-close", // 2026-10-31T22:59:59Z
            "edge-close",        // 2026-10-31T23:00:00Z
            "trap-late",         // 2026-11-01T03:30:00Z
        ]
        .map(String::from),
    );
    assert_eq!(in_order("date"), earliest_first);
    // Newest first is that order turned over, with the moments that are the same still by id.
    let mut latest_first = vec!["trap-late", "edge-close", "edge-before-close"]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    latest_first.extend(same_and_stay());
    latest_first.extend(["edge-open", "edge-before-open", "trap-early"].map(String::from));
    assert_eq!(in_order("-date"), latest_first);
}
