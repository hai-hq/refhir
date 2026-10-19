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
//! **What is never returned.** A Patient with `active` false, and one with a `link` of type
//! `replaced-by`, are not who a new record should be filed against, so they are left out whatever
//! they match. The Patient that replaced one is returned when it matches on its own. The server
//! does not follow the link to it: a merge carries the identifiers across, and a link followed on
//! the server's word would say a person is someone nobody said they were.
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

use std::collections::BTreeMap;

use refhir_database::Transaction;
use serde_json::{json, Value};
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

use crate::catalog::Catalog;
use crate::error::FhirError;
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

    /// What the match was found by, one extension for each thing: `identifier`, `name`,
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
                return Err(invalid(format!(
                    "Patient.birthDate {date:?} is not a date (YYYY, YYYY-MM or YYYY-MM-DD)"
                )));
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
        if let (Some(ours), Some(theirs)) = (&self.gender, &stored.gender) {
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

/// A FHIR date: `YYYY`, `YYYY-MM` or a real `YYYY-MM-DD`.
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
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let longest = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    parts.next().is_none() && number(day, 2).is_some_and(|day| (1..=longest).contains(&day))
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
    let mut matches = Vec::new();
    if request.subject.can_match() {
        for (id, resource) in candidates(tx, catalog, &request.subject, rules)? {
            if !is_target(&resource) {
                continue;
            }
            let Ok(stored) = Subject::read(&resource, rules) else {
                continue;
            };
            let evidence = request.subject.compare(&stored, rules);
            if let Some((grade, score)) = rules.grade(&evidence) {
                matches.push(Match {
                    id,
                    resource,
                    evidence,
                    grade,
                    score,
                });
            }
        }
    }
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

/// Whether a stored Patient can be the one a new record is filed against: it is in use, and no
/// other Patient replaced it.
fn is_target(patient: &Value) -> bool {
    if patient.get("active") == Some(&Value::Bool(false)) {
        return false;
    }
    !patient
        .get("link")
        .and_then(Value::as_array)
        .is_some_and(|links| {
            links
                .iter()
                .any(|link| link.get("type").and_then(Value::as_str) == Some("replaced-by"))
        })
}

/// The stored Patients worth comparing with the subject, by id: those that have one of its
/// identifiers, and those born on its birth date when it has a name to compare.
fn candidates(
    tx: &mut Transaction,
    catalog: &Catalog,
    subject: &Subject,
    rules: &Rules,
) -> Result<BTreeMap<String, Value>, FhirError> {
    let model = catalog.get("Patient")?;
    let mut queries: Vec<(&str, String)> = subject
        .identifiers
        .iter()
        .filter_map(|(system, value)| Some(("identifier", token(system, value)?)))
        .collect();
    if let (true, Some(date)) = (subject.can_match_by_name(), &subject.birth_date) {
        queries.push(("birthdate", date.clone()));
    }
    let mut found = BTreeMap::new();
    for (parameter, value) in queries {
        let mut offset = 0;
        loop {
            let params = BTreeMap::from([
                (parameter.to_string(), value.clone()),
                ("_count".to_string(), rules.page.to_string()),
                ("_offset".to_string(), offset.to_string()),
            ]);
            let page = search::search(tx, catalog, model, &params)?;
            let mut read = 0;
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
                    found
                        .entry(id.to_string())
                        .or_insert_with(|| resource.clone());
                }
            }
            if found.len() > rules.max_candidates {
                return Err(FhirError::Unprocessable(json!({
                    "resourceType": "OperationOutcome",
                    "issue": [{
                        "severity": "error",
                        "code": "too-costly",
                        "diagnostics": format!(
                            "more than {} Patients share an identifier or a birth date with the \
                             resource, which is more than $match compares; the answer would not \
                             be complete",
                            rules.max_candidates
                        )
                    }]
                })));
            }
            if read < rules.page {
                break;
            }
            offset += read;
        }
    }
    Ok(found)
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
                spacing and whether it has a country code. A Patient that is not active, or that \
                another replaced (link type replaced-by), is never returned. With \
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
    fn a_patient_that_is_not_in_use_is_not_a_target() {
        assert!(is_target(&json!({"resourceType": "Patient"})));
        assert!(is_target(
            &json!({"resourceType": "Patient", "active": true})
        ));
        assert!(!is_target(
            &json!({"resourceType": "Patient", "active": false})
        ));
        let link = |kind: &str| json!([{"other": {"reference": "Patient/b"}, "type": kind}]);
        assert!(!is_target(
            &json!({"resourceType": "Patient", "link": link("replaced-by")})
        ));
        // Whatever its `active` says. Systems forget to set it.
        assert!(!is_target(
            &json!({"resourceType": "Patient", "active": true, "link": link("replaced-by")})
        ));
        for kind in ["replaces", "refer", "seealso"] {
            assert!(
                is_target(&json!({"resourceType": "Patient", "link": link(kind)})),
                "{kind}"
            );
        }
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
}
