//! Disposable, checksummed stat cache. No source-control state lives here.
use scion_types::{Error, FileEntry, FileKind, ObjectId, ObjectKind, RepoPath, Result};
use std::{
    collections::BTreeMap,
    fs::{self, File, Metadata},
    io::{Cursor, Read, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const MAGIC: &[u8; 8] = b"SCNSTAT1";
const MAX_CACHE_BYTES: u64 = 512 * 1024 * 1024;
// Filesystems with coarse timestamps can hide multiple writes in one timestamp
// tick. An observation made inside this window must be hashed again, even after
// the wall clock advances. It is never promoted merely because time has passed.
const RACY_WINDOW_NS: u64 = 2_000_000_000;

#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct StatusStats {
    pub files_examined: u64,
    pub files_hashed: u64,
    pub bytes_hashed: u64,
    pub cache_hits: u64,
    pub untracked_entries: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint([u64; 10]);
impl Fingerprint {
    fn new(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self([
                metadata.dev(),
                metadata.ino(),
                metadata.len(),
                u64::from(metadata.mode()),
                metadata.nlink(),
                metadata.mtime() as u64,
                metadata.mtime_nsec() as u64,
                metadata.ctime() as u64,
                metadata.ctime_nsec() as u64,
                u64::from(metadata.file_type().is_symlink()),
            ])
        }
        #[cfg(not(unix))]
        {
            // std does not provide a portable change counter and stable file ID.
            // Never infer unchanged contents from length and mtime alone.
            let stamp = metadata.modified().ok().map(time_ns).unwrap_or_default();
            Self([
                0,
                0,
                metadata.len(),
                0,
                0,
                stamp / 1_000_000_000,
                stamp % 1_000_000_000,
                0,
                0,
                u64::from(metadata.file_type().is_symlink()),
            ])
        }
    }
    fn trustworthy(&self, observed: u64, now: u64) -> bool {
        if !cfg!(unix)
            || self.0[1] == 0
            || self.0[7] == 0
            || self.0[6] >= 1_000_000_000
            || self.0[8] >= 1_000_000_000
            || observed > now
        {
            return false;
        }
        let timestamp = |seconds: u64, nanos: u64| {
            // Negative and implausible timestamps are deliberately uncacheable.
            seconds.checked_mul(1_000_000_000)?.checked_add(nanos)
        };
        [
            timestamp(self.0[5], self.0[6]),
            timestamp(self.0[7], self.0[8]),
        ]
        .into_iter()
        .all(|time| {
            time.and_then(|t| t.checked_add(RACY_WINDOW_NS))
                .is_some_and(|safe_after| observed > safe_after)
        })
    }
}
fn time_ns(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| d.as_nanos().try_into().ok())
        .unwrap_or_default()
}
#[derive(Debug, Clone)]
struct Cached {
    fingerprint: Fingerprint,
    observed: u64,
    expected: FileKind,
    entry: FileEntry,
}
#[derive(Default)]
pub(crate) struct StatCache {
    entries: BTreeMap<RepoPath, Cached>,
    dirty: bool,
}
impl StatCache {
    pub fn load(path: &Path) -> Self {
        // Platforms without a trustworthy fingerprint verify content and avoid
        // paying to load/rewrite a sidecar that they could never safely reuse.
        if !cfg!(unix) {
            return Self::default();
        }
        // A damaged or unsupported sidecar only costs a content scan.
        Self::read(path).unwrap_or_default()
    }
    fn read(path: &Path) -> Result<Self> {
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_file() || meta.len() > MAX_CACHE_BYTES {
            return Err(Error::Corrupt("invalid stat-cache file".into()));
        }
        let mut bytes = Vec::new();
        File::open(path)?
            .take(MAX_CACHE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CACHE_BYTES {
            return Err(Error::Corrupt("stat-cache exceeds size limit".into()));
        }
        if bytes.len() < 48
            || &bytes[..8] != MAGIC
            || blake3::hash(&bytes[40..]).as_bytes() != &bytes[8..40]
        {
            return Err(Error::Corrupt(
                "stat-cache checksum or version mismatch".into(),
            ));
        }
        let mut reader = Cursor::new(&bytes[40..]);
        let count = read_u64(&mut reader)?;
        if count > bytes.len() as u64 / 130 {
            return Err(Error::Corrupt("invalid stat-cache entry count".into()));
        }
        let mut entries = BTreeMap::new();
        for _ in 0..count {
            let len = read_u64(&mut reader)?;
            if len > 4096 || len > bytes.len() as u64 {
                return Err(Error::Corrupt("invalid stat-cache path".into()));
            }
            let mut path = vec![0; len as usize];
            reader.read_exact(&mut path)?;
            let path =
                RepoPath::new(String::from_utf8(path).map_err(|e| Error::Corrupt(e.to_string()))?)?;
            let mut fp = [0; 10];
            for field in &mut fp {
                *field = read_u64(&mut reader)?;
            }
            let observed = read_u64(&mut reader)?;
            let expected = read_kind(&mut reader)?;
            let kind = read_kind(&mut reader)?;
            let size = read_u64(&mut reader)?;
            let mut id = [0; 32];
            reader.read_exact(&mut id)?;
            if entries
                .insert(
                    path,
                    Cached {
                        fingerprint: Fingerprint(fp),
                        observed,
                        expected,
                        entry: FileEntry {
                            id: ObjectId(id),
                            size,
                            kind,
                        },
                    },
                )
                .is_some()
            {
                return Err(Error::Corrupt("duplicate stat-cache path".into()));
            }
        }
        if reader.position() != (bytes.len() - 40) as u64 {
            return Err(Error::Corrupt("trailing stat-cache bytes".into()));
        }
        Ok(Self {
            entries,
            dirty: false,
        })
    }
    pub fn save(&mut self, path: &Path) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity(self.entries.len().saturating_mul(160));
        bytes.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        for (path, value) in &self.entries {
            bytes.extend_from_slice(&(path.as_str().len() as u64).to_le_bytes());
            bytes.extend_from_slice(path.as_str().as_bytes());
            for field in value.fingerprint.0 {
                bytes.extend_from_slice(&field.to_le_bytes());
            }
            bytes.extend_from_slice(&value.observed.to_le_bytes());
            bytes.push(kind_byte(value.expected));
            bytes.push(kind_byte(value.entry.kind));
            bytes.extend_from_slice(&value.entry.size.to_le_bytes());
            bytes.extend_from_slice(&value.entry.id.0);
        }
        let parent = path
            .parent()
            .ok_or_else(|| Error::Invalid("cache has no parent".into()))?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(MAGIC)?;
        temp.write_all(blake3::hash(&bytes).as_bytes())?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|e| Error::Io(e.error))?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        self.dirty = false;
        Ok(())
    }
    pub fn remove(&mut self, path: &RepoPath) {
        self.dirty |= self.entries.remove(path).is_some();
    }
    pub fn invalidate(&mut self, prefix: &RepoPath) {
        if prefix.is_root() {
            self.dirty |= !self.entries.is_empty();
            self.entries.clear();
            return;
        }
        // Ordered bounds limit invalidation to this path and its descendants.
        // A slash sorts immediately before '0' in portable UTF-8 paths.
        let upper = format!("{}0", prefix.as_str());
        let paths: Vec<_> = self
            .entries
            .range(prefix.clone()..)
            .take_while(|(path, _)| path.as_str() < upper.as_str())
            .filter(|(path, _)| prefix.contains(path))
            .map(|(path, _)| path.clone())
            .collect();
        for path in paths {
            self.remove(&path);
        }
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn retain(&mut self, present: impl Fn(&RepoPath) -> bool) {
        let count = self.entries.len();
        self.entries.retain(|path, _| present(path));
        self.dirty |= count != self.entries.len();
    }
    pub fn inspect(
        &mut self,
        path: &RepoPath,
        disk: &Path,
        metadata: &Metadata,
        expected: FileKind,
        stats: &mut StatusStats,
    ) -> Result<FileEntry> {
        let before = Fingerprint::new(metadata);
        let now = time_ns(SystemTime::now());
        if let Some(cached) = self.entries.get(path)
            && cached.expected == expected
            && cached.fingerprint == before
            && before.trustworthy(cached.observed, now)
        {
            stats.cache_hits += 1;
            return Ok(cached.entry.clone());
        }
        let (id, size, kind) = if metadata.file_type().is_symlink() {
            let target = fs::read_link(disk)?;
            let text = target
                .to_str()
                .ok_or_else(|| Error::Invalid("non-UTF-8 symlink target".into()))?;
            (
                ObjectId::hash(ObjectKind::Blob, text.as_bytes()),
                text.len() as u64,
                FileKind::Symlink,
            )
        } else if metadata.is_file() {
            if metadata.len() > scion_store::MAX_OBJECT_SIZE as u64 {
                return Err(Error::Limit(
                    "working file exceeds object size limit".into(),
                ));
            }
            let file = File::open(disk)?;
            // A symlink swapped in after lstat must not cause cached contents to
            // be associated with the earlier inode.
            if Fingerprint::new(&file.metadata()?) != before {
                return Err(Error::Conflict(format!(
                    "{path} changed while reading status; retry"
                )));
            }
            let (id, size) = ObjectId::hash_reader(
                ObjectKind::Blob,
                file.take(scion_store::MAX_OBJECT_SIZE as u64 + 1),
            )?;
            if size > scion_store::MAX_OBJECT_SIZE as u64 {
                return Err(Error::Limit(
                    "working file exceeds object size limit".into(),
                ));
            }
            let kind = if expected == FileKind::Submodule {
                FileKind::Submodule
            } else {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o111 != 0 {
                        FileKind::Executable
                    } else {
                        FileKind::Regular
                    }
                }
                #[cfg(not(unix))]
                {
                    if expected == FileKind::Executable {
                        FileKind::Executable
                    } else {
                        FileKind::Regular
                    }
                }
            };
            (id, size, kind)
        } else {
            return Err(Error::Invalid(format!(
                "unsupported filesystem entry {path}"
            )));
        };
        stats.files_hashed += 1;
        stats.bytes_hashed += size;
        let after = Fingerprint::new(&fs::symlink_metadata(disk)?);
        if before != after {
            self.remove(path);
            return Err(Error::Conflict(format!(
                "{path} changed while reading status; retry"
            )));
        }
        let entry = FileEntry { id, size, kind };
        if cfg!(unix) {
            self.entries.insert(
                path.clone(),
                Cached {
                    fingerprint: before,
                    observed: time_ns(SystemTime::now()),
                    expected,
                    entry: entry.clone(),
                },
            );
            self.dirty = true;
        }
        Ok(entry)
    }
}
fn kind_byte(kind: FileKind) -> u8 {
    match kind {
        FileKind::Regular => 0,
        FileKind::Executable => 1,
        FileKind::Symlink => 2,
        FileKind::Submodule => 3,
    }
}
fn read_kind(reader: &mut impl Read) -> Result<FileKind> {
    let mut byte = [0];
    reader.read_exact(&mut byte)?;
    match byte[0] {
        0 => Ok(FileKind::Regular),
        1 => Ok(FileKind::Executable),
        2 => Ok(FileKind::Symlink),
        3 => Ok(FileKind::Submodule),
        _ => Err(Error::Corrupt("unknown stat-cache kind".into())),
    }
}
fn read_u64(reader: &mut impl Read) -> Result<u64> {
    let mut bytes = [0; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn racy_observation_never_matures_without_rehashing() {
        let fp = Fingerprint([1, 1, 3, 0o100644, 1, 10, 1, 10, 1, 0]);
        assert!(!fp.trustworthy(10_000_000_002, 50_000_000_000));
        assert!(fp.trustworthy(13_000_000_000, 50_000_000_000) == cfg!(unix));
        assert!(!fp.trustworthy(13_000_000_000, 12_000_000_000));
    }
}
