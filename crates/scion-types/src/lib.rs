//! Versioned, deterministic data model shared by all Scion components.
//!
//! IDs hash canonical uncompressed bytes with an object-kind domain separator.
//! Ordered maps and ordered parents are part of the format contract.

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, str::FromStr};

mod tree_json;

/// Errors cross crate boundaries without depending on a particular backend.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("permission denied: {0}")]
    Permission(String),
    #[error("corrupt data: {0}")]
    Corrupt(String),
    #[error("resource limit: {0}")]
    Limit(String),
    #[error("storage: {0}")]
    Storage(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Common result type.
pub type Result<T> = std::result::Result<T, Error>;

/// BLAKE3 object identifier; serialized as 64 lowercase hexadecimal characters.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(pub [u8; 32]);

impl ObjectId {
    /// Compute a domain-separated, versioned object ID.
    pub fn hash(kind: ObjectKind, bytes: &[u8]) -> Self {
        let mut h = Self::hasher(kind);
        h.update(bytes);
        Self(*h.finalize().as_bytes())
    }

    /// Hash a stream with constant memory using the same domain and format as
    /// [`Self::hash`]. Returns the content ID and number of bytes read. Interrupted
    /// reads are retried; all other I/O errors prevent a partial result.
    pub fn hash_reader(kind: ObjectKind, mut reader: impl std::io::Read) -> Result<(Self, u64)> {
        let mut h = Self::hasher(kind);
        let mut buffer = [0; 64 * 1024];
        let mut length = 0u64;
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return Ok((Self(*h.finalize().as_bytes()), length)),
                Ok(count) => {
                    length = length
                        .checked_add(count as u64)
                        .ok_or_else(|| Error::Limit("stream length exceeds u64".into()))?;
                    h.update(&buffer[..count]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn hasher(kind: ObjectKind) -> blake3::Hasher {
        let mut h = blake3::Hasher::new();
        h.update(b"scion\0v1\0");
        h.update(kind.as_str().as_bytes());
        h.update(b"\0");
        h
    }

    fn hex(self) -> [u8; 64] {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = [0; 64];
        for (value, pair) in self.0.iter().zip(encoded.as_chunks_mut::<2>().0) {
            pair[0] = DIGITS[(value >> 4) as usize];
            pair[1] = DIGITS[(value & 15) as usize];
        }
        encoded
    }

    /// Short display form; never used for lookup or authorization.
    pub fn short(self) -> String {
        self.to_string()[..12].to_owned()
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(std::str::from_utf8(&self.hex()).expect("hex digits are UTF-8"))
    }
}
impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
impl FromStr for ObjectId {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        if s.len() != 64 {
            return Err(Error::Invalid(
                "object ID must contain 64 hex digits".into(),
            ));
        }
        let mut bytes = [0; 32];
        // The table keeps unpredictable digit/letter branches out of the loop
        // when parsing many content IDs in a directory or graph node. Invalid
        // bytes set the high bits and are rejected after the complete decode.
        const HEX: [u8; 256] = {
            let mut table = [0xff; 256];
            let mut digit = 0;
            while digit < 10 {
                table[b'0' as usize + digit] = digit as u8;
                digit += 1;
            }
            let mut letter = 0;
            while letter < 6 {
                table[b'a' as usize + letter] = letter as u8 + 10;
                table[b'A' as usize + letter] = letter as u8 + 10;
                letter += 1;
            }
            table
        };
        let mut invalid = 0;
        for (byte, pair) in bytes.iter_mut().zip(s.as_bytes().as_chunks::<2>().0) {
            let high = HEX[pair[0] as usize];
            let low = HEX[pair[1] as usize];
            invalid |= high | low;
            *byte = high << 4 | low;
        }
        if invalid & 0xf0 != 0 {
            return Err(Error::Invalid(
                "object ID must contain 64 hex digits".into(),
            ));
        }
        Ok(Self(bytes))
    }
}
impl Serialize for ObjectId {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(std::str::from_utf8(&self.hex()).expect("hex digits are UTF-8"))
    }
}
impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct IdVisitor;
        impl serde::de::Visitor<'_> for IdVisitor {
            type Value = ObjectId;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object ID containing 64 hexadecimal digits")
            }
            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<ObjectId, E> {
                value.parse().map_err(E::custom)
            }
        }
        d.deserialize_str(IdVisitor)
    }
}

/// Domain of a content-addressed object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Blob,
    Tree,
    Changeset,
    Derived,
}
impl ObjectKind {
    /// Stable format name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Changeset => "changeset",
            Self::Derived => "derived",
        }
    }
}
impl FromStr for ObjectKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "blob" => Ok(Self::Blob),
            "tree" => Ok(Self::Tree),
            "changeset" => Ok(Self::Changeset),
            "derived" => Ok(Self::Derived),
            _ => Err(Error::Invalid(format!("unknown object kind {s}"))),
        }
    }
}

/// Normalized repository-relative UTF-8 path. Empty denotes the repository root.
/// Backslashes, traversal, platform drive prefixes and metadata directories are rejected.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RepoPath(String);
impl RepoPath {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_path(&value)?;
        Ok(Self(value))
    }
    pub fn root() -> Self {
        Self(String::new())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|s| !s.is_empty())
    }
    pub fn join(&self, name: &str) -> Result<Self> {
        Self::new(if self.is_root() {
            name.to_owned()
        } else {
            format!("{}/{name}", self.0)
        })
    }
    /// Component-aware containment; `src` does not contain `src-other`.
    pub fn contains(&self, other: &Self) -> bool {
        self.is_root()
            || self == other
            || other
                .0
                .strip_prefix(&self.0)
                .is_some_and(|r| r.starts_with('/'))
    }
}

// Validate borrowed names so tree decoding does not clone every component.
fn validate_path(value: &str) -> Result<()> {
    if value.len() > 4096
        || value.contains(['\\', '\0', ':'])
        || value.starts_with('/')
        || value.ends_with('/') && !value.is_empty()
    {
        return Err(Error::Invalid(format!(
            "nonportable repository path: {value:?}"
        )));
    }
    if !value.is_empty()
        && value.split('/').any(|p| {
            p.is_empty()
                || p == "."
                || p == ".."
                || p.eq_ignore_ascii_case(".scion")
                || p.eq_ignore_ascii_case(".git")
                || p.len() > 255
                || reserved_component(p)
        })
    {
        return Err(Error::Invalid(format!("unsafe repository path: {value:?}")));
    }
    Ok(())
}

fn reserved_component(name: &str) -> bool {
    if name.ends_with(['.', ' ']) {
        return true;
    }
    let stem = name.split('.').next().unwrap_or(name);
    ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
        || (stem.len() == 4
            && (stem.as_bytes()[..3].eq_ignore_ascii_case(b"COM")
                || stem.as_bytes()[..3].eq_ignore_ascii_case(b"LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}
impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for RepoPath {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::new(s)
    }
}
impl<'de> Deserialize<'de> for RepoPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// Validate a repository or reference name used in metadata namespaces.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 240
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_. /".contains(&b))
        || name.contains("..")
        || name.starts_with('/')
        || name.ends_with('/')
        || name.contains("//")
        || name.contains(' ')
    {
        return Err(Error::Invalid(format!("invalid name: {name:?}")));
    }
    Ok(())
}

/// File mode retained independently of content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Regular,
    Executable,
    Symlink,
    Submodule,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub id: ObjectId,
    pub size: u64,
    pub kind: FileKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopySource {
    pub changeset: ObjectId,
    pub path: RepoPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FileChange {
    Put {
        entry: FileEntry,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        copy_from: Option<CopySource>,
    },
    Delete,
}
impl FileChange {
    pub fn put(entry: FileEntry) -> Self {
        Self::Put {
            entry,
            copy_from: None,
        }
    }
    pub fn entry(&self) -> Option<&FileEntry> {
        match self {
            Self::Put { entry, .. } => Some(entry),
            Self::Delete => None,
        }
    }
}

/// Immutable changeset; all deltas are relative to the first parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changeset {
    pub parents: Vec<ObjectId>,
    pub author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committer: Option<String>,
    pub timestamp_ms: i64,
    pub message: String,
    pub changes: BTreeMap<RepoPath, FileChange>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}
pub type CommitRequest = Changeset;
impl Changeset {
    pub fn id(&self) -> Result<ObjectId> {
        Ok(ObjectId::hash(ObjectKind::Changeset, &canonical(self)?))
    }
    pub fn validate(&self) -> Result<()> {
        if self.parents.len() > 64
            || self
                .parents
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.parents.len()
        {
            return Err(Error::Invalid(
                "duplicate parents or more than 64 parents".into(),
            ));
        }
        if self.author.is_empty()
            || self.author.len() > 4096
            || self.message.len() > 1_048_576
            || self.changes.len() > 1_000_000
        {
            return Err(Error::Limit("changeset metadata limits exceeded".into()));
        }
        let mut live = std::collections::BTreeSet::new();
        for (path, change) in &self.changes {
            if path.is_root() {
                return Err(Error::Invalid("cannot change the root as a file".into()));
            }
            if change.entry().is_some() {
                live.insert(path.as_str());
            }
        }
        for path in &live {
            for (i, _) in path.match_indices('/') {
                if live.contains(&path[..i]) {
                    return Err(Error::Conflict(format!(
                        "file/directory collision at {path}"
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TreeEntry {
    File { entry: FileEntry },
    Directory { id: ObjectId },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    pub entries: BTreeMap<String, TreeEntry>,
}
impl Tree {
    /// Decode stored tree JSON with a streaming path for the canonical shape.
    ///
    /// The accepted inputs and resulting values are identical to the derived
    /// [`Deserialize`] implementation. Noncanonical shapes use that decoder as
    /// a fallback; canonical bytes avoid internally tagged enum buffering.
    /// Names are validated separately by [`Self::validate`].
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        match serde_json::from_slice::<tree_json::CanonicalTree>(bytes) {
            Ok(tree) => Ok(Self {
                entries: tree.entries,
            }),
            Err(_) => Ok(serde_json::from_slice(bytes)?),
        }
    }

    /// Look up one entry without allocating a map or strings for its siblings.
    ///
    /// Every sibling payload and name is validated, including content after the
    /// matching entry. Noncanonical JSON retains the ordinary decoder's exact
    /// compatibility behavior. The result is equivalent to decoding, validating
    /// the complete tree, and retrieving `name` from its entries.
    pub fn entry_from_json(bytes: &[u8], name: &str) -> Result<Option<TreeEntry>> {
        match tree_json::lookup(bytes, name) {
            Ok(entry) => Ok(entry),
            Err(_) => {
                let tree = Self::from_json(bytes)?;
                tree.validate()?;
                Ok(tree.entries.get(name).cloned())
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        for name in self.entries.keys() {
            validate_path(name)?;
            if name.is_empty() || name.contains('/') {
                return Err(Error::Invalid(
                    "tree entries must be single path components".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Deterministic JSON; model fields have fixed order and maps use BTreeMap.
pub fn canonical<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(value)?)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub sequence: u64,
    pub id: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: RepoPath,
    pub before: Option<FileEntry>,
    pub after: Option<FileEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameLine {
    pub line: usize,
    pub changeset: ObjectId,
    pub original_line: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeConflict {
    pub path: RepoPath,
    pub reason: String,
    pub base: Option<FileEntry>,
    pub ours: Option<FileEntry>,
    pub theirs: Option<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeResult {
    pub changeset: Option<ObjectId>,
    pub conflicts: Vec<MergeConflict>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streaming_hash_matches_canonical_bytes_and_propagates_read_errors() {
        struct InterruptedOnce<'a> {
            bytes: &'a [u8],
            interrupted: bool,
        }
        impl std::io::Read for InterruptedOnce<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                std::io::Read::read(&mut self.bytes, output)
            }
        }
        for kind in [
            ObjectKind::Blob,
            ObjectKind::Tree,
            ObjectKind::Changeset,
            ObjectKind::Derived,
        ] {
            for length in [0, 1, 65_537, 131_091] {
                let data: Vec<u8> = (0..length).map(|n| (n % 251) as u8).collect();
                let stream = InterruptedOnce {
                    bytes: &data,
                    interrupted: false,
                };
                assert_eq!(
                    ObjectId::hash_reader(kind, stream).unwrap(),
                    (ObjectId::hash(kind, &data), length as u64)
                );
            }
        }
        struct Failed;
        impl std::io::Read for Failed {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::PermissionDenied.into())
            }
        }
        assert!(matches!(
            ObjectId::hash_reader(ObjectKind::Blob, Failed),
            Err(Error::Io(_))
        ));
    }
    #[test]
    fn identifiers_are_domain_separated_and_roundtrip() {
        let id = ObjectId::hash(ObjectKind::Blob, b"hello");
        assert_ne!(id, ObjectId::hash(ObjectKind::Tree, b"hello"));
        assert_eq!(id.to_string().parse::<ObjectId>().unwrap(), id);
        assert_eq!(
            serde_json::from_slice::<ObjectId>(&canonical(&id).unwrap()).unwrap(),
            id
        );
    }
    #[test]
    fn untrusted_paths_are_validated_during_deserialization() {
        for path in [
            "../a",
            "a/../b",
            "/tmp/a",
            "a//b",
            "a\\b",
            "C:x",
            ".scion/state",
            ".GIT/config",
            "a/",
            "NUL.txt",
            "folder/COM1",
            "trailing.",
            "trailing ",
        ] {
            assert!(RepoPath::new(path).is_err(), "{path}");
            assert!(
                serde_json::from_str::<RepoPath>(&serde_json::to_string(path).unwrap()).is_err()
            );
        }
        assert!(
            RepoPath::new("src")
                .unwrap()
                .contains(&RepoPath::new("src/lib.rs").unwrap())
        );
        assert!(
            !RepoPath::new("src")
                .unwrap()
                .contains(&RepoPath::new("src-other/lib.rs").unwrap())
        );
    }
}
