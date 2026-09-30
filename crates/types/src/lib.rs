//! Identifiers and timestamps shared by the engine.
//!
//! Convex equivalent: `Timestamp` and document/index ids in `crates/common`.
//! HaiFHIR keeps the same roles and drops packed Convex values.

use std::fmt;

/// Logical commit timestamp. Convex stores this as a newtype over `u64`
/// (`Timestamp` in `crates/common`). The committer is the only writer.
/// Zero is the empty snapshot. Commits start at 1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(u64);

impl Timestamp {
    pub const ZERO: Timestamp = Timestamp(0);
    /// Largest timestamp that still fits in a SQLite/Postgres BIGINT.
    pub const MAX_PERSISTED: Timestamp = Timestamp(i64::MAX as u64);

    pub fn from_u64(value: u64) -> Result<Self, ()> {
        if value > Self::MAX_PERSISTED.0 {
            return Err(());
        }
        Ok(Timestamp(value))
    }

    pub fn from_i64(value: i64) -> Result<Self, ()> {
        if value < 0 {
            return Err(());
        }
        Ok(Timestamp(value as u64))
    }

    pub fn from_version_id(text: &str) -> Result<Self, ()> {
        let value: u64 = text.parse().map_err(|_| ())?;
        Self::from_u64(value)
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }

    pub fn to_i64(self) -> Result<i64, ()> {
        i64::try_from(self.0).map_err(|_| ())
    }

    /// Convex `Timestamp::succ`.
    pub fn succ(self) -> Result<Self, ()> {
        Self::from_u64(self.0.checked_add(1).ok_or(())?)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// FHIR resource type, stored as Convex `table_id` / tablet bytes.
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

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ()> {
        let name = std::str::from_utf8(bytes).map_err(|_| ())?;
        Ok(Self::new(name))
    }
}

impl fmt::Display for TabletId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Index identity. Convex `IndexId`, here a name we declare (`Patient/by_id`).
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

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ()> {
        let name = std::str::from_utf8(bytes).map_err(|_| ())?;
        Ok(Self::new(name))
    }
}

impl fmt::Display for IndexId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Convex `InternalDocumentId`: tablet plus id bytes.
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

/// Scan direction. Convex `Order`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    Asc,
    Desc,
}

/// Inclusive start, exclusive end. `None` end means unbounded.
/// Convex `Interval` over index key bytes.
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
/// Convex index keys include the id so `(index_id, key, ts)` stays unique
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
    fn token_interval_includes_every_document_with_that_token() {
        let interval = token_interval("abc");
        assert!(interval.contains(&token_key("abc", "p1")));
        assert!(interval.contains(&token_key("abc", "p2")));
        assert!(!interval.contains(&token_key("abd", "p1")));
    }
}
