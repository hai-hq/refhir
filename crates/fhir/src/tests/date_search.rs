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
            store(&harness, encounter(id, json!({"start": start})));
        }
        store(
            &harness,
            encounter("stay", json!({"start": STAY_START, "end": STAY_END})),
        );
        for (id, written) in PRECISIONS {
            store(&harness, effective(&format!("obs-{id}"), written));
        }
        store(&harness, json!({"resourceType": "Patient", "id": "p1"}));
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

/// The ids a search finds, in order. A parameter given more than once is AND.
fn found(type_name: &str, param: &str, values: &[&str]) -> Vec<String> {
    let bundle = apply(
        fixture(),
        &format!("{type_name}/search"),
        json!({param: values.join("\u{1f}")}),
    );
    matched_ids(&bundle)
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
    // wholly before it, the one at the close is not, and one at the opening is not after it.
    let mut wholly = same_ids();
    wholly.extend(["edge-before-close", "stay"].map(String::from));
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

/// A period is read at both ends, each in the zone it was written in.
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

/// `have` is what the search must find; of the rest, what is not in `open` it must not find. A
/// stored value wider than the search value overlaps it without lying inside it, and for `eq`,
/// `ne`, and `ge` and `le` at the edge of it, R4 and a plain overlap answer differently; those are
/// left `open`, and each is the stored value that is wider than the one asked for.
fn check(query: &str, have: &[&str], open: &[&str]) {
    let names = by_name(query);
    for name in have.iter().copied() {
        assert!(
            names.iter().any(|found| found == name),
            "{query} should find {name}: {names:?}"
        );
    }
    for name in all_names() {
        if !have.contains(&name) && !open.contains(&name) {
            assert!(
                !names.iter().any(|found| found == name),
                "{query} should not find {name}: {names:?}"
            );
        }
    }
}

fn all_but(left_out: &[&str]) -> Vec<&'static str> {
    all_names()
        .into_iter()
        .filter(|name| !left_out.contains(name))
        .collect()
}

#[test]
fn a_value_to_the_day_is_the_day_in_utc() {
    // 2026-10-31 up to 2026-11-01.
    let (day, narrower) = (
        "2026-10-31",
        [
            "day",
            "second",
            "second-plus01",
            "tenth",
            "milli",
            "prev-second",
            "next-second",
        ],
    );
    check(&format!("eq{day}"), &narrower, &["year", "month"]);
    check(day, &narrower, &["year", "month"]);
    check(
        &format!("ne{day}"),
        &["next-day", "next-month", "next-year"],
        &["year", "month"],
    );
    check(
        &format!("gt{day}"),
        &["year", "next-day", "next-month", "next-year"],
        &[],
    );
    check(&format!("lt{day}"), &["year", "month"], &[]);
    // The month ends where the day does, and a plain overlap takes it; R4 does not.
    check(&format!("ge{day}"), &all_but(&["month"]), &["month"]);
    check(
        &format!("le{day}"),
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
        &[],
    );
    check(
        &format!("sa{day}"),
        &["next-day", "next-month", "next-year"],
        &[],
    );
    check(&format!("eb{day}"), &[], &[]);
}

#[test]
fn a_value_to_the_minute_is_the_minute() {
    // 22:30:00 up to 22:31:00.
    let q = "2026-10-31T22:30Z";
    let inside = ["second", "second-plus01", "tenth", "milli", "next-second"];
    check(&format!("eq{q}"), &inside, &["year", "month", "day"]);
    check(
        &format!("ne{q}"),
        &["prev-second", "next-day", "next-month", "next-year"],
        &["year", "month", "day"],
    );
    check(
        &format!("gt{q}"),
        &[
            "year",
            "month",
            "day",
            "next-day",
            "next-month",
            "next-year",
        ],
        &[],
    );
    check(
        &format!("lt{q}"),
        &["year", "month", "day", "prev-second"],
        &[],
    );
    check(&format!("ge{q}"), &all_but(&["prev-second"]), &[]);
    check(
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
        &[],
    );
    check(
        &format!("sa{q}"),
        &["next-day", "next-month", "next-year"],
        &[],
    );
    check(&format!("eb{q}"), &["prev-second"], &[]);
    // The same minute in another zone.
    check(
        "eq2026-10-31T23:30+01:00",
        &inside,
        &["year", "month", "day"],
    );
    check(
        "eq2026-10-31T17:30-05:00",
        &inside,
        &["year", "month", "day"],
    );
}

#[test]
fn a_value_to_the_second_is_the_second() {
    // 22:30:00 up to 22:30:01.
    let q = "2026-10-31T22:30:00Z";
    check(
        &format!("eq{q}"),
        &["second", "second-plus01", "tenth", "milli"],
        &["year", "month", "day"],
    );
    check(
        &format!("ne{q}"),
        &[
            "prev-second",
            "next-second",
            "next-day",
            "next-month",
            "next-year",
        ],
        &["year", "month", "day"],
    );
    check(
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
        &[],
    );
    check(
        &format!("lt{q}"),
        &["year", "month", "day", "prev-second"],
        &[],
    );
    check(&format!("ge{q}"), &all_but(&["prev-second"]), &[]);
    check(
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
        &[],
    );
    check(
        &format!("sa{q}"),
        &["next-second", "next-day", "next-month", "next-year"],
        &[],
    );
    check(&format!("eb{q}"), &["prev-second"], &[]);
    check(
        "eq2026-10-31T23:30:00+01:00",
        &["second", "second-plus01", "tenth", "milli"],
        &["year", "month", "day"],
    );
}

#[test]
fn a_value_to_the_millisecond_is_the_millisecond() {
    // 22:30:00.123 up to 22:30:00.124.
    let q = "2026-10-31T22:30:00.123Z";
    let wider = ["year", "month", "day", "second", "second-plus01", "tenth"];
    check(&format!("eq{q}"), &["milli"], &wider);
    check(
        &format!("ne{q}"),
        &[
            "prev-second",
            "next-second",
            "next-day",
            "next-month",
            "next-year",
        ],
        &wider,
    );
    check(
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
        &[],
    );
    check(
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
        &[],
    );
    check(&format!("ge{q}"), &all_but(&["prev-second"]), &[]);
    check(
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
        &[],
    );
    check(
        &format!("sa{q}"),
        &["next-second", "next-day", "next-month", "next-year"],
        &[],
    );
    check(&format!("eb{q}"), &["prev-second"], &[]);
    check("eq2026-10-31T23:30:00.123+01:00", &["milli"], &wider);
    // A fraction is a tenth, a hundredth or a thousandth, however many digits are written.
    check(
        "eq2026-10-31T22:30:00.1Z",
        &["tenth", "milli"],
        &["year", "month", "day", "second", "second-plus01"],
    );
    check("eq2026-10-31T22:30:00.12Z", &["milli"], &wider);
    check("eq2026-10-31T22:30:00.123456Z", &["milli"], &wider);
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
