use crate::{ImmutableRecord, Storage, decode_object, encode_object, validate_publication};
use parking_lot::RwLock;
use scion_types::{Error, LogEntry, ObjectId, ObjectKind, Result};
use std::collections::BTreeMap;

#[derive(Default)]
struct State {
    objects: BTreeMap<(ObjectKind, ObjectId), Vec<u8>>,
    records: BTreeMap<(String, String), Vec<u8>>,
    logs: BTreeMap<String, Vec<ObjectId>>,
    log_sequences: BTreeMap<(String, ObjectId), u64>,
}

/// Deterministic hermetic storage, useful for embedding and simulations.
#[derive(Default)]
pub struct MemoryStore {
    state: RwLock<State>,
}
impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Storage for MemoryStore {
    fn put_object(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectId> {
        let id = ObjectId::hash(kind, bytes);
        let encoded = encode_object(kind, bytes)?;
        self.state
            .write()
            .objects
            .entry((kind, id))
            .or_insert(encoded);
        Ok(id)
    }
    fn get_object(&self, kind: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        decode_object(kind, id, &self.get_encoded(kind, id)?)
    }
    fn try_get_small_object(
        &self,
        kind: ObjectKind,
        id: ObjectId,
        max_decoded_bytes: usize,
    ) -> Result<Option<Vec<u8>>> {
        let state = self.state.read();
        let encoded = state
            .objects
            .get(&(kind, id))
            .ok_or_else(|| Error::NotFound(format!("{kind:?}/{id}")))?;
        crate::codec::decode_small_object(kind, id, encoded, max_decoded_bytes)
    }
    fn get_encoded(&self, kind: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        self.state
            .read()
            .objects
            .get(&(kind, id))
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("{kind:?}/{id}")))
    }
    fn put_encoded(&self, kind: ObjectKind, id: ObjectId, bytes: &[u8]) -> Result<()> {
        decode_object(kind, id, bytes)?;
        self.state
            .write()
            .objects
            .entry((kind, id))
            .or_insert_with(|| bytes.to_vec());
        Ok(())
    }
    fn get_record(&self, ns: &str, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .state
            .read()
            .records
            .get(&(ns.into(), key.into()))
            .cloned())
    }
    fn get_record_pair(
        &self,
        first: (&str, &str),
        second: (&str, &str),
    ) -> Result<[Option<Vec<u8>>; 2]> {
        let state = self.state.read();
        Ok([
            state
                .records
                .get(&(first.0.into(), first.1.into()))
                .cloned(),
            state
                .records
                .get(&(second.0.into(), second.1.into()))
                .cloned(),
        ])
    }
    fn put_record(&self, ns: &str, key: &str, value: &[u8]) -> Result<()> {
        let mut state = self.state.write();
        let entry = state
            .records
            .entry((ns.into(), key.into()))
            .or_insert_with(|| value.to_vec());
        if entry.as_slice() != value {
            return Err(Error::Conflict(format!(
                "immutable metadata collision: {ns}/{key}"
            )));
        }
        Ok(())
    }
    fn compare_and_swap(
        &self,
        ns: &str,
        key: &str,
        expected: Option<&[u8]>,
        value: Option<&[u8]>,
    ) -> Result<bool> {
        let mut state = self.state.write();
        let map_key = (ns.into(), key.into());
        if state.records.get(&map_key).map(Vec::as_slice) != expected {
            return Ok(false);
        }
        if let Some(value) = value {
            state.records.insert(map_key, value.to_vec());
        } else {
            state.records.remove(&map_key);
        }
        Ok(true)
    }
    fn append_log(&self, repo: &str, id: ObjectId) -> Result<u64> {
        self.publish_commit(repo, id, &[])
    }
    fn publish_commit(&self, repo: &str, id: ObjectId, records: &[ImmutableRecord]) -> Result<u64> {
        validate_publication(repo, id, records)?;
        let mut state = self.state.write();
        let membership = (format!("repo/{repo}/changesets"), id.to_string());
        for (key, value) in records
            .iter()
            .map(|r| ((r.namespace.clone(), r.key.clone()), r.value.as_slice()))
            .chain(std::iter::once((membership.clone(), b"1".as_slice())))
        {
            if state
                .records
                .get(&key)
                .is_some_and(|existing| existing != value)
            {
                return Err(Error::Conflict(format!(
                    "immutable metadata collision: {}/{}",
                    key.0, key.1
                )));
            }
        }
        for record in records {
            state
                .records
                .entry((record.namespace.clone(), record.key.clone()))
                .or_insert_with(|| record.value.clone());
        }
        let log_key = (repo.to_owned(), id);
        let seq = if let Some(sequence) = state.log_sequences.get(&log_key) {
            *sequence
        } else {
            let log = state.logs.entry(repo.into()).or_default();
            log.push(id);
            let sequence = log.len() as u64;
            state.log_sequences.insert(log_key, sequence);
            sequence
        };
        state.records.insert(membership, b"1".to_vec());
        Ok(seq)
    }
    fn read_log(&self, repo: &str, after: u64, limit: usize) -> Result<Vec<LogEntry>> {
        Ok(self
            .state
            .read()
            .logs
            .get(repo)
            .into_iter()
            .flatten()
            .enumerate()
            .filter(|(i, _)| *i as u64 >= after)
            .take(limit)
            .map(|(i, id)| LogEntry {
                sequence: i as u64 + 1,
                id: *id,
            })
            .collect())
    }
    fn scan_records(&self, ns: &str, prefix: &str, limit: usize) -> Result<Vec<(String, Vec<u8>)>> {
        Ok(self
            .state
            .read()
            .records
            .range((ns.into(), prefix.into())..)
            .take_while(|((n, k), _)| n == ns && k.starts_with(prefix))
            .take(limit)
            .map(|((_, k), v)| (k.clone(), v.clone()))
            .collect())
    }
}
