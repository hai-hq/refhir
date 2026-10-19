//! Identifiers and timestamps shared by the engine.

use std::fmt;

/// A value that does not fit the type it was meant for: a timestamp the database cannot store, or
/// bytes that are not a valid name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidValue;

impl fmt::Display for InvalidValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("value out of range")
    }
}

impl std::error::Error for InvalidValue {}

/// Logical commit timestamp, a newtype over `u64`. The committer is the only writer.
/// Zero is the empty snapshot. Commits start at 1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(u64);

impl Timestamp {
    pub const ZERO: Timestamp = Timestamp(0);
    /// Largest timestamp that still fits in a SQLite/Postgres BIGINT.
    pub const MAX_PERSISTED: Timestamp = Timestamp(i64::MAX as u64);

    pub fn from_u64(value: u64) -> Result<Self, InvalidValue> {
        if value > Self::MAX_PERSISTED.0 {
            return Err(InvalidValue);
        }
        Ok(Timestamp(value))
    }

    pub fn from_i64(value: i64) -> Result<Self, InvalidValue> {
        if value < 0 {
            return Err(InvalidValue);
        }
        Ok(Timestamp(value as u64))
    }

    pub fn from_version_id(text: &str) -> Result<Self, InvalidValue> {
        let value: u64 = text.parse().map_err(|_| InvalidValue)?;
        Self::from_u64(value)
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }

    pub fn to_i64(self) -> Result<i64, InvalidValue> {
        i64::try_from(self.0).map_err(|_| InvalidValue)
    }

    /// The next timestamp. Fails when there is none.
    pub fn succ(self) -> Result<Self, InvalidValue> {
        Self::from_u64(self.0.checked_add(1).ok_or(InvalidValue)?)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// FHIR resource type, stored as the `table_id` of a document.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabletId(String);

impl TabletId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, InvalidValue> {
        let name = std::str::from_utf8(bytes).map_err(|_| InvalidValue)?;
        Ok(Self::new(name))
    }
}

impl fmt::Display for TabletId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Index identity: a declared name such as `Patient/by_id`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexId(String);

impl IndexId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, InvalidValue> {
        let name = std::str::from_utf8(bytes).map_err(|_| InvalidValue)?;
        Ok(Self::new(name))
    }
}

impl fmt::Display for IndexId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Document identity: the resource type (tablet) plus the id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocumentId {
    pub tablet: TabletId,
    pub id: Vec<u8>,
}

impl DocumentId {
    pub fn new(tablet: TabletId, id: impl AsRef<[u8]>) -> Self {
        Self {
            tablet,
            id: id.as_ref().to_vec(),
        }
    }

    pub fn id_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.id).ok()
    }
}

/// Scan direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    Asc,
    Desc,
}

/// Interval over index key bytes. Inclusive start, exclusive end. `None` end means
/// unbounded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interval {
    pub start: Vec<u8>,
    pub end: Option<Vec<u8>>,
}

impl Interval {
    pub fn all() -> Self {
        Self {
            start: Vec::new(),
            end: None,
        }
    }

    /// Point read. The exclusive end is `key` plus a trailing 0 byte.
    /// Key encoding must not use a proper prefix of another key on the same index.
    pub fn point(key: Vec<u8>) -> Self {
        let mut end = key.clone();
        end.push(0);
        Self {
            start: key,
            end: Some(end),
        }
    }

    /// Prefix range over raw bytes, for string search parameters.
    pub fn prefix(prefix: Vec<u8>) -> Self {
        let end = prefix_upper_bound(&prefix);
        Self { start: prefix, end }
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        if key < self.start.as_slice() {
            return false;
        }
        match &self.end {
            Some(end) => key < end.as_slice(),
            None => true,
        }
    }
}

fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut bound = prefix.to_vec();
    while let Some(byte) = bound.last_mut() {
        if *byte < 255 {
            *byte += 1;
            return Some(bound);
        }
        bound.pop();
    }
    None
}

/// Whether `id` is a FHIR resource id: one to 64 of letters, digits, `-` and `.`.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.')
}

/// Exact token / id key. Length-prefixed so one key is never a prefix of another.
pub fn exact_key(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(4 + bytes.len());
    let len = bytes.len() as u32;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

/// String search key. Lowercased UTF-8 plus a 0 terminator so prefix scans work.
pub fn prefixable_key(value: &str) -> Vec<u8> {
    let mut out = value.to_lowercase().into_bytes();
    out.push(0);
    out
}

/// Non-unique index key: field encoding, then the document id.
/// The key includes the id so `(index_id, key, ts)` stays unique
/// while a range scan still returns every match.
pub fn token_key(token: &str, doc_id: &str) -> Vec<u8> {
    let mut key = exact_key(token);
    key.push(0);
    key.extend_from_slice(doc_id.as_bytes());
    key
}

pub fn token_interval(token: &str) -> Interval {
    let mut start = exact_key(token);
    start.push(0);
    let mut end = exact_key(token);
    end.push(1);
    Interval {
        start,
        end: Some(end),
    }
}

pub fn string_key(value: &str, doc_id: &str) -> Vec<u8> {
    let mut key = prefixable_key(value);
    key.extend_from_slice(doc_id.as_bytes());
    key
}

pub fn string_interval(prefix: &str) -> Interval {
    Interval::prefix(prefix.to_lowercase().into_bytes())
}

/// Keys at or after this string. The match function still applies the prefix.
pub fn string_at_least(prefix: &str) -> Interval {
    Interval {
        start: prefix.to_lowercase().into_bytes(),
        end: None,
    }
}

/// Keys before or within this string prefix. Exclusive end is the next byte prefix.
pub fn string_at_most(prefix: &str) -> Interval {
    let bytes = prefix.to_lowercase().into_bytes();
    Interval {
        start: Vec::new(),
        end: prefix_upper_bound(&bytes),
    }
}

/// Order-preserving IEEE-754 encoding. Negative numbers sort before positives.
pub fn sortable_f64(value: f64) -> [u8; 8] {
    let bits = value.to_bits();
    let ordered = if bits & (1 << 63) != 0 {
        !bits
    } else {
        bits | (1 << 63)
    };
    ordered.to_be_bytes()
}

pub fn number_key(value: f64, doc_id: &str) -> Vec<u8> {
    let mut key = sortable_f64(value).to_vec();
    key.push(0);
    key.extend_from_slice(doc_id.as_bytes());
    key
}

/// Inclusive `start`, exclusive `end`. `None` is unbounded on that side.
pub fn number_range(start: Option<f64>, end_exclusive: Option<f64>) -> Interval {
    Interval {
        start: start
            .map(|value| sortable_f64(value).to_vec())
            .unwrap_or_default(),
        end: end_exclusive.map(|value| sortable_f64(value).to_vec()),
    }
}

fn quantity_prefix(code: &str) -> Vec<u8> {
    let mut key = exact_key(code);
    key.push(0);
    key
}

fn quantity_prefix_end(code: &str) -> Vec<u8> {
    let mut key = exact_key(code);
    key.push(1);
    key
}

pub fn quantity_key(code: &str, value: f64, doc_id: &str) -> Vec<u8> {
    let mut key = quantity_prefix(code);
    key.extend_from_slice(&sortable_f64(value));
    key.push(0);
    key.extend_from_slice(doc_id.as_bytes());
    key
}

pub fn quantity_range(code: &str, start: Option<f64>, end_exclusive: Option<f64>) -> Interval {
    let mut lower = quantity_prefix(code);
    if let Some(value) = start {
        lower.extend_from_slice(&sortable_f64(value));
    }
    let upper = match end_exclusive {
        Some(value) => {
            let mut end = quantity_prefix(code);
            end.extend_from_slice(&sortable_f64(value));
            end
        }
        None => quantity_prefix_end(code),
    };
    Interval {
        start: lower,
        end: Some(upper),
    }
}

/// Unix milliseconds as an RFC 3339 UTC instant, to the millisecond.
pub fn rfc3339(ms: u64) -> String {
    let secs = ms / 1000;
    let (year, month, day) = civil_from_days(secs / 86_400);
    let rest = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        ms % 1000
    )
}

/// Days since 1970-01-01 as a civil date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_interval_contains_only_that_key() {
        let key = exact_key("abc");
        let interval = Interval::point(key.clone());
        assert!(interval.contains(&key));
        let mut other = key.clone();
        other.push(1);
        assert!(!interval.contains(&other));
        assert!(!interval.contains(&exact_key("abd")));
    }

    #[test]
    fn prefix_interval_matches_string_keys() {
        let interval = string_interval("jo");
        assert!(interval.contains(&string_key("John", "a")));
        assert!(interval.contains(&string_key("jo", "b")));
        assert!(!interval.contains(&string_key("jp", "c")));
        assert_ne!(string_key("John", "a"), string_key("John", "b"));
    }

    #[test]
    fn number_keys_sort_with_the_value() {
        let keys = [-2.0, -0.5, 0.0, 1.0, 10.0].map(|value| number_key(value, "a"));
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        let above = number_range(Some(1.0), None);
        assert!(!above.contains(&number_key(0.5, "a")));
        assert!(above.contains(&number_key(1.0, "a")));
        assert!(above.contains(&number_key(10.0, "b")));
        let below = number_range(None, Some(1.0));
        assert!(below.contains(&number_key(0.5, "a")));
        assert!(!below.contains(&number_key(1.0, "a")));
        let mg = quantity_range("mg", Some(5.0), None);
        assert!(mg.contains(&quantity_key("mg", 5.4, "o1")));
        assert!(!mg.contains(&quantity_key("mg", 4.0, "o1")));
        assert!(!mg.contains(&quantity_key("g", 9.0, "o1")));
    }

    #[test]
    fn a_resource_id_is_up_to_64_letters_digits_dashes_and_dots() {
        assert!(valid_id("p1"));
        assert!(valid_id("a-1.b"));
        assert!(valid_id(&"x".repeat(64)));
        assert!(!valid_id(""));
        assert!(!valid_id(&"x".repeat(65)));
        assert!(!valid_id("Patient/1"));
        assert!(!valid_id("urn:uuid:1"));
        assert!(!valid_id("a b"));
    }

    #[test]
    fn token_interval_includes_every_document_with_that_token() {
        let interval = token_interval("abc");
        assert!(interval.contains(&token_key("abc", "p1")));
        assert!(interval.contains(&token_key("abc", "p2")));
        assert!(!interval.contains(&token_key("abd", "p1")));
    }

    #[test]
    fn instants_are_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(rfc3339(4_102_444_799_999), "2099-12-31T23:59:59.999Z");
    }
}
