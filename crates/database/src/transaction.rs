//! In-memory transaction.
//!
//! A transaction has a `begin_ts`, a `ReadSet` and buffered writes. Reads see `begin_ts`
//! plus this transaction's own buffer.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use refhir_persistence::{
    DocumentLogEntry, PersistError, PersistenceIndexEntry, PersistenceReader, TimestampRange,
};
use refhir_types::{exact_key, DocumentId, IndexId, Interval, Order, TabletId, Timestamp};
use serde_json::Value;

use crate::reads::ReadSet;

#[derive(Clone, Debug)]
pub struct Stored {
    /// `None` when this transaction staged the document and it has no commit ts yet.
    pub ts: Option<Timestamp>,
    pub prev_ts: Option<Timestamp>,
    pub json: Value,
}

#[derive(Clone, Debug)]
pub struct Scanned {
    pub key: Vec<u8>,
    pub ts: Option<Timestamp>,
    pub id: DocumentId,
    pub json: Value,
}

/// A row of an index range whose document has not been decoded yet. `P` is what the row carries
/// instead: the document's JSON text (`ScannedText`), or, for a range read without the documents,
/// `Some` JSON text for a document this transaction staged itself and `None` for one that is still
/// in storage (`ScannedEntry`, whose `ts` is `None`: `entry_json` returns the revision's own).
#[derive(Clone, Debug)]
pub struct ScannedRow<P> {
    pub key: Vec<u8>,
    pub ts: Option<Timestamp>,
    pub id: DocumentId,
    payload: P,
}

pub type ScannedText = ScannedRow<String>;
pub type ScannedEntry = ScannedRow<Option<String>>;

impl ScannedRow<String> {
    /// Decodes the document.
    pub fn json(&self) -> Result<Value, PersistError> {
        parse_json(&self.payload)
    }
}

#[derive(Clone, Debug)]
pub struct Revision {
    pub ts: Timestamp,
    pub prev_ts: Option<Timestamp>,
    pub deleted: bool,
    pub json: Option<Value>,
}

pub struct Put {
    pub tablet: TabletId,
    pub id: String,
    pub json: Value,
    pub new_keys: Vec<(IndexId, Vec<u8>)>,
    pub old_keys: Vec<(IndexId, Vec<u8>)>,
    pub prev_ts: Option<Timestamp>,
}

pub struct Delete {
    pub tablet: TabletId,
    pub id: String,
    pub old_keys: Vec<(IndexId, Vec<u8>)>,
    pub prev_ts: Timestamp,
}

/// Index rows for an existing document. No document revision is written.
/// `old_keys` are tombstoned when they are absent from `keys`.
pub struct IndexPut {
    pub tablet: TabletId,
    pub id: String,
    pub keys: Vec<(IndexId, Vec<u8>)>,
    pub old_keys: Vec<(IndexId, Vec<u8>)>,
}

/// A document create, update, or delete staged in this transaction.
#[derive(Clone, Debug)]
pub struct DocumentWrite {
    pub tablet: String,
    pub id: String,
    pub json: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct BufferedWrite {
    pub tablet: TabletId,
    pub id: String,
    pub new_json: Option<String>,
    pub prev_ts: Option<Timestamp>,
    pub new_keys: Vec<(IndexId, Vec<u8>)>,
    pub old_keys: Vec<(IndexId, Vec<u8>)>,
    /// `new_json: None` is a delete. This flag writes index rows and leaves the document alone.
    pub index_only: bool,
}

pub struct Transaction {
    begin_ts: Timestamp,
    reads: ReadSet,
    writes: BTreeMap<(TabletId, String), BufferedWrite>,
    reader: Arc<dyn PersistenceReader>,
}

impl Transaction {
    pub(crate) fn new(begin_ts: Timestamp, reader: Arc<dyn PersistenceReader>) -> Self {
        Self {
            begin_ts,
            reads: ReadSet::default(),
            writes: BTreeMap::new(),
            reader,
        }
    }

    pub fn begin_timestamp(&self) -> Timestamp {
        self.begin_ts
    }

    pub fn read_set(&self) -> &ReadSet {
        &self.reads
    }

    pub fn has_writes(&self) -> bool {
        !self.writes.is_empty()
    }

    pub fn document_writes(&self) -> Vec<DocumentWrite> {
        self.writes
            .values()
            .filter(|write| !write.index_only)
            .map(|write| DocumentWrite {
                tablet: write.tablet.as_str().to_string(),
                id: write.id.clone(),
                json: write.new_json.clone(),
            })
            .collect()
    }

    /// Live keys in `index` at this transaction's snapshot. One entry per key.
    pub fn live_index_keys(
        &mut self,
        index: &IndexId,
    ) -> Result<Vec<(String, Vec<u8>)>, PersistError> {
        self.reads.record(index.clone(), Interval::all());
        let rows = self
            .reader
            .index_scan(index, self.begin_ts, &Interval::all(), Order::Asc)?;
        let mut keys = Vec::new();
        for row in rows {
            let Some(id) = row.document.id.id_str() else {
                continue;
            };
            keys.push((id.to_string(), row.key));
        }
        Ok(keys)
    }

    pub fn written_ids(&self) -> Vec<WrittenId> {
        self.writes
            .values()
            .filter(|write| write.new_json.is_some())
            .map(|write| WrittenId {
                tablet: write.tablet.as_str().to_string(),
                id: write.id.clone(),
            })
            .collect()
    }

    /// Point read on `by_id`. The indexed key is [`exact_key`] of the FHIR id.
    pub fn get(
        &mut self,
        tablet: &TabletId,
        id: &str,
        by_id: &IndexId,
    ) -> Result<Option<Stored>, PersistError> {
        self.reads
            .record(by_id.clone(), Interval::point(exact_key(id)));
        let map_key = (tablet.clone(), id.to_string());
        let index_only = self
            .writes
            .get(&map_key)
            .map(|write| write.index_only)
            .unwrap_or(false);
        if let Some(write) = self.writes.get(&map_key) {
            if !index_only {
                return match &write.new_json {
                    Some(json) => Ok(Some(Stored {
                        ts: None,
                        prev_ts: write.prev_ts,
                        json: parse_json(json)?,
                    })),
                    None => Ok(None),
                };
            }
        }
        let Some(entry) = self.reader.latest_document(
            &DocumentId::new(tablet.clone(), id.as_bytes()),
            self.begin_ts,
        )?
        else {
            return Ok(None);
        };
        let Some(text) = entry.value else {
            return Ok(None);
        };
        Ok(Some(Stored {
            ts: Some(entry.ts),
            prev_ts: entry.prev_ts,
            json: parse_json(&text)?,
        }))
    }

    pub fn index_range(
        &mut self,
        index: &IndexId,
        interval: Interval,
    ) -> Result<Vec<Scanned>, PersistError> {
        self.index_range_text(index, interval)?
            .into_iter()
            .map(|row| {
                Ok(Scanned {
                    json: row.json()?,
                    key: row.key,
                    ts: row.ts,
                    id: row.id,
                })
            })
            .collect()
    }

    /// The rows of `index_range` with each document left as the JSON text the scan returned, so a
    /// caller that rejects or never uses most of them does not pay to decode them.
    pub fn index_range_text(
        &mut self,
        index: &IndexId,
        interval: Interval,
    ) -> Result<Vec<ScannedText>, PersistError> {
        self.reads.record(index.clone(), interval.clone());
        let persisted = self
            .reader
            .index_scan(index, self.begin_ts, &interval, Order::Asc)?
            .into_iter()
            .map(|row| {
                (
                    row.key,
                    Some(row.document.ts),
                    row.document.id,
                    row.document.value,
                )
            })
            .collect();
        Ok(self.merge_scanned(index, &interval, persisted, |json| json.to_string()))
    }

    /// How many intervals the backend reads in one statement (`index_ranges_text` splits a longer
    /// list), so a caller can bound what a list costs in statements.
    pub fn ranges_per_statement(&self) -> usize {
        self.reader.ranges_per_statement()
    }

    /// `index_range_text` for each of `intervals`: the rows of each, in a list of its own, and every
    /// interval recorded as read. Storage is asked for all of them at once where it can be, which
    /// is what a search narrowed to dozens of ranges needs.
    pub fn index_ranges_text(
        &mut self,
        index: &IndexId,
        intervals: &[Interval],
    ) -> Result<Vec<Vec<ScannedText>>, PersistError> {
        for interval in intervals {
            self.reads.record(index.clone(), interval.clone());
        }
        let scanned = self
            .reader
            .index_scan_ranges(index, self.begin_ts, intervals)?;
        // A list too few would drop the last intervals' rows without a sign.
        if scanned.len() != intervals.len() {
            return Err(PersistError::Definite(format!(
                "a read of {} index ranges came back with {} lists",
                intervals.len(),
                scanned.len()
            )));
        }
        Ok(scanned
            .into_iter()
            .zip(intervals)
            .map(|(rows, interval)| {
                let persisted = rows
                    .into_iter()
                    .map(|row| {
                        (
                            row.key,
                            Some(row.document.ts),
                            row.document.id,
                            row.document.value,
                        )
                    })
                    .collect();
                self.merge_scanned(index, interval, persisted, |json| json.to_string())
            })
            .collect())
    }

    /// The same rows, in the same order and recording the same read, without reading any document
    /// from storage. `entry_json` reads the ones a caller turns out to need.
    pub fn index_range_entries(
        &mut self,
        index: &IndexId,
        interval: Interval,
    ) -> Result<Vec<ScannedEntry>, PersistError> {
        self.reads.record(index.clone(), interval.clone());
        let persisted = self
            .reader
            .index_scan_ids(index, self.begin_ts, &interval, Order::Asc, None)?
            .into_iter()
            .map(|row| (row.key, None, row.id, None))
            .collect();
        Ok(self.merge_scanned(index, &interval, persisted, |json| Some(json.to_string())))
    }

    /// The first `wanted` rows of `index_range_entries` and whether the range holds more, reading
    /// no further than that from storage. The read recorded is still the whole range: the result
    /// depends on the rows read, but a change anywhere in the range may change which come first.
    ///
    /// A transaction that has written already merges what it staged with what it reads, which
    /// needs the whole range, so it reads all of it.
    pub fn index_range_first(
        &mut self,
        index: &IndexId,
        interval: Interval,
        wanted: usize,
    ) -> Result<(Vec<ScannedEntry>, bool), PersistError> {
        if self.has_writes() {
            let mut rows = self.index_range_entries(index, interval)?;
            let more = rows.len() > wanted;
            rows.truncate(wanted);
            return Ok((rows, more));
        }
        self.reads.record(index.clone(), interval.clone());
        // One more than wanted shows whether there is more. A document with several keys in the
        // range is several entries but one row, so if the entries run out before the rows do, ask
        // for twice as many.
        let mut limit = wanted.saturating_add(1);
        loop {
            let entries = self.reader.index_scan_ids(
                index,
                self.begin_ts,
                &interval,
                Order::Asc,
                Some(limit),
            )?;
            let exhausted = entries.len() < limit;
            let persisted = entries
                .into_iter()
                .map(|row| (row.key, None, row.id, None))
                .collect();
            let mut rows =
                self.merge_scanned(index, &interval, persisted, |json| Some(json.to_string()));
            if rows.len() > wanted || exhausted {
                let more = rows.len() > wanted;
                rows.truncate(wanted);
                return Ok((rows, more));
            }
            limit = limit.saturating_mul(2);
        }
    }

    /// The document of a row from `index_range_entries`, and the timestamp of the revision read
    /// (`None` for one this transaction staged). A staged document is its staged JSON; any other is
    /// read from the snapshot as `get` reads it. This records no read of its own: the range the
    /// row came from is already in the read set.
    pub fn entry_json(
        &self,
        row: &ScannedEntry,
    ) -> Result<(Value, Option<Timestamp>), PersistError> {
        if let Some(text) = &row.payload {
            return Ok((parse_json(text)?, None));
        }
        let entry = self
            .reader
            .latest_document(&row.id, self.begin_ts)?
            .and_then(|entry| entry.value.map(|text| (entry.ts, text)))
            .ok_or_else(|| {
                PersistError::Definite(format!(
                    "document {} is in an index range but cannot be read at its snapshot",
                    row.id.id_str().unwrap_or("?")
                ))
            })?;
        Ok((parse_json(&entry.1)?, Some(entry.0)))
    }

    /// Rows of `index` inside `interval`: those from storage (minus the documents this
    /// transaction has overwritten or deleted) and those it staged itself, in key order, one row
    /// per document (the first, by key).
    fn merge_scanned<P>(
        &self,
        index: &IndexId,
        interval: &Interval,
        persisted: Vec<(Vec<u8>, Option<Timestamp>, DocumentId, P)>,
        staged: impl Fn(&str) -> P,
    ) -> Vec<ScannedRow<P>> {
        let buffered: BTreeSet<(String, String)> = self
            .writes
            .iter()
            .filter(|(_, write)| !write.index_only)
            .map(|((tablet, id), _)| (tablet.as_str().to_string(), id.clone()))
            .collect();
        let mut rows = Vec::new();
        for (key, ts, id, payload) in persisted {
            let tablet = id.tablet.as_str().to_string();
            let doc_id = id.id_str().unwrap_or_default().to_string();
            if buffered.contains(&(tablet, doc_id)) {
                continue;
            }
            rows.push(ScannedRow {
                key,
                ts,
                id,
                payload,
            });
        }
        for ((tablet, id), write) in &self.writes {
            let Some(json) = &write.new_json else {
                continue;
            };
            for (key_index, key) in &write.new_keys {
                if key_index == index && interval.contains(key) {
                    rows.push(ScannedRow {
                        key: key.clone(),
                        ts: None,
                        id: DocumentId::new(tablet.clone(), id.as_bytes()),
                        payload: staged(json),
                    });
                    break;
                }
            }
        }
        rows.sort_by(|left, right| left.key.cmp(&right.key));
        let mut seen = BTreeSet::new();
        rows.retain(|row| seen.insert((row.id.tablet.as_str().to_string(), row.id.id.clone())));
        rows
    }

    /// Records a `by_id` read so a later revision invalidates history subscribers.
    pub fn history(
        &mut self,
        tablet: &TabletId,
        id: &str,
        by_id: &IndexId,
    ) -> Result<Vec<Revision>, PersistError> {
        self.reads
            .record(by_id.clone(), Interval::point(exact_key(id)));
        let entries = self.reader.document_history(
            &DocumentId::new(tablet.clone(), id.as_bytes()),
            self.begin_ts,
        )?;
        entries.into_iter().map(revision_from_entry).collect()
    }

    /// Document log rows visible at this snapshot, newest first.
    /// `start` is inclusive. Callers record the read set that should wake subscribers.
    pub fn log_since(&self, start: Timestamp) -> Result<Vec<DocumentLogEntry>, PersistError> {
        let end = self.begin_ts.succ().unwrap_or(Timestamp::MAX_PERSISTED);
        self.reader.load_documents(
            TimestampRange {
                start_inclusive: start,
                end_exclusive: end,
            },
            Order::Desc,
        )
    }

    pub fn record_span(&mut self, index: IndexId, interval: Interval) {
        self.reads.record(index, interval);
    }

    /// A historical row does not change, so this does not record a read.
    pub fn exact_revision(
        &self,
        tablet: &TabletId,
        id: &str,
        ts: Timestamp,
    ) -> Result<Option<Revision>, PersistError> {
        if ts > self.begin_ts {
            return Ok(None);
        }
        let Some(entry) = self
            .reader
            .exact_revision(&DocumentId::new(tablet.clone(), id.as_bytes()), ts)?
        else {
            return Ok(None);
        };
        revision_from_entry(entry).map(Some)
    }

    pub fn stage_put(&mut self, put: Put) -> Result<(), PersistError> {
        self.record_keys(&put.new_keys);
        self.record_keys(&put.old_keys);
        let json = serde_json::to_string(&put.json)
            .map_err(|err| PersistError::Definite(format!("json: {err}")))?;
        let map_key = (put.tablet.clone(), put.id.clone());
        if let Some(existing) = self.writes.get_mut(&map_key) {
            existing.index_only = false;
            existing.new_json = Some(json);
            existing.new_keys = put.new_keys;
            return Ok(());
        }
        self.writes.insert(
            map_key,
            BufferedWrite {
                tablet: put.tablet,
                id: put.id,
                new_json: Some(json),
                prev_ts: put.prev_ts,
                new_keys: put.new_keys,
                old_keys: put.old_keys,
                index_only: false,
            },
        );
        Ok(())
    }

    pub fn stage_delete(&mut self, delete: Delete) -> Result<(), PersistError> {
        self.record_keys(&delete.old_keys);
        let map_key = (delete.tablet.clone(), delete.id.clone());
        if let Some(existing) = self.writes.get_mut(&map_key) {
            if existing.prev_ts.is_none() && existing.old_keys.is_empty() {
                self.writes.remove(&map_key);
                return Ok(());
            }
            existing.index_only = false;
            existing.new_json = None;
            existing.new_keys.clear();
            return Ok(());
        }
        self.writes.insert(
            map_key,
            BufferedWrite {
                tablet: delete.tablet,
                id: delete.id,
                new_json: None,
                prev_ts: Some(delete.prev_ts),
                new_keys: Vec::new(),
                old_keys: delete.old_keys,
                index_only: false,
            },
        );
        Ok(())
    }

    pub fn stage_index(&mut self, put: IndexPut) -> Result<(), PersistError> {
        self.record_keys(&put.keys);
        self.record_keys(&put.old_keys);
        let map_key = (put.tablet.clone(), put.id.clone());
        if let Some(existing) = self.writes.get_mut(&map_key) {
            if existing.index_only {
                existing.new_keys.extend(put.keys);
                existing.old_keys.extend(put.old_keys);
            }
            return Ok(());
        }
        self.writes.insert(
            map_key,
            BufferedWrite {
                tablet: put.tablet,
                id: put.id,
                new_json: None,
                prev_ts: None,
                new_keys: put.keys,
                old_keys: put.old_keys,
                index_only: true,
            },
        );
        Ok(())
    }

    pub(crate) fn stamp_new_json(
        &mut self,
        ts: Timestamp,
        stamp: &mut dyn FnMut(&mut String, Timestamp),
    ) {
        for write in self.writes.values_mut() {
            if let Some(json) = write.new_json.as_mut() {
                stamp(json, ts);
            }
        }
    }

    pub(crate) fn occ_keys(&self) -> Vec<(IndexId, Vec<u8>)> {
        let mut keys = Vec::new();
        for write in self.writes.values() {
            keys.extend(diff_keys(write).1);
        }
        keys
    }

    pub(crate) fn log_entries(
        &self,
        ts: Timestamp,
    ) -> (Vec<DocumentLogEntry>, Vec<PersistenceIndexEntry>) {
        let mut documents = Vec::new();
        let mut indexes = Vec::new();
        for write in self.writes.values() {
            if !write.index_only {
                documents.push(DocumentLogEntry {
                    ts,
                    id: DocumentId::new(write.tablet.clone(), write.id.as_bytes()),
                    value: write.new_json.clone(),
                    prev_ts: write.prev_ts,
                });
            }
            for draft in diff_keys(write).0 {
                indexes.push(PersistenceIndexEntry {
                    ts,
                    index_id: draft.index_id,
                    key: draft.key,
                    value: draft.value,
                });
            }
        }
        (documents, indexes)
    }

    fn record_keys(&mut self, keys: &[(IndexId, Vec<u8>)]) {
        for (index, key) in keys {
            self.reads
                .record(index.clone(), Interval::point(key.clone()));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrittenId {
    pub tablet: String,
    pub id: String,
}

struct IndexDraft {
    index_id: IndexId,
    key: Vec<u8>,
    value: Option<DocumentId>,
}

fn diff_keys(write: &BufferedWrite) -> (Vec<IndexDraft>, Vec<(IndexId, Vec<u8>)>) {
    let new_set: BTreeSet<_> = write.new_keys.iter().cloned().collect();
    let old_set: BTreeSet<_> = write.old_keys.iter().cloned().collect();
    let mut drafts = Vec::new();
    let mut occ = Vec::new();
    for (index_id, key) in old_set.difference(&new_set) {
        drafts.push(IndexDraft {
            index_id: index_id.clone(),
            key: key.clone(),
            value: None,
        });
        occ.push((index_id.clone(), key.clone()));
    }
    if write.index_only || write.new_json.is_some() {
        let doc_id = DocumentId::new(write.tablet.clone(), write.id.as_bytes());
        for (index_id, key) in &new_set {
            drafts.push(IndexDraft {
                index_id: index_id.clone(),
                key: key.clone(),
                value: Some(doc_id.clone()),
            });
            occ.push((index_id.clone(), key.clone()));
        }
    }
    (drafts, occ)
}

fn parse_json(text: &str) -> Result<Value, PersistError> {
    serde_json::from_str(text).map_err(|err| PersistError::Definite(format!("json: {err}")))
}

fn revision_from_entry(entry: DocumentLogEntry) -> Result<Revision, PersistError> {
    let json = match entry.value {
        Some(text) => Some(parse_json(&text)?),
        None => None,
    };
    Ok(Revision {
        ts: entry.ts,
        prev_ts: entry.prev_ts,
        deleted: json.is_none(),
        json,
    })
}
