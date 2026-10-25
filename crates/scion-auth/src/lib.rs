//! Repository permissions, federated bearer identities, and atomic policy reload.
//!
//! [`PolicyStore`] authenticates static bearer tokens and signed OIDC tokens
//! before a request body is buffered. Its [`Authenticated`] proof must then be
//! authorized against the current policy before accessing repository data.
mod oidc;
mod store;

pub use oidc::{IdentityBinding, IssuerConfig};
pub use store::{Authenticated, PolicyStore};

use http::HeaderMap;
use scion_types::{Error, RepoPath, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default)]
    pub issuers: Vec<IssuerConfig>,
    pub principals: Vec<Principal>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    pub name: String,
    #[serde(default)]
    pub token_hash: String,
    #[serde(default)]
    pub identities: Vec<IdentityBinding>,
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
            if rule.prefix.contains(path)
                && rule.prefix.as_str().len() >= specificity
                && let Some(allow) = if write { rule.write } else { rule.read }
            {
                result = allow;
                specificity = rule.prefix.as_str().len();
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
    /// Read a bounded JSON policy, validating all identities and permissions.
    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self> {
        use std::io::Read;
        const LIMIT: u64 = 8 * 1024 * 1024;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(LIMIT + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > LIMIT {
            return Err(Error::Limit("policy exceeds 8 MiB".into()));
        }
        let policy: Self = serde_json::from_slice(&bytes)?;
        policy.validate()?;
        Ok(policy)
    }

    /// Authenticate static credentials. Use [`PolicyStore::authenticate`] for
    /// federated identities and policy-versioned proofs.
    pub fn authenticate_headers(&self, headers: &HeaderMap) -> Result<()> {
        self.token_principal(bearer_token(headers)?)
            .map(|_| ())
            .ok_or_else(|| Error::Permission("invalid credentials".into()))
    }
    pub fn validate(&self) -> Result<()> {
        let mut names = BTreeSet::new();
        let mut hashes = BTreeSet::new();
        let mut issuers = BTreeSet::new();
        let mut bindings = BTreeSet::new();
        for issuer in &self.issuers {
            issuer.validate()?;
            if !issuers.insert(&issuer.issuer) {
                return Err(Error::Invalid("duplicate OIDC issuer".into()));
            }
        }
        for principal in &self.principals {
            scion_types::validate_name(&principal.name)?;
            if !names.insert(&principal.name) {
                return Err(Error::Invalid("principal names must be unique".into()));
            }
            if principal.token_hash.is_empty() {
                if principal.identities.is_empty() {
                    return Err(Error::Invalid(
                        "principal requires a token hash or federated identity".into(),
                    ));
                }
            } else {
                let hash = principal
                    .token_hash
                    .parse::<scion_types::ObjectId>()
                    .map_err(|_| Error::Invalid("token hashes must be 64 hex digits".into()))?;
                if !hashes.insert(hash) {
                    return Err(Error::Invalid("token hashes must be unique".into()));
                }
            }
            for binding in &principal.identities {
                binding.validate(self)?;
                if !bindings.insert(binding) {
                    return Err(Error::Invalid(
                        "federated identities must bind to exactly one principal".into(),
                    ));
                }
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
        let principal = self
            .token_principal(bearer_token(headers)?)
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

    fn token_principal(&self, token: &str) -> Option<&Principal> {
        let hash = blake3::hash(token.as_bytes());
        self.principals.iter().find(|principal| {
            principal
                .token_hash
                .parse::<scion_types::ObjectId>()
                .is_ok_and(|expected| constant_eq(hash.as_bytes(), &expected.0))
        })
    }
}

fn bearer_token(headers: &HeaderMap) -> Result<&str> {
    // A duplicate Authorization header is ambiguous and must not depend on
    // which proxy or HTTP implementation happened to select the first value.
    let mut values = headers.get_all(http::header::AUTHORIZATION).iter();
    let value = values.next().and_then(|header| header.to_str().ok());
    if values.next().is_some() {
        return Err(Error::Permission("ambiguous bearer authentication".into()));
    }
    value
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, token)| {
            scheme.eq_ignore_ascii_case("bearer")
                && !token.is_empty()
                && token.len() <= 16 * 1024
                && !token.bytes().any(|byte| byte.is_ascii_whitespace())
        })
        .map(|(_, token)| token)
        .ok_or_else(|| Error::Permission("bearer authentication required".into()))
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
