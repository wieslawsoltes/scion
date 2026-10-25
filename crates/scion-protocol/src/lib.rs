//! Version 1 JSON control RPCs and identical binary object RPCs for every cache tier.
use scion_types::{Changeset, Error, ObjectId, ObjectKind, RepoPath, Result, canonical};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_BATCH_OBJECTS: usize = 256;
pub const MAX_RPC_BYTES: usize = 16 * 1024 * 1024;
pub const CAPABILITY_TTL_MS: i64 = 300_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Server,
    Cache,
    Daemon,
}
impl Tier {
    pub fn upstream(self) -> Option<Self> {
        match self {
            Self::Server => None,
            Self::Cache => Some(Self::Server),
            Self::Daemon => Some(Self::Cache),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub protocol: u32,
    pub tier: Tier,
    pub version: String,
}

/// Object-specific, short-lived capability. A hash alone never grants read access.
/// Cache operators share a 32-byte key with the authority over a trusted channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectTicket {
    pub repo: String,
    pub commit: ObjectId,
    pub path: RepoPath,
    pub kind: ObjectKind,
    pub id: ObjectId,
    pub expires_ms: i64,
    pub signature: String,
}
impl ObjectTicket {
    fn payload(&self) -> Result<Vec<u8>> {
        canonical(&(
            &self.repo,
            self.commit,
            &self.path,
            self.kind,
            self.id,
            self.expires_ms,
        ))
    }
    pub fn sign(&mut self, key: &[u8; 32]) -> Result<()> {
        self.signature = blake3::keyed_hash(key, &self.payload()?)
            .to_hex()
            .to_string();
        Ok(())
    }
    pub fn verify(&self, key: &[u8; 32], repo: &str, now: i64) -> Result<()> {
        if self.repo != repo
            || self.expires_ms < now
            || self.expires_ms > now.saturating_add(CAPABILITY_TTL_MS)
        {
            return Err(Error::Permission(
                "expired or incorrectly scoped object capability".into(),
            ));
        }
        let expected = blake3::keyed_hash(key, &self.payload()?)
            .to_hex()
            .to_string();
        let mut mismatch = expected.len() ^ self.signature.len();
        for (a, b) in expected.bytes().zip(self.signature.bytes()) {
            mismatch |= (a ^ b) as usize;
        }
        if mismatch != 0 {
            return Err(Error::Permission("invalid object capability".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectRequest {
    pub ticket: ObjectTicket,
}

/// Control RPC responses contain a JSON result or a structured error. Objects use
/// POST /v1/repos/{repo}/objects and an application/vnd.scion.object response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum RpcRequest {
    PutBlob {
        data: String,
    },
    Commit {
        changeset: Changeset,
    },
    GetChangeset {
        id: ObjectId,
    },
    Resolve {
        revision: String,
    },
    GetRef {
        name: String,
    },
    SetRef {
        name: String,
        expected: Option<ObjectId>,
        target: Option<ObjectId>,
    },
    ListRefs,
    RootTree {
        commit: ObjectId,
    },
    TreeAt {
        commit: ObjectId,
        path: RepoPath,
    },
    ListFiles {
        commit: ObjectId,
        prefix: RepoPath,
    },
    ListFilesPage {
        commit: ObjectId,
        prefix: RepoPath,
        after: Option<RepoPath>,
        limit: usize,
    },
    ReadFile {
        commit: ObjectId,
        path: RepoPath,
    },
    Diff {
        base: ObjectId,
        target: ObjectId,
    },
    Log {
        revision: ObjectId,
        limit: usize,
    },
    History {
        revision: ObjectId,
        path: RepoPath,
        limit: usize,
    },
    Blame {
        revision: ObjectId,
        path: RepoPath,
    },
    Search {
        revision: ObjectId,
        prefix: RepoPath,
        query: String,
        limit: usize,
    },
    Copies {
        revision: ObjectId,
    },
    IsAncestor {
        ancestor: ObjectId,
        descendant: ObjectId,
    },
    MergeBases {
        a: ObjectId,
        b: ObjectId,
    },
    NthParent {
        revision: ObjectId,
        distance: u64,
    },
    Merge {
        ours: ObjectId,
        theirs: ObjectId,
        author: String,
        message: String,
        timestamp_ms: i64,
    },
    Rebase {
        revision: ObjectId,
        onto: ObjectId,
    },
    Revert {
        revision: ObjectId,
        onto: ObjectId,
        mainline: usize,
        author: String,
        message: String,
        timestamp_ms: i64,
    },
    Bisect {
        good: ObjectId,
        bad: ObjectId,
        skip: Vec<ObjectId>,
    },
    ObjectAccess {
        commit: ObjectId,
        path: RepoPath,
        kind: ObjectKind,
        id: ObjectId,
    },
    ObjectAccessBatch {
        objects: Vec<ObjectWitness>,
    },
    ChangeLog {
        after: u64,
        limit: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectWitness {
    pub commit: ObjectId,
    pub path: RepoPath,
    pub kind: ObjectKind,
    pub id: ObjectId,
}

/// Lexically ordered directory page. `next` is an exclusive cursor, including
/// when all entries on a page were filtered by directory permissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePage {
    pub entries: std::collections::BTreeMap<RepoPath, scion_types::FileEntry>,
    pub next: Option<RepoPath>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcReply {
    pub result: Option<serde_json::Value>,
    pub error: Option<RpcError>,
}
impl RpcReply {
    pub fn success<T: Serialize>(value: T) -> Result<Self> {
        Ok(Self {
            result: Some(serde_json::to_value(value)?),
            error: None,
        })
    }
    pub fn failure(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            result: None,
            error: Some(RpcError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
    pub fn decode<T: serde::de::DeserializeOwned>(self) -> Result<T> {
        if let Some(e) = self.error {
            return Err(match e.code.as_str() {
                "not_found" => Error::NotFound(e.message),
                "permission" => Error::Permission(e.message),
                "conflict" => Error::Conflict(e.message),
                "invalid" => Error::Invalid(e.message),
                "limit" => Error::Limit(e.message),
                _ => Error::Transport(e.message),
            });
        }
        Ok(serde_json::from_value(
            self.result.unwrap_or(serde_json::Value::Null),
        )?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Parse a 256-bit hex secret without ever displaying the secret in an error.
pub fn parse_secret(value: &str) -> Result<[u8; 32]> {
    value
        .parse::<ObjectId>()
        .map(|id| id.0)
        .map_err(|_| Error::Invalid("capability key must be 64 hexadecimal characters".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capability_is_bound_to_object_repository_and_time() {
        let key = [7; 32];
        let id = ObjectId::hash(ObjectKind::Blob, b"x");
        let mut t = ObjectTicket {
            repo: "test".into(),
            commit: id,
            path: RepoPath::new("x").unwrap(),
            kind: ObjectKind::Blob,
            id,
            expires_ms: 1000,
            signature: String::new(),
        };
        t.sign(&key).unwrap();
        assert!(t.verify(&key, "test", 0).is_ok());
        assert!(t.verify(&key, "another", 0).is_err());
        assert!(t.verify(&key, "test", 1001).is_err());
        t.path = RepoPath::new("secret").unwrap();
        assert!(t.verify(&key, "test", 0).is_err());
    }
}
