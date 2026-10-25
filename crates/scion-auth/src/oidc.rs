//! Bounded OIDC bearer verification against administrator-configured providers.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::StreamExt;
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{JwkSet, KeyOperations, PublicKeyUse},
};
use scion_types::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use url::Url;

const MAX_DOCUMENT: usize = 1024 * 1024;
const MIN_REFRESH: Duration = Duration::from_secs(30);
fn algorithms() -> Vec<Algorithm> {
    vec![Algorithm::RS256]
}
fn key_ttl() -> u64 {
    300
}
fn leeway() -> u64 {
    30
}

/// Provider trust is configured independently of token-controlled claims/URLs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssuerConfig {
    pub issuer: String,
    pub audience: String,
    #[serde(default)]
    pub jwks_url: Option<String>,
    #[serde(default = "algorithms")]
    pub algorithms: Vec<Algorithm>,
    #[serde(default = "key_ttl")]
    pub cache_seconds: u64,
    #[serde(default = "leeway")]
    pub leeway_seconds: u64,
    /// Explicit development exception; HTTP remains restricted to loopback.
    #[serde(default)]
    pub allow_http_loopback: bool,
}
impl IssuerConfig {
    pub fn validate(&self) -> Result<()> {
        let issuer = self.url(&self.issuer)?;
        if issuer.query().is_some() || self.audience.is_empty() || self.audience.len() > 1024 {
            return Err(Error::Invalid("OIDC issuer/audience is invalid".into()));
        }
        if !(30..=3600).contains(&self.cache_seconds) || self.leeway_seconds > 120 {
            return Err(Error::Invalid(
                "OIDC cache must be 30..3600 seconds and leeway at most 120 seconds".into(),
            ));
        }
        if self.algorithms.is_empty()
            || self.algorithms.iter().any(|algorithm| {
                !matches!(
                    algorithm,
                    Algorithm::RS256
                        | Algorithm::RS384
                        | Algorithm::RS512
                        | Algorithm::PS256
                        | Algorithm::PS384
                        | Algorithm::PS512
                        | Algorithm::ES256
                        | Algorithm::ES384
                        | Algorithm::EdDSA
                )
            })
        {
            return Err(Error::Invalid(
                "OIDC requires explicitly allowed asymmetric signature algorithms".into(),
            ));
        }
        if let Some(url) = &self.jwks_url {
            self.url(url)?;
        }
        Ok(())
    }
    fn url(&self, value: &str) -> Result<Url> {
        let url = Url::parse(value).map_err(|_| Error::Invalid("invalid OIDC URL".into()))?;
        let loopback = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        if !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https"
                || (url.scheme() == "http" && self.allow_http_loopback && loopback))
        {
            return Err(Error::Invalid(
                "OIDC URLs require HTTPS without embedded credentials or fragments".into(),
            ));
        }
        Ok(url)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityBinding {
    pub issuer: String,
    pub subject: String,
}

pub(crate) struct VerifiedIdentity {
    pub binding: IdentityBinding,
    pub expires: u64,
    pub leeway: u64,
}
#[derive(Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    exp: u64,
}
#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
}
#[derive(Clone)]
struct SigningKey {
    algorithm: Algorithm,
    key: Arc<DecodingKey>,
}
#[derive(Default)]
struct KeyCache {
    keys: BTreeMap<String, SigningKey>,
    fetched: Option<Instant>,
    attempted: Option<Instant>,
}
struct Provider {
    config: IssuerConfig,
    cache: Mutex<KeyCache>,
}
pub(crate) struct Verifier {
    providers: BTreeMap<String, Provider>,
    client: reqwest::Client,
}

impl Verifier {
    pub(crate) fn new(configurations: &[IssuerConfig]) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::Transport("could not initialize OIDC transport".into()))?;
        let mut providers = BTreeMap::new();
        for config in configurations {
            config.validate()?;
            if providers
                .insert(
                    config.issuer.clone(),
                    Provider {
                        config: config.clone(),
                        cache: Mutex::new(KeyCache::default()),
                    },
                )
                .is_some()
            {
                return Err(Error::Invalid("duplicate OIDC issuer".into()));
            }
        }
        Ok(Self { providers, client })
    }
    pub(crate) async fn verify(&self, token: &str) -> Result<VerifiedIdentity> {
        if token.len() > 16 * 1024 {
            return Err(denied());
        }
        let header = decode_header(token).map_err(|_| denied())?;
        if header
            .crit
            .as_ref()
            .is_some_and(|critical| !critical.is_empty())
            || header.enc.is_some()
            || header.zip.is_some()
            || header.extras.get::<bool>("b64").map_err(|_| denied())? == Some(false)
        {
            return Err(denied());
        }
        let kid = header
            .kid
            .as_deref()
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .ok_or_else(denied)?;
        // This unverified claim only selects from an already-configured issuer
        // map. No URL or permissions from it are trusted before signature checks.
        let payload = token.split('.').nth(1).ok_or_else(denied)?;
        let hint: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).map_err(|_| denied())?)
                .map_err(|_| denied())?;
        let issuer = hint
            .get("iss")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(denied)?;
        let provider = self.providers.get(issuer).ok_or_else(denied)?;
        if !provider.config.algorithms.contains(&header.alg) {
            return Err(denied());
        }
        let signing = self.key(provider, kid).await?;
        if signing.algorithm != header.alg {
            return Err(denied());
        }
        let mut validation = Validation::new(header.alg);
        validation.set_audience(&[&provider.config.audience]);
        validation.set_issuer(&[&provider.config.issuer]);
        validation
            .required_spec_claims
            .extend(["iss", "sub", "aud", "exp"].into_iter().map(str::to_owned));
        validation.leeway = provider.config.leeway_seconds;
        validation.validate_nbf = true;
        let claims = decode::<Claims>(token, &signing.key, &validation)
            .map_err(|_| denied())?
            .claims;
        if claims.sub.is_empty() || claims.sub.len() > 512 {
            return Err(denied());
        }
        Ok(VerifiedIdentity {
            binding: IdentityBinding {
                issuer: claims.iss,
                subject: claims.sub,
            },
            expires: claims.exp,
            leeway: provider.config.leeway_seconds,
        })
    }
    async fn document<T: serde::de::DeserializeOwned>(&self, url: Url) -> Result<T> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| Error::Transport("OIDC provider is unavailable".into()))?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|length| length > MAX_DOCUMENT as u64)
        {
            return Err(Error::Transport(
                "OIDC provider returned an invalid or oversized response".into(),
            ));
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|_| Error::Transport("OIDC response was interrupted".into()))?;
            if bytes.len().saturating_add(chunk.len()) > MAX_DOCUMENT {
                return Err(Error::Limit("OIDC response exceeds 1 MiB".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| Error::Transport("OIDC provider returned invalid JSON".into()))
    }
    async fn key(&self, provider: &Provider, kid: &str) -> Result<SigningKey> {
        let mut cache = provider.cache.lock().await;
        let fresh = cache.fetched.is_some_and(|fetched| {
            fetched.elapsed() < Duration::from_secs(provider.config.cache_seconds)
        });
        if fresh && let Some(key) = cache.keys.get(kid) {
            return Ok(key.clone());
        }
        if cache
            .attempted
            .is_some_and(|attempt| attempt.elapsed() < MIN_REFRESH)
        {
            return Err(if fresh {
                denied()
            } else {
                Error::Transport("OIDC key refresh is temporarily unavailable".into())
            });
        }
        cache.attempted = Some(Instant::now());
        let url = if let Some(url) = &provider.config.jwks_url {
            provider.config.url(url)?
        } else {
            let url = provider.config.url(&format!(
                "{}/.well-known/openid-configuration",
                provider.config.issuer.trim_end_matches('/')
            ))?;
            let discovery: Discovery = self.document(url).await?;
            if discovery.issuer != provider.config.issuer {
                return Err(Error::Transport("OIDC discovery issuer mismatch".into()));
            }
            provider.config.url(&discovery.jwks_uri)?
        };
        let jwks: JwkSet = self.document(url).await?;
        if jwks.keys.len() > 256 {
            return Err(Error::Limit("OIDC provider returned too many keys".into()));
        }
        let mut keys = BTreeMap::new();
        for jwk in jwks.keys {
            if jwk
                .common
                .public_key_use
                .as_ref()
                .is_some_and(|usage| usage != &PublicKeyUse::Signature)
                || jwk
                    .common
                    .key_operations
                    .as_ref()
                    .is_some_and(|operations| !operations.contains(&KeyOperations::Verify))
            {
                continue;
            }
            let Some(id) = jwk
                .common
                .key_id
                .clone()
                .filter(|id| !id.is_empty() && id.len() <= 256)
            else {
                continue;
            };
            let algorithm = match jwk
                .common
                .key_algorithm
                .map(Algorithm::try_from)
                .transpose()
            {
                Ok(value) => value,
                Err(_) => continue,
            };
            if algorithm.is_some_and(|algorithm| !provider.config.algorithms.contains(&algorithm)) {
                continue;
            }
            let Ok(key) = DecodingKey::from_jwk(&jwk) else {
                continue;
            };
            // Bind every accepted key to one algorithm. A provider omitting
            // JWK `alg` is unambiguous only when policy permits one algorithm
            // in that key family (RFC 8725 section 3.1).
            let algorithm = if let Some(algorithm) = algorithm {
                if algorithm.family() != key.family() {
                    continue;
                }
                algorithm
            } else {
                let mut allowed = provider
                    .config
                    .algorithms
                    .iter()
                    .copied()
                    .filter(|algorithm| algorithm.family() == key.family());
                let Some(algorithm) = allowed.next() else {
                    continue;
                };
                if allowed.next().is_some() {
                    continue;
                }
                algorithm
            };
            if keys
                .insert(
                    id,
                    SigningKey {
                        algorithm,
                        key: Arc::new(key),
                    },
                )
                .is_some()
            {
                return Err(Error::Transport(
                    "OIDC provider returned duplicate key identifiers".into(),
                ));
            }
        }
        if keys.is_empty() {
            return Err(Error::Transport(
                "OIDC provider returned no usable signing keys".into(),
            ));
        }
        cache.keys = keys;
        cache.fetched = Some(Instant::now());
        cache.keys.get(kid).cloned().ok_or_else(denied)
    }
}

fn denied() -> Error {
    Error::Permission("invalid bearer identity".into())
}

#[cfg(test)]
#[path = "oidc_tests.rs"]
mod tests;
