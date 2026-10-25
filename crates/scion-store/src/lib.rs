//! Immutable storage and transactional metadata contracts.
use scion_types::{LogEntry, ObjectId, ObjectKind, Result};
use std::sync::Arc;

mod codec;
mod local;
mod memory;
mod sharded;
pub use codec::{decode_object, encode_object, MAX_OBJECT_SIZE};
pub use local::LocalStore;
pub use memory::MemoryStore;
pub use sharded::ShardedStore;

#[cfg(feature = "postgres")]
mod pg;
#[cfg(feature = "postgres")]
pub use pg::PostgresStore;

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
    fn compare_and_swap(&self, namespace: &str, key: &str, expected: Option<&[u8]>, value: Option<&[u8]>) -> Result<bool>;
    /// Atomically append/deduplicate the changeset and publish membership at
    /// `repo/{repo}/changesets`, with key `id.to_string()` and value `b"1"`.
    fn append_log(&self, repo: &str, id: ObjectId) -> Result<u64>;
    fn read_log(&self, repo: &str, after: u64, limit: usize) -> Result<Vec<LogEntry>>;
    fn scan_records(&self, namespace: &str, prefix: &str, limit: usize) -> Result<Vec<(String, Vec<u8>)>>;
}

pub type SharedStore = Arc<dyn Storage>;
