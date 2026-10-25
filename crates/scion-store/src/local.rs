use crate::codec::{HEADER, decode_small_object, object_size, small_object_encoded_limit};
use crate::{
    ImmutableRecord, PreparedObject, PublicationResult, RecordCas, Storage, decode_object,
    encode_object, validate_publication, validate_publication_cas,
};
use parking_lot::{Mutex, MutexGuard};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, TransactionBehavior, config::DbConfig, params,
};
use scion_types::{Error, LogEntry, ObjectId, ObjectKind, Result};
use std::collections::HashSet;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

fn db_error(e: rusqlite::Error) -> Error {
    Error::Storage(e.to_string())
}

// Small encoded objects share the FULL-synchronous metadata transaction. Larger
// objects keep the original sharded-file representation. Batches are flushed in
// bounded chunks, always before the final membership visibility boundary.
const INLINE_OBJECT_LIMIT: usize = 256 * 1024;
const INLINE_BATCH_LIMIT: usize = 16 * 1024 * 1024;
type EncodedObject = (ObjectKind, ObjectId, Vec<u8>);

/// Per-connection local I/O tuning. Both regular and read-only opens use these
/// defaults; database contents, FULL write synchronization and normal WAL
/// auto-checkpointing are independent of these options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalStoreOptions {
    /// Maximum bytes of the main database that SQLite may map for reads.
    /// Zero disables mapping. SQLite may clamp this to its platform/VFS limit.
    /// Mapping preserves live-WAL coordination, but underlying mapped I/O errors
    /// can become process signals; disable it when recoverable I/O errors are
    /// required. Windows mappings can delay database-file truncation by VACUUM.
    pub mmap_size: u64,
    /// Whether the last connection checkpoints and removes WAL files on close.
    /// False avoids teardown/recreation for short-lived readers. FULL commits and
    /// normal WAL auto-checkpointing remain enabled. Backups must include the WAL.
    pub checkpoint_on_close: bool,
}

impl Default for LocalStoreOptions {
    fn default() -> Self {
        Self {
            mmap_size: 0,
            checkpoint_on_close: true,
        }
    }
}

impl LocalStoreOptions {
    fn validate(self) -> Result<()> {
        if self.mmap_size > i64::MAX as u64 {
            return Err(Error::Invalid(
                "SQLite mmap_size exceeds signed 64-bit range".into(),
            ));
        }
        Ok(())
    }
}

/// Durable local backend: bounded small objects and metadata in SQLite WAL,
/// with larger immutable compressed objects in sharded files. Existing loose
/// objects remain readable; the encoded object format and content IDs do not change.
/// Multiple processes may open a store. Shared deployments should use PostgresStore.
pub struct LocalStore {
    root: PathBuf,
    db: Mutex<Connection>,
    options: LocalStoreOptions,
    mmap_configured: AtomicBool,
    read_only: bool,
    has_inline_objects: AtomicBool,
    #[cfg(unix)]
    durable_directories: Mutex<HashSet<PathBuf>>,
    durable_objects: Mutex<HashSet<(ObjectKind, ObjectId)>>,
}
impl LocalStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_options(path, LocalStoreOptions::default())
    }

    /// Open or create a writable store with explicit per-connection I/O policy.
    pub fn open_with_options(path: impl AsRef<Path>, options: LocalStoreOptions) -> Result<Self> {
        options.validate()?;
        fs::create_dir_all(path.as_ref())?;
        let root = fs::canonicalize(path)?;
        let db = Connection::open(root.join("metadata.sqlite3")).map_err(db_error)?;
        configure_connection(&db, options)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS records(namespace TEXT NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL, PRIMARY KEY(namespace,key)) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS inline_objects(kind TEXT NOT NULL, id BLOB NOT NULL, encoded BLOB NOT NULL, PRIMARY KEY(kind,id)) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS changeset_log(sequence INTEGER PRIMARY KEY AUTOINCREMENT, repo TEXT NOT NULL, id TEXT NOT NULL, UNIQUE(repo,id));
            CREATE INDEX IF NOT EXISTS changeset_log_repo ON changeset_log(repo,sequence);").map_err(db_error)?;
        Ok(Self {
            root,
            db: Mutex::new(db),
            options,
            mmap_configured: AtomicBool::new(false),
            read_only: false,
            has_inline_objects: AtomicBool::new(true),
            #[cfg(unix)]
            durable_directories: Default::default(),
            durable_objects: Default::default(),
        })
    }

    /// Open an existing store without creating directories, migrating its schema
    /// or changing journal settings. Queries see commits made by concurrent
    /// writers, including their live WAL; this is not SQLite's immutable mode.
    /// All mutation methods return an error, including writes to loose objects.
    /// SQLite may maintain WAL reader coordination files when writers are active.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_read_only_with_options(path, LocalStoreOptions::default())
    }

    /// Open an existing read-only store with explicit per-connection I/O policy.
    /// Database reads and mmap configuration are deferred until the first query;
    /// corrupt database contents are reported by that query, never hidden.
    pub fn open_read_only_with_options(
        path: impl AsRef<Path>,
        options: LocalStoreOptions,
    ) -> Result<Self> {
        options.validate()?;
        let root = fs::canonicalize(path)?;
        let db = Connection::open_with_flags(
            root.join("metadata.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(db_error)?;
        configure_connection(&db, options)?;
        // Probe the optional inline table only when reading an object. Graph and
        // metadata-only clients do not need an extra schema query. Legacy stores
        // still detect a concurrent writer's schema upgrade on later object reads.
        let has_inline_objects = false;
        Ok(Self {
            root,
            db: Mutex::new(db),
            options,
            mmap_configured: AtomicBool::new(false),
            read_only: true,
            has_inline_objects: AtomicBool::new(has_inline_objects),
            #[cfg(unix)]
            durable_directories: Default::default(),
            durable_objects: Default::default(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn options(&self) -> LocalStoreOptions {
        self.options
    }

    // Configure mapping lazily under the same lock as the connection. A failed
    // PRAGMA propagates before the requested operation and is retried later.
    // The flag is accessed only while holding this mutex, so Relaxed is enough.
    fn database(&self) -> Result<MutexGuard<'_, Connection>> {
        let db = self.db.lock();
        if !self.mmap_configured.load(Ordering::Relaxed) {
            db.pragma_update(None, "mmap_size", self.options.mmap_size as i64)
                .map_err(db_error)?;
            self.mmap_configured.store(true, Ordering::Relaxed);
        }
        Ok(db)
    }
    fn require_writable(&self) -> Result<()> {
        if self.read_only {
            return Err(Error::Storage("local store was opened read-only".into()));
        }
        Ok(())
    }
    fn path(&self, kind: ObjectKind, id: ObjectId) -> PathBuf {
        let hex = id.to_string();
        self.root
            .join("objects")
            .join(kind.as_str())
            .join(&hex[..2])
            .join(&hex[2..])
    }
    fn has_inline_table(&self, db: &Connection) -> Result<bool> {
        if !self.has_inline_objects.load(Ordering::Relaxed) {
            if !has_inline_table(db)? {
                return Ok(false);
            }
            // A writer can upgrade a legacy store after this reader opens.
            self.has_inline_objects.store(true, Ordering::Relaxed);
        }
        Ok(true)
    }
    fn inline_encoded(&self, kind: ObjectKind, id: ObjectId) -> Result<Option<Vec<u8>>> {
        let db = self.database()?;
        if !self.has_inline_table(&db)? {
            return Ok(None);
        }
        db.prepare_cached("SELECT encoded FROM inline_objects WHERE kind=?1 AND id=?2")
            .map_err(db_error)?
            .query_row(params![kind.as_str(), id.0.as_slice()], |row| row.get(0))
            .optional()
            .map_err(db_error)
    }
    fn contains_inline(&self, kind: ObjectKind, id: ObjectId) -> Result<bool> {
        if let Some(encoded) = self.inline_encoded(kind, id)? {
            decode_object(kind, id, &encoded)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn write_inline(&self, kind: ObjectKind, id: ObjectId, encoded: &[u8]) -> Result<()> {
        let db = self.database()?;
        let inserted = db
            .prepare_cached(
                "INSERT INTO inline_objects(kind,id,encoded) VALUES (?1,?2,?3) ON CONFLICT DO NOTHING",
            )
            .map_err(db_error)?
            .execute(params![kind.as_str(), id.0.as_slice(), encoded])
            .map_err(db_error)?;
        if inserted == 0 {
            verify_inline_duplicate(&db, kind, id)?;
        }
        Ok(())
    }
    fn flush_inline(&self, objects: &[EncodedObject]) -> Result<()> {
        let mut db = self.database()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        insert_objects(&tx, objects)?;
        tx.commit().map_err(db_error)
    }
    fn publish_batch(
        &self,
        repo: &str,
        id: ObjectId,
        objects: &[EncodedObject],
        records: &[ImmutableRecord],
        cas: Option<&RecordCas>,
    ) -> Result<PublicationResult> {
        let mut db = self.database()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        insert_objects(&tx, objects)?;
        for record in records {
            insert_immutable(&tx, &record.namespace, &record.key, &record.value)?;
        }
        let id = id.to_string();
        tx.prepare_cached(
            "INSERT INTO changeset_log(repo,id) VALUES (?1,?2) ON CONFLICT DO NOTHING",
        )
        .map_err(db_error)?
        .execute(params![repo, id])
        .map_err(db_error)?;
        insert_immutable(&tx, &format!("repo/{repo}/changesets"), &id, b"1")?;
        let seq: i64 = tx
            .prepare_cached("SELECT sequence FROM changeset_log WHERE repo=?1 AND id=?2")
            .map_err(db_error)?
            .query_row(params![repo, id], |r| r.get(0))
            .map_err(db_error)?;
        let cas_applied = cas
            .map(|cas| {
                apply_cas(
                    &tx,
                    &cas.namespace,
                    &cas.key,
                    cas.expected.as_deref(),
                    cas.value.as_deref(),
                )
            })
            .transpose()?;
        tx.commit().map_err(db_error)?;
        Ok(PublicationResult {
            sequence: seq as u64,
            cas_applied,
        })
    }
    fn write_encoded(&self, kind: ObjectKind, id: ObjectId, encoded: &[u8]) -> Result<()> {
        let path = self.path(kind, id);
        if path.is_file() {
            decode_object(kind, id, &fs::read(&path)?)?;
            return self.establish_durability(kind, id, &path);
        }
        let parent = path
            .parent()
            .ok_or_else(|| Error::Storage("object parent absent".into()))?;
        fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(encoded)?;
        temp.as_file().sync_all()?;
        match temp.persist_noclobber(&path) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                // A concurrent immutable writer may win the filename. Verify
                // that winner instead of acknowledging unrelated/corrupt data.
                decode_object(kind, id, &fs::read(&path)?)?;
            }
            Err(e) => return Err(e.error.into()),
        }
        self.establish_durability(kind, id, &path)
    }
    fn establish_durability(&self, kind: ObjectKind, id: ObjectId, path: &Path) -> Result<()> {
        if self.durable_objects.lock().contains(&(kind, id)) {
            return Ok(());
        }
        #[cfg(unix)]
        let parent = path
            .parent()
            .ok_or_else(|| Error::Storage("object parent absent".into()))?;
        #[cfg(unix)]
        {
            fs::File::open(path)?.sync_all()?;
            // Each process establishes durability on its first visit. A directory
            // merely existing is insufficient: another writer may just have
            // created it. Cache only after synchronizing all uncached ancestors.
            let mut durable = self.durable_directories.lock();
            let unsynced: Vec<_> = parent
                .ancestors()
                .take_while(|path| path.starts_with(&self.root) && !durable.contains(*path))
                .map(Path::to_path_buf)
                .collect();
            for directory in &unsynced {
                fs::File::open(directory)?.sync_all()?;
            }
            durable.extend(unsynced);
        }
        #[cfg(unix)]
        // Every new object name still requires its immediate directory barrier.
        // Already-durable ancestors need not be flushed for unrelated new files.
        fs::File::open(parent)?.sync_all()?;
        #[cfg(not(unix))]
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?
            .sync_all()?;
        // An existing name can be another writer's not-yet-acknowledged rename.
        // Cache it only after this process has established the durability barrier.
        let mut durable = self.durable_objects.lock();
        if durable.len() >= 32_768 {
            durable.clear();
        }
        durable.insert((kind, id));
        Ok(())
    }
}
impl Storage for LocalStore {
    fn put_object(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectId> {
        self.require_writable()?;
        if bytes.len() > crate::MAX_OBJECT_SIZE {
            return Err(Error::Limit("object exceeds 256 MiB".into()));
        }
        let id = ObjectId::hash(kind, bytes);
        if self.contains_inline(kind, id)? {
            return Ok(id);
        }
        let path = self.path(kind, id);
        if path.is_file() {
            decode_object(kind, id, &fs::read(&path)?)?;
            self.establish_durability(kind, id, &path)?;
        } else {
            let encoded = encode_object(kind, bytes)?;
            if encoded.len() <= INLINE_OBJECT_LIMIT {
                self.write_inline(kind, id, &encoded)?;
            } else {
                self.write_encoded(kind, id, &encoded)?;
            }
        }
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
        let encoded_limit = small_object_encoded_limit(max_decoded_bytes);
        let inline: Option<(i64, Vec<u8>)> = {
            let db = self.database()?;
            if self.has_inline_table(&db)? {
                db.prepare_cached("SELECT length(encoded), CASE WHEN length(encoded)<=?3 THEN encoded ELSE substr(encoded,1,49) END FROM inline_objects WHERE kind=?1 AND id=?2")
                    .map_err(db_error)?
                    .query_row(params![kind.as_str(), id.0.as_slice(), encoded_limit as i64], |row| Ok((row.get(0)?, row.get(1)?)))
                    .optional().map_err(db_error)?
            } else {
                None
            }
        };
        if let Some((encoded_length, encoded)) = inline {
            let size = object_size(kind, id, &encoded, encoded_length as u64)?;
            if size > max_decoded_bytes as u64 || encoded_length as u64 > encoded_limit as u64 {
                return Ok(None);
            }
            return decode_small_object(kind, id, &encoded, max_decoded_bytes);
        }

        let mut file = match fs::File::open(self.path(kind, id)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound(format!("{kind:?}/{id}")));
            }
            Err(error) => return Err(error.into()),
        };
        let encoded_length = file.metadata()?.len();
        let mut header = [0u8; HEADER];
        if let Err(error) = file.read_exact(&mut header) {
            return Err(if error.kind() == std::io::ErrorKind::UnexpectedEof {
                Error::Corrupt("short immutable object envelope".into())
            } else {
                error.into()
            });
        }
        let size = object_size(kind, id, &header, encoded_length)?;
        if size > max_decoded_bytes as u64 || encoded_length > encoded_limit as u64 {
            return Ok(None);
        }
        let mut encoded = Vec::with_capacity(encoded_length as usize + 1);
        encoded.extend_from_slice(&header);
        // Read at most the observed length plus one byte, detecting truncation
        // or growth without allowing concurrent file changes to defeat the cap.
        file.take(encoded_length - HEADER as u64 + 1)
            .read_to_end(&mut encoded)?;
        if encoded.len() as u64 != encoded_length {
            return Err(Error::Corrupt(
                "immutable object changed during bounded read".into(),
            ));
        }
        decode_small_object(kind, id, &encoded, max_decoded_bytes)
    }
    fn get_encoded(&self, kind: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        if let Some(encoded) = self.inline_encoded(kind, id)? {
            return Ok(encoded);
        }
        match fs::read(self.path(kind, id)) {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::NotFound(format!("{kind:?}/{id}")))
            }
            Err(e) => Err(e.into()),
        }
    }
    fn put_encoded(&self, kind: ObjectKind, id: ObjectId, bytes: &[u8]) -> Result<()> {
        self.require_writable()?;
        decode_object(kind, id, bytes)?;
        if self.contains_inline(kind, id)? {
            return Ok(());
        }
        let path = self.path(kind, id);
        if path.is_file() {
            decode_object(kind, id, &fs::read(&path)?)?;
            self.establish_durability(kind, id, &path)
        } else if bytes.len() <= INLINE_OBJECT_LIMIT {
            self.write_inline(kind, id, bytes)
        } else {
            self.write_encoded(kind, id, bytes)
        }
    }
    fn get_record(&self, ns: &str, key: &str) -> Result<Option<Vec<u8>>> {
        let db = self.database()?;
        db.prepare_cached("SELECT value FROM records WHERE namespace=?1 AND key=?2")
            .map_err(db_error)?
            .query_row(params![ns, key], |r| r.get(0))
            .optional()
            .map_err(db_error)
    }
    fn get_record_pair(
        &self,
        first: (&str, &str),
        second: (&str, &str),
    ) -> Result<[Option<Vec<u8>>; 2]> {
        let db = self.database()?;
        db.prepare_cached("SELECT (SELECT value FROM records WHERE namespace=?1 AND key=?2), (SELECT value FROM records WHERE namespace=?3 AND key=?4)")
            .map_err(db_error)?
            .query_row(params![first.0, first.1, second.0, second.1], |row| Ok([row.get(0)?, row.get(1)?]))
            .map_err(db_error)
    }
    fn put_record(&self, ns: &str, key: &str, value: &[u8]) -> Result<()> {
        self.require_writable()?;
        let db = self.database()?;
        insert_immutable(&db, ns, key, value)
    }
    fn compare_and_swap(
        &self,
        ns: &str,
        key: &str,
        expected: Option<&[u8]>,
        value: Option<&[u8]>,
    ) -> Result<bool> {
        self.require_writable()?;
        // Each conditional mutation is one SQLite statement, which supplies its
        // own atomic FULL-synchronous transaction. No read/modify/write window.
        let db = self.database()?;
        apply_cas(&db, ns, key, expected, value)
    }
    fn append_log(&self, repo: &str, id: ObjectId) -> Result<u64> {
        self.publish_commit(repo, id, &[])
    }
    fn publish_commit(&self, repo: &str, id: ObjectId, records: &[ImmutableRecord]) -> Result<u64> {
        self.require_writable()?;
        validate_publication(repo, id, records)?;
        Ok(self.publish_batch(repo, id, &[], records, None)?.sequence)
    }
    fn publish_commit_with_objects(
        &self,
        repo: &str,
        id: ObjectId,
        objects: &[PreparedObject],
        records: &[ImmutableRecord],
    ) -> Result<u64> {
        Ok(self
            .publish_commit_with_objects_and_cas(repo, id, objects, records, None)?
            .sequence)
    }
    fn publish_commit_with_objects_and_cas(
        &self,
        repo: &str,
        id: ObjectId,
        objects: &[PreparedObject],
        records: &[ImmutableRecord],
        cas: Option<&RecordCas>,
    ) -> Result<PublicationResult> {
        self.require_writable()?;
        validate_publication_cas(repo, id, records, cas)?;
        let mut pending = Vec::new();
        let mut pending_bytes = 0;
        let mut seen = HashSet::new();
        for object in objects {
            if !seen.insert((object.kind(), object.id())) {
                continue;
            }
            let encoded = encode_object(object.kind(), object.bytes())?;
            if encoded.len() > INLINE_OBJECT_LIMIT {
                // Encoding choice is not identity: another valid compression
                // may already occupy the inline tier. Reject corrupt content
                // there before routing this representation to a loose file.
                if self.contains_inline(object.kind(), object.id())? {
                    continue;
                }
                // A durable file may become an unreachable orphan if subsequent
                // metadata validation fails, but membership is never exposed.
                self.write_encoded(object.kind(), object.id(), &encoded)?;
                continue;
            }
            let path = self.path(object.kind(), object.id());
            if path.is_file() {
                decode_object(object.kind(), object.id(), &fs::read(&path)?)?;
                self.establish_durability(object.kind(), object.id(), &path)?;
            }
            if pending_bytes + encoded.len() > INLINE_BATCH_LIMIT {
                self.flush_inline(&pending)?;
                pending.clear();
                pending_bytes = 0;
            }
            pending_bytes += encoded.len();
            pending.push((object.kind(), object.id(), encoded));
        }
        self.publish_batch(repo, id, &pending, records, cas)
    }
    fn read_log(&self, repo: &str, after: u64, limit: usize) -> Result<Vec<LogEntry>> {
        let db = self.database()?;
        let mut stmt = db.prepare_cached("SELECT sequence,id FROM changeset_log WHERE repo=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3").map_err(db_error)?;
        let rows = stmt
            .query_map(
                params![
                    repo,
                    after.min(i64::MAX as u64) as i64,
                    limit.min(10_000) as i64
                ],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .map_err(db_error)?;
        rows.map(|r| {
            let (seq, id) = r.map_err(db_error)?;
            Ok(LogEntry {
                sequence: seq as u64,
                id: id.parse()?,
            })
        })
        .collect()
    }
    fn scan_records(&self, ns: &str, prefix: &str, limit: usize) -> Result<Vec<(String, Vec<u8>)>> {
        let db = self.database()?;
        let mut stmt = db.prepare_cached("SELECT key,value FROM records WHERE namespace=?1 AND key>=?2 ORDER BY key LIMIT ?3").map_err(db_error)?;
        let rows = stmt
            .query_map(params![ns, prefix, limit.min(100_000) as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(db_error)?;
        let mut result = Vec::new();
        for row in rows {
            let (k, v) = row.map_err(db_error)?;
            if !k.starts_with(prefix) {
                break;
            }
            result.push((k, v));
        }
        Ok(result)
    }
}

fn configure_connection(db: &Connection, options: LocalStoreOptions) -> Result<()> {
    db.busy_timeout(Duration::from_secs(30)).map_err(db_error)?;
    db.set_db_config(
        DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
        !options.checkpoint_on_close,
    )
    .map_err(db_error)?;
    Ok(())
}

fn has_inline_table(db: &Connection) -> Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='inline_objects')",
        [],
        |row| row.get(0),
    )
    .map_err(db_error)
}

fn insert_objects(tx: &rusqlite::Transaction<'_>, objects: &[EncodedObject]) -> Result<()> {
    let mut statement = tx
        .prepare_cached(
            "INSERT INTO inline_objects(kind,id,encoded) VALUES (?1,?2,?3) ON CONFLICT DO NOTHING",
        )
        .map_err(db_error)?;
    for (kind, id, encoded) in objects {
        let inserted = statement
            .execute(params![kind.as_str(), id.0.as_slice(), encoded])
            .map_err(db_error)?;
        if inserted == 0 {
            verify_inline_duplicate(tx, *kind, *id)?;
        }
    }
    Ok(())
}

fn verify_inline_duplicate(db: &Connection, kind: ObjectKind, id: ObjectId) -> Result<()> {
    let encoded: Vec<u8> = db
        .prepare_cached("SELECT encoded FROM inline_objects WHERE kind=?1 AND id=?2")
        .map_err(db_error)?
        .query_row(params![kind.as_str(), id.0.as_slice()], |row| row.get(0))
        .map_err(db_error)?;
    decode_object(kind, id, &encoded)?;
    Ok(())
}

fn insert_immutable(tx: &Connection, namespace: &str, key: &str, value: &[u8]) -> Result<()> {
    let inserted = tx
        .prepare_cached(
            "INSERT INTO records(namespace,key,value) VALUES (?1,?2,?3) ON CONFLICT DO NOTHING",
        )
        .map_err(db_error)?
        .execute(params![namespace, key, value])
        .map_err(db_error)?;
    if inserted == 0 {
        let previous: Vec<u8> = tx
            .prepare_cached("SELECT value FROM records WHERE namespace=?1 AND key=?2")
            .map_err(db_error)?
            .query_row(params![namespace, key], |row| row.get(0))
            .map_err(db_error)?;
        if previous != value {
            return Err(Error::Conflict(format!(
                "immutable metadata collision: {namespace}/{key}"
            )));
        }
    }
    Ok(())
}

fn apply_cas(
    db: &Connection,
    namespace: &str,
    key: &str,
    expected: Option<&[u8]>,
    value: Option<&[u8]>,
) -> Result<bool> {
    let affected = match (expected, value) {
        (None, Some(value)) => db
            .prepare_cached(
                "INSERT INTO records(namespace,key,value) VALUES (?1,?2,?3) ON CONFLICT DO NOTHING",
            )
            .map_err(db_error)?
            .execute(params![namespace, key, value])
            .map_err(db_error)?,
        (Some(expected), Some(value)) => db
            .prepare_cached(
                "UPDATE records SET value=?3 WHERE namespace=?1 AND key=?2 AND value=?4",
            )
            .map_err(db_error)?
            .execute(params![namespace, key, value, expected])
            .map_err(db_error)?,
        (Some(expected), None) => db
            .prepare_cached("DELETE FROM records WHERE namespace=?1 AND key=?2 AND value=?3")
            .map_err(db_error)?
            .execute(params![namespace, key, expected])
            .map_err(db_error)?,
        (None, None) => {
            let exists: bool = db
                .prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM records WHERE namespace=?1 AND key=?2)",
                )
                .map_err(db_error)?
                .query_row(params![namespace, key], |row| row.get(0))
                .map_err(db_error)?;
            return Ok(!exists);
        }
    };
    Ok(affected == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn durable_publication_and_compare_exchange() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::open(dir.path()).unwrap();
        let id = store.put_object(ObjectKind::Blob, b"binary\0data").unwrap();
        let seq = store.append_log("r", id).unwrap();
        assert_eq!(store.append_log("r", id).unwrap(), seq);
        assert_eq!(
            store
                .get_record("repo/r/changesets", &id.to_string())
                .unwrap(),
            Some(b"1".to_vec())
        );
        assert!(
            store
                .compare_and_swap("refs", "main", None, Some(b"v1"))
                .unwrap()
        );
        assert!(
            !store
                .compare_and_swap("refs", "main", None, Some(b"v2"))
                .unwrap()
        );
        drop(store);
        let store = LocalStore::open(dir.path()).unwrap();
        assert_eq!(
            store.get_object(ObjectKind::Blob, id).unwrap(),
            b"binary\0data"
        );
        assert_eq!(store.read_log("r", 0, 10).unwrap().len(), 1);
    }
    #[test]
    fn concurrent_cas_has_exactly_one_winner_across_connections() {
        let dir = tempfile::tempdir().unwrap();
        let stores: Vec<_> = (0..8)
            .map(|_| Arc::new(LocalStore::open(dir.path()).unwrap()))
            .collect();
        let threads: Vec<_> = stores
            .into_iter()
            .map(|s| {
                std::thread::spawn(move || {
                    s.compare_and_swap("refs", "main", None, Some(b"one"))
                        .unwrap()
                })
            })
            .collect();
        assert_eq!(
            threads
                .into_iter()
                .filter_map(|t| t.join().ok())
                .filter(|x| *x)
                .count(),
            1
        );
    }

    #[test]
    fn inline_batch_rolls_back_on_metadata_collision_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::open(dir.path()).unwrap();
        let objects = [
            PreparedObject::new(ObjectKind::Tree, b"new tree".to_vec()).unwrap(),
            PreparedObject::new(ObjectKind::Changeset, b"new commit".to_vec()).unwrap(),
        ];
        let id = objects[1].id();
        store.put_record("indexes", "root", b"existing").unwrap();
        let mut records = [ImmutableRecord {
            namespace: "indexes".into(),
            key: "root".into(),
            value: b"conflicting".to_vec(),
        }];
        assert!(
            store
                .publish_commit_with_objects("r", id, &objects, &records)
                .is_err()
        );
        for object in &objects {
            assert!(store.get_object(object.kind(), object.id()).is_err());
        }
        assert!(store.read_log("r", 0, 10).unwrap().is_empty());
        records[0].value = b"existing".to_vec();
        let sequence = store
            .publish_commit_with_objects("r", id, &objects, &records)
            .unwrap();
        assert_eq!(
            store
                .publish_commit_with_objects("r", id, &objects, &records)
                .unwrap(),
            sequence
        );
        for object in &objects {
            assert!(store.contains_inline(object.kind(), object.id()).unwrap());
            assert!(!store.path(object.kind(), object.id()).exists());
        }
        drop(store);
        let reopened = LocalStore::open(dir.path()).unwrap();
        for object in &objects {
            assert_eq!(
                reopened.get_object(object.kind(), object.id()).unwrap(),
                object.bytes()
            );
        }
        assert_eq!(reopened.read_log("r", 0, 10).unwrap().len(), 1);
    }

    #[test]
    fn publication_reads_old_loose_and_new_inline_and_large_objects() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::open(dir.path()).unwrap();
        let old = PreparedObject::new(ObjectKind::Blob, b"old loose file".to_vec()).unwrap();
        store
            .write_encoded(
                old.kind(),
                old.id(),
                &encode_object(old.kind(), old.bytes()).unwrap(),
            )
            .unwrap();
        store.put_record("legacy", "retained", b"metadata").unwrap();
        // Reopen a pre-inline schema; upgrading must preserve existing rows and
        // old loose objects without a content or identity migration.
        store
            .db
            .lock()
            .execute_batch("DROP TABLE inline_objects")
            .unwrap();
        drop(store);
        let store = LocalStore::open(dir.path()).unwrap();
        // Deterministic incompressible data exercises the large-file fallback.
        let mut random = 0x1234_5678_90ab_cdefu64;
        let bytes: Vec<_> = (0..1024 * 1024)
            .map(|_| {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                random as u8
            })
            .collect();
        let large = PreparedObject::new(ObjectKind::Tree, bytes).unwrap();
        let small = PreparedObject::new(ObjectKind::Changeset, b"small commit".to_vec()).unwrap();
        store
            .publish_commit_with_objects("r", small.id(), &[large.clone(), small.clone()], &[])
            .unwrap();
        assert!(store.path(large.kind(), large.id()).is_file());
        assert!(store.contains_inline(small.kind(), small.id()).unwrap());
        drop(store);
        let reopened = LocalStore::open(dir.path()).unwrap();
        assert_eq!(
            reopened.get_record("legacy", "retained").unwrap(),
            Some(b"metadata".to_vec())
        );
        for object in [old, large, small] {
            assert_eq!(
                reopened.get_object(object.kind(), object.id()).unwrap(),
                object.bytes()
            );
        }
    }

    #[test]
    fn concurrent_identical_object_batches_deduplicate_publication() {
        let dir = tempfile::tempdir().unwrap();
        let stores: Vec<_> = (0..8)
            .map(|_| LocalStore::open(dir.path()).unwrap())
            .collect();
        let barrier = Arc::new(std::sync::Barrier::new(stores.len()));
        let objects = Arc::new([
            PreparedObject::new(ObjectKind::Tree, b"concurrent tree".to_vec()).unwrap(),
            PreparedObject::new(ObjectKind::Changeset, b"concurrent commit".to_vec()).unwrap(),
        ]);
        let id = objects[1].id();
        let threads: Vec<_> = stores
            .into_iter()
            .map(|store| {
                let barrier = barrier.clone();
                let objects = objects.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let sequence = store
                        .publish_commit_with_objects("r", id, objects.as_slice(), &[])
                        .unwrap();
                    for object in objects.iter() {
                        assert_eq!(
                            store.get_object(object.kind(), object.id()).unwrap(),
                            object.bytes()
                        );
                    }
                    sequence
                })
            })
            .collect();
        let sequences: HashSet<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(sequences.len(), 1);
        let reopened = LocalStore::open(dir.path()).unwrap();
        assert_eq!(reopened.read_log("r", 0, 10).unwrap().len(), 1);
    }
    #[test]
    fn options_preserve_full_sync_and_normal_wal_autocheckpoint() {
        for mmap_size in [0, 256 * 1024 * 1024] {
            let dir = tempfile::tempdir().unwrap();
            let store = LocalStore::open_with_options(
                dir.path(),
                LocalStoreOptions {
                    mmap_size,
                    checkpoint_on_close: false,
                },
            )
            .unwrap();
            assert!(!store.mmap_configured.load(Ordering::Relaxed));
            store.put_record("options", "key", b"value").unwrap();
            let db = store.db.lock();
            let sync: i64 = db
                .pragma_query_value(None, "synchronous", |row| row.get(0))
                .unwrap();
            assert_eq!(sync, 2, "FULL synchronization must remain enabled");
            let auto: i64 = db
                .pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))
                .unwrap();
            assert_eq!(
                auto, 1000,
                "normal runtime auto-checkpointing must remain enabled"
            );
            let effective: i64 = db
                .pragma_query_value(None, "mmap_size", |row| row.get(0))
                .unwrap();
            assert!((0..=mmap_size as i64).contains(&effective));
            if mmap_size == 0 {
                assert_eq!(effective, 0);
            }
            assert!(
                db.db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE)
                    .unwrap()
            );
        }
    }
}
