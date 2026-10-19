//! `Patient/$match`: which Patients in this store may be the person a Patient resource describes.
//!
//! The operation is FHIR R4's (<https://hl7.org/fhir/R4/patient-operation-match.html>), and its
//! algorithm is this server's own, written here as data ([`Rules`]) so that it can be read and
//! tuned in one place.
//!
//! **Evidence.** A stored Patient is compared with the one in the request, and what they have in
//! common is recorded as [`Need`]s:
//!
//! - `Identifier`: an identifier with the same `system` and the same `value`, both compared
//!   exactly. An identifier without a system names nothing, so it is never evidence; nor is one
//!   whose `use` is `temp`, because a temporary number is reissued.
//! - `Name`: a name with the same words. Case, accents, punctuation, spacing and the order of the
//!   words are ignored, so `Okafor Ada` is `ada OKAFOR`, and a given name written as the family
//!   name (and the other way round) is the same name. A name of fewer than two words is not
//!   evidence: a first name alone says too little.
//! - `BirthDate`: the same full date. A year, or a year and month, is never evidence.
//! - `Contact`: the same phone number or email address in `Patient.telecom` (the person's own,
//!   not `Patient.contact`, which is somebody else's). Phone numbers are compared by what they
//!   dial, so `+234 803 123 4567`, `0803 123 4567` and `2348031234567` are one number. A telecom
//!   whose `use` is `old` is not evidence, because a number that was given up is given to someone
//!   else.
//!
//! **Grades.** The first of these that the evidence satisfies is the match's grade, and a Patient
//! that satisfies none is not a match at all. A birth date that differs is no evidence, so the
//! same name with another birth date is not a match.
//!
//! | grade      | needs                                | score, from–to |
//! |------------|--------------------------------------|----------------|
//! | `certain`  | `Identifier`                         | 1              |
//! | `probable` | `Name` + `BirthDate` + `Contact`     | 0.75–0.95      |
//! | `possible` | `Name` + `BirthDate`                 | 0.40–0.70      |
//!
//! The score starts from the middle of its grade's range and moves with the rest of what is known:
//! a name written in another order is a little lower, a gender that agrees is a little higher, and
//! one that differs is lower. It never leaves the range, so the order of the grades always holds.
//! The server returns `certain`, `probable` and `possible`. It never returns `certainly-not`: a
//! Patient that cannot be the person is left out.
//!
//! **What it says matched.** Each entry carries the grade and, as well, one `match-evidence`
//! extension for each thing the match was found by ([`Evidence::extensions`]). The identifier, the
//! phone number and the email address are the request's own, as it wrote them, so a caller that
//! has verified one number among several can see whether that is the one that matched.
//!
//! **Patients that are not in use.** A Patient with `active` false and no other that took its place
//! is never returned: it is not who a new record should be filed against. One that another
//! replaced (a `link` of type `replaced-by`, whether or not it was also switched off) is not
//! returned either, but a match on it is a match on the person, so the server follows the link to
//! the Patient that replaced it ([`ReplacedBy`]): through links to Patients in this store, no more
//! than a few of them, never round a cycle, and only to a Patient that is in use and has no
//! replacement of its own. What it finds there is that Patient, with the grade of the match on
//! the replaced one but never above `probable`, which is what no one compared the Patient with the
//! request for. The evidence says `replaced-record` with the replaced one, and then only what the
//! Patient returned itself shares with the request, which can be less than the replaced one had. A Patient that is
//! found both ways is one match, the better of the two. A link that leads nowhere (the Patient is
//! gone, switched off, or on another server) leaves nothing to return.
//!
//! **`onlyCertainMatches`.** FHIR says that when there is more than one potential match, nothing is
//! returned for it, so that the caller never links on a guess. A potential match is any Patient
//! with a grade. The answer is the one Patient when it is the only one and it is `certain`, and
//! nothing otherwise. `count` is applied afterwards, so it cannot turn many potential matches
//! into one.
//!
//! **Where it looks.** Only this store's Patients are read. They are found through the indexes:
//! an `identifier` search for each identifier of the request, and a `birthdate` search for its
//! birth date. Everything else is compared in memory, on what those two found.

use std::collections::{BTreeMap, BTreeSet};

use refhir_database::Transaction;
use serde_json::{json, Value};
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

use crate::catalog::Catalog;
use crate::error::FhirError;
use crate::ops;
use crate::search;

/// The extension that carries a match's grade on `Bundle.entry.search`.
const MATCH_GRADE: &str = "http://hl7.org/fhir/StructureDefinition/match-grade";

/// The extension, repeated on `Bundle.entry.search`, that says what a match was found by.
const MATCH_EVIDENCE: &str = "https://refhir.dev/StructureDefinition/match-evidence";

/// The core definition of the operation, for the CapabilityStatement.
const DEFINITION: &str = "http://hl7.org/fhir/OperationDefinition/Patient-match";

/// How sure a match is. Declared weakest first, so that `Ord` puts a stronger grade higher.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Grade {
    Possible,
    Probable,
    Certain,
}

impl Grade {
    fn code(self) -> &'static str {
        match self {
            Grade::Possible => "possible",
            Grade::Probable => "probable",
            Grade::Certain => "certain",
        }
    }
}

/// One kind of agreement between two Patients.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Need {
    Identifier,
    Name,
    BirthDate,
    Contact,
}

/// What it takes to be a grade, and what the match scores. Scores are in hundredths, so that two
/// of them are compared exactly.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GradeRule {
    pub grade: Grade,
    /// Every one of these must be found.
    pub needs: &'static [Need],
    /// The score before the adjustments, and the range it is kept to.
    pub base: i32,
    pub floor: i32,
    pub ceiling: i32,
}

/// What to do with a match on a Patient that another replaced (`link` of type `replaced-by`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReplacedBy {
    /// How many `replaced-by` links to follow from the replaced Patient to one that is in use.
    pub max_hops: usize,
    /// The strongest grade the Patient that is reached may have. A match on a replaced Patient
    /// is a match on the person, but the server did not compare that Patient with the request,
    /// and the link was made by whoever merged the two, so it never has more than this.
    pub cap: Grade,
}

/// Everything the algorithm decides, in one place.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rules {
    /// The grades, strongest first. The first whose needs are all found is the match's grade.
    pub grades: &'static [GradeRule],
    /// Fewest words a name needs to be evidence.
    pub min_name_words: usize,
    /// Whether a name whose family and given words are exchanged is the same name.
    pub swapped_names: bool,
    /// What a name in another order, a gender that agrees and one that differs add to a score.
    pub swapped_names_score: i32,
    pub gender_agrees_score: i32,
    pub gender_differs_score: i32,
    /// The country code a phone number is taken to have when it is written without one, as
    /// `0803 …` is. Nigeria's.
    pub country_code: &'static str,
    /// Identifier `use`s that name nobody: they are not evidence.
    pub ignored_identifier_uses: &'static [&'static str],
    /// Telecom `use`s that are not the person's any more.
    pub ignored_contact_uses: &'static [&'static str],
    /// Where a match on a replaced Patient goes: to the Patient that replaced it, within these
    /// limits. `None` leaves a replaced Patient out and follows nothing.
    pub replaced_by: Option<ReplacedBy>,
    /// Most identifiers, names or telecoms a request may carry. Each identifier is a search of its
    /// own, and every name and telecom is compared with every candidate, so a request is refused
    /// that has more of them than a registration has.
    pub max_elements: usize,
    /// How many Patients one page of candidates holds.
    pub page: usize,
    /// Most candidates one request may compare. More than this is refused: an answer that left
    /// some out would look like a complete one.
    pub max_candidates: usize,
}

impl Rules {
    pub(crate) const STANDARD: Rules = Rules {
        grades: &[
            GradeRule {
                grade: Grade::Certain,
                needs: &[Need::Identifier],
                base: 100,
                floor: 100,
                ceiling: 100,
            },
            GradeRule {
                grade: Grade::Probable,
                needs: &[Need::Name, Need::BirthDate, Need::Contact],
                base: 85,
                floor: 75,
                ceiling: 95,
            },
            GradeRule {
                grade: Grade::Possible,
                needs: &[Need::Name, Need::BirthDate],
                base: 55,
                floor: 40,
                ceiling: 70,
            },
        ],
        min_name_words: 2,
        swapped_names: true,
        swapped_names_score: -5,
        gender_agrees_score: 5,
        gender_differs_score: -10,
        country_code: "234",
        ignored_identifier_uses: &["temp"],
        ignored_contact_uses: &["old"],
        replaced_by: Some(ReplacedBy {
            max_hops: 5,
            cap: Grade::Probable,
        }),
        max_elements: 50,
        page: 500,
        max_candidates: 20_000,
    };
}

/// What two Patients have in common. What the request says it has in common, in the words the
/// request used: the caller knows which of its own identifiers and numbers it can vouch for.
#[derive(Clone, Debug, Default)]
struct Evidence {
    /// The identifiers of the request, `(system, value)`, that the Patient has too.
    identifiers: Vec<(String, String)>,
    name: Option<NameOrder>,
    birth_date: bool,
    /// The phone numbers and the email addresses of the request, as it wrote them, that the
    /// Patient has too.
    phones: Vec<String>,
    emails: Vec<String>,
    gender: Option<bool>,
    /// The ids of the replaced Patients this was found on, when the Patient is the one that
    /// replaced them, and not itself what the request matched.
    replaced: Vec<String>,
}

/// How the words of two names that are the same line up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NameOrder {
    /// The family words are the same.
    Same,
    /// The family words of one are the given words of the other.
    Swapped,
}

impl Evidence {
    fn has(&self, need: Need) -> bool {
        match need {
            Need::Identifier => !self.identifiers.is_empty(),
            Need::Name => self.name.is_some(),
            Need::BirthDate => self.birth_date,
            Need::Contact => !self.phones.is_empty() || !self.emails.is_empty(),
        }
    }

    /// What the match was found by, one extension for each thing: `replaced-record` first, when
    /// the Patient is not what matched but what replaced it (the value is a `valueReference` to
    /// the replaced one), then `identifier`, `name`,
    /// `name-reordered` (as well as `name`, when the words are the same in another arrangement),
    /// `birth-date`, `phone`, `email`, and for gender `gender` when it agrees and `gender-differs`
    /// when it does not. An identifier, a phone number and an email address carry the one of the
    /// request that matched.
    fn extensions(&self) -> Vec<Value> {
        let item = |kind: &str, value: Option<(&str, Value)>| {
            let mut parts = vec![json!({"url": "kind", "valueCode": kind})];
            if let Some((key, value)) = value {
                let mut part = json!({"url": "value"});
                part[key] = value;
                parts.push(part);
            }
            json!({"url": MATCH_EVIDENCE, "extension": parts})
        };
        let mut items = Vec::new();
        for id in &self.replaced {
            let reference = json!({"reference": format!("Patient/{id}")});
            items.push(item("replaced-record", Some(("valueReference", reference))));
        }
        for (system, value) in &self.identifiers {
            let identifier = json!({"system": system, "value": value});
            items.push(item("identifier", Some(("valueIdentifier", identifier))));
        }
        if self.name.is_some() {
            items.push(item("name", None));
        }
        if self.name == Some(NameOrder::Swapped) {
            items.push(item("name-reordered", None));
        }
        if self.birth_date {
            items.push(item("birth-date", None));
        }
        for (kind, values) in [("phone", &self.phones), ("email", &self.emails)] {
            for value in values {
                let point = json!({"system": kind, "value": value});
                items.push(item(kind, Some(("valueContactPoint", point))));
            }
        }
        match self.gender {
            Some(true) => items.push(item("gender", None)),
            Some(false) => items.push(item("gender-differs", None)),
            None => {}
        }
        items
    }
}

impl Rules {
    /// `grade` and `score`, brought down to `cap` when they are above it: the cap's grade, scored
    /// as high as that grade goes.
    fn capped(&self, grade: Grade, score: i32, cap: Grade) -> (Grade, i32) {
        if grade <= cap {
            return (grade, score);
        }
        match self.grades.iter().find(|rule| rule.grade == cap) {
            Some(rule) => (cap, rule.ceiling),
            None => (grade, score),
        }
    }

    /// The grade and score the evidence earns, or `None` when it is not a match.
    fn grade(&self, evidence: &Evidence) -> Option<(Grade, i32)> {
        let rule = self
            .grades
            .iter()
            .find(|rule| rule.needs.iter().all(|need| evidence.has(*need)))?;
        let mut score = rule.base;
        if evidence.name == Some(NameOrder::Swapped) {
            score += self.swapped_names_score;
        }
        match evidence.gender {
            Some(true) => score += self.gender_agrees_score,
            Some(false) => score += self.gender_differs_score,
            None => {}
        }
        Some((rule.grade, score.clamp(rule.floor, rule.ceiling)))
    }
}

/// The words of a name or the digits of a number as they are compared: what a Patient says about
/// itself, ready to be set against another.
#[derive(Clone, Debug, Default)]
struct Subject {
    /// `(system, value)`, as written.
    identifiers: Vec<(String, String)>,
    names: Vec<Name>,
    /// A full `YYYY-MM-DD` date.
    birth_date: Option<String>,
    /// What each number dials or each address is, and how it was written.
    phones: BTreeMap<String, String>,
    emails: BTreeMap<String, String>,
    gender: Option<String>,
}

#[derive(Clone, Debug)]
struct Name {
    /// Every word of the name, in order of spelling.
    words: Vec<String>,
    /// The words of the family name, in order of spelling.
    family: Vec<String>,
}

impl Subject {
    /// What the Patient says, or why it cannot be read. A malformed element is an error rather
    /// than something left out: the request would be answered for a different person.
    fn read(patient: &Value, rules: &Rules) -> Result<Subject, FhirError> {
        let mut subject = Subject::default();
        for identifier in elements(patient, "identifier")? {
            let system = text(identifier, "system", "Patient.identifier")?;
            let value = text(identifier, "value", "Patient.identifier")?;
            let usage = text(identifier, "use", "Patient.identifier")?;
            if usage.is_some_and(|usage| rules.ignored_identifier_uses.contains(&usage)) {
                continue;
            }
            if let (Some(system), Some(value)) = (system, value) {
                if !system.is_empty() && !value.is_empty() {
                    let pair = (system.to_string(), value.to_string());
                    if !subject.identifiers.contains(&pair) {
                        subject.identifiers.push(pair);
                    }
                }
            }
        }
        for name in elements(patient, "name")? {
            let mut words = Vec::new();
            let mut family = Vec::new();
            if let Some(given) = name.get("given") {
                let given = given
                    .as_array()
                    .ok_or_else(|| invalid("Patient.name.given must be an array of strings"))?;
                for item in given {
                    let item = item
                        .as_str()
                        .ok_or_else(|| invalid("Patient.name.given must be an array of strings"))?;
                    words.extend(words_of(item));
                }
            }
            if let Some(text) = text(name, "family", "Patient.name")? {
                family = words_of(text);
                words.extend(family.iter().cloned());
            }
            if words.is_empty() {
                // A name written out whole has no parts to say which word is the family's.
                if let Some(text) = text(name, "text", "Patient.name")? {
                    words = words_of(text);
                }
            }
            if words.len() >= rules.min_name_words {
                words.sort();
                family.sort();
                subject.names.push(Name { words, family });
            }
        }
        if let Some(date) = text(patient, "birthDate", "Patient")? {
            if !is_date(date) {
                // Not the value: it is a person's date of birth, and error text is logged.
                return Err(invalid(
                    "Patient.birthDate is not a date (YYYY, YYYY-MM or YYYY-MM-DD)",
                ));
            }
            if date.len() == 10 {
                subject.birth_date = Some(date.to_string());
            }
        }
        for point in elements(patient, "telecom")? {
            let system = text(point, "system", "Patient.telecom")?;
            let usage = text(point, "use", "Patient.telecom")?;
            let Some(value) = text(point, "value", "Patient.telecom")? else {
                continue;
            };
            if usage.is_some_and(|usage| rules.ignored_contact_uses.contains(&usage)) {
                continue;
            }
            // A telecom without a system is required to have one, and some do not: the value says
            // what it is.
            let kind = match system {
                Some("phone" | "sms") => Some(false),
                Some("email") => Some(true),
                Some(_) => None,
                None => Some(value.contains('@')),
            };
            match kind {
                Some(true) => {
                    if let Some(key) = email_key(value) {
                        subject
                            .emails
                            .entry(key)
                            .or_insert_with(|| value.to_string());
                    }
                }
                Some(false) => {
                    if let Some(key) = phone_key(value, rules.country_code) {
                        subject
                            .phones
                            .entry(key)
                            .or_insert_with(|| value.to_string());
                    }
                }
                None => {}
            }
        }
        subject.gender = text(patient, "gender", "Patient")?.map(str::to_string);
        Ok(subject)
    }

    /// Whether this Patient can be looked up by name and birth date.
    fn can_match_by_name(&self) -> bool {
        self.birth_date.is_some() && !self.names.is_empty()
    }

    /// Whether there is anything at all to look for.
    fn can_match(&self) -> bool {
        !self.identifiers.is_empty() || self.can_match_by_name()
    }

    fn compare(&self, stored: &Subject, rules: &Rules) -> Evidence {
        // What of ours the stored Patient has, written as we wrote it.
        let shared = |ours: &BTreeMap<String, String>, theirs: &BTreeMap<String, String>| {
            ours.iter()
                .filter(|(key, _)| theirs.contains_key(*key))
                .map(|(_, written)| written.clone())
                .collect()
        };
        let mut evidence = Evidence {
            identifiers: self
                .identifiers
                .iter()
                .filter(|pair| stored.identifiers.contains(pair))
                .cloned()
                .collect(),
            birth_date: self.birth_date.is_some() && self.birth_date == stored.birth_date,
            phones: shared(&self.phones, &stored.phones),
            emails: shared(&self.emails, &stored.emails),
            ..Evidence::default()
        };
        for ours in &self.names {
            for theirs in &stored.names {
                if ours.words != theirs.words {
                    continue;
                }
                if ours.family == theirs.family {
                    evidence.name = Some(NameOrder::Same);
                } else if rules.swapped_names && evidence.name.is_none() {
                    evidence.name = Some(NameOrder::Swapped);
                }
            }
        }
        // `unknown` says that nobody knows, which neither agrees with a gender nor differs from it.
        let known = |gender: &Option<String>| gender.clone().filter(|gender| gender != "unknown");
        if let (Some(ours), Some(theirs)) = (known(&self.gender), known(&stored.gender)) {
            evidence.gender = Some(ours == theirs);
        }
        evidence
    }
}

fn invalid(message: impl Into<String>) -> FhirError {
    FhirError::Invalid(message.into())
}

/// The elements of a repeating field of a Patient, none if it is not there.
fn elements<'a>(patient: &'a Value, field: &str) -> Result<&'a [Value], FhirError> {
    let Some(items) = patient.get(field) else {
        return Ok(&[]);
    };
    let items = items
        .as_array()
        .ok_or_else(|| invalid(format!("Patient.{field} must be an array")))?;
    if items.iter().any(|item| !item.is_object()) {
        return Err(invalid(format!("Patient.{field} must hold objects")));
    }
    Ok(items)
}

/// A string field, none if it is not there.
fn text<'a>(object: &'a Value, field: &str, path: &str) -> Result<Option<&'a str>, FhirError> {
    match object.get(field) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(invalid(format!("{path}.{field} must be a string"))),
    }
}

/// The words of a name, lower case, without accents or apostrophes. A hyphen, a full stop and any
/// other mark between words separates them. Letters that do not come apart into a letter and an
/// accent (Hausa `ɗ`, `ƙ`, `ɓ`, the `ø` of other languages) are written with the letter they
/// stand for.
fn words_of(text: &str) -> Vec<String> {
    let lowered = text.to_lowercase();
    let mut words = Vec::new();
    let mut word = String::new();
    for c in lowered.nfd() {
        if is_combining_mark(c) || matches!(c, '\'' | '’' | '‘' | 'ʼ' | 'ʻ' | '`') {
            continue;
        }
        if let Some(plain) = plain_letter(c) {
            word.push_str(plain);
        } else if c.is_alphanumeric() {
            word.push(c);
        } else if !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

fn plain_letter(c: char) -> Option<&'static str> {
    Some(match c {
        'ɓ' => "b",
        'ɗ' | 'đ' | 'ð' => "d",
        'ƙ' => "k",
        'ƴ' => "y",
        'ɛ' => "e",
        'ɔ' | 'ø' => "o",
        'ł' => "l",
        'ı' => "i",
        'ß' => "ss",
        'æ' => "ae",
        'œ' => "oe",
        'þ' => "th",
        _ => return None,
    })
}

/// A phone number as what it dials: the country code, then the number with no zero before it, and
/// no spacing or marks. `+234 803 123 4567`, `0803 123 4567`, `(0)803-123-4567` after `+234`, and
/// `2348031234567` are all `2348031234567`. A number written with a `+` or `00` has its own
/// country code; one written without keeps the digits it has, with `country_code` put before a
/// national number. `None` when there are too few digits to identify anyone.
fn phone_key(raw: &str, country_code: &str) -> Option<String> {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    let mut international = raw.trim_start().starts_with('+');
    let mut digits = digits.as_str();
    if !international {
        if let Some(rest) = digits.strip_prefix("00") {
            international = true;
            digits = rest;
        }
    }
    if digits.len() < 7 {
        return None;
    }
    // After its country code a number may still carry the zero it is dialled with at home.
    let after_code =
        |rest: &str| format!("{country_code}{}", rest.strip_prefix('0').unwrap_or(rest));
    if let Some(rest) = digits.strip_prefix(country_code) {
        // Written without a `+`, a number that begins with the code and is as long as a number with
        // one is that number: no national number begins with the code, because it begins with 0.
        if international || rest.len() >= 9 {
            return Some(after_code(rest));
        }
    }
    if international {
        return Some(digits.to_string());
    }
    if let Some(rest) = digits.strip_prefix('0') {
        return Some(format!("{country_code}{rest}"));
    }
    if digits.len() == 10 {
        // A national number whose zero was left off.
        return Some(format!("{country_code}{digits}"));
    }
    Some(digits.to_string())
}

fn email_key(raw: &str) -> Option<String> {
    let address = raw.trim();
    let address = address.strip_prefix("mailto:").unwrap_or(address);
    let (local, domain) = address.split_once('@')?;
    if local.is_empty() || domain.is_empty() {
        return None;
    }
    Some(address.to_lowercase())
}

/// A FHIR date: `YYYY`, `YYYY-MM` or a real `YYYY-MM-DD`. A date and a time is not one: that is a
/// `dateTime`, and a birth date is a day.
fn is_date(text: &str) -> bool {
    let number = |part: &str, width: usize| {
        (part.len() == width && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse::<u32>().ok())
            .flatten()
    };
    let mut parts = text.split('-');
    let Some(year) = parts.next().and_then(|part| number(part, 4)) else {
        return false;
    };
    let Some(month) = parts.next() else {
        return true;
    };
    let Some(month) = number(month, 2).filter(|month| (1..=12).contains(month)) else {
        return false;
    };
    let Some(day) = parts.next() else {
        return true;
    };
    let longest = u32::from(refhir_fhirpath::days_in_month(year as i32, month as u8));
    parts.next().is_none() && number(day, 2).is_some_and(|day| (1..=longest).contains(&day))
}

/// A request with more identifiers, names or telecoms than `rules` allow is refused, before any of
/// them is read.
fn limit_elements(patient: &Value, rules: &Rules) -> Result<(), FhirError> {
    for field in ["identifier", "name", "telecom"] {
        let count = patient
            .get(field)
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if count > rules.max_elements {
            return Err(invalid(format!(
                "Patient.{field} has {count} entries, and $match compares at most {}",
                rules.max_elements
            )));
        }
    }
    Ok(())
}

/// The request, read.
struct Request {
    subject: Subject,
    only_certain: bool,
    count: Option<usize>,
}

const PARAMETERS: &str = "resource, onlyCertainMatches and count";

impl Request {
    /// `args` is the `Parameters` body of the HTTP request, or the same three parameters as the
    /// members of a plain object.
    fn from_args(args: &Value, rules: &Rules) -> Result<Request, FhirError> {
        let mut resource = None;
        let mut only_certain = None;
        let mut count = None;
        let put = |name: &str, found: &mut Option<Value>, value: Value| {
            if found.replace(value).is_some() {
                return Err(invalid(format!("{name} was given more than once")));
            }
            Ok(())
        };
        if args.get("resourceType").and_then(Value::as_str) == Some("Parameters") {
            let items = match args.get("parameter") {
                None => &[][..],
                Some(items) => items
                    .as_array()
                    .ok_or_else(|| invalid("Parameters.parameter must be an array"))?,
            };
            for item in items {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("every parameter needs a name"))?;
                match name {
                    "resource" => put(
                        name,
                        &mut resource,
                        item.get("resource")
                            .cloned()
                            .ok_or_else(|| invalid("parameter resource needs a resource"))?,
                    )?,
                    "onlyCertainMatches" => put(
                        name,
                        &mut only_certain,
                        item.get("valueBoolean")
                            .cloned()
                            .ok_or_else(|| invalid("onlyCertainMatches needs a valueBoolean"))?,
                    )?,
                    "count" => put(
                        name,
                        &mut count,
                        ["valueInteger", "valuePositiveInt", "valueUnsignedInt"]
                            .iter()
                            .find_map(|key| item.get(*key).cloned())
                            .ok_or_else(|| invalid("count needs a valueInteger"))?,
                    )?,
                    other => {
                        return Err(invalid(format!(
                            "unknown parameter {other}: $match takes {PARAMETERS}"
                        )))
                    }
                }
            }
        } else if let Some(object) = args.as_object() {
            for (name, value) in object {
                match name.as_str() {
                    "resource" => resource = Some(value.clone()),
                    "onlyCertainMatches" => only_certain = Some(value.clone()),
                    "count" => count = Some(value.clone()),
                    // The server's own controls, which a client cannot set.
                    other if other.starts_with('_') => {}
                    other => {
                        return Err(invalid(format!(
                            "unknown parameter {other}: $match takes {PARAMETERS}"
                        )))
                    }
                }
            }
        } else {
            return Err(invalid("the body must be a Parameters resource"));
        }
        let resource = resource.ok_or_else(|| invalid("resource is required"))?;
        if resource.get("resourceType").and_then(Value::as_str) != Some("Patient") {
            return Err(invalid("resource must be a Patient"));
        }
        let only_certain = match only_certain {
            None => false,
            Some(Value::Bool(flag)) => flag,
            Some(_) => return Err(invalid("onlyCertainMatches must be a boolean")),
        };
        let count = match count {
            None => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .and_then(|count| usize::try_from(count).ok())
                    .ok_or_else(|| invalid("count must be a whole number, 0 or more"))?,
            ),
        };
        limit_elements(&resource, rules)?;
        let subject = Subject::read(&resource, rules)?;
        Ok(Request {
            subject,
            only_certain,
            count,
        })
    }
}

/// The operation: the Bundle of Patients that may be the one in `args`.
pub(crate) fn operation(
    tx: &mut Transaction,
    catalog: &Catalog,
    type_name: &str,
    args: &Value,
) -> Result<Value, FhirError> {
    if type_name != "Patient" {
        return Err(FhirError::Unsupported(format!(
            "unsupported operation {type_name}/$match"
        )));
    }
    catalog.deny_response_read(type_name)?;
    let rules = Rules::STANDARD;
    let request = Request::from_args(args, &rules)?;
    find(tx, catalog, &request, &rules)
}

struct Match {
    id: String,
    resource: Value,
    evidence: Evidence,
    grade: Grade,
    /// In hundredths.
    score: i32,
}

fn find(
    tx: &mut Transaction,
    catalog: &Catalog,
    request: &Request,
    rules: &Rules,
) -> Result<Value, FhirError> {
    // By id, so that a Patient reached in two ways is one match, the better of the two.
    let mut found: BTreeMap<String, Match> = BTreeMap::new();
    if request.subject.can_match() {
        let mut candidates = Candidates::of(&request.subject);
        while let Some(page) = candidates.next_page(tx, catalog, rules)? {
            for (id, resource) in page {
                let standing = standing(&resource);
                if standing == Standing::Out
                    || (standing == Standing::Replaced && rules.replaced_by.is_none())
                {
                    continue;
                }
                let Ok(stored) = Subject::read(&resource, rules) else {
                    continue;
                };
                let evidence = request.subject.compare(&stored, rules);
                let Some((grade, score)) = rules.grade(&evidence) else {
                    continue;
                };
                let Some(follow) = rules.replaced_by.filter(|_| standing == Standing::Replaced)
                else {
                    keep(
                        &mut found,
                        Match {
                            id,
                            resource,
                            evidence,
                            grade,
                            score,
                        },
                    );
                    continue;
                };
                // What the request matched is a Patient that another took the place of: the match is
                // the one that took its place, no stronger than the rules let a link make it.
                let (grade, score) = rules.capped(grade, score, follow.cap);
                for (survivor, resource) in survivors(tx, catalog, &id, &resource, follow.max_hops)?
                {
                    // What the Patient that is returned shares with the request, which is not what the
                    // replaced one did: the entry says only what it can vouch for.
                    let mut evidence = match Subject::read(&resource, rules) {
                        Ok(own) => request.subject.compare(&own, rules),
                        Err(_) => Evidence::default(),
                    };
                    evidence.replaced = vec![id.clone()];
                    keep(
                        &mut found,
                        Match {
                            id: survivor,
                            resource,
                            evidence,
                            grade,
                            score,
                        },
                    );
                }
            }
        }
    }
    let mut matches: Vec<Match> = found.into_values().collect();
    matches.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.id.cmp(&right.id))
    });
    if request.only_certain && !(matches.len() == 1 && matches[0].grade == Grade::Certain) {
        matches.clear();
    }
    matches.truncate(request.count.unwrap_or(usize::MAX));
    Ok(bundle(&matches))
}

/// Add `found` to the matches. A Patient already there is kept as the better of the two: the one
/// with the higher score, and when the scores are the same, the one the request matched itself and
/// not one it was reached through. Two replaced Patients that lead to it equally both stay named.
fn keep(matches: &mut BTreeMap<String, Match>, found: Match) {
    let Some(have) = matches.get_mut(&found.id) else {
        matches.insert(found.id.clone(), found);
        return;
    };
    let (have_via, found_via) = (
        !have.evidence.replaced.is_empty(),
        !found.evidence.replaced.is_empty(),
    );
    if found.score > have.score || (found.score == have.score && have_via && !found_via) {
        *have = found;
    } else if found.score == have.score && have_via && found_via {
        have.evidence.replaced.extend(found.evidence.replaced);
        have.evidence.replaced.sort();
        have.evidence.replaced.dedup();
    }
}

fn bundle(matches: &[Match]) -> Value {
    let entries: Vec<Value> = matches
        .iter()
        .map(|found| {
            let mut extensions = vec![json!({"url": MATCH_GRADE, "valueCode": found.grade.code()})];
            extensions.extend(found.evidence.extensions());
            json!({
                "resource": found.resource,
                "search": {
                    "extension": extensions,
                    "mode": "match",
                    "score": f64::from(found.score) / 100.0
                }
            })
        })
        .collect();
    let mut bundle = json!({
        "resourceType": "Bundle",
        "type": "searchset",
        "total": entries.len()
    });
    // FHIR JSON has no empty arrays.
    if !entries.is_empty() {
        bundle["entry"] = Value::Array(entries);
    }
    bundle
}

/// Where a stored Patient stands as the answer to a match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    /// In use: it can be what a new record is filed against.
    InUse,
    /// Another Patient took its place (`link` of type `replaced-by`), whether or not it was also
    /// switched off, and whatever the link points at.
    Replaced,
    /// Switched off (`active` false), and nothing took its place.
    Out,
}

fn standing(patient: &Value) -> Standing {
    let replaced = patient
        .get("link")
        .and_then(Value::as_array)
        .is_some_and(|links| {
            links
                .iter()
                .any(|link| link_type(link) == Some("replaced-by"))
        });
    if replaced {
        Standing::Replaced
    } else if patient.get("active") == Some(&Value::Bool(false)) {
        Standing::Out
    } else {
        Standing::InUse
    }
}

fn link_type(link: &Value) -> Option<&str> {
    link.get("type").and_then(Value::as_str)
}

/// The ids of the Patients in this store that `patient` says took its place. Only a relative
/// `Patient/id` is a Patient in this store: an absolute URL, or an identifier, is not followed.
fn replaced_by_ids(patient: &Value) -> Vec<String> {
    patient
        .get("link")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|link| link_type(link) == Some("replaced-by"))
        .filter_map(|link| link.pointer("/other/reference").and_then(Value::as_str))
        .filter_map(|reference| reference.strip_prefix("Patient/"))
        .filter(|id| !id.is_empty() && !id.contains('/'))
        .map(str::to_string)
        .collect()
}

/// The Patients in use that `replaced` (the Patient with id `own_id`) leads to by `replaced-by`
/// links, none further than `max_hops` links away. A link that leads nowhere ends there: to a
/// Patient that is not stored here or was deleted, that is switched off, or that is not a
/// relative `Patient/id`. A Patient is visited once, so a cycle ends where it began.
fn survivors(
    tx: &mut Transaction,
    catalog: &Catalog,
    own_id: &str,
    replaced: &Value,
    max_hops: usize,
) -> Result<Vec<(String, Value)>, FhirError> {
    let model = catalog.get("Patient")?;
    let mut seen = BTreeSet::from([own_id.to_string()]);
    let mut frontier = vec![replaced.clone()];
    let mut survivors = Vec::new();
    for _ in 0..max_hops {
        let mut next = Vec::new();
        for record in &frontier {
            for id in replaced_by_ids(record) {
                if !seen.insert(id.clone()) {
                    continue;
                }
                let patient = match ops::read(tx, model, &id) {
                    Ok(patient) => patient,
                    // A link to what is not there, or to what is not an id, leads nowhere.
                    Err(FhirError::NotFound(_) | FhirError::Gone(_) | FhirError::Invalid(_)) => {
                        continue
                    }
                    Err(err) => return Err(err),
                };
                match standing(&patient) {
                    Standing::InUse => survivors.push((id, patient)),
                    Standing::Replaced => next.push(patient),
                    Standing::Out => {}
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(survivors)
}

/// The stored Patients worth comparing with the subject: those that have one of its identifiers,
/// and those born on its birth date when it has a name to compare. They are read a page at a time
/// and each is given once, so no more than a page of them is held, whatever the search finds.
struct Candidates {
    /// `(search parameter, value)`, one search each.
    queries: Vec<(&'static str, String)>,
    /// The search being read, and how far into it.
    at: usize,
    offset: usize,
    /// The ids already given, which is also how many there are.
    seen: BTreeSet<String>,
}

impl Candidates {
    fn of(subject: &Subject) -> Candidates {
        let mut queries: Vec<(&'static str, String)> = subject
            .identifiers
            .iter()
            .filter_map(|(system, value)| Some(("identifier", token(system, value)?)))
            .collect();
        if let (true, Some(date)) = (subject.can_match_by_name(), &subject.birth_date) {
            queries.push(("birthdate", date.clone()));
        }
        Candidates {
            queries,
            at: 0,
            offset: 0,
            seen: BTreeSet::new(),
        }
    }

    /// The next Patients not given before, `(id, resource)`, or `None` when there are no more.
    fn next_page(
        &mut self,
        tx: &mut Transaction,
        catalog: &Catalog,
        rules: &Rules,
    ) -> Result<Option<Vec<(String, Value)>>, FhirError> {
        let model = catalog.get("Patient")?;
        // A page of nothing would never reach the end of a search.
        let size = rules.page.max(1);
        while let Some((parameter, value)) = self.queries.get(self.at).cloned() {
            let params = BTreeMap::from([
                (parameter.to_string(), value),
                ("_count".to_string(), size.to_string()),
                ("_offset".to_string(), self.offset.to_string()),
            ]);
            let page = search::search(tx, catalog, model, &params)?;
            let mut read = 0;
            let mut fresh = Vec::new();
            for entry in page
                .get("entry")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if entry.pointer("/search/mode").and_then(Value::as_str) != Some("match") {
                    continue;
                }
                read += 1;
                let Some(resource) = entry.get("resource") else {
                    continue;
                };
                if let Some(id) = resource.get("id").and_then(Value::as_str) {
                    if self.seen.insert(id.to_string()) {
                        fresh.push((id.to_string(), resource.clone()));
                    }
                }
            }
            if self.seen.len() > rules.max_candidates {
                return Err(too_costly(rules));
            }
            if read < size {
                self.at += 1;
                self.offset = 0;
            } else {
                self.offset += read;
            }
            if !fresh.is_empty() {
                return Ok(Some(fresh));
            }
        }
        Ok(None)
    }
}

fn too_costly(rules: &Rules) -> FhirError {
    FhirError::Unprocessable(json!({
        "resourceType": "OperationOutcome",
        "issue": [{
            "severity": "error",
            "code": "too-costly",
            "diagnostics": format!(
                "more than {} Patients share an identifier or a birth date with the resource, \
                 which is more than $match compares; the answer would not be complete",
                rules.max_candidates
            )
        }]
    }))
}

/// `system|value` as a token search reads it, or `None` for one it cannot say. A comma in a
/// search value separates alternatives unless it is escaped, and a backslash next to one cannot be;
/// a `|` in the system or the control character that joins repeated parameters would be cut in
/// two. No identifier in use has any of these.
fn token(system: &str, value: &str) -> Option<String> {
    if system.contains(['|', search::AND]) || value.contains(search::AND) {
        return None;
    }
    if (system.contains(',') || value.contains(','))
        && (system.contains('\\') || value.contains('\\'))
    {
        return None;
    }
    Some(format!("{system}|{value}").replace(',', "\\,"))
}

/// Say on the CapabilityStatement that Patient has the operation.
pub(crate) fn annotate_capability(statement: &mut Value) {
    let Some(resources) = statement
        .pointer_mut("/rest/0/resource")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for resource in resources {
        if resource.get("type").and_then(Value::as_str) != Some("Patient") {
            continue;
        }
        let operation = json!({
            "name": "match",
            "definition": DEFINITION,
            "documentation": "POST [base]/Patient/$match finds the Patients in this server that \
                may be the person the `resource` parameter describes, best first. Each entry has \
                `search.score` (0 to 1) and the `match-grade` extension: `certain` when an \
                identifier has the same system and value; `probable` when the name, the full \
                birth date and a phone number or email address (Patient.telecom) are the same; \
                `possible` when the name and the full birth date are the same. Each entry says what it \
                was found by in `match-evidence` extensions on `search`. A name matches \
                whatever its case, accents and order of words, and a phone number whatever its \
                spacing and whether it has a country code. A Patient that is not active is never \
                returned, and a match on one that another replaced (link type replaced-by) is the \
                Patient that replaced it, probable at the most, with `replaced-record` evidence. With \
                `onlyCertainMatches` true nothing is returned unless exactly one Patient is a \
                potential match and it is certain. `count` limits the entries."
        });
        match resource.get_mut("operation").and_then(Value::as_array_mut) {
            Some(operations) => operations.push(operation),
            None => resource["operation"] = json!([operation]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: Rules = Rules::STANDARD;

    fn subject(patient: Value) -> Subject {
        Subject::read(&patient, &RULES).unwrap()
    }

    fn graded(input: Value, stored: Value) -> Option<(Grade, i32)> {
        RULES.grade(&subject(input).compare(&subject(stored), &RULES))
    }

    fn named(family: &str, given: &[&str]) -> Value {
        json!({"resourceType": "Patient", "name": [{"family": family, "given": given}]})
    }

    #[test]
    fn a_phone_number_is_what_it_dials() {
        let ng = "2348031234567";
        for written in [
            "+234 803 123 4567",
            "+234-803-123-4567",
            "+234 (0) 803 123 4567",
            "0803 123 4567",
            "08031234567",
            "(0803) 123-4567",
            "2348031234567",
            "234 803 123 4567",
            "00234 803 123 4567",
            "803 123 4567",
            " +2348031234567 ",
        ] {
            assert_eq!(phone_key(written, "234").as_deref(), Some(ng), "{written}");
        }
        // Another country's number keeps its own code, and is no Nigerian number.
        assert_eq!(
            phone_key("+44 20 7946 0958", "234").as_deref(),
            Some("442079460958")
        );
        assert_eq!(
            phone_key("0044 20 7946 0958", "234").as_deref(),
            Some("442079460958")
        );
        assert_ne!(phone_key("+44 803 123 4567", "234").as_deref(), Some(ng));
        // A different number is a different number.
        assert_ne!(phone_key("0803 123 4568", "234").as_deref(), Some(ng));
        // Too short to be anyone's.
        assert_eq!(phone_key("123456", "234"), None);
        assert_eq!(phone_key("call me", "234"), None);
        assert_eq!(phone_key("", "234"), None);
    }

    #[test]
    fn an_email_address_is_not_case_sensitive() {
        assert_eq!(
            email_key(" Ada.Okafor@Example.ORG ").as_deref(),
            Some("ada.okafor@example.org")
        );
        assert_eq!(
            email_key("mailto:ada@example.org").as_deref(),
            Some("ada@example.org")
        );
        assert_eq!(email_key("ada"), None);
        assert_eq!(email_key("@example.org"), None);
        assert_eq!(email_key("ada@"), None);
    }

    #[test]
    fn names_are_compared_by_their_words() {
        assert_eq!(words_of("Okafor"), ["okafor"]);
        assert_eq!(words_of("  Ada   Chinwe "), ["ada", "chinwe"]);
        assert_eq!(words_of("O'Brien"), ["obrien"]);
        assert_eq!(words_of("O’Brien"), ["obrien"]);
        assert_eq!(words_of("Okafor-Nwosu"), ["okafor", "nwosu"]);
        assert_eq!(words_of("Adeyemi."), ["adeyemi"]);
        // Accents, and Yoruba's dots and tone marks, which are accents.
        assert_eq!(words_of("José"), ["jose"]);
        assert_eq!(words_of("Ọláwálé"), ["olawale"]);
        assert_eq!(words_of("Adébáyò Ṣọlá"), ["adebayo", "sola"]);
        assert_eq!(words_of("Chidi Ụzọ"), ["chidi", "uzo"]);
        // Letters that are not a letter and an accent.
        assert_eq!(words_of("Ɗanjuma"), ["danjuma"]);
        assert_eq!(words_of("Ƙasim Ɓello"), ["kasim", "bello"]);
        assert_eq!(words_of("Strauß"), ["strauss"]);
        assert_eq!(words_of("İbrahim"), ["ibrahim"]);
        assert!(words_of("  - ").is_empty());
    }

    #[test]
    fn dates_are_real_dates() {
        for good in ["1985", "1985-03", "1985-03-02", "2000-02-29", "1999-12-31"] {
            assert!(is_date(good), "{good}");
        }
        for bad in [
            "",
            "85",
            "1985-3",
            "1985-13",
            "1985-00",
            "1985-03-32",
            "1985-03-00",
            "1900-02-29",
            "2001-02-29",
            "1985-04-31",
            "1985-03-02-01",
            "March 2nd",
            "1985/03/02",
            "1985-03-0x",
        ] {
            assert!(!is_date(bad), "{bad}");
        }
    }

    #[test]
    fn a_shared_identifier_is_certain_whatever_else_differs() {
        let input = json!({
            "resourceType": "Patient",
            "identifier": [{"system": "urn:example:id", "value": "BC-1"}],
            "name": [{"family": "Okafor", "given": ["Ada"]}],
            "birthDate": "1985-03-02"
        });
        let stored = json!({
            "resourceType": "Patient",
            "identifier": [
                {"system": "urn:example:mrn", "value": "77"},
                {"system": "urn:example:id", "value": "BC-1"}
            ],
            "name": [{"family": "Someone", "given": ["Else"]}],
            "birthDate": "1990-01-01"
        });
        assert_eq!(graded(input, stored), Some((Grade::Certain, 100)));
    }

    #[test]
    fn an_identifier_needs_the_same_system_and_the_same_value() {
        let stored = json!({"resourceType": "Patient",
            "identifier": [{"system": "urn:example:id", "value": "BC-1"}]});
        for input in [
            json!({"system": "urn:example:id", "value": "BC-2"}),
            json!({"system": "urn:example:other", "value": "BC-1"}),
            json!({"system": "urn:example:id", "value": "bc-1"}),
            // No system names nothing.
            json!({"value": "BC-1"}),
            json!({"system": "urn:example:id"}),
            // A temporary number is reissued.
            json!({"system": "urn:example:id", "value": "BC-1", "use": "temp"}),
        ] {
            let input = json!({"resourceType": "Patient", "identifier": [input]});
            assert_eq!(graded(input, stored.clone()), None);
        }
        // A temporary number on the stored record is no more evidence than one in the request.
        let temp = json!({"resourceType": "Patient",
            "identifier": [{"system": "urn:example:id", "value": "BC-1", "use": "temp"}]});
        let input = json!({"resourceType": "Patient",
            "identifier": [{"system": "urn:example:id", "value": "BC-1"}]});
        assert_eq!(graded(input, temp), None);
    }

    fn person(telecom: Value) -> Value {
        json!({
            "resourceType": "Patient",
            "name": [{"family": "Okafor", "given": ["Ada"]}],
            "birthDate": "1985-03-02",
            "telecom": telecom
        })
    }

    #[test]
    fn name_birth_date_and_a_phone_or_email_is_probable() {
        let stored = person(json!([
            {"system": "phone", "value": "+234 803 123 4567"},
            {"system": "email", "value": "ada@example.org"}
        ]));
        for telecom in [
            json!([{"system": "phone", "value": "0803 123 4567"}]),
            json!([{"system": "sms", "value": "08031234567"}]),
            json!([{"system": "email", "value": "ADA@example.org"}]),
            // No system: the value says what it is.
            json!([{"value": "08031234567"}]),
            json!([{"value": "Ada@Example.org"}]),
            // One of several.
            json!([{"system": "phone", "value": "0701 000 0000"}, {"system": "email", "value": "ada@example.org"}]),
        ] {
            let (grade, score) = graded(person(telecom.clone()), stored.clone())
                .unwrap_or_else(|| panic!("{telecom}"));
            assert_eq!(grade, Grade::Probable, "{telecom}");
            assert!((75..=95).contains(&score), "{telecom}: {score}");
        }
    }

    #[test]
    fn name_and_birth_date_alone_is_possible_and_a_different_contact_does_not_change_it() {
        let stored = person(json!([{"system": "phone", "value": "0803 123 4567"}]));
        for telecom in [
            json!([]),
            json!([{"system": "phone", "value": "0803 123 4568"}]),
            // Not a way to reach the person.
            json!([{"system": "fax", "value": "08031234567"}]),
            json!([{"system": "url", "value": "08031234567"}]),
            // A number that was given up.
            json!([{"system": "phone", "value": "0803 123 4567", "use": "old"}]),
        ] {
            let (grade, score) = graded(person(telecom.clone()), stored.clone()).unwrap();
            assert_eq!(grade, Grade::Possible, "{telecom}");
            assert!((40..=70).contains(&score), "{telecom}: {score}");
        }
    }

    #[test]
    fn a_contact_of_the_patient_is_not_the_patients_own() {
        // `Patient.contact` is somebody else (a parent, a spouse), who may be the same for two
        // people.
        let mut stored = person(json!([]));
        stored["contact"] = json!([{"telecom": [{"system": "phone", "value": "0803 123 4567"}]}]);
        let input = person(json!([{"system": "phone", "value": "0803 123 4567"}]));
        assert_eq!(graded(input, stored).unwrap().0, Grade::Possible);
    }

    #[test]
    fn a_different_birth_date_is_not_a_match_and_neither_is_an_unknown_one() {
        let stored = person(json!([{"system": "phone", "value": "0803 123 4567"}]));
        for date in [
            json!("1985-03-03"),
            json!("1986-03-02"),
            json!("1985"),
            json!("1985-03"),
        ] {
            let mut input = person(json!([{"system": "phone", "value": "0803 123 4567"}]));
            input["birthDate"] = date.clone();
            assert_eq!(graded(input, stored.clone()), None, "{date}");
        }
        // A stored record that knows only the year is not known to be born on the day.
        let mut vague = stored.clone();
        vague["birthDate"] = json!("1985");
        assert_eq!(graded(stored.clone(), vague), None);
        let mut undated = stored.clone();
        undated.as_object_mut().unwrap().remove("birthDate");
        assert_eq!(graded(stored, undated), None);
    }

    #[test]
    fn names_that_are_the_same_in_another_spelling_match() {
        let stored = json!({"resourceType": "Patient",
            "name": [{"family": "Okafor", "given": ["Ada", "Chinwe"]}], "birthDate": "1985-03-02"});
        for name in [
            json!({"family": "OKAFOR", "given": ["ada", "chinwe"]}),
            json!({"family": "  Okafor ", "given": ["Ada  Chinwe"]}),
            // The given names in another order.
            json!({"family": "Okafor", "given": ["Chinwe", "Ada"]}),
            json!({"family": "Okáfor", "given": ["Àda", "Chínwé"]}),
            json!({"text": "Ada Chinwe Okafor"}),
            // The family name first, as it is often written.
            json!({"family": "Ada", "given": ["Okafor", "Chinwe"]}),
            json!({"text": "Okafor Ada Chinwe"}),
        ] {
            let input =
                json!({"resourceType": "Patient", "name": [name], "birthDate": "1985-03-02"});
            let (grade, _) = graded(input, stored.clone()).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(grade, Grade::Possible, "{name}");
        }
        // Any name of the request against any name of the record.
        let input = json!({"resourceType": "Patient", "birthDate": "1985-03-02", "name": [
            {"family": "Nwosu", "given": ["Ada"]}, {"family": "Okafor", "given": ["Ada", "Chinwe"]}]});
        assert!(graded(input, stored.clone()).is_some());
    }

    #[test]
    fn names_that_are_not_the_same_do_not_match() {
        let stored = json!({"resourceType": "Patient",
            "name": [{"family": "Okafor", "given": ["Ada", "Chinwe"]}], "birthDate": "1985-03-02"});
        for name in [
            // A word missing, a word added, a word different.
            json!({"family": "Okafor", "given": ["Ada"]}),
            json!({"family": "Okafor", "given": ["Ada", "Chinwe", "Grace"]}),
            json!({"family": "Okafo", "given": ["Ada", "Chinwe"]}),
            json!({"family": "Okafor", "given": ["Ada", "Chiamaka"]}),
            // Too little to say anything.
            json!({"family": "Okafor"}),
            json!({"given": ["Ada"]}),
            json!({"text": "Ada"}),
            json!({}),
        ] {
            let input =
                json!({"resourceType": "Patient", "name": [name], "birthDate": "1985-03-02"});
            assert_eq!(graded(input, stored.clone()), None, "{name}");
        }
    }

    #[test]
    fn a_name_in_another_order_scores_a_little_lower_and_can_be_refused() {
        let input = |family: &str, given: &str| {
            json!({"resourceType": "Patient", "birthDate": "1985-03-02",
                "name": [{"family": family, "given": [given]}]})
        };
        let (_, same) = graded(input("Okafor", "Ada"), input("Okafor", "Ada")).unwrap();
        let (_, swapped) = graded(input("Ada", "Okafor"), input("Okafor", "Ada")).unwrap();
        assert!(swapped < same, "{swapped} {same}");
        let strict = Rules {
            swapped_names: false,
            ..RULES
        };
        let evidence =
            subject(input("Ada", "Okafor")).compare(&subject(input("Okafor", "Ada")), &strict);
        assert_eq!(strict.grade(&evidence), None);
        // The exact order still wins when the record holds both.
        let both = json!({"resourceType": "Patient", "birthDate": "1985-03-02", "name": [
            {"family": "Ada", "given": ["Okafor"]}, {"family": "Okafor", "given": ["Ada"]}]});
        let (_, both_score) = graded(input("Okafor", "Ada"), both).unwrap();
        assert_eq!(both_score, same);
    }

    #[test]
    fn gender_moves_the_score_inside_the_grade_and_never_decides_it() {
        let with = |gender: Option<&str>| {
            let mut patient = person(json!([]));
            if let Some(gender) = gender {
                patient["gender"] = json!(gender);
            }
            patient
        };
        let score = |input: Option<&str>, stored: Option<&str>| {
            let (grade, score) = graded(with(input), with(stored)).unwrap();
            assert_eq!(grade, Grade::Possible);
            score
        };
        let unknown = score(None, Some("female"));
        assert_eq!(unknown, score(Some("female"), None));
        assert!(score(Some("female"), Some("female")) > unknown);
        assert!(score(Some("male"), Some("female")) < unknown);
        // A grade's range holds the order of the grades whatever the adjustments are.
        for rule in RULES.grades {
            assert!(rule.floor <= rule.base && rule.base <= rule.ceiling);
        }
        for pair in RULES.grades.windows(2) {
            assert!(pair[0].floor > pair[1].ceiling || pair[0].grade == Grade::Certain);
            assert!(pair[0].grade > pair[1].grade);
        }
    }

    #[test]
    fn a_patient_is_in_use_replaced_or_switched_off() {
        let standing_of = |extra: Value| {
            let mut patient = json!({"resourceType": "Patient"});
            for (field, value) in extra.as_object().unwrap() {
                patient[field] = value.clone();
            }
            standing(&patient)
        };
        assert_eq!(standing_of(json!({})), Standing::InUse);
        assert_eq!(standing_of(json!({"active": true})), Standing::InUse);
        assert_eq!(standing_of(json!({"active": false})), Standing::Out);
        let link = |kind: &str, other: Value| json!([{"other": other, "type": kind}]);
        let local = json!({"reference": "Patient/b"});
        assert_eq!(
            standing_of(json!({"link": link("replaced-by", local.clone())})),
            Standing::Replaced
        );
        // Whatever its `active` says (systems forget to set it), and whatever the link points at.
        assert_eq!(
            standing_of(json!({"active": true, "link": link("replaced-by", local.clone())})),
            Standing::Replaced
        );
        assert_eq!(
            standing_of(json!({"active": false, "link": link("replaced-by", local.clone())})),
            Standing::Replaced
        );
        let elsewhere = json!({"reference": "https://other.example/fhir/Patient/b"});
        assert_eq!(
            standing_of(json!({"link": link("replaced-by", elsewhere)})),
            Standing::Replaced
        );
        for kind in ["replaces", "refer", "seealso"] {
            assert_eq!(
                standing_of(json!({"link": link(kind, local.clone())})),
                Standing::InUse,
                "{kind}"
            );
        }
    }

    #[test]
    fn only_a_relative_patient_reference_is_a_link_to_follow() {
        let ids =
            |links: Value| replaced_by_ids(&json!({"resourceType": "Patient", "link": links}));
        let by = |kind: &str, reference: Value| json!({"type": kind, "other": reference});
        assert_eq!(
            ids(json!([
                by("replaced-by", json!({"reference": "Patient/b"})),
                by("replaced-by", json!({"reference": "Patient/c"})),
                by("replaces", json!({"reference": "Patient/d"})),
                by("refer", json!({"reference": "Patient/e"})),
            ])),
            ["b", "c"]
        );
        for not_followed in [
            json!({"reference": "https://other.example/fhir/Patient/b"}),
            json!({"reference": "Patient/"}),
            json!({"reference": "Patient/b/_history/1"}),
            json!({"reference": "RelatedPerson/b"}),
            json!({"reference": "#b"}),
            json!({"identifier": {"system": "urn:x", "value": "1"}}),
            json!({}),
        ] {
            assert!(
                ids(json!([by("replaced-by", not_followed.clone())])).is_empty(),
                "{not_followed}"
            );
        }
        assert!(replaced_by_ids(&json!({"resourceType": "Patient"})).is_empty());
    }

    #[test]
    fn a_grade_is_brought_down_to_the_cap_and_no_further() {
        // Certain becomes probable, as high as probable goes. The rest is as it was.
        assert_eq!(
            RULES.capped(Grade::Certain, 100, Grade::Probable),
            (Grade::Probable, 95)
        );
        assert_eq!(
            RULES.capped(Grade::Probable, 85, Grade::Probable),
            (Grade::Probable, 85)
        );
        assert_eq!(
            RULES.capped(Grade::Possible, 55, Grade::Probable),
            (Grade::Possible, 55)
        );
        assert_eq!(
            RULES.capped(Grade::Probable, 85, Grade::Possible),
            (Grade::Possible, 70)
        );
        // A grade the rules do not have cannot be capped to.
        let none = Rules {
            grades: &[],
            ..RULES
        };
        assert_eq!(
            none.capped(Grade::Certain, 100, Grade::Probable),
            (Grade::Certain, 100)
        );
    }

    #[test]
    fn a_request_is_read_from_parameters_or_from_a_plain_object() {
        let patient = named("Okafor", &["Ada"]);
        let from_parameters = Request::from_args(
            &json!({"resourceType": "Parameters", "parameter": [
                {"name": "resource", "resource": patient},
                {"name": "onlyCertainMatches", "valueBoolean": true},
                {"name": "count", "valueInteger": 3}
            ]}),
            &RULES,
        )
        .unwrap();
        let from_object = Request::from_args(
            &json!({"resource": patient, "onlyCertainMatches": true, "count": 3,
                "_readScopes": ["user/*.*"]}),
            &RULES,
        )
        .unwrap();
        for request in [from_parameters, from_object] {
            assert!(request.only_certain);
            assert_eq!(request.count, Some(3));
        }
        let bare = Request::from_args(
            &json!({"resourceType": "Parameters", "parameter": [{"name": "resource", "resource": patient}]}),
            &RULES,
        )
        .unwrap();
        assert!(!bare.only_certain);
        assert_eq!(bare.count, None);
    }

    #[test]
    fn a_request_that_cannot_be_read_is_refused() {
        let patient = named("Okafor", &["Ada"]);
        let parameters = |items: Value| json!({"resourceType": "Parameters", "parameter": items});
        let resource = json!({"name": "resource", "resource": patient});
        let cases = [
            ("no resource", parameters(json!([]))),
            ("no parameter", json!({"resourceType": "Parameters"})),
            (
                "not a Patient",
                parameters(json!([{"name": "resource", "resource": {"resourceType": "Person"}}])),
            ),
            (
                "no resourceType",
                parameters(json!([{"name": "resource", "resource": {"name": []}}])),
            ),
            (
                "resource as a string",
                parameters(json!([{"name": "resource", "valueString": "x"}])),
            ),
            ("twice", parameters(json!([resource, resource]))),
            (
                "a typo",
                parameters(json!([resource, {"name": "onlyCertainMatch", "valueBoolean": true}])),
            ),
            (
                "not a boolean",
                parameters(
                    json!([resource, {"name": "onlyCertainMatches", "valueString": "true"}]),
                ),
            ),
            (
                "no boolean",
                parameters(json!([resource, {"name": "onlyCertainMatches"}])),
            ),
            (
                "a negative count",
                parameters(json!([resource, {"name": "count", "valueInteger": -1}])),
            ),
            (
                "a count with a fraction",
                parameters(json!([resource, {"name": "count", "valueDecimal": 1.5}])),
            ),
            (
                "a count as a string",
                parameters(json!([resource, {"name": "count", "valueString": "2"}])),
            ),
            (
                "parameter not an array",
                json!({"resourceType": "Parameters", "parameter": {}}),
            ),
            ("nameless", parameters(json!([{"valueBoolean": true}]))),
            ("an array", json!([])),
            ("a string", json!("Patient")),
            (
                "a plain typo",
                json!({"resource": patient, "onlyCertain": true}),
            ),
            (
                "a bad name",
                parameters(
                    json!([{"name": "resource", "resource": {"resourceType": "Patient", "name": "Ada Okafor"}}]),
                ),
            ),
            (
                "a bad given",
                parameters(
                    json!([{"name": "resource", "resource": {"resourceType": "Patient", "name": [{"given": "Ada"}]}}]),
                ),
            ),
            (
                "a bad given item",
                parameters(
                    json!([{"name": "resource", "resource": {"resourceType": "Patient", "name": [{"given": [1]}]}}]),
                ),
            ),
            (
                "a bad family",
                parameters(
                    json!([{"name": "resource", "resource": {"resourceType": "Patient", "name": [{"family": ["Okafor"]}]}}]),
                ),
            ),
            (
                "a bad date",
                parameters(
                    json!([{"name": "resource", "resource": {"resourceType": "Patient", "birthDate": "2nd March"}}]),
                ),
            ),
            (
                "an impossible date",
                parameters(
                    json!([{"name": "resource", "resource": {"resourceType": "Patient", "birthDate": "1985-02-30"}}]),
                ),
            ),
            (
                "a numeric date",
                parameters(
                    json!([{"name": "resource", "resource": {"resourceType": "Patient", "birthDate": 19850302}}]),
                ),
            ),
            (
                "a bad identifier",
                parameters(
                    json!([{"name": "resource", "resource": {"resourceType": "Patient", "identifier": {"value": "1"}}}]),
                ),
            ),
            (
                "a bad identifier value",
                parameters(
                    json!([{"name": "resource", "resource": {"resourceType": "Patient", "identifier": [{"system": "urn:x", "value": 1}]}}]),
                ),
            ),
            (
                "a bad telecom",
                parameters(
                    json!([{"name": "resource", "resource": {"resourceType": "Patient", "telecom": ["0803"]}}]),
                ),
            ),
        ];
        for (what, args) in cases {
            match Request::from_args(&args, &RULES) {
                Err(FhirError::Invalid(message)) => assert!(!message.is_empty(), "{what}"),
                other => panic!("{what}: {:?}", other.map(|request| request.count)),
            }
        }
    }

    #[test]
    fn a_patient_with_nothing_to_look_up_has_nothing_to_match() {
        for patient in [
            json!({"resourceType": "Patient"}),
            json!({"resourceType": "Patient", "gender": "female"}),
            // A name and no birth date, a birth date and no name, and only a year.
            named("Okafor", &["Ada"]),
            json!({"resourceType": "Patient", "birthDate": "1985-03-02"}),
            json!({"resourceType": "Patient", "birthDate": "1985", "name": [{"family": "Okafor", "given": ["Ada"]}]}),
            json!({"resourceType": "Patient", "identifier": [{"value": "1"}]}),
        ] {
            assert!(!subject(patient.clone()).can_match(), "{patient}");
        }
        assert!(subject(json!({"resourceType": "Patient",
            "identifier": [{"system": "urn:x", "value": "1"}]}))
        .can_match());
    }

    #[test]
    fn an_identifier_is_a_token_a_search_can_read() {
        assert_eq!(token("urn:x", "BC-1").as_deref(), Some("urn:x|BC-1"));
        // A comma would end the value.
        assert_eq!(token("urn:x", "a,b").as_deref(), Some(r"urn:x|a\,b"));
        // A pipe in the value is part of it: a token has one system.
        assert_eq!(token("urn:x", "a|b").as_deref(), Some("urn:x|a|b"));
        assert_eq!(token("urn:x", r"a\b").as_deref(), Some(r"urn:x|a\b"));
        // What cannot be said is left to the other kinds of evidence.
        assert_eq!(token("urn:x|y", "1"), None);
        assert_eq!(token("urn:x", r"a\,b"), None);
        assert_eq!(token("urn:x", "a\u{1f}b"), None);
    }

    #[test]
    fn the_capability_statement_lists_the_operation_on_patient_only() {
        let mut statement = json!({"rest": [{"resource": [
            {"type": "Observation"},
            {"type": "Patient", "operation": [{"name": "other", "definition": "urn:x"}]},
            {"type": "Encounter"}
        ]}]});
        annotate_capability(&mut statement);
        let resources = statement["rest"][0]["resource"].as_array().unwrap();
        assert!(resources[0].get("operation").is_none());
        assert!(resources[2].get("operation").is_none());
        let operations = resources[1]["operation"].as_array().unwrap();
        assert_eq!(operations.len(), 2);
        assert_eq!(operations[1]["name"], "match");
        assert_eq!(operations[1]["definition"], DEFINITION);
        // A server with no Patient and one with no `rest` are left as they are.
        let mut none = json!({"rest": [{"resource": [{"type": "Observation"}]}]});
        annotate_capability(&mut none);
        assert!(none["rest"][0]["resource"][0].get("operation").is_none());
        let mut bare = json!({});
        annotate_capability(&mut bare);
        assert_eq!(bare, json!({}));
    }

    /// A store with the small Patient package, which has the search parameters the operation
    /// reads, and `count` Patients who share a name and a birth date. Reading is all the tests do.
    fn store_of(count: usize) -> (refhir_database::Database, Catalog) {
        let store = refhir_sqlite::SqlitePersistence::open_in_memory().unwrap();
        let db = refhir_database::Database::open(std::sync::Arc::new(store.clone())).unwrap();
        let catalog = std::sync::RwLock::new(Catalog::empty());
        crate::package::install_package_dir(
            &store,
            &db,
            &catalog,
            crate::package::patient_match_dir(),
        )
        .unwrap();
        let catalog = catalog.into_inner().unwrap();
        for n in 0..count {
            let mut patient = json!({
                "resourceType": "Patient", "id": format!("q{n}"),
                "name": [{"family": "Okafor", "given": ["Ada"]}], "birthDate": "1985-03-02"
            });
            // Every other one has the identifier the request will carry, so they are found
            // twice, by it and by the birth date, and must be counted once.
            if n % 2 == 0 {
                patient["identifier"] = json!([{"system": "urn:example:id", "value": "BC-1"}]);
            }
            crate::run::import_resource(&db, &catalog, &patient).unwrap();
        }
        (db, catalog)
    }

    fn request() -> Value {
        json!({"resource": {
            "resourceType": "Patient",
            "identifier": [{"system": "urn:example:id", "value": "BC-1"}],
            "name": [{"family": "Okafor", "given": ["Ada"]}], "birthDate": "1985-03-02"
        }})
    }

    fn ask(
        db: &refhir_database::Database,
        catalog: &Catalog,
        rules: &Rules,
    ) -> Result<Value, FhirError> {
        let request = Request::from_args(&request(), rules)?;
        find(&mut db.begin(), catalog, &request, rules)
    }

    #[test]
    fn candidates_are_read_a_page_at_a_time_and_each_is_counted_once() {
        let (db, catalog) = store_of(7);
        let rules = Rules { page: 2, ..RULES };
        let answer = ask(&db, &catalog, &rules).unwrap();
        let found: Vec<(&str, &str)> = answer["entry"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["resource"]["id"].as_str().unwrap(),
                    entry["search"]["extension"][0]["valueCode"]
                        .as_str()
                        .unwrap(),
                )
            })
            .collect();
        // Seven Patients over four pages of two, the ones with the identifier first.
        assert_eq!(
            found,
            [
                ("q0", "certain"),
                ("q2", "certain"),
                ("q4", "certain"),
                ("q6", "certain"),
                ("q1", "possible"),
                ("q3", "possible"),
                ("q5", "possible")
            ]
        );
        // The page is the same whatever its size, a page that ends exactly where the matches do
        // included (the search is asked for one more page, and finds none).
        for page in [1, 3, 7, 8, 500] {
            let rules = Rules { page, ..RULES };
            assert_eq!(
                ask(&db, &catalog, &rules).unwrap(),
                answer,
                "page of {page}"
            );
        }
    }

    #[test]
    fn more_candidates_than_it_compares_is_refused_and_not_cut_short() {
        let (db, catalog) = store_of(7);
        let tight = Rules {
            max_candidates: 6,
            page: 2,
            ..RULES
        };
        match ask(&db, &catalog, &tight) {
            Err(FhirError::Unprocessable(outcome)) => {
                assert_eq!(outcome["issue"][0]["code"], "too-costly");
                assert!(outcome["issue"][0]["diagnostics"]
                    .as_str()
                    .unwrap()
                    .contains('6'));
            }
            other => panic!("{other:?}"),
        }
        // Exactly as many as it compares is answered.
        let exact = Rules {
            max_candidates: 7,
            page: 2,
            ..RULES
        };
        assert_eq!(ask(&db, &catalog, &exact).unwrap()["total"], 7);
    }

    #[test]
    fn a_store_without_the_patient_is_an_empty_answer() {
        let (db, catalog) = store_of(0);
        let answer = ask(&db, &catalog, &RULES).unwrap();
        assert_eq!(answer["total"], 0);
        assert!(answer.get("entry").is_none());
    }

    #[test]
    fn it_is_an_operation_of_patient_and_no_other_type() {
        let store = refhir_sqlite::SqlitePersistence::open_in_memory().unwrap();
        let db = refhir_database::Database::open(std::sync::Arc::new(store.clone())).unwrap();
        let catalog = std::sync::RwLock::new(Catalog::empty());
        crate::package::install_package_dir(&store, &db, &catalog, crate::package::minir4_dir())
            .unwrap();
        let catalog = catalog.into_inner().unwrap();
        // Observation is a type this server has, and `$match` is not defined on it.
        let args = json!({"resource": {"resourceType": "Patient"}});
        match operation(&mut db.begin(), &catalog, "Observation", &args) {
            Err(FhirError::Unsupported(message)) => assert!(message.contains("Observation/$match")),
            other => panic!("{other:?}"),
        }
        assert!(operation(&mut db.begin(), &catalog, "Patient", &args).is_ok());
    }

    /// The `(kind, value)` of each evidence extension, the value as `type: text` or `system|value`.
    fn said(evidence: &Evidence) -> Vec<(String, Option<String>)> {
        evidence
            .extensions()
            .iter()
            .map(|item| {
                assert_eq!(item["url"], MATCH_EVIDENCE);
                let parts = item["extension"].as_array().unwrap();
                assert_eq!(parts[0]["url"], "kind");
                let value = parts.get(1).map(|part| {
                    assert_eq!(part["url"], "value");
                    if let Some(identifier) = part.get("valueIdentifier") {
                        format!("{}|{}", identifier["system"], identifier["value"])
                    } else {
                        let point = &part["valueContactPoint"];
                        format!("{}:{}", point["system"], point["value"])
                    }
                });
                (parts[0]["valueCode"].as_str().unwrap().to_string(), value)
            })
            .collect()
    }

    fn told(kind: &str, value: Option<&str>) -> (String, Option<String>) {
        (kind.to_string(), value.map(str::to_string))
    }

    #[test]
    fn what_matched_is_said_in_the_words_of_the_request() {
        let stored = json!({
            "resourceType": "Patient", "gender": "female", "birthDate": "1985-03-02",
            "identifier": [
                {"system": "urn:example:id", "value": "BC-1"},
                {"system": "urn:example:mrn", "value": "77"}
            ],
            "name": [{"family": "Okafor", "given": ["Ada"]}],
            "telecom": [
                {"system": "phone", "value": "+234 803 123 4567"},
                {"system": "email", "value": "ada@example.org"}
            ]
        });
        // Written another way, and with things that are not hers among them.
        let input = json!({
            "resourceType": "Patient", "gender": "male", "birthDate": "1985-03-02",
            "identifier": [
                {"system": "urn:example:id", "value": "BC-1"},
                {"system": "urn:example:id", "value": "BC-2"}
            ],
            "name": [{"family": "Ada", "given": ["OKAFOR"]}],
            "telecom": [
                {"system": "phone", "value": "0701 000 0000"},
                {"system": "phone", "value": "0803 123 4567"},
                {"system": "email", "value": "ADA@example.org"},
                {"system": "email", "value": "other@example.org"}
            ]
        });
        let evidence = subject(input).compare(&subject(stored), &RULES);
        assert_eq!(
            said(&evidence),
            [
                told("identifier", Some("\"urn:example:id\"|\"BC-1\"")),
                told("name", None),
                told("name-reordered", None),
                told("birth-date", None),
                told("phone", Some("\"phone\":\"0803 123 4567\"")),
                told("email", Some("\"email\":\"ADA@example.org\"")),
                told("gender-differs", None),
            ]
        );
    }

    #[test]
    fn nothing_is_said_of_what_did_not_match() {
        let stored = person(json!([{"system": "phone", "value": "0803 123 4567"}]));
        let input = with_gender(
            person(json!([{"system": "phone", "value": "0803 123 4568"}])),
            "female",
        );
        let evidence = subject(input).compare(&subject(stored), &RULES);
        // The name and the birth date, and no phone: this is a possible match.
        assert_eq!(
            said(&evidence),
            [told("name", None), told("birth-date", None)]
        );
        let none = Evidence::default();
        assert!(none.extensions().is_empty());
        // Gender that agrees is said, when both have one.
        let both = subject(with_gender(person(json!([])), "female"))
            .compare(&subject(with_gender(person(json!([])), "female")), &RULES);
        assert_eq!(said(&both).last().unwrap(), &told("gender", None));
    }

    fn with_gender(mut patient: Value, gender: &str) -> Value {
        patient["gender"] = json!(gender);
        patient
    }

    // Patients that replace one another. They all have one name and birth date.

    fn segun(id: &str) -> Value {
        json!({
            "resourceType": "Patient", "id": id,
            "name": [{"family": "Balogun", "given": ["Segun"]}], "birthDate": "1969-06-06"
        })
    }

    /// `segun(id)` with the identifier `BC-9`, and `replaced_by` and `active` as asked.
    fn old_segun(id: &str, replaced_by: Option<&str>, active: bool) -> Value {
        let mut patient = segun(id);
        patient["active"] = json!(active);
        patient["identifier"] = json!([{"system": "urn:example:id", "value": "BC-9"}]);
        if let Some(other) = replaced_by {
            patient["link"] = json!([{"other": {"reference": other}, "type": "replaced-by"}]);
        }
        patient
    }

    /// A store of these Patients, created in order (a reference needs its target to be there).
    fn store_with(
        policy: crate::catalog::ReferencePolicy,
        patients: &[Value],
    ) -> (refhir_database::Database, Catalog) {
        let (db, mut catalog) = store_of(0);
        catalog.set_reference_policy(policy);
        for patient in patients {
            crate::run::import_resource(&db, &catalog, patient)
                .unwrap_or_else(|err| panic!("{patient}: {err}"));
        }
        (db, catalog)
    }

    fn replacing(patients: &[Value]) -> (refhir_database::Database, Catalog) {
        store_with(crate::catalog::ReferencePolicy::Strict, patients)
    }

    /// `$match` for the identifier `BC-9` alone, as the answer's entries: the id, the grade, the
    /// score and what each says it was found by.
    fn by_old_identifier(
        db: &refhir_database::Database,
        catalog: &Catalog,
        rules: &Rules,
        only_certain: bool,
    ) -> Vec<(String, String, f64, Vec<String>)> {
        let args = json!({"onlyCertainMatches": only_certain, "resource": {
            "resourceType": "Patient",
            "identifier": [{"system": "urn:example:id", "value": "BC-9"}]
        }});
        let request = Request::from_args(&args, rules).unwrap();
        let answer = find(&mut db.begin(), catalog, &request, rules).unwrap();
        answer["entry"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|entry| {
                let extensions = entry["search"]["extension"].as_array().unwrap();
                let grade = extensions[0]["valueCode"].as_str().unwrap().to_string();
                let evidence = extensions[1..]
                    .iter()
                    .map(|item| {
                        let parts = item["extension"].as_array().unwrap();
                        let kind = parts[0]["valueCode"].as_str().unwrap();
                        match parts.get(1).and_then(|part| part.get("valueReference")) {
                            Some(reference) => {
                                format!("{kind} {}", reference["reference"].as_str().unwrap())
                            }
                            None => kind.to_string(),
                        }
                    })
                    .collect();
                (
                    entry["resource"]["id"].as_str().unwrap().to_string(),
                    grade,
                    entry["search"]["score"].as_f64().unwrap(),
                    evidence,
                )
            })
            .collect()
    }

    fn ids(found: &[(String, String, f64, Vec<String>)]) -> Vec<&str> {
        found.iter().map(|entry| entry.0.as_str()).collect()
    }

    #[test]
    fn a_match_on_a_replaced_patient_is_the_patient_that_replaced_it_and_never_certain() {
        let (db, catalog) =
            replacing(&[segun("new"), old_segun("old", Some("Patient/new"), false)]);
        let found = by_old_identifier(&db, &catalog, &RULES, false);
        // The old record holds the identifier, so it is certain. It is not the record in use, and
        // what is found is the one that is: probable at the most, and it says which it came from.
        // It does not say `identifier`: that is on the old record, and this one has none.
        assert_eq!(
            found,
            [(
                "new".to_string(),
                "probable".to_string(),
                0.95,
                vec!["replaced-record Patient/old".to_string()]
            )]
        );
        // A match that no one compared with the record that is returned is not one to link on.
        assert!(by_old_identifier(&db, &catalog, &RULES, true).is_empty());
        // Replaced and not switched off is replaced all the same.
        let (db, catalog) = replacing(&[segun("new"), old_segun("old", Some("Patient/new"), true)]);
        assert_eq!(
            ids(&by_old_identifier(&db, &catalog, &RULES, false)),
            ["new"]
        );
    }

    #[test]
    fn what_is_found_through_a_replaced_patient_is_graded_by_the_match_on_it() {
        let (db, catalog) =
            replacing(&[segun("new"), old_segun("old", Some("Patient/new"), false)]);
        let args = |resource: Value| json!({"resource": resource});
        let ask = |resource: Value, rules: &Rules| {
            let request = Request::from_args(&args(resource), rules).unwrap();
            let answer = find(&mut db.begin(), &catalog, &request, rules).unwrap();
            answer["entry"].as_array().cloned().unwrap_or_default()
        };
        // The name and the birth date alone are possible on the old record, and on the new one:
        // the grade the cap allows is not given to a match that did not earn it.
        let by_name = segun("ignored");
        let entries = ask(by_name.clone(), &RULES);
        let new: Vec<_> = entries
            .iter()
            .filter(|entry| entry["resource"]["id"] == "new")
            .collect();
        assert_eq!(new.len(), 1, "one entry for a Patient reached two ways");
        assert_eq!(new[0]["search"]["extension"][0]["valueCode"], "possible");
        assert_eq!(new[0]["search"]["score"], 0.55);
        // A cap of possible brings a certain match down to it.
        let low = Rules {
            replaced_by: Some(ReplacedBy {
                max_hops: 5,
                cap: Grade::Possible,
            }),
            ..RULES
        };
        let by_id = json!({"resourceType": "Patient",
            "identifier": [{"system": "urn:example:id", "value": "BC-9"}]});
        let entries = ask(by_id, &low);
        assert_eq!(
            entries[0]["search"]["extension"][0]["valueCode"],
            "possible"
        );
        assert_eq!(entries[0]["search"]["score"], 0.7);
    }

    #[test]
    fn the_better_of_two_ways_to_a_patient_is_the_one_kept() {
        // The record in use holds the identifier itself: it is certain, and not through the old one.
        let (db, catalog) = replacing(&[
            old_segun("new", None, true),
            old_segun("old", Some("Patient/new"), false),
        ]);
        let found = by_old_identifier(&db, &catalog, &RULES, false);
        assert_eq!(
            found,
            [(
                "new".to_string(),
                "certain".to_string(),
                1.0,
                vec!["identifier".to_string()]
            )]
        );
        // The same Patient is what two replaced ones lead to: it is one match, and says both.
        let (db, catalog) = replacing(&[
            segun("new"),
            old_segun("old-b", Some("Patient/new"), false),
            old_segun("old-a", Some("Patient/new"), false),
        ]);
        let found = by_old_identifier(&db, &catalog, &RULES, false);
        assert_eq!(
            found,
            [(
                "new".to_string(),
                "probable".to_string(),
                0.95,
                vec![
                    "replaced-record Patient/old-a".to_string(),
                    "replaced-record Patient/old-b".to_string(),
                ]
            )]
        );
    }

    #[test]
    fn a_followed_entry_says_only_what_the_patient_it_returns_shares_with_the_request() {
        // The old record has the name, the birth date and the phone number of the request. It was
        // replaced by one that has the name, and another birth date and no phone number: a merge
        // that was wrong, or has since gone stale.
        let mut new = segun("new");
        new["birthDate"] = json!("1970-07-07");
        let mut old = old_segun("old", Some("Patient/new"), false);
        old["telecom"] = json!([{"system": "phone", "value": "0803 123 4567"}]);
        let (db, catalog) = replacing(&[new, old]);
        let request = json!({"resource": {
            "resourceType": "Patient", "birthDate": "1969-06-06",
            "name": [{"family": "Balogun", "given": ["Segun"]}],
            "telecom": [{"system": "phone", "value": "+234 803 123 4567"}]
        }});
        let request = Request::from_args(&request, &RULES).unwrap();
        let answer = find(&mut db.begin(), &catalog, &request, &RULES).unwrap();
        let entry = &answer["entry"][0];
        assert_eq!(entry["resource"]["id"], "new");
        // Probable by the old record, which had all three.
        assert_eq!(entry["search"]["extension"][0]["valueCode"], "probable");
        let kinds: Vec<&str> = entry["search"]["extension"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .map(|item| item["extension"][0]["valueCode"].as_str().unwrap())
            .collect();
        // But the entry does not say that this Patient has the birth date or the phone number.
        assert_eq!(kinds, ["replaced-record", "name"]);
    }

    #[test]
    fn a_chain_of_replacements_is_followed_as_far_as_the_rules_say() {
        let (db, catalog) = replacing(&[
            segun("c"),
            with_link(segun("b"), "Patient/c"),
            old_segun("a", Some("Patient/b"), false),
        ]);
        let within = |max_hops| Rules {
            replaced_by: Some(ReplacedBy {
                max_hops,
                cap: Grade::Probable,
            }),
            ..RULES
        };
        // `a` is replaced by `b`, which is replaced by `c`, which is in use: two links.
        assert_eq!(
            ids(&by_old_identifier(&db, &catalog, &within(2), false)),
            ["c"]
        );
        assert_eq!(ids(&by_old_identifier(&db, &catalog, &RULES, false)), ["c"]);
        assert!(by_old_identifier(&db, &catalog, &within(1), false).is_empty());
        assert!(by_old_identifier(&db, &catalog, &within(0), false).is_empty());
        // The Patient the name and birth date find is `b`, and is followed to `c` too.
        let found = by_old_identifier(&db, &catalog, &RULES, false);
        assert_eq!(found[0].3[0], "replaced-record Patient/a");
    }

    fn with_link(mut patient: Value, other: &str) -> Value {
        patient["link"] = json!([{"other": {"reference": other}, "type": "replaced-by"}]);
        patient
    }

    #[test]
    fn a_replacement_that_comes_back_to_where_it_began_leads_nowhere() {
        let (db, catalog) = replacing(&[segun("y"), old_segun("x", Some("Patient/y"), false)]);
        // Now `y` is replaced by `x`.
        let mut y = with_link(segun("y"), "Patient/x");
        y["active"] = json!(false);
        crate::run::run(
            &db,
            &catalog,
            "Patient/update",
            &json!({"id": "y", "resource": y}),
        )
        .unwrap();
        assert!(by_old_identifier(&db, &catalog, &RULES, false).is_empty());
        // And a Patient that replaces itself.
        let (db, catalog) = replacing(&[old_segun("z", None, false)]);
        crate::run::run(
            &db,
            &catalog,
            "Patient/update",
            &json!({"id": "z", "resource": old_segun("z", Some("Patient/z"), false)}),
        )
        .unwrap();
        assert!(by_old_identifier(&db, &catalog, &RULES, false).is_empty());
    }

    #[test]
    fn a_replacement_that_leads_to_no_patient_in_use_is_not_a_match() {
        // To a Patient that was switched off, without another to take its place.
        let (db, catalog) = replacing(&[
            old_segun("off", None, false),
            old_segun("old", Some("Patient/off"), false),
        ]);
        assert!(by_old_identifier(&db, &catalog, &RULES, false).is_empty());
        // To a Patient that was deleted.
        let (db, catalog) =
            replacing(&[segun("gone"), old_segun("old", Some("Patient/gone"), false)]);
        crate::run::run(&db, &catalog, "Patient/delete", &json!({"id": "gone"})).unwrap();
        assert!(by_old_identifier(&db, &catalog, &RULES, false).is_empty());
        // To a Patient on another server, or to one that is only an identifier.
        let (db, catalog) = store_with(
            crate::catalog::ReferencePolicy::Fhir,
            &[
                old_segun("far", Some("https://other.example/fhir/Patient/new"), false),
                {
                    let mut patient = old_segun("named", None, false);
                    patient["link"] = json!([{"type": "replaced-by",
                        "other": {"identifier": {"system": "urn:x", "value": "1"}}}]);
                    patient
                },
            ],
        );
        assert!(by_old_identifier(&db, &catalog, &RULES, false).is_empty());
    }

    #[test]
    fn a_rule_that_does_not_follow_leaves_a_replaced_patient_out() {
        let (db, catalog) =
            replacing(&[segun("new"), old_segun("old", Some("Patient/new"), false)]);
        let none = Rules {
            replaced_by: None,
            ..RULES
        };
        assert!(by_old_identifier(&db, &catalog, &none, false).is_empty());
    }

    #[test]
    fn a_request_with_more_identifiers_names_or_telecoms_than_allowed_is_refused() {
        let many = |field: &str, count: usize| {
            let element = match field {
                "identifier" => |n: usize| json!({"system": "urn:x", "value": n.to_string()}),
                "name" => |n: usize| json!({"family": format!("Family{n}"), "given": ["Ada"]}),
                _ => |n: usize| json!({"system": "phone", "value": format!("0803{n:07}")}),
            };
            let items: Vec<Value> = (0..count).map(element).collect();
            json!({"resource": {"resourceType": "Patient", field: items}})
        };
        let limit = RULES.max_elements;
        for field in ["identifier", "name", "telecom"] {
            assert!(
                Request::from_args(&many(field, limit), &RULES).is_ok(),
                "{field}"
            );
            match Request::from_args(&many(field, limit + 1), &RULES) {
                Err(FhirError::Invalid(message)) => {
                    assert!(
                        message.contains(field) && message.contains("at most"),
                        "{message}"
                    )
                }
                other => panic!("{field}: {:?}", other.map(|request| request.count)),
            }
        }
    }

    #[test]
    fn a_birth_date_that_is_not_a_date_is_refused_without_saying_what_it_was() {
        let patient = json!({"resource": {"resourceType": "Patient", "birthDate": "02/03/1985"}});
        match Request::from_args(&patient, &RULES) {
            Err(FhirError::Invalid(message)) => {
                assert!(message.contains("birthDate"), "{message}");
                assert!(
                    !message.contains("1985") && !message.contains("02/03"),
                    "{message}"
                );
            }
            other => panic!("{:?}", other.map(|request| request.count)),
        }
    }

    #[test]
    fn a_gender_that_nobody_knows_is_no_evidence_either_way() {
        let with_gender = |gender: &str| {
            let mut patient = person(json!([]));
            patient["gender"] = json!(gender);
            subject(patient)
        };
        let evidence = |ours: &str, theirs: &str| {
            with_gender(ours)
                .compare(&with_gender(theirs), &RULES)
                .gender
        };
        assert_eq!(evidence("female", "female"), Some(true));
        assert_eq!(evidence("female", "male"), Some(false));
        assert_eq!(evidence("female", "other"), Some(false));
        assert_eq!(evidence("other", "other"), Some(true));
        // Unknown on either side, or on both, says nothing.
        assert_eq!(evidence("female", "unknown"), None);
        assert_eq!(evidence("unknown", "female"), None);
        assert_eq!(evidence("unknown", "unknown"), None);
        // So it does not lower the score, and is not said to differ.
        let (_, score) = RULES
            .grade(&with_gender("female").compare(&with_gender("unknown"), &RULES))
            .unwrap();
        let (_, without) = graded(person(json!([])), person(json!([]))).unwrap();
        assert_eq!(score, without);
    }

    #[test]
    fn a_page_of_nothing_still_reaches_the_end() {
        let (db, catalog) = store_of(3);
        let empty = Rules { page: 0, ..RULES };
        // As one at a time: the search is asked for pages of one.
        let one = Rules { page: 1, ..RULES };
        assert_eq!(
            ask(&db, &catalog, &empty).unwrap(),
            ask(&db, &catalog, &one).unwrap()
        );
        assert_eq!(ask(&db, &catalog, &empty).unwrap()["total"], 3);
    }

    #[test]
    fn the_fixture_search_parameters_are_the_ones_the_other_packages_have() {
        // They are copies, so that this package stands alone, and are kept the same as the
        // originals so that these tests do not run against other definitions than the rest.
        let pairs = [
            (
                crate::package::minir4_dir(),
                "SearchParameter-Patient-identifier.json",
            ),
            (
                crate::package::minir4_dir(),
                "SearchParameter-Patient-name.json",
            ),
            (
                crate::package::minir4_dir(),
                "SearchParameter-Patient-active.json",
            ),
            (
                crate::package::extra_dir(),
                "SearchParameter-Patient-birthdate.json",
            ),
        ];
        for (original, file) in pairs {
            assert_eq!(
                std::fs::read_to_string(crate::package::patient_match_dir().join(file)).unwrap(),
                std::fs::read_to_string(original.join(file)).unwrap(),
                "{file}"
            );
        }
    }
}
