//! In-memory transaction.
//!
//! Convex `crates/database/src/transaction.rs`: `begin_timestamp`, `ReadSet`,
//! buffered writes. Reads see `begin_ts` plus this transaction's own buffer.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use haifhir_persistence::{
    DocumentLogEntry, PersistError, PersistenceIndexEntry, PersistenceReader,
};
use haifhir_types::{exact_key, DocumentId, IndexId, Interval, Order, TabletId, Timestamp};
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

#[derive(Clone, Debug)]
pub(crate) struct BufferedWrite {
    pub tablet: TabletId,
    pub id: String,
    pub new_json: Option<String>,
    pub prev_ts: Option<Timestamp>,
    pub new_keys: Vec<(IndexId, Vec<u8>)>,
    pub old_keys: Vec<(IndexId, Vec<u8>)>,
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
        if let Some(write) = self.writes.get(&map_key) {
            return match &write.new_json {
                Some(json) => Ok(Some(Stored {
                    ts: None,
                    prev_ts: write.prev_ts,
                    json: parse_json(json)?,
                })),
                None => Ok(None),
            };
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
        self.reads.record(index.clone(), interval.clone());
        let persisted = self
            .reader
            .index_scan(index, self.begin_ts, &interval, Order::Asc)?;
        let buffered: BTreeSet<(String, String)> = self
            .writes
            .keys()
            .map(|(tablet, id)| (tablet.as_str().to_string(), id.clone()))
            .collect();
        let mut rows = Vec::new();
        for row in persisted {
            let tablet = row.document.id.tablet.as_str().to_string();
            let id = row.document.id.id_str().unwrap_or_default().to_string();
            if buffered.contains(&(tablet, id)) {
                continue;
            }
            rows.push(Scanned {
                key: row.key,
                ts: Some(row.document.ts),
                id: row.document.id,
                json: parse_json(&row.document.value)?,
            });
        }
        for ((tablet, id), write) in &self.writes {
            let Some(json) = &write.new_json else {
                continue;
            };
            for (key_index, key) in &write.new_keys {
                if key_index == index && interval.contains(key) {
                    rows.push(Scanned {
                        key: key.clone(),
                        ts: None,
                        id: DocumentId::new(tablet.clone(), id.as_bytes()),
                        json: parse_json(json)?,
                    });
                    break;
                }
            }
        }
        rows.sort_by(|left, right| left.key.cmp(&right.key));
        let mut seen = BTreeSet::new();
        rows.retain(|row| seen.insert((row.id.tablet.as_str().to_string(), row.id.id.clone())));
        Ok(rows)
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
            documents.push(DocumentLogEntry {
                ts,
                id: DocumentId::new(write.tablet.clone(), write.id.as_bytes()),
                value: write.new_json.clone(),
                prev_ts: write.prev_ts,
            });
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
    if write.new_json.is_some() {
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
