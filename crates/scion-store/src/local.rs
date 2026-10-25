use crate::{
    ImmutableRecord, PreparedObject, Storage, decode_object, encode_object, validate_publication,
};
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use scion_types::{Error, LogEntry, ObjectId, ObjectKind, Result};
use std::collections::HashSet;
use std::{
    fs,
    io::Write,
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

/// Durable local backend: bounded small objects and metadata in SQLite WAL,
/// with larger immutable compressed objects in sharded files. Existing loose
/// objects remain readable; the encoded object format and content IDs do not change.
/// Multiple processes may open a store. Shared deployments should use PostgresStore.
pub struct LocalStore {
    root: PathBuf,
    db: Mutex<Connection>,
    read_only: bool,
    has_inline_objects: AtomicBool,
    #[cfg(unix)]
    durable_directories: Mutex<HashSet<PathBuf>>,
    durable_objects: Mutex<HashSet<(ObjectKind, ObjectId)>>,
}
impl LocalStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(path.as_ref())?;
        let root = fs::canonicalize(path)?;
        let db = Connection::open(root.join("metadata.sqlite3")).map_err(db_error)?;
        db.busy_timeout(Duration::from_secs(30)).map_err(db_error)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS records(namespace TEXT NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL, PRIMARY KEY(namespace,key)) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS inline_objects(kind TEXT NOT NULL, id BLOB NOT NULL, encoded BLOB NOT NULL, PRIMARY KEY(kind,id)) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS changeset_log(sequence INTEGER PRIMARY KEY AUTOINCREMENT, repo TEXT NOT NULL, id TEXT NOT NULL, UNIQUE(repo,id));
            CREATE INDEX IF NOT EXISTS changeset_log_repo ON changeset_log(repo,sequence);").map_err(db_error)?;
        Ok(Self {
            root,
            db: Mutex::new(db),
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
        let root = fs::canonicalize(path)?;
        let db = Connection::open_with_flags(
            root.join("metadata.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(db_error)?;
        db.busy_timeout(Duration::from_secs(30)).map_err(db_error)?;
        // Old file-only stores remain readable without an implicit migration.
        let has_inline_objects = has_inline_table(&db)?;
        Ok(Self {
            root,
            db: Mutex::new(db),
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
    fn inline_encoded(&self, kind: ObjectKind, id: ObjectId) -> Result<Option<Vec<u8>>> {
        let db = self.db.lock();
        if !self.has_inline_objects.load(Ordering::Relaxed) {
            if !has_inline_table(&db)? {
                return Ok(None);
            }
            // A writer can upgrade a legacy store after this reader opens.
            self.has_inline_objects.store(true, Ordering::Relaxed);
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
        let db = self.db.lock();
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
        let mut db = self.db.lock();
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
    ) -> Result<u64> {
        let mut db = self.db.lock();
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        insert_objects(&tx, objects)?;
        for record in records {
            insert_immutable(&tx, &record.namespace, &record.key, &record.value)?;
        }
        let id = id.to_string();
        tx.execute(
            "INSERT INTO changeset_log(repo,id) VALUES (?1,?2) ON CONFLICT DO NOTHING",
            params![repo, id],
        )
        .map_err(db_error)?;
        insert_immutable(&tx, &format!("repo/{repo}/changesets"), &id, b"1")?;
        let seq: i64 = tx
            .query_row(
                "SELECT sequence FROM changeset_log WHERE repo=?1 AND id=?2",
                params![repo, id],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(seq as u64)
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
        let db = self.db.lock();
        db.prepare_cached("SELECT value FROM records WHERE namespace=?1 AND key=?2")
            .map_err(db_error)?
            .query_row(params![ns, key], |r| r.get(0))
            .optional()
            .map_err(db_error)
    }
    fn put_record(&self, ns: &str, key: &str, value: &[u8]) -> Result<()> {
        self.require_writable()?;
        let db = self.db.lock();
        db.execute(
            "INSERT INTO records(namespace,key,value) VALUES (?1,?2,?3) ON CONFLICT DO NOTHING",
            params![ns, key, value],
        )
        .map_err(db_error)?;
        let existing: Vec<u8> = db
            .query_row(
                "SELECT value FROM records WHERE namespace=?1 AND key=?2",
                params![ns, key],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        if existing != value {
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
        self.require_writable()?;
        let mut db = self.db.lock();
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let current: Option<Vec<u8>> = tx
            .query_row(
                "SELECT value FROM records WHERE namespace=?1 AND key=?2",
                params![ns, key],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        if current.as_deref() != expected {
            return Ok(false);
        }
        if let Some(value) = value {
            tx.execute("INSERT INTO records(namespace,key,value) VALUES (?1,?2,?3) ON CONFLICT(namespace,key) DO UPDATE SET value=excluded.value", params![ns,key,value]).map_err(db_error)?;
        } else {
            tx.execute(
                "DELETE FROM records WHERE namespace=?1 AND key=?2",
                params![ns, key],
            )
            .map_err(db_error)?;
        }
        tx.commit().map_err(db_error)?;
        Ok(true)
    }
    fn append_log(&self, repo: &str, id: ObjectId) -> Result<u64> {
        self.publish_commit(repo, id, &[])
    }
    fn publish_commit(&self, repo: &str, id: ObjectId, records: &[ImmutableRecord]) -> Result<u64> {
        self.require_writable()?;
        validate_publication(repo, id, records)?;
        self.publish_batch(repo, id, &[], records)
    }
    fn publish_commit_with_objects(
        &self,
        repo: &str,
        id: ObjectId,
        objects: &[PreparedObject],
        records: &[ImmutableRecord],
    ) -> Result<u64> {
        self.require_writable()?;
        validate_publication(repo, id, records)?;
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
        self.publish_batch(repo, id, &pending, records)
    }
    fn read_log(&self, repo: &str, after: u64, limit: usize) -> Result<Vec<LogEntry>> {
        let db = self.db.lock();
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
        let db = self.db.lock();
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

fn insert_immutable(
    tx: &rusqlite::Transaction<'_>,
    namespace: &str,
    key: &str,
    value: &[u8],
) -> Result<()> {
    let inserted = tx
        .execute(
            "INSERT INTO records(namespace,key,value) VALUES (?1,?2,?3) ON CONFLICT DO NOTHING",
            params![namespace, key, value],
        )
        .map_err(db_error)?;
    if inserted == 0 {
        let previous: Vec<u8> = tx
            .query_row(
                "SELECT value FROM records WHERE namespace=?1 AND key=?2",
                params![namespace, key],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        if previous != value {
            return Err(Error::Conflict(format!(
                "immutable metadata collision: {namespace}/{key}"
            )));
        }
    }
    Ok(())
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
}
