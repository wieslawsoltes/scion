//! Immutable storage and transactional metadata contracts.
use scion_types::{LogEntry, ObjectId, ObjectKind, Result};
use std::sync::Arc;

mod codec;
mod local;
mod memory;
mod sharded;
pub use codec::{MAX_OBJECT_SIZE, decode_object, encode_object};
pub use local::LocalStore;
pub use memory::MemoryStore;
pub use sharded::ShardedStore;

#[cfg(feature = "postgres")]
mod pg;
#[cfg(feature = "postgres")]
pub use pg::PostgresStore;

/// An immutable metadata entry prepared before a changeset is published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImmutableRecord {
    pub namespace: String,
    pub key: String,
    pub value: Vec<u8>,
}

/// Immutable content prepared for a durable publication batch. Construction
/// validates the size and computes its content-addressed identifier; private
/// fields keep backends from accepting caller-supplied, unverified identifiers.
#[derive(Clone, Debug)]
pub struct PreparedObject {
    kind: ObjectKind,
    id: ObjectId,
    bytes: Vec<u8>,
}

impl PreparedObject {
    pub fn new(kind: ObjectKind, bytes: Vec<u8>) -> Result<Self> {
        if bytes.len() > MAX_OBJECT_SIZE {
            return Err(scion_types::Error::Limit("object exceeds 256 MiB".into()));
        }
        Ok(Self {
            kind,
            id: ObjectId::hash(kind, &bytes),
            bytes,
        })
    }
    pub fn kind(&self) -> ObjectKind {
        self.kind
    }
    pub fn id(&self) -> ObjectId {
        self.id
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Validate an immutable publication batch before performing any writes. Custom
/// backends should call this before their transaction to reject conflicting
/// duplicate records and caller-supplied changeset membership entries.
pub fn validate_publication(repo: &str, id: ObjectId, records: &[ImmutableRecord]) -> Result<()> {
    let namespace = format!("repo/{repo}/changesets");
    let key = id.to_string();
    let mut seen = std::collections::BTreeMap::new();
    seen.insert((namespace.as_str(), key.as_str()), b"1".as_slice());
    for record in records {
        if record.namespace == namespace && record.key == key {
            return Err(scion_types::Error::Invalid(
                "changeset membership is managed by publication, not caller-supplied records"
                    .into(),
            ));
        }
        if seen
            .insert((&record.namespace, &record.key), record.value.as_slice())
            .is_some_and(|previous| previous != record.value)
        {
            return Err(scion_types::Error::Conflict(format!(
                "inconsistent publication metadata: {}/{}",
                record.namespace, record.key
            )));
        }
    }
    Ok(())
}

/// Storage backend. Immutable writes must be idempotent, refs must use atomic CAS,
/// and append_log must durably deduplicate IDs per repository.
pub trait Storage: Send + Sync {
    fn put_object(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectId>;
    fn get_object(&self, kind: ObjectKind, id: ObjectId) -> Result<Vec<u8>>;
    fn get_encoded(&self, kind: ObjectKind, id: ObjectId) -> Result<Vec<u8>>;
    fn put_encoded(&self, kind: ObjectKind, id: ObjectId, bytes: &[u8]) -> Result<()>;
    fn get_record(&self, namespace: &str, key: &str) -> Result<Option<Vec<u8>>>;
    /// Insert an immutable record, accepting identical concurrent/repeated writes.
    fn put_record(&self, namespace: &str, key: &str, value: &[u8]) -> Result<()>;
    fn compare_and_swap(
        &self,
        namespace: &str,
        key: &str,
        expected: Option<&[u8]>,
        value: Option<&[u8]>,
    ) -> Result<bool>;
    /// Atomically append/deduplicate the changeset and publish membership at
    /// `repo/{repo}/changesets`, with key `id.to_string()` and value `b"1"`.
    fn append_log(&self, repo: &str, id: ObjectId) -> Result<u64>;
    /// Publish prepared immutable metadata and the changeset's membership/log.
    /// Built-in backends commit the complete batch atomically with one durability
    /// barrier. The fallback persists records first and publishes membership last,
    /// preserving visibility safety for third-party implementations.
    /// Caller-supplied records must not include this changeset's membership key.
    fn publish_commit(&self, repo: &str, id: ObjectId, records: &[ImmutableRecord]) -> Result<u64> {
        validate_publication(repo, id, records)?;
        for record in records {
            self.put_record(&record.namespace, &record.key, &record.value)?;
        }
        self.append_log(repo, id)
    }
    /// Persist prepared immutable content before making a changeset visible.
    /// Backends may combine objects and metadata into one durable transaction.
    /// The default preserves object routing through `self.put_object`, then
    /// publishes metadata/membership with `self.publish_commit`. A failure may
    /// leave unreachable immutable objects, but must never expose membership
    /// before every supplied object and metadata record is durable.
    ///
    /// Previously uploaded blobs retain their independent durability contract;
    /// this method does not defer or weaken acknowledgement of `put_object`.
    fn publish_commit_with_objects(
        &self,
        repo: &str,
        id: ObjectId,
        objects: &[PreparedObject],
        records: &[ImmutableRecord],
    ) -> Result<u64> {
        validate_publication(repo, id, records)?;
        for object in objects {
            let stored = self.put_object(object.kind(), object.bytes())?;
            if stored != object.id() {
                return Err(scion_types::Error::Corrupt(
                    "storage returned an incorrect prepared object identifier".into(),
                ));
            }
        }
        self.publish_commit(repo, id, records)
    }
    fn read_log(&self, repo: &str, after: u64, limit: usize) -> Result<Vec<LogEntry>>;
    fn scan_records(
        &self,
        namespace: &str,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<(String, Vec<u8>)>>;
}

pub type SharedStore = Arc<dyn Storage>;
