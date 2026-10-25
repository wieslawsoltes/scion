//! Google Cloud Spanner transactional metadata for Scion.
//!
//! Apply [`SCHEMA`] once using an administrator before connecting. Immutable
//! object bytes are delegated to a separate [`SharedStore`]; this backend stores
//! metadata, atomic reference updates, and the committed publication log.
//! The official SDK supplies credentials, strong reads, and automatic retries
//! for aborted read/write transactions.

use google_cloud_spanner::{
    client::{DatabaseClient, Spanner},
    key::KeySet,
    mutation::Mutation,
    read::ReadRequest,
    result::ResultSet,
    statement::Statement,
    transaction::ReadWriteTransaction,
};
use scion_store::{ImmutableRecord, SharedStore, Storage, validate_publication};
use scion_types::{Error, LogEntry, ObjectId, ObjectKind, Result};
use std::{
    collections::BTreeMap, error::Error as StdError, fmt, str::FromStr, sync::Arc, time::Duration,
};

mod config;
mod runtime;
pub use config::SpannerConfig;
/// Re-export the exact official SDK version used by this backend.
pub use google_cloud_spanner;
use runtime::IoRuntime;

/// GoogleSQL schema. Provision it explicitly before using [`SpannerStore`].
pub const SCHEMA: &str = include_str!("../schema.sql");
/// Maximum metadata value, below Spanner's per-cell limit.
pub const MAX_RECORD_SIZE: usize = 8 * 1024 * 1024;
/// Maximum number of caller-supplied records in one atomic publication.
pub const MAX_PUBLICATION_RECORDS: usize = 1024;
/// Maximum total bytes of caller-supplied publication metadata.
pub const MAX_PUBLICATION_BYTES: usize = 32 * 1024 * 1024;
/// Maximum rows returned by one scan or log read.
pub const MAX_SCAN_ROWS: usize = 100_000;
/// Maximum combined key/value bytes returned by one metadata scan.
pub const MAX_SCAN_BYTES: usize = 64 * 1024 * 1024;

/// Transactional metadata backed by Google Cloud Spanner.
///
/// Publication locks one counter row per repository until commit. It does not
/// move references. This establishes a committed sequence frontier: once a
/// consumer observes sequence N, an earlier sequence cannot commit later.
/// Calls are synchronous and may block; async callers should use
/// `tokio::task::spawn_blocking` as the Scion authority does.
pub struct SpannerStore {
    objects: SharedStore,
    database: DatabaseClient,
    timeout: Duration,
    io: IoRuntime,
}

impl SpannerStore {
    /// Connect using Application Default Credentials. The official SDK honors
    /// `SPANNER_EMULATOR_HOST` for an explicitly configured plaintext emulator.
    /// This does not create a database, apply DDL, or request administrator roles.
    pub fn connect(objects: SharedStore, config: SpannerConfig) -> Result<Self> {
        config.validate()?;
        let io = IoRuntime::new(config.max_inflight, config.operation_timeout)?;
        let timeout = config.operation_timeout;
        let database = io.run(io.admit()?, async move {
            let client = Spanner::builder().build().await.map_err(|_| {
                Error::Transport(
                    "Spanner client configuration failed; check credentials and endpoint".into(),
                )
            })?;
            let mut builder = client.database_client(config.database);
            if let Some(role) = config.database_role {
                builder = builder.with_database_role(role);
            }
            builder.build().await.map_err(map_error)
        })?;
        Ok(Self {
            objects,
            database,
            timeout,
            io,
        })
    }

    fn publication_records(
        repo: &str,
        id: ObjectId,
        records: &[ImmutableRecord],
    ) -> Result<BTreeMap<(String, String), Vec<u8>>> {
        validate_repo(repo)?;
        if records.len() > MAX_PUBLICATION_RECORDS {
            return Err(Error::Limit("too many Spanner publication records".into()));
        }
        let mut bytes = 0usize;
        for record in records {
            validate_key(&record.namespace, &record.key)?;
            validate_value(&record.value)?;
            bytes = bytes
                .checked_add(record.namespace.len())
                .and_then(|n| n.checked_add(record.key.len()))
                .and_then(|n| n.checked_add(record.value.len()))
                .ok_or_else(|| Error::Limit("Spanner publication size overflow".into()))?;
            if bytes > MAX_PUBLICATION_BYTES {
                return Err(Error::Limit("Spanner publication exceeds 32 MiB".into()));
            }
        }
        validate_publication(repo, id, records)?;
        let mut batch: BTreeMap<_, _> = records
            .iter()
            .map(|record| {
                (
                    (record.namespace.clone(), record.key.clone()),
                    record.value.clone(),
                )
            })
            .collect();
        batch.insert(
            (format!("repo/{repo}/changesets"), id.to_string()),
            b"1".to_vec(),
        );
        Ok(batch)
    }
}

impl Storage for SpannerStore {
    fn put_object(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectId> {
        self.objects.put_object(kind, bytes)
    }

    fn get_object(&self, kind: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        self.objects.get_object(kind, id)
    }

    fn get_encoded(&self, kind: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        self.objects.get_encoded(kind, id)
    }

    fn put_encoded(&self, kind: ObjectKind, id: ObjectId, bytes: &[u8]) -> Result<()> {
        self.objects.put_encoded(kind, id, bytes)
    }

    fn get_record(&self, namespace: &str, key: &str) -> Result<Option<Vec<u8>>> {
        validate_key(namespace, key)?;
        let admission = self.io.admit()?;
        let db = self.database.clone();
        let request = read_record(namespace, key);
        self.io.run(admission, async move {
            let rows = db
                .single_use()
                .build()
                .execute_read(request)
                .await
                .map_err(map_error)?;
            read_optional_value(rows).await.map_err(map_error)
        })
    }

    fn put_record(&self, namespace: &str, key: &str, value: &[u8]) -> Result<()> {
        validate_key(namespace, key)?;
        validate_value(value)?;
        let admission = self.io.admit()?;
        let db = self.database.clone();
        let timeout = self.timeout;
        let input = Arc::new((namespace.to_owned(), key.to_owned(), value.to_vec()));
        self.io.run(admission, async move {
            let runner = db
                .read_write_transaction()
                .with_transaction_timeout(timeout)
                .build()
                .await
                .map_err(map_error)?;
            runner
                .run(move |tx: ReadWriteTransaction| {
                    let input = input.clone();
                    async move {
                        let (namespace, key, value) = input.as_ref();
                        let current = read_optional_value(
                            tx.execute_read(read_record(namespace, key)).await?,
                        )
                        .await?;
                        match current {
                            Some(current) if current != *value => {
                                return Err(domain(Failure::Conflict));
                            }
                            Some(_) => {}
                            None => tx.buffer([insert_record(namespace, key, value)])?,
                        }
                        Ok(())
                    }
                })
                .await
                .map_err(map_error)?;
            Ok(())
        })
    }

    fn compare_and_swap(
        &self,
        namespace: &str,
        key: &str,
        expected: Option<&[u8]>,
        value: Option<&[u8]>,
    ) -> Result<bool> {
        validate_key(namespace, key)?;
        if let Some(bytes) = expected {
            validate_value(bytes)?;
        }
        if let Some(bytes) = value {
            validate_value(bytes)?;
        }
        let admission = self.io.admit()?;
        let db = self.database.clone();
        let timeout = self.timeout;
        let input = Arc::new((
            namespace.to_owned(),
            key.to_owned(),
            expected.map(<[u8]>::to_vec),
            value.map(<[u8]>::to_vec),
        ));
        self.io.run(admission, async move {
            let runner = db
                .read_write_transaction()
                .with_transaction_timeout(timeout)
                .build()
                .await
                .map_err(map_error)?;
            let committed = runner
                .run(move |tx: ReadWriteTransaction| {
                    let input = input.clone();
                    async move {
                        let (namespace, key, expected, value) = input.as_ref();
                        let current = read_optional_value(
                            tx.execute_read(read_record(namespace, key)).await?,
                        )
                        .await?;
                        if current != *expected {
                            return Ok(false);
                        }
                        if current == *value {
                            return Ok(true);
                        }
                        let mutation = if let Some(value) = value {
                            Mutation::new_insert_or_update_builder("ScionRecords")
                                .set("Namespace")
                                .to(namespace)
                                .set("RecordKey")
                                .to(key)
                                .set("ValueBytes")
                                .to(value)
                                .build()
                        } else {
                            Mutation::delete("ScionRecords", one_key(namespace, key))
                        };
                        tx.buffer([mutation])?;
                        Ok(true)
                    }
                })
                .await
                .map_err(map_error)?;
            Ok(committed.result)
        })
    }

    fn append_log(&self, repo: &str, id: ObjectId) -> Result<u64> {
        self.publish_commit(repo, id, &[])
    }

    fn publish_commit(&self, repo: &str, id: ObjectId, records: &[ImmutableRecord]) -> Result<u64> {
        // Admit before cloning a potentially large batch. Validation itself
        // allocates only bounded keys and does not perform external writes.
        let admission = self.io.admit()?;
        let input = Arc::new((
            repo.to_owned(),
            id.to_string(),
            Self::publication_records(repo, id, records)?,
        ));
        let db = self.database.clone();
        let timeout = self.timeout;
        self.io.run(admission, async move {
            let runner = db
                .read_write_transaction()
                .with_transaction_timeout(timeout)
                .build()
                .await
                .map_err(map_error)?;
            let committed = runner
                .run(move |tx: ReadWriteTransaction| {
                    let input = input.clone();
                    async move {
                        let (repo, id, records) = input.as_ref();
                        publish(&tx, repo, id, records).await
                    }
                })
                .await
                .map_err(map_error)?;
            Ok(committed.result)
        })
    }

    fn read_log(&self, repo: &str, after: u64, limit: usize) -> Result<Vec<LogEntry>> {
        validate_repo(repo)?;
        if limit == 0 || after >= i64::MAX as u64 {
            return Ok(Vec::new());
        }
        let admission = self.io.admit()?;
        let db = self.database.clone();
        let statement = Statement::builder("SELECT SequenceNumber, ChangesetId FROM ScionLog WHERE Repository = @repo AND SequenceNumber > @after ORDER BY SequenceNumber LIMIT @limit")
            .add_param("repo", repo).add_param("after", after as i64).add_param("limit", limit.min(MAX_SCAN_ROWS + 1) as i64).build();
        self.io.run(admission, async move {
            let mut rows = db
                .single_use()
                .build()
                .execute_query(statement)
                .await
                .map_err(map_error)?;
            let mut entries = Vec::new();
            let mut last = after;
            while let Some(row) = rows.next().await {
                let row = row.map_err(map_error)?;
                let sequence: i64 = row.try_get("SequenceNumber").map_err(map_error)?;
                let id: String = row.try_get("ChangesetId").map_err(map_error)?;
                if entries.len() >= MAX_SCAN_ROWS {
                    return Err(Error::Limit("Spanner log read exceeds 100,000 rows".into()));
                }
                if sequence <= 0 || sequence as u64 <= last || entries.len() >= limit {
                    return Err(Error::Corrupt(
                        "invalid Spanner publication log order".into(),
                    ));
                }
                last = sequence as u64;
                entries.push(LogEntry {
                    sequence: last,
                    id: ObjectId::from_str(&id)
                        .map_err(|_| Error::Corrupt("invalid Spanner changeset ID".into()))?,
                });
            }
            Ok(entries)
        })
    }

    fn scan_records(
        &self,
        namespace: &str,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        validate_key(namespace, prefix)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let admission = self.io.admit()?;
        let db = self.database.clone();
        let statement = Statement::builder("SELECT RecordKey, ValueBytes FROM ScionRecords WHERE Namespace = @namespace AND STARTS_WITH(RecordKey, @prefix) ORDER BY RecordKey LIMIT @limit")
            .add_param("namespace", namespace).add_param("prefix", prefix).add_param("limit", limit.min(MAX_SCAN_ROWS + 1) as i64).build();
        self.io.run(admission, async move {
            let mut rows = db
                .single_use()
                .build()
                .execute_query(statement)
                .await
                .map_err(map_error)?;
            let mut result = Vec::new();
            let mut total = 0usize;
            while let Some(row) = rows.next().await {
                let row = row.map_err(map_error)?;
                let key: String = row.try_get("RecordKey").map_err(map_error)?;
                let value: Vec<u8> = row.try_get("ValueBytes").map_err(map_error)?;
                validate_value(&value)?;
                total = total.saturating_add(key.len()).saturating_add(value.len());
                if total > MAX_SCAN_BYTES || result.len() >= MAX_SCAN_ROWS || result.len() >= limit
                {
                    return Err(Error::Limit(
                        "Spanner metadata scan exceeds resource bounds".into(),
                    ));
                }
                result.push((key, value));
            }
            Ok(result)
        })
    }
}

async fn publish(
    tx: &ReadWriteTransaction,
    repo: &str,
    id: &str,
    records: &BTreeMap<(String, String), Vec<u8>>,
) -> google_cloud_spanner::Result<u64> {
    let mut keys = KeySet::builder();
    for (namespace, key) in records.keys() {
        keys = keys.add_key(google_cloud_spanner::key![namespace.as_str(), key.as_str()]);
    }
    let request = ReadRequest::builder("ScionRecords", ["Namespace", "RecordKey", "ValueBytes"])
        .with_keys(keys.build())
        .build();
    let mut rows = tx.execute_read(request).await?;
    let mut existing = BTreeMap::new();
    while let Some(row) = rows.next().await {
        let row = row?;
        let namespace: String = row.try_get("Namespace")?;
        let key: String = row.try_get("RecordKey")?;
        let value: Vec<u8> = row.try_get("ValueBytes")?;
        if value.len() > MAX_RECORD_SIZE || existing.len() >= records.len() {
            return Err(domain(Failure::Corrupt));
        }
        existing.insert((namespace, key), value);
    }
    for ((namespace, key), value) in records {
        match existing.get(&(namespace.clone(), key.clone())) {
            Some(previous) if previous != value => return Err(domain(Failure::Conflict)),
            Some(_) => {}
            None => tx.buffer([insert_record(namespace, key, value)])?,
        }
    }
    // Reading and updating this row in one serializable transaction establishes
    // sequence order by commit, without a process-local repository lock.
    let statement = Statement::builder("SELECT (SELECT SequenceNumber FROM ScionPublished WHERE Repository = @repo AND ChangesetId = @id) AS ExistingSequence, (SELECT LastSequence FROM ScionLogCounters WHERE Repository = @repo) AS LastSequence")
        .add_param("repo", repo).add_param("id", id).build();
    let mut rows = tx.execute_query(statement).await?;
    let row = rows
        .next()
        .await
        .ok_or_else(|| domain(Failure::Corrupt))??;
    let existing: Option<i64> = row.try_get("ExistingSequence")?;
    let last: Option<i64> = row.try_get("LastSequence")?;
    if rows.next().await.is_some() {
        return Err(domain(Failure::Corrupt));
    }
    let last = last.unwrap_or(0);
    if last < 0 {
        return Err(domain(Failure::Corrupt));
    }
    if let Some(existing) = existing {
        if existing <= 0 || existing > last {
            return Err(domain(Failure::Corrupt));
        }
        return Ok(existing as u64);
    }
    let sequence = last
        .checked_add(1)
        .ok_or_else(|| domain(Failure::Exhausted))?;
    tx.buffer([
        Mutation::new_insert_or_update_builder("ScionLogCounters")
            .set("Repository")
            .to(repo)
            .set("LastSequence")
            .to(sequence)
            .build(),
        Mutation::new_insert_builder("ScionLog")
            .set("Repository")
            .to(repo)
            .set("SequenceNumber")
            .to(sequence)
            .set("ChangesetId")
            .to(id)
            .build(),
        Mutation::new_insert_builder("ScionPublished")
            .set("Repository")
            .to(repo)
            .set("ChangesetId")
            .to(id)
            .set("SequenceNumber")
            .to(sequence)
            .build(),
    ])?;
    Ok(sequence as u64)
}

fn one_key(namespace: &str, key: &str) -> KeySet {
    KeySet::builder()
        .add_key(google_cloud_spanner::key![namespace, key])
        .build()
}

fn read_record(namespace: &str, key: &str) -> ReadRequest {
    ReadRequest::builder("ScionRecords", ["ValueBytes"])
        .with_keys(one_key(namespace, key))
        .build()
}

async fn read_optional_value(mut rows: ResultSet) -> google_cloud_spanner::Result<Option<Vec<u8>>> {
    let Some(row) = rows.next().await else {
        return Ok(None);
    };
    let value: Vec<u8> = row?.try_get("ValueBytes")?;
    if value.len() > MAX_RECORD_SIZE || rows.next().await.is_some() {
        return Err(domain(Failure::Corrupt));
    }
    Ok(Some(value))
}

fn insert_record(namespace: &str, key: &str, value: &[u8]) -> Mutation {
    Mutation::new_insert_builder("ScionRecords")
        .set("Namespace")
        .to(namespace)
        .set("RecordKey")
        .to(key)
        .set("ValueBytes")
        .to(value)
        .build()
}

fn validate_repo(repo: &str) -> Result<()> {
    if repo.is_empty() || repo.len() > 256 {
        return Err(Error::Invalid(
            "Spanner repository name must contain 1..=256 UTF-8 bytes".into(),
        ));
    }
    Ok(())
}

fn validate_key(namespace: &str, key: &str) -> Result<()> {
    if namespace.len() > 1024 || key.len() > 4096 {
        return Err(Error::Limit(
            "Spanner metadata key exceeds byte limit".into(),
        ));
    }
    Ok(())
}

fn validate_value(value: &[u8]) -> Result<()> {
    if value.len() > MAX_RECORD_SIZE {
        return Err(Error::Limit("Spanner metadata value exceeds 8 MiB".into()));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum Failure {
    Conflict,
    Corrupt,
    Exhausted,
}
impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Scion transaction invariant rejected")
    }
}
impl StdError for Failure {}

fn domain(failure: Failure) -> google_cloud_spanner::Error {
    // Returning an actual SDK error makes the runner roll back. Returning
    // Ok(Err(...)) would incorrectly commit earlier buffered mutations.
    google_cloud_spanner::Error::deser(failure)
}

fn map_error(error: google_cloud_spanner::Error) -> Error {
    let mut source: Option<&(dyn StdError + 'static)> = Some(&error);
    while let Some(current) = source {
        if let Some(failure) = current.downcast_ref::<Failure>() {
            return match failure {
                Failure::Conflict => Error::Conflict("immutable Spanner metadata differs".into()),
                Failure::Corrupt => Error::Corrupt("Spanner metadata invariant violated".into()),
                Failure::Exhausted => Error::Limit("Spanner publication sequence exhausted".into()),
            };
        }
        source = current.source();
    }
    // SDK errors may include credential file names, endpoint details, and request
    // data. Canonical status names are static SDK constants, not service text.
    let status = error.status().map(|status| status.code.name());
    match status {
        Some("PERMISSION_DENIED" | "UNAUTHENTICATED") => {
            Error::Permission("Spanner credentials or database role denied access".into())
        }
        Some("RESOURCE_EXHAUSTED") => Error::Limit("Spanner service quota exhausted".into()),
        Some("NOT_FOUND") => {
            Error::NotFound("Spanner database, table, or session unavailable".into())
        }
        Some("ALREADY_EXISTS") => Error::Conflict("Spanner conditional write conflicted".into()),
        Some(code) => Error::Transport(format!(
            "Spanner request failed ({code}); inspect service-side diagnostics"
        )),
        None => Error::Transport("Spanner request failed; inspect service-side diagnostics".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_validation_rejects_reserved_membership_and_conflicting_duplicates() {
        let id = ObjectId::hash(ObjectKind::Changeset, b"test");
        let membership = ImmutableRecord {
            namespace: "repo/demo/changesets".into(),
            key: id.to_string(),
            value: b"1".to_vec(),
        };
        assert!(matches!(
            SpannerStore::publication_records("demo", id, &[membership]),
            Err(Error::Invalid(_))
        ));
        let record = ImmutableRecord {
            namespace: "n".into(),
            key: "k".into(),
            value: b"one".to_vec(),
        };
        let mut conflicting = record.clone();
        conflicting.value = b"two".to_vec();
        assert!(matches!(
            SpannerStore::publication_records("demo", id, &[record, conflicting]),
            Err(Error::Conflict(_))
        ));
    }

    #[test]
    fn domain_failures_remain_typed_and_do_not_commit_successfully() {
        assert!(matches!(
            map_error(domain(Failure::Conflict)),
            Error::Conflict(_)
        ));
        assert!(matches!(
            map_error(domain(Failure::Corrupt)),
            Error::Corrupt(_)
        ));
        assert!(matches!(
            map_error(domain(Failure::Exhausted)),
            Error::Limit(_)
        ));
    }

    #[test]
    fn metadata_limits_are_bytes_and_publication_deduplicates_equal_records() {
        assert!(validate_key("n", &"é".repeat(2049)).is_err());
        assert!(validate_repo("").is_err());
        let id = ObjectId::hash(ObjectKind::Changeset, b"test");
        let record = ImmutableRecord {
            namespace: "n".into(),
            key: "k".into(),
            value: b"v".to_vec(),
        };
        let batch =
            SpannerStore::publication_records("demo", id, &[record.clone(), record]).unwrap();
        assert_eq!(batch.len(), 2);
    }
}
