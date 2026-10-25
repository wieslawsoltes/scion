//! Versioned, deterministic data model shared by all Scion components.
//!
//! IDs hash canonical uncompressed bytes with an object-kind domain separator.
//! Ordered maps and ordered parents are part of the format contract.

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, str::FromStr};

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
        let mut h = blake3::Hasher::new();
        h.update(b"scion\0v1\0");
        h.update(kind.as_str().as_bytes());
        h.update(b"\0");
        h.update(bytes);
        Self(*h.finalize().as_bytes())
    }

    /// Short display form; never used for lookup or authorization.
    pub fn short(self) -> String {
        self.to_string()[..12].to_owned()
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
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
        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Invalid(
                "object ID must contain 64 hex digits".into(),
            ));
        }
        let mut bytes = [0; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|e| Error::Invalid(e.to_string()))?;
        }
        Ok(Self(bytes))
    }
}
impl Serialize for ObjectId {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
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

fn reserved_component(name: &str) -> bool {
    if name.ends_with(['.', ' ']) {
        return true;
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
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
    pub fn validate(&self) -> Result<()> {
        for name in self.entries.keys() {
            let path = RepoPath::new(name.clone())?;
            if path.is_root() || path.components().count() != 1 {
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
