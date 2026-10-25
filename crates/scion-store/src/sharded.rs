use crate::{ImmutableRecord, SharedStore, Storage};
use scion_types::{Error, LogEntry, ObjectId, ObjectKind, Result};

/// Independently scale immutable object storage and authoritative metadata.
/// Objects are partitioned by their hash; no object shard needs the full repo.
/// The ordered shard set is a persistent deployment invariant. Rebalancing must
/// copy existing objects before changing the routing configuration.
pub struct ShardedStore {
    metadata: SharedStore,
    shards: Vec<SharedStore>,
}
impl ShardedStore {
    pub fn new(metadata: SharedStore, shards: Vec<SharedStore>) -> Result<Self> {
        if shards.is_empty() {
            return Err(Error::Invalid(
                "at least one object shard is required".into(),
            ));
        }
        Ok(Self { metadata, shards })
    }
    fn shard(&self, id: ObjectId) -> &SharedStore {
        let key = u64::from_le_bytes(id.0[..8].try_into().expect("fixed width identifier"));
        &self.shards[(key % self.shards.len() as u64) as usize]
    }
}
impl Storage for ShardedStore {
    fn put_object(&self, k: ObjectKind, b: &[u8]) -> Result<ObjectId> {
        self.shard(ObjectId::hash(k, b)).put_object(k, b)
    }
    fn get_object(&self, k: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        self.shard(id).get_object(k, id)
    }
    fn try_get_small_object(
        &self,
        kind: ObjectKind,
        id: ObjectId,
        max_decoded_bytes: usize,
    ) -> Result<Option<Vec<u8>>> {
        self.shard(id)
            .try_get_small_object(kind, id, max_decoded_bytes)
    }
    fn get_encoded(&self, k: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        self.shard(id).get_encoded(k, id)
    }
    fn put_encoded(&self, k: ObjectKind, id: ObjectId, b: &[u8]) -> Result<()> {
        self.shard(id).put_encoded(k, id, b)
    }
    fn get_record(&self, n: &str, k: &str) -> Result<Option<Vec<u8>>> {
        self.metadata.get_record(n, k)
    }
    fn get_record_pair(
        &self,
        first: (&str, &str),
        second: (&str, &str),
    ) -> Result<[Option<Vec<u8>>; 2]> {
        self.metadata.get_record_pair(first, second)
    }
    fn put_record(&self, n: &str, k: &str, v: &[u8]) -> Result<()> {
        self.metadata.put_record(n, k, v)
    }
    fn compare_and_swap(
        &self,
        n: &str,
        k: &str,
        e: Option<&[u8]>,
        v: Option<&[u8]>,
    ) -> Result<bool> {
        self.metadata.compare_and_swap(n, k, e, v)
    }
    fn append_log(&self, r: &str, id: ObjectId) -> Result<u64> {
        self.metadata.append_log(r, id)
    }
    fn publish_commit(&self, r: &str, id: ObjectId, records: &[ImmutableRecord]) -> Result<u64> {
        self.metadata.publish_commit(r, id, records)
    }
    fn read_log(&self, r: &str, a: u64, l: usize) -> Result<Vec<LogEntry>> {
        self.metadata.read_log(r, a, l)
    }
    fn scan_records(&self, n: &str, p: &str, l: usize) -> Result<Vec<(String, Vec<u8>)>> {
        self.metadata.scan_records(n, p, l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryStore, PreparedObject};
    use std::sync::Arc;
    #[test]
    fn shard_data_is_not_replicated_to_metadata() {
        let metadata = Arc::new(MemoryStore::new());
        let a = Arc::new(MemoryStore::new());
        let b = Arc::new(MemoryStore::new());
        let s = ShardedStore::new(metadata.clone(), vec![a.clone(), b.clone()]).unwrap();
        for n in 0u64..100 {
            let id = s.put_object(ObjectKind::Blob, &n.to_le_bytes()).unwrap();
            assert!(metadata.get_object(ObjectKind::Blob, id).is_err());
            assert_ne!(
                a.get_object(ObjectKind::Blob, id).is_ok(),
                b.get_object(ObjectKind::Blob, id).is_ok()
            );
            assert_eq!(s.get_object(ObjectKind::Blob, id).unwrap(), n.to_le_bytes());
        }
    }

    #[test]
    fn publication_batch_keeps_objects_out_of_metadata_backend() {
        let metadata = Arc::new(MemoryStore::new());
        let shard = Arc::new(MemoryStore::new());
        let store = ShardedStore::new(metadata.clone(), vec![shard.clone()]).unwrap();
        let objects = [
            PreparedObject::new(ObjectKind::Tree, b"prepared tree".to_vec()).unwrap(),
            PreparedObject::new(ObjectKind::Changeset, b"prepared commit".to_vec()).unwrap(),
        ];
        let id = objects[1].id();
        store
            .publish_commit_with_objects("r", id, &objects, &[])
            .unwrap();
        for object in &objects {
            assert!(metadata.get_object(object.kind(), object.id()).is_err());
            assert_eq!(
                shard.get_object(object.kind(), object.id()).unwrap(),
                object.bytes()
            );
        }
        assert_eq!(metadata.read_log("r", 0, 10).unwrap().len(), 1);
        assert!(shard.read_log("r", 0, 10).unwrap().is_empty());
    }
}
