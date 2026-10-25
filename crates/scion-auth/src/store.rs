use crate::{Access, IdentityBinding, Permissions, Policy, bearer_token, oidc::Verifier};
use parking_lot::RwLock;
use scion_types::{Error, Result};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

struct Snapshot {
    policy: Arc<Policy>,
    verifier: Verifier,
    fingerprint: [u8; 32],
    tokens: BTreeMap<scion_types::ObjectId, usize>,
    identities: BTreeMap<IdentityBinding, usize>,
    principals: BTreeMap<String, usize>,
}

/// Validated identity proof scoped to one policy version. It contains no bearer
/// token and cannot be deserialized or constructed by untrusted protocol input.
#[derive(Clone)]
pub struct Authenticated {
    principal: String,
    fingerprint: [u8; 32],
    expires: Option<u64>,
    leeway: u64,
    development: bool,
}
impl Authenticated {
    pub fn principal(&self) -> &str {
        &self.principal
    }
}

/// Atomically replaceable policy with shared, bounded OIDC signing-key caches.
pub struct PolicyStore {
    current: RwLock<Arc<Snapshot>>,
}
impl PolicyStore {
    pub fn new(policy: Policy) -> Result<Self> {
        Ok(Self {
            current: RwLock::new(Arc::new(Self::snapshot(policy)?)),
        })
    }
    fn snapshot(policy: Policy) -> Result<Snapshot> {
        policy.validate()?;
        let fingerprint = *blake3::hash(&serde_json::to_vec(&policy)?).as_bytes();
        let verifier = Verifier::new(&policy.issuers)?;
        let mut tokens = BTreeMap::new();
        let mut identities = BTreeMap::new();
        let mut principals = BTreeMap::new();
        for (index, principal) in policy.principals.iter().enumerate() {
            if !principal.token_hash.is_empty() {
                tokens.insert(principal.token_hash.parse()?, index);
            }
            for identity in &principal.identities {
                identities.insert(identity.clone(), index);
            }
            principals.insert(principal.name.clone(), index);
        }
        Ok(Snapshot {
            policy: Arc::new(policy),
            verifier,
            fingerprint,
            tokens,
            identities,
            principals,
        })
    }
    pub fn policy(&self) -> Arc<Policy> {
        self.current.read().policy.clone()
    }
    /// Return true only when a validated replacement changed the active policy.
    /// Failed validation leaves the last valid snapshot and its key cache intact.
    pub fn replace(&self, policy: Policy) -> Result<bool> {
        let replacement = Self::snapshot(policy)?;
        let mut current = self.current.write();
        if current.fingerprint == replacement.fingerprint {
            return Ok(false);
        }
        *current = Arc::new(replacement);
        Ok(true)
    }
    pub fn reload_file(&self, path: impl AsRef<Path>) -> Result<bool> {
        self.replace(Policy::from_file(path)?)
    }
    /// Authenticate before body extraction. Providers are selected only from
    /// the validated policy; token-controlled issuer/key URLs are never fetched.
    pub async fn authenticate(
        &self,
        headers: &http::HeaderMap,
        development: bool,
    ) -> Result<Authenticated> {
        let snapshot = self.current.read().clone();
        if development {
            return Ok(Authenticated {
                principal: "development".into(),
                fingerprint: snapshot.fingerprint,
                expires: None,
                leeway: 0,
                development: true,
            });
        }
        let token = bearer_token(headers)?;
        let digest = scion_types::ObjectId(*blake3::hash(token.as_bytes()).as_bytes());
        if let Some(index) = snapshot.tokens.get(&digest) {
            let principal = &snapshot.policy.principals[*index];
            return Ok(Authenticated {
                principal: principal.name.clone(),
                fingerprint: snapshot.fingerprint,
                expires: None,
                leeway: 0,
                development: false,
            });
        }
        let verified = snapshot.verifier.verify(token).await?;
        let index = snapshot
            .identities
            .get(&verified.binding)
            .ok_or_else(|| Error::Permission("federated identity has no policy binding".into()))?;
        let principal = &snapshot.policy.principals[*index];
        Ok(Authenticated {
            principal: principal.name.clone(),
            fingerprint: snapshot.fingerprint,
            expires: Some(verified.expires),
            leeway: verified.leeway,
            development: false,
        })
    }
    /// Recheck policy version and expiration after request buffering. This avoids
    /// granting stale permissions to requests that straddle a policy reload.
    pub fn authorize(&self, identity: &Authenticated, repo: &str) -> Result<Access> {
        let snapshot = self.current.read();
        if snapshot.fingerprint != identity.fingerprint {
            return Err(Error::Permission(
                "authentication policy changed; retry the request".into(),
            ));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Permission("invalid system clock".into()))?
            .as_secs();
        if identity
            .expires
            .is_some_and(|expires| now > expires.saturating_add(identity.leeway))
        {
            return Err(Error::Permission("bearer identity expired".into()));
        }
        if identity.development {
            return Ok(Access {
                principal: identity.principal.clone(),
                permissions: Permissions {
                    read: true,
                    write: true,
                    admin: true,
                    rules: vec![],
                },
            });
        }
        let index = snapshot
            .principals
            .get(&identity.principal)
            .ok_or_else(|| Error::Permission("principal no longer exists".into()))?;
        let principal = &snapshot.policy.principals[*index];
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

impl IdentityBinding {
    pub(crate) fn validate(&self, policy: &Policy) -> Result<()> {
        if self.subject.is_empty()
            || self.subject.len() > 512
            || !policy
                .issuers
                .iter()
                .any(|issuer| issuer.issuer == self.issuer)
        {
            return Err(Error::Invalid(
                "federated identity requires a configured issuer and nonempty bounded subject"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Principal;
    use http::{HeaderMap, HeaderValue};

    fn policy(token: &str) -> Policy {
        Policy {
            principals: vec![Principal {
                name: "test".into(),
                token_hash: blake3::hash(token.as_bytes())
                    .to_hex()
                    .to_string()
                    .to_uppercase(),
                repositories: BTreeMap::from([(
                    "demo".into(),
                    Permissions {
                        read: true,
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            }],
            ..Default::default()
        }
    }
    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("bEaReR {token}").parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn policy_replacement_invalidates_proofs_and_invalid_reload_keeps_last_good() {
        let initial = policy("old-test-token");
        let store = PolicyStore::new(initial.clone()).unwrap();
        let proof = store
            .authenticate(&headers("old-test-token"), false)
            .await
            .unwrap();
        assert!(store.authorize(&proof, "demo").unwrap().permissions.read);
        assert!(!store.replace(initial.clone()).unwrap());
        assert!(store.authorize(&proof, "demo").is_ok());
        let mut invalid = initial.clone();
        invalid.principals.push(invalid.principals[0].clone());
        assert!(store.replace(invalid).is_err());
        assert!(store.authorize(&proof, "demo").is_ok());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.json");
        std::fs::write(&path, b"{partial edit").unwrap();
        assert!(store.reload_file(&path).is_err());
        assert!(store.authorize(&proof, "demo").is_ok());
        let replacement = policy("new-test-token");
        std::fs::write(&path, serde_json::to_vec(&replacement).unwrap()).unwrap();
        assert!(store.reload_file(&path).unwrap());
        assert!(store.authorize(&proof, "demo").is_err());
        assert!(
            store
                .authenticate(&headers("old-test-token"), false)
                .await
                .is_err()
        );
        let current = store
            .authenticate(&headers("new-test-token"), false)
            .await
            .unwrap();
        assert!(store.authorize(&current, "demo").is_ok());
        let mut expired = current.clone();
        expired.expires = Some(1);
        assert!(store.authorize(&expired, "demo").is_err());
        // A valid empty policy is an explicit revoke-all replacement.
        store.replace(Policy::default()).unwrap();
        assert!(store.authorize(&current, "demo").is_err());
    }

    #[tokio::test]
    async fn ambiguous_or_malformed_authorization_and_duplicate_normalized_hashes_fail() {
        let p = policy("token");
        let store = PolicyStore::new(p.clone()).unwrap();
        let mut ambiguous = headers("token");
        ambiguous.append("authorization", HeaderValue::from_static("Bearer another"));
        assert!(store.authenticate(&ambiguous, false).await.is_err());
        for value in [
            "Bearer ",
            "Basic token",
            "Bearer token extra",
            "Bearer  token",
        ] {
            let mut h = HeaderMap::new();
            h.insert("authorization", value.parse().unwrap());
            assert!(store.authenticate(&h, false).await.is_err());
        }
        p.authenticate_headers(&headers("token")).unwrap();
        let mut duplicate = p.clone();
        let mut other = p.principals[0].clone();
        other.name = "other".into();
        other.token_hash = other.token_hash.to_lowercase();
        duplicate.principals.push(other);
        assert!(duplicate.validate().is_err());
    }
}
