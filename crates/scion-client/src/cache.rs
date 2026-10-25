//! A byte-bounded disk cache with a smaller verified in-memory tier.
use parking_lot::Mutex;
use scion_store::{MAX_OBJECT_SIZE, decode_object};
use scion_types::{Error, ObjectId, ObjectKind, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

type Key = (ObjectKind, ObjectId);
#[derive(Clone)]
struct Entry {
    size: u64,
    used: u64,
}
#[derive(Default)]
struct Index {
    entries: BTreeMap<Key, Entry>,
    memory: BTreeMap<Key, Arc<Vec<u8>>>,
    disk_lru: BTreeSet<(u64, Key)>,
    memory_lru: BTreeSet<(u64, Key)>,
    bytes: u64,
    memory_bytes: u64,
    clock: u64,
}

/// A cache owns its directory; do not share one directory between daemon processes.
/// Eviction is deterministic LRU and affects only re-fetchable immutable objects.
#[derive(Clone)]
pub struct ObjectCache {
    root: Arc<PathBuf>,
    max_bytes: u64,
    memory_bytes: u64,
    index: Arc<Mutex<Index>>,
}

impl ObjectCache {
    pub fn open(path: impl AsRef<Path>, max_bytes: u64) -> Result<Self> {
        fs::create_dir_all(path.as_ref())?;
        let root = fs::canonicalize(path.as_ref())?;
        let mut index = Index::default();
        for kind in [
            ObjectKind::Blob,
            ObjectKind::Tree,
            ObjectKind::Changeset,
            ObjectKind::Derived,
        ] {
            let dir = root.join(kind.as_str());
            fs::create_dir_all(&dir)?;
            if fs::symlink_metadata(&dir)?.file_type().is_symlink() {
                return Err(Error::Invalid(
                    "cache object directories must not be symlinks".into(),
                ));
            }
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Ok(id) = name.parse::<ObjectId>() else {
                    continue;
                };
                let size = entry.metadata()?.len();
                index.clock += 1;
                index.entries.insert(
                    (kind, id),
                    Entry {
                        size,
                        used: index.clock,
                    },
                );
                index.disk_lru.insert((index.clock, (kind, id)));
                index.bytes += size;
            }
        }
        let result = Self {
            root: Arc::new(root),
            max_bytes,
            memory_bytes: (max_bytes / 4).min(64 * 1024 * 1024),
            index: Arc::new(Mutex::new(index)),
        };
        result.trim(&mut result.index.lock())?;
        Ok(result)
    }
    fn path(&self, key: Key) -> PathBuf {
        self.root.join(key.0.as_str()).join(key.1.to_string())
    }
    pub fn bytes(&self) -> u64 {
        self.index.lock().bytes
    }
    pub fn len(&self) -> usize {
        self.index.lock().entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }
    pub fn get(&self, kind: ObjectKind, id: ObjectId) -> Result<Option<Arc<Vec<u8>>>> {
        let key = (kind, id);
        {
            let mut index = self.index.lock();
            let Some(old) = index.entries.get(&key).cloned() else {
                return Ok(None);
            };
            index.clock += 1;
            let used = index.clock;
            index.disk_lru.remove(&(old.used, key));
            index.disk_lru.insert((used, key));
            if index.memory.contains_key(&key) {
                index.memory_lru.remove(&(old.used, key));
                index.memory_lru.insert((used, key));
            }
            if let Some(entry) = index.entries.get_mut(&key) {
                entry.used = used;
            }
            if let Some(bytes) = index.memory.get(&key) {
                return Ok(Some(bytes.clone()));
            }
        }
        // No global index mutex is held during disk reads or decompression.
        let bytes = match fs::File::open(self.path(key)) {
            Ok(file) => {
                let limit = MAX_OBJECT_SIZE as u64 + 1024 * 1024;
                if file.metadata()?.len() > limit {
                    self.discard(key)?;
                    return Ok(None);
                }
                let mut bytes = Vec::new();
                file.take(limit + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > limit || decode_object(kind, id, &bytes).is_err() {
                    self.discard(key)?;
                    return Ok(None);
                }
                bytes
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.discard_index(key);
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let bytes = Arc::new(bytes);
        let mut index = self.index.lock();
        if index.entries.contains_key(&key) {
            self.remember(&mut index, key, bytes.clone());
        }
        Ok(Some(bytes))
    }
    pub fn put(&self, kind: ObjectKind, id: ObjectId, bytes: &[u8]) -> Result<()> {
        decode_object(kind, id, bytes)?;
        if bytes.len() as u64 > self.max_bytes {
            return Ok(());
        }
        let key = (kind, id);
        let mut tmp = tempfile::NamedTempFile::new_in(self.root.join(kind.as_str()))?;
        tmp.write_all(bytes)?;
        tmp.as_file().sync_all()?;
        let mut index = self.index.lock();
        tmp.persist(self.path(key))
            .map_err(|e| Error::Io(e.error))?;
        index.clock += 1;
        let used = index.clock;
        if let Some(old) = index.entries.insert(
            key,
            Entry {
                size: bytes.len() as u64,
                used,
            },
        ) {
            index.bytes -= old.size;
            index.disk_lru.remove(&(old.used, key));
            index.memory_lru.remove(&(old.used, key));
        }
        if let Some(old) = index.memory.remove(&key) {
            index.memory_bytes -= old.len() as u64;
        }
        index.disk_lru.insert((used, key));
        index.bytes += bytes.len() as u64;
        self.remember(&mut index, key, Arc::new(bytes.to_vec()));
        self.trim(&mut index)
    }
    fn discard_index(&self, key: Key) {
        let mut index = self.index.lock();
        if let Some(old) = index.entries.remove(&key) {
            index.bytes -= old.size;
            index.disk_lru.remove(&(old.used, key));
            index.memory_lru.remove(&(old.used, key));
        }
        if let Some(old) = index.memory.remove(&key) {
            index.memory_bytes -= old.len() as u64;
        }
    }
    fn discard(&self, key: Key) -> Result<()> {
        match fs::remove_file(self.path(key)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.discard_index(key);
        Ok(())
    }
    fn remember(&self, index: &mut Index, key: Key, bytes: Arc<Vec<u8>>) {
        if bytes.len() as u64 > self.memory_bytes {
            return;
        }
        if let Some(old) = index.memory.insert(key, bytes.clone()) {
            index.memory_bytes -= old.len() as u64;
        }
        let used = index.entries.get(&key).map_or(0, |entry| entry.used);
        index.memory_lru.insert((used, key));
        index.memory_bytes += bytes.len() as u64;
        while index.memory_bytes > self.memory_bytes {
            let Some((_, oldest)) = index.memory_lru.pop_first() else {
                break;
            };
            if let Some(old) = index.memory.remove(&oldest) {
                index.memory_bytes -= old.len() as u64;
            }
        }
    }
    fn trim(&self, index: &mut Index) -> Result<()> {
        while index.bytes > self.max_bytes {
            let Some((used, key)) = index.disk_lru.pop_first() else {
                break;
            };
            match fs::remove_file(self.path(key)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    index.disk_lru.insert((used, key));
                    return Err(e.into());
                }
            }
            if let Some(old) = index.entries.remove(&key) {
                index.bytes -= old.size;
                index.memory_lru.remove(&(old.used, key));
            }
            if let Some(old) = index.memory.remove(&key) {
                index.memory_bytes -= old.len() as u64;
            }
        }
        Ok(())
    }
    /// Export a portable framed snapshot. No filesystem names are trusted on import.
    pub fn export_snapshot(&self, mut output: impl Write) -> Result<usize> {
        output.write_all(b"SCIONSNP1")?;
        let keys: Vec<_> = self.index.lock().entries.keys().copied().collect();
        let mut count = 0;
        for (kind, id) in keys {
            let Some(bytes) = self.get(kind, id)? else {
                continue;
            };
            let tag = match kind {
                ObjectKind::Blob => 0,
                ObjectKind::Tree => 1,
                ObjectKind::Changeset => 2,
                ObjectKind::Derived => 3,
            };
            output.write_all(&[tag])?;
            output.write_all(&id.0)?;
            output.write_all(&(bytes.len() as u64).to_le_bytes())?;
            output.write_all(&bytes)?;
            count += 1;
        }
        Ok(count)
    }
    /// Incremental/idempotent import; incomplete final frames leave earlier verified objects usable.
    pub fn import_snapshot(&self, mut input: impl Read) -> Result<usize> {
        let mut magic = [0u8; 9];
        input.read_exact(&mut magic)?;
        if &magic != b"SCIONSNP1" {
            return Err(Error::Corrupt("invalid cache snapshot".into()));
        }
        let mut count = 0;
        loop {
            let mut tag = [0];
            match input.read(&mut tag) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }
            let kind = match tag[0] {
                0 => ObjectKind::Blob,
                1 => ObjectKind::Tree,
                2 => ObjectKind::Changeset,
                3 => ObjectKind::Derived,
                _ => return Err(Error::Corrupt("invalid snapshot object kind".into())),
            };
            let mut id = [0; 32];
            input.read_exact(&mut id)?;
            let mut size = [0; 8];
            input.read_exact(&mut size)?;
            let len = u64::from_le_bytes(size);
            if len > MAX_OBJECT_SIZE as u64 + 1024 * 1024 {
                return Err(Error::Limit("snapshot object exceeds size limit".into()));
            }
            let mut bytes = vec![0; len as usize];
            input.read_exact(&mut bytes)?;
            self.put(kind, ObjectId(id), &bytes)?;
            count += 1;
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_store::encode_object;
    #[test]
    fn bounded_cache_and_verified_snapshot_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let data = b"hello";
        let id = ObjectId::hash(ObjectKind::Blob, data);
        let encoded = encode_object(ObjectKind::Blob, data).unwrap();
        let cache = ObjectCache::open(temp.path().join("first"), encoded.len() as u64 * 2).unwrap();
        cache.put(ObjectKind::Blob, id, &encoded).unwrap();
        assert_eq!(
            &**cache.get(ObjectKind::Blob, id).unwrap().unwrap(),
            &encoded
        );
        let mut snapshot = Vec::new();
        assert_eq!(cache.export_snapshot(&mut snapshot).unwrap(), 1);
        let other = ObjectCache::open(temp.path().join("other"), 10000).unwrap();
        assert_eq!(other.import_snapshot(snapshot.as_slice()).unwrap(), 1);
        assert!(other.get(ObjectKind::Blob, id).unwrap().is_some());
        let bad = ObjectId::hash(ObjectKind::Blob, b"wrong");
        assert!(other.put(ObjectKind::Blob, bad, &encoded).is_err());
        for n in 0..30 {
            let d = n.to_string();
            let i = ObjectId::hash(ObjectKind::Blob, d.as_bytes());
            cache
                .put(
                    ObjectKind::Blob,
                    i,
                    &encode_object(ObjectKind::Blob, d.as_bytes()).unwrap(),
                )
                .unwrap();
        }
        assert!(cache.bytes() <= cache.max_bytes());
    }
    #[test]
    fn corrupted_disk_entry_is_evicted_on_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let id = ObjectId::hash(ObjectKind::Blob, b"x");
        let c = ObjectCache::open(temp.path(), 10000).unwrap();
        c.put(
            ObjectKind::Blob,
            id,
            &encode_object(ObjectKind::Blob, b"x").unwrap(),
        )
        .unwrap();
        fs::write(c.path((ObjectKind::Blob, id)), b"corrupt").unwrap();
        drop(c);
        let c = ObjectCache::open(temp.path(), 10000).unwrap();
        assert!(c.get(ObjectKind::Blob, id).unwrap().is_none());
    }
}
