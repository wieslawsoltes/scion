# scion-auth

Reusable repository and directory authorization, verified OIDC bearer identities,
and atomic policy replacement for Scion services. Existing hashed bearer policies
remain supported. Federated identities are mapped by the exact issuer and subject;
unmapped claims never grant repository access.

`PolicyStore` validates policy replacement before making it visible. A request
authenticated against an older policy cannot acquire permissions after a reload.
OIDC verification uses configured issuers, explicit audience/algorithm validation,
required expiration, public JWKS, bounded responses, key caching and throttled
refresh. Token-controlled key URLs are never fetched. Expired key caches fail
closed if their provider cannot be refreshed.

```rust,no_run
use scion_auth::{Policy, PolicyStore};

# async fn example() -> scion_types::Result<()> {
let identities = PolicyStore::new(Policy::from_file("policy.json")?)?;
let mut headers = http::HeaderMap::new();
headers.insert("authorization", "Bearer provider-issued-token".parse().unwrap());
let proof = identities.authenticate(&headers, false).await?;
let access = identities.authorize(&proof, "product")?;
access.permissions.require(&scion_types::RepoPath::new("src/lib.rs")?, false)?;
// Replacement is atomic; a validation error retains the previous policy.
identities.reload_file("policy.json")?;
# Ok(())
# }
```

See the [identity guide](https://github.com/wieslawsoltes/scion/blob/main/docs/identity.md) for policy examples, key rotation and service
integration. This crate does not provision accounts or register applications with
an external identity provider.
