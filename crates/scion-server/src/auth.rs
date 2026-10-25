//! Repository permissions plus component-aware, longest-prefix path policies.
use axum::http::HeaderMap;
use scion_types::{Error, RepoPath, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub principals: Vec<Principal>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    pub name: String,
    pub token_hash: String,
    pub repositories: BTreeMap<String, Permissions>,
}
impl std::fmt::Debug for Principal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Principal")
            .field("name", &self.name)
            .field("repositories", &self.repositories)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub write: bool,
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub rules: Vec<PathRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathRule {
    pub prefix: RepoPath,
    pub read: Option<bool>,
    pub write: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct Access {
    pub principal: String,
    pub permissions: Permissions,
}

impl Permissions {
    pub fn allows(&self, path: &RepoPath, write: bool) -> bool {
        let mut result = if write { self.write } else { self.read };
        let mut specificity = 0;
        for rule in &self.rules {
            if rule.prefix.contains(path) && rule.prefix.as_str().len() >= specificity {
                if let Some(allow) = if write { rule.write } else { rule.read } {
                    result = allow;
                    specificity = rule.prefix.as_str().len();
                }
            }
        }
        result
    }
    /// A raw tree exposes every child name, so it needs uniform subtree access.
    pub fn allows_subtree(&self, path: &RepoPath, write: bool) -> bool {
        self.allows(path, write)
            && self
                .rules
                .iter()
                .filter(|r| path.contains(&r.prefix))
                .all(|r| self.allows(&r.prefix, write))
    }
    pub fn require(&self, path: &RepoPath, write: bool) -> Result<()> {
        if self.allows(path, write) {
            Ok(())
        } else {
            Err(Error::Permission(format!(
                "{} access to {path}",
                if write { "write" } else { "read" }
            )))
        }
    }
    pub fn require_full(&self, write: bool) -> Result<()> {
        if self.allows_subtree(&RepoPath::root(), write) {
            Ok(())
        } else {
            Err(Error::Permission(
                "operation requires access to the complete repository".into(),
            ))
        }
    }
}

impl Policy {
    /// Authenticate headers before a potentially large body is read.
    pub fn authenticate_headers(&self, headers: &HeaderMap) -> Result<()> {
        let token = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Permission("bearer authentication required".into()))?;
        let hash = blake3::hash(token.as_bytes()).to_hex().to_string();
        if self
            .principals
            .iter()
            .any(|p| constant_eq(hash.as_bytes(), p.token_hash.as_bytes()))
        {
            Ok(())
        } else {
            Err(Error::Permission("invalid credentials".into()))
        }
    }
    pub fn validate(&self) -> Result<()> {
        let mut names = BTreeSet::new();
        let mut hashes = BTreeSet::new();
        for principal in &self.principals {
            scion_types::validate_name(&principal.name)?;
            if !names.insert(&principal.name)
                || !hashes.insert(&principal.token_hash)
                || principal
                    .token_hash
                    .parse::<scion_types::ObjectId>()
                    .is_err()
            {
                return Err(Error::Invalid("principal names and token hashes must be unique and hashes must be 64 hex digits".into()));
            }
            for (repo, permissions) in &principal.repositories {
                scion_types::validate_name(repo)?;
                let mut prefixes = BTreeSet::new();
                for rule in &permissions.rules {
                    if !prefixes.insert(&rule.prefix) {
                        return Err(Error::Invalid("duplicate path policy prefix".into()));
                    }
                }
            }
        }
        Ok(())
    }
    pub fn authenticate(
        &self,
        headers: &HeaderMap,
        repo: &str,
        development: bool,
    ) -> Result<Access> {
        if development {
            return Ok(Access {
                principal: "development".into(),
                permissions: Permissions {
                    read: true,
                    write: true,
                    admin: true,
                    rules: vec![],
                },
            });
        }
        let token = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Permission("bearer authentication required".into()))?;
        let hash = blake3::hash(token.as_bytes()).to_hex().to_string();
        let principal = self
            .principals
            .iter()
            .find(|p| constant_eq(hash.as_bytes(), p.token_hash.as_bytes()))
            .ok_or_else(|| Error::Permission("invalid credentials".into()))?;
        let permissions = principal
            .repositories
            .get(repo)
            .cloned()
            .ok_or_else(|| Error::Permission("repository access denied".into()))?;
        Ok(Access {
            principal: principal.name.clone(),
            permissions,
        })
    }
}
fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    let mut d = a.len() ^ b.len();
    for (a, b) in a.iter().zip(b) {
        d |= (a ^ b) as usize;
    }
    d == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn subtree_check_prevents_hidden_names_and_prefix_collisions() {
        let p = Permissions {
            read: true,
            write: false,
            admin: false,
            rules: vec![
                PathRule {
                    prefix: RepoPath::new("secret").unwrap(),
                    read: Some(false),
                    write: None,
                },
                PathRule {
                    prefix: RepoPath::new("secret/public").unwrap(),
                    read: Some(true),
                    write: None,
                },
            ],
        };
        assert!(!p.allows_subtree(&RepoPath::root(), false));
        assert!(!p.allows(&RepoPath::new("secret/x").unwrap(), false));
        assert!(p.allows(&RepoPath::new("secretary").unwrap(), false));
        assert!(p.allows_subtree(&RepoPath::new("secret/public").unwrap(), false));
    }
}
