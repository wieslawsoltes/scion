use crate::{Storage, decode_object, encode_object};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use scion_types::{Error, LogEntry, ObjectId, ObjectKind, Result};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

fn db_error(e: rusqlite::Error) -> Error {
    Error::Storage(e.to_string())
}

/// Durable local backend: sharded immutable compressed files plus SQLite WAL.
/// Multiple processes may open a store. Shared deployments should use PostgresStore.
pub struct LocalStore {
    root: PathBuf,
    db: Mutex<Connection>,
}
impl LocalStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(path.as_ref())?;
        let root = fs::canonicalize(path)?;
        let db = Connection::open(root.join("metadata.sqlite3")).map_err(db_error)?;
        db.busy_timeout(Duration::from_secs(30)).map_err(db_error)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS records(namespace TEXT NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL, PRIMARY KEY(namespace,key)) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS changeset_log(sequence INTEGER PRIMARY KEY AUTOINCREMENT, repo TEXT NOT NULL, id TEXT NOT NULL, UNIQUE(repo,id));
            CREATE INDEX IF NOT EXISTS changeset_log_repo ON changeset_log(repo,sequence);").map_err(db_error)?;
        Ok(Self {
            root,
            db: Mutex::new(db),
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    fn path(&self, kind: ObjectKind, id: ObjectId) -> PathBuf {
        let hex = id.to_string();
        self.root
            .join("objects")
            .join(kind.as_str())
            .join(&hex[..2])
            .join(&hex[2..])
    }
    fn write_encoded(&self, kind: ObjectKind, id: ObjectId, encoded: &[u8]) -> Result<()> {
        let path = self.path(kind, id);
        if path.is_file() {
            return Ok(());
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
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.error.into()),
        }
        #[cfg(unix)]
        // A newly-created shard also needs its directory entries persisted in
        // each ancestor before a changeset may make the object reachable.
        for ancestor in parent.ancestors().take_while(|p| p.starts_with(&self.root)) {
            fs::File::open(ancestor)?.sync_all()?;
        }
        Ok(())
    }
}
impl Storage for LocalStore {
    fn put_object(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectId> {
        let id = ObjectId::hash(kind, bytes);
        if !self.path(kind, id).is_file() {
            self.write_encoded(kind, id, &encode_object(kind, bytes)?)?;
        }
        Ok(id)
    }
    fn get_object(&self, kind: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        decode_object(kind, id, &self.get_encoded(kind, id)?)
    }
    fn get_encoded(&self, kind: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        match fs::read(self.path(kind, id)) {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::NotFound(format!("{kind:?}/{id}")))
            }
            Err(e) => Err(e.into()),
        }
    }
    fn put_encoded(&self, kind: ObjectKind, id: ObjectId, bytes: &[u8]) -> Result<()> {
        decode_object(kind, id, bytes)?;
        self.write_encoded(kind, id, bytes)
    }
    fn get_record(&self, ns: &str, key: &str) -> Result<Option<Vec<u8>>> {
        self.db
            .lock()
            .query_row(
                "SELECT value FROM records WHERE namespace=?1 AND key=?2",
                params![ns, key],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)
    }
    fn put_record(&self, ns: &str, key: &str, value: &[u8]) -> Result<()> {
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
        let mut db = self.db.lock();
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let id = id.to_string();
        tx.execute(
            "INSERT INTO changeset_log(repo,id) VALUES (?1,?2) ON CONFLICT DO NOTHING",
            params![repo, id],
        )
        .map_err(db_error)?;
        tx.execute(
            "INSERT INTO records(namespace,key,value) VALUES (?1,?2,?3) ON CONFLICT DO NOTHING",
            params![format!("repo/{repo}/changesets"), id, b"1".as_slice()],
        )
        .map_err(db_error)?;
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
    fn read_log(&self, repo: &str, after: u64, limit: usize) -> Result<Vec<LogEntry>> {
        let db = self.db.lock();
        let mut stmt = db.prepare("SELECT sequence,id FROM changeset_log WHERE repo=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3").map_err(db_error)?;
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
        let mut stmt = db.prepare("SELECT key,value FROM records WHERE namespace=?1 AND key>=?2 ORDER BY key LIMIT ?3").map_err(db_error)?;
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
}
