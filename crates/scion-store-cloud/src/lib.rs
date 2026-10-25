//! Cloud-backed immutable Scion objects with independent transactional metadata.
//!
//! [`CloudStore`] implements [`Storage`] by keeping objects in an existing S3,
//! Google Cloud Storage, or Azure bucket and delegating records, references, and
//! log publication to a supplied metadata backend. Cloud writes always use
//! create-if-absent preconditions; a repeated key is accepted only after its
//! existing content has been verified. No bucket listing or deletion permission
//! is required for normal operation.
//!
//! The storage API is synchronous. Async applications should call it using
//! `tokio::task::spawn_blocking`; the adapter maintains an independent, bounded
//! I/O runtime and can safely be constructed or dropped inside a Tokio runtime.

mod config;
mod runtime;

pub use config::{CloudConfig, CloudProvider};
pub use object_store;

use bytes::Bytes;
use futures::TryStreamExt;
use object_store::{ObjectStore, PutMode, PutOptions, path::Path};
use runtime::{Admission, IoRuntime};
use scion_store::{
    ImmutableRecord, MAX_OBJECT_SIZE, SharedStore, Storage, decode_object, encode_object,
};
use scion_types::{Error, LogEntry, ObjectId, ObjectKind, Result};
use std::{sync::Arc, time::Duration};

/// Encoded envelope ceiling, including compression framing overhead.
pub const MAX_ENCODED_SIZE: usize = MAX_OBJECT_SIZE + 1_048_576;

/// Admission and deadline limits for remote object operations.
#[derive(Debug, Clone)]
pub struct CloudLimits {
    /// Calls above this limit are rejected before copying upload data.
    pub max_inflight: usize,
    /// Deadline includes SDK retries, response bodies, and duplicate retrieval.
    pub operation_timeout: Duration,
    /// May reduce but cannot increase the Scion format's encoded-object ceiling.
    pub max_encoded_size: usize,
}

impl Default for CloudLimits {
    fn default() -> Self {
        Self {
            max_inflight: 16,
            operation_timeout: Duration::from_secs(60),
            max_encoded_size: MAX_ENCODED_SIZE,
        }
    }
}

impl CloudLimits {
    pub fn validate(&self) -> Result<()> {
        if !(1..=1024).contains(&self.max_inflight)
            || self.operation_timeout.is_zero()
            || self.operation_timeout > Duration::from_secs(300)
            || !(49..=MAX_ENCODED_SIZE).contains(&self.max_encoded_size)
        {
            return Err(Error::Invalid(
                "invalid cloud object resource limits".into(),
            ));
        }
        Ok(())
    }
}

/// Immutable cloud objects composed with an independent transactional authority.
pub struct CloudStore {
    metadata: SharedStore,
    objects: Arc<dyn ObjectStore>,
    prefix: String,
    limits: CloudLimits,
    io: IoRuntime,
}

impl CloudStore {
    /// Connect using SDK credential discovery. The bucket/container must exist.
    pub fn connect(metadata: SharedStore, config: CloudConfig) -> Result<Self> {
        let objects = config.build()?;
        Self::with_object_store(metadata, objects, config.prefix, config.limits)
    }

    /// Supply an SDK object store with custom credentials, transport, or a test
    /// implementation. The object store must honor atomic [`PutMode::Create`].
    pub fn with_object_store(
        metadata: SharedStore,
        objects: Arc<dyn ObjectStore>,
        prefix: impl Into<String>,
        limits: CloudLimits,
    ) -> Result<Self> {
        let prefix = prefix.into();
        config::validate_prefix(&prefix)?;
        limits.validate()?;
        let io = IoRuntime::new(limits.max_inflight, limits.operation_timeout)?;
        Ok(Self {
            metadata,
            objects,
            prefix,
            limits,
            io,
        })
    }

    /// Stable remote key for diagnostics and administrative migration tools.
    pub fn object_key(&self, kind: ObjectKind, id: ObjectId) -> Path {
        let digest = id.to_string();
        Path::from(format!(
            "{}/objects/v1/{}/{}/{}",
            self.prefix,
            kind.as_str(),
            &digest[..2],
            digest
        ))
    }

    fn read_encoded(&self, kind: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        let admission = self.io.admit()?;
        let objects = self.objects.clone();
        let key = self.object_key(kind, id);
        let limit = self.limits.max_encoded_size;
        self.io.run(admission, async move {
            fetch_bounded(&objects, &key, limit).await
        })
    }

    fn write_validated(
        &self,
        admission: Admission,
        kind: ObjectKind,
        id: ObjectId,
        encoded: Vec<u8>,
    ) -> Result<()> {
        if encoded.len() > self.limits.max_encoded_size {
            return Err(Error::Limit(
                "cloud encoded object exceeds byte limit".into(),
            ));
        }
        let objects = self.objects.clone();
        let key = self.object_key(kind, id);
        let limit = self.limits.max_encoded_size;
        let existing = self.io.run(admission, async move {
            let result = objects
                .put_opts(
                    &key,
                    Bytes::from(encoded).into(),
                    PutOptions {
                        mode: PutMode::Create,
                        ..Default::default()
                    },
                )
                .await;
            match result {
                Ok(_) => Ok(None),
                Err(
                    object_store::Error::AlreadyExists { .. }
                    | object_store::Error::Precondition { .. },
                ) => Ok(Some(fetch_bounded(&objects, &key, limit).await?)),
                Err(error) => Err(provider_error(error, "write")),
            }
        })?;
        if let Some(existing) = existing {
            // Valid alternative compression is idempotent; corrupt or foreign
            // content is never repaired by silently overwriting a cloud object.
            decode_object(kind, id, &existing)?;
        }
        Ok(())
    }
}

async fn fetch_bounded(
    objects: &Arc<dyn ObjectStore>,
    key: &Path,
    limit: usize,
) -> Result<Vec<u8>> {
    let response = objects
        .get_opts(key, Default::default())
        .await
        .map_err(|error| provider_error(error, "read"))?;
    let expected = response.meta.size;
    if expected > limit as u64 || response.range.start != 0 || response.range.end != expected {
        return Err(Error::Limit(
            "cloud object response exceeds byte or range limit".into(),
        ));
    }
    let mut data = Vec::with_capacity((expected as usize).min(65_536));
    let mut chunks = response.into_stream();
    while let Some(chunk) = chunks
        .try_next()
        .await
        .map_err(|error| provider_error(error, "read"))?
    {
        if data.len().saturating_add(chunk.len()) > limit {
            return Err(Error::Limit(
                "cloud object stream exceeds byte limit".into(),
            ));
        }
        data.extend_from_slice(&chunk);
    }
    if data.len() as u64 != expected {
        return Err(Error::Corrupt(
            "cloud object length differs from response metadata".into(),
        ));
    }
    Ok(data)
}

// SDK errors can embed endpoint URLs, signed queries, or credential paths.
// Preserve actionable categories without propagating their raw source strings.
fn provider_error(error: object_store::Error, operation: &'static str) -> Error {
    match error {
        object_store::Error::NotFound { .. } => Error::NotFound("cloud object".into()),
        object_store::Error::PermissionDenied { .. }
        | object_store::Error::Unauthenticated { .. } => {
            Error::Permission(format!("cloud provider denied object {operation}"))
        }
        object_store::Error::NotSupported { .. } => Error::Invalid(
            "cloud provider does not support required atomic object operations".into(),
        ),
        _ => Error::Transport(format!("cloud object {operation} failed")),
    }
}

impl Storage for CloudStore {
    fn put_object(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectId> {
        if bytes.len() > MAX_OBJECT_SIZE {
            return Err(Error::Limit("object exceeds 256 MiB".into()));
        }
        let admission = self.io.admit()?;
        let id = ObjectId::hash(kind, bytes);
        self.write_validated(admission, kind, id, encode_object(kind, bytes)?)?;
        Ok(id)
    }
    fn get_object(&self, kind: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        decode_object(kind, id, &self.read_encoded(kind, id)?)
    }
    fn get_encoded(&self, kind: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        let bytes = self.read_encoded(kind, id)?;
        decode_object(kind, id, &bytes)?;
        Ok(bytes)
    }
    fn put_encoded(&self, kind: ObjectKind, id: ObjectId, bytes: &[u8]) -> Result<()> {
        if bytes.len() > self.limits.max_encoded_size {
            return Err(Error::Limit(
                "cloud encoded object exceeds byte limit".into(),
            ));
        }
        let admission = self.io.admit()?;
        decode_object(kind, id, bytes)?;
        self.write_validated(admission, kind, id, bytes.to_vec())
    }
    fn get_record(&self, namespace: &str, key: &str) -> Result<Option<Vec<u8>>> {
        self.metadata.get_record(namespace, key)
    }
    fn put_record(&self, namespace: &str, key: &str, value: &[u8]) -> Result<()> {
        self.metadata.put_record(namespace, key, value)
    }
    fn compare_and_swap(
        &self,
        namespace: &str,
        key: &str,
        expected: Option<&[u8]>,
        value: Option<&[u8]>,
    ) -> Result<bool> {
        self.metadata
            .compare_and_swap(namespace, key, expected, value)
    }
    fn append_log(&self, repo: &str, id: ObjectId) -> Result<u64> {
        self.metadata.append_log(repo, id)
    }
    fn publish_commit(&self, repo: &str, id: ObjectId, records: &[ImmutableRecord]) -> Result<u64> {
        self.metadata.publish_commit(repo, id, records)
    }
    fn read_log(&self, repo: &str, after: u64, limit: usize) -> Result<Vec<LogEntry>> {
        self.metadata.read_log(repo, after, limit)
    }
    fn scan_records(
        &self,
        namespace: &str,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        self.metadata.scan_records(namespace, prefix, limit)
    }
}
