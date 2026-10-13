//! A hash chain over the audit trail, so that an edit or a deletion in it shows.
//!
//! Each audit row is stored with `hash = H(previous row's hash || the row)`, and with the
//! `prev_hash` it was made from. Change a row and its hash no longer matches; remove or insert
//! one and the next row no longer follows. [`verify_audit_chain`] walks the table and says where.
//!
//! **What it proves depends on the key.** With a [`ChainKey`] the hash is an HMAC, so someone who
//! can write the database but does not have the key cannot make a row whose hash is right, and so
//! cannot rewrite the trail without it showing. With none it is plain SHA-256, which anyone who
//! can write the table can recompute from the start: it then catches mistakes and clumsy
//! edits and nothing more.
//!
//! **What it cannot prove.** Deleting the *newest* rows leaves a chain that is intact, only
//! shorter. The head is therefore reported (and the server logs it as a checkpoint) so that it
//! can be kept somewhere the database's writers cannot reach, and a later verification can be
//! asked whether the trail still contains it.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::{AuditRow, GovernanceStore, PersistError};

/// What the first row of a chain is chained to.
pub const CHAIN_GENESIS: [u8; 32] = [0; 32];

/// The secret that keys the chain. Kept out of the database, and out of `Debug` output.
#[derive(Clone)]
pub struct ChainKey(Vec<u8>);

impl ChainKey {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for ChainKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChainKey(redacted)")
    }
}

fn put_text(message: &mut Vec<u8>, text: &str) {
    message.extend_from_slice(&(text.len() as u32).to_be_bytes());
    message.extend_from_slice(text.as_bytes());
}

fn put_optional(message: &mut Vec<u8>, text: &Option<String>) {
    match text {
        Some(text) => {
            message.push(1);
            put_text(message, text);
        }
        None => message.push(0),
    }
}

/// The hash of `row` chained to `previous`. Every field of the row is in it, each with its
/// length, so that no two rows can be made to hash alike by moving text from one field to
/// another. The `id` is not: the database gives it out, and the chain's own order is what
/// fixes a row's place.
pub fn audit_digest(key: Option<&ChainKey>, previous: &[u8], row: &AuditRow) -> [u8; 32] {
    let mut message = Vec::with_capacity(256);
    message.extend_from_slice(previous);
    message.extend_from_slice(&row.ts_ms.to_be_bytes());
    put_text(&mut message, &row.request_id);
    put_text(&mut message, &row.source);
    put_text(&mut message, &row.action);
    put_text(&mut message, &row.subtype);
    message.push(row.outcome);
    message.extend_from_slice(&row.status.to_be_bytes());
    put_text(&mut message, &row.principal);
    put_optional(&mut message, &row.client_id);
    put_optional(&mut message, &row.user);
    put_optional(&mut message, &row.fhir_user);
    put_optional(&mut message, &row.remote_ip);
    put_optional(&mut message, &row.resource_type);
    put_optional(&mut message, &row.resource_id);
    put_optional(&mut message, &row.patient);
    put_optional(&mut message, &row.query);
    put_optional(&mut message, &row.detail);
    match key {
        Some(key) => {
            let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&key.0)
                .expect("an HMAC accepts a key of any length");
            mac.update(&message);
            mac.finalize().into_bytes().into()
        }
        None => Sha256::digest(&message).into(),
    }
}

/// A stored audit row with the hashes it was chained with. Rows written before the chain began
/// have neither.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainedRow {
    pub row: AuditRow,
    pub prev_hash: Option<Vec<u8>>,
    pub hash: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainProblem {
    /// The row's hash is not what its contents hash to: it was changed, or was made with
    /// another key.
    Edited,
    /// The row was not chained to the one before it: rows between them were removed, or this
    /// one was put in.
    Detached,
    /// A row with no hash after the chain had begun: put in by something that was not the
    /// server.
    Unhashed,
}

impl ChainProblem {
    pub fn as_str(self) -> &'static str {
        match self {
            ChainProblem::Edited => "edited",
            ChainProblem::Detached => "detached",
            ChainProblem::Unhashed => "unhashed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainBreak {
    pub id: u64,
    pub problem: ChainProblem,
}

/// What a verification found about a checkpoint it was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckpointResult {
    /// The row is in the trail with that hash.
    Matches,
    /// A row with that id is in the trail with another hash.
    Different,
    /// No row has that id, and the trail goes past it: it was deleted, or aged out.
    Missing,
    /// The trail ends before that id: the rows the checkpoint covered are gone.
    Truncated,
}

impl CheckpointResult {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckpointResult::Matches => "matches",
            CheckpointResult::Different => "different",
            CheckpointResult::Missing => "missing",
            CheckpointResult::Truncated => "truncated",
        }
    }
}

/// The most breaks a report lists. The count of all of them is kept.
pub const MAX_LISTED_BREAKS: usize = 20;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChainReport {
    /// Rows whose hash was checked.
    pub checked: u64,
    /// Rows with no hash, from before the chain began.
    pub unchained: u64,
    pub first_id: Option<u64>,
    pub last_id: Option<u64>,
    /// The newest hashed row, which is what a checkpoint is.
    pub head: Option<(u64, Vec<u8>)>,
    pub breaks: Vec<ChainBreak>,
    pub breaks_total: u64,
    pub checkpoint: Option<CheckpointResult>,
}

impl ChainReport {
    /// Whether nothing was found wrong, and a checkpoint, if one was given, was found. A trail
    /// in which no row could be checked at all (every hash stripped, or nothing but rows from
    /// before the chain) has had nothing verified, so it is not reported as intact.
    pub fn intact(&self) -> bool {
        self.breaks_total == 0
            && !(self.checked == 0 && self.unchained > 0)
            && self
                .checkpoint
                .is_none_or(|result| result == CheckpointResult::Matches)
    }
}

const PAGE: u32 = 5_000;

/// Walk the audit trail oldest first and check every link. The oldest row kept is taken as the
/// start, since what came before it may have been removed on purpose by retention. With a
/// `checkpoint` (an id and the hash it had), also say whether the trail still holds it.
pub fn verify_audit_chain(
    store: &dyn GovernanceStore,
    key: Option<&ChainKey>,
    checkpoint: Option<(u64, &[u8])>,
) -> Result<ChainReport, PersistError> {
    let mut report = ChainReport::default();
    let mut after = 0;
    let mut previous: Option<Vec<u8>> = None;
    let mut chained = false;
    // Rows with no hash have been seen since the last hashed one, in the stretch before the chain.
    let mut after_unchained = false;
    let mut found: Option<CheckpointResult> = None;
    let note = |report: &mut ChainReport, id, problem| {
        report.breaks_total += 1;
        if report.breaks.len() < MAX_LISTED_BREAKS {
            report.breaks.push(ChainBreak { id, problem });
        }
    };
    loop {
        let rows = store.audit_scan(after, PAGE)?;
        let Some(last) = rows.last() else {
            break;
        };
        after = last.row.id;
        for stored in &rows {
            let id = stored.row.id;
            report.first_id.get_or_insert(id);
            report.last_id = Some(id);
            match &stored.hash {
                None => {
                    if chained {
                        note(&mut report, id, ChainProblem::Unhashed);
                    } else {
                        report.unchained += 1;
                        after_unchained = true;
                    }
                    previous = None;
                }
                Some(hash) => {
                    chained = true;
                    let claimed = stored.prev_hash.as_deref().unwrap_or(&CHAIN_GENESIS);
                    // The server starts a chain from the start when the row before it has no hash,
                    // and retention only ever removes rows from the front, so rows with no hash
                    // can only precede a chain that starts there. One that follows on from a row
                    // that was there once is a trail whose front had its hashes stripped.
                    if std::mem::take(&mut after_unchained) && claimed != CHAIN_GENESIS {
                        note(&mut report, id, ChainProblem::Detached);
                    }
                    if previous
                        .as_deref()
                        .is_some_and(|expected| expected != claimed)
                    {
                        note(&mut report, id, ChainProblem::Detached);
                    }
                    if audit_digest(key, claimed, &stored.row).as_slice() != hash.as_slice() {
                        note(&mut report, id, ChainProblem::Edited);
                    }
                    report.checked += 1;
                    previous = Some(hash.clone());
                }
            }
            if let Some((want, hash)) = checkpoint {
                if id == want {
                    found = Some(if stored.hash.as_deref() == Some(hash) {
                        CheckpointResult::Matches
                    } else {
                        CheckpointResult::Different
                    });
                }
            }
        }
    }
    report.head = store.audit_head()?;
    if let Some((want, _)) = checkpoint {
        report.checkpoint = Some(match found {
            Some(result) => result,
            None if report.last_id.is_none_or(|last| last < want) => CheckpointResult::Truncated,
            None => CheckpointResult::Missing,
        });
    }
    Ok(report)
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> AuditRow {
        AuditRow {
            ts_ms: 1_700_000_000_000,
            request_id: "r".into(),
            source: "http".into(),
            action: "U".into(),
            subtype: "update".into(),
            outcome: 0,
            status: 200,
            principal: "client:a".into(),
            client_id: Some("a".into()),
            resource_type: Some("Patient".into()),
            resource_id: Some("p1".into()),
            ..AuditRow::default()
        }
    }

    #[test]
    fn a_digest_depends_on_every_field_the_chain_before_it_and_the_key() {
        let key = ChainKey::new(b"a key of some length".to_vec());
        let base = audit_digest(Some(&key), &CHAIN_GENESIS, &row());
        assert_eq!(
            base,
            audit_digest(Some(&key), &CHAIN_GENESIS, &row()),
            "deterministic"
        );
        // The id is the database's to give, and is not part of what was recorded.
        let mut with_id = row();
        with_id.id = 99;
        assert_eq!(base, audit_digest(Some(&key), &CHAIN_GENESIS, &with_id));
        let edits: Vec<Box<dyn Fn(&mut AuditRow)>> = vec![
            Box::new(|r| r.ts_ms += 1),
            Box::new(|r| r.request_id.push('x')),
            Box::new(|r| r.source = "sync".into()),
            Box::new(|r| r.action = "D".into()),
            Box::new(|r| r.subtype = "delete".into()),
            Box::new(|r| r.outcome = 4),
            Box::new(|r| r.status = 201),
            Box::new(|r| r.principal = "client:b".into()),
            Box::new(|r| r.client_id = Some("b".into())),
            Box::new(|r| r.user = Some("u".into())),
            Box::new(|r| r.fhir_user = Some("Practitioner/x".into())),
            Box::new(|r| r.remote_ip = Some("10.0.0.1".into())),
            Box::new(|r| r.resource_type = Some("Observation".into())),
            Box::new(|r| r.resource_id = Some("p2".into())),
            Box::new(|r| r.patient = Some("p1".into())),
            Box::new(|r| r.query = Some("name=x".into())),
            Box::new(|r| r.detail = Some("d".into())),
            Box::new(|r| r.client_id = None),
        ];
        for (index, edit) in edits.iter().enumerate() {
            let mut edited = row();
            edit(&mut edited);
            assert_ne!(
                base,
                audit_digest(Some(&key), &CHAIN_GENESIS, &edited),
                "edit {index}"
            );
        }
        assert_ne!(
            base,
            audit_digest(Some(&key), &[1; 32], &row()),
            "the row before it"
        );
        assert_ne!(base, audit_digest(None, &CHAIN_GENESIS, &row()), "the key");
        assert_ne!(
            base,
            audit_digest(
                Some(&ChainKey::new(b"another key entirely".to_vec())),
                &CHAIN_GENESIS,
                &row()
            )
        );
    }

    #[test]
    fn text_moved_between_fields_does_not_make_the_same_hash() {
        let key = None;
        let mut one = row();
        one.client_id = Some("ab".into());
        one.user = Some("c".into());
        let mut two = row();
        two.client_id = Some("a".into());
        two.user = Some("bc".into());
        assert_ne!(
            audit_digest(key, &CHAIN_GENESIS, &one),
            audit_digest(key, &CHAIN_GENESIS, &two)
        );
        // Absent is not empty.
        let mut empty = row();
        empty.user = Some(String::new());
        assert_ne!(
            audit_digest(key, &CHAIN_GENESIS, &row()),
            audit_digest(key, &CHAIN_GENESIS, &empty)
        );
    }

    #[test]
    fn a_key_is_never_printed() {
        let text = format!("{:?}", ChainKey::new(b"super secret value".to_vec()));
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn hex_round_trips_and_refuses_what_is_not_hex() {
        let bytes = vec![0, 1, 0xab, 0xff, 0x10];
        assert_eq!(to_hex(&bytes), "0001abff10");
        assert_eq!(from_hex("0001abff10"), Some(bytes));
        assert_eq!(
            from_hex("0001ABFF10").map(|v| v.len()),
            Some(5),
            "either case"
        );
        for bad in ["abc", "zz", "0x12", "é1"] {
            assert_eq!(from_hex(bad), None, "{bad}");
        }
    }
}
