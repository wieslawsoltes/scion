use super::*;
use crate::{Permissions, Policy, PolicyStore, Principal};
use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use base64::engine::general_purpose::STANDARD;
use http::HeaderMap;
use jsonwebtoken::{EncodingKey, Header, encode};
use serde_json::{Value, json};
use std::sync::{
    RwLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

struct ProviderState {
    url: String,
    keys: RwLock<Value>,
    requests: AtomicUsize,
    discoveries: AtomicUsize,
    failed: AtomicBool,
    oversized: AtomicBool,
}
struct Fixture {
    state: Arc<ProviderState>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = Arc::new(ProviderState {
            url: format!("http://{}", listener.local_addr().unwrap()),
            keys: RwLock::new(json!({"keys": [jwk(0, "old")]})),
            requests: AtomicUsize::new(0),
            discoveries: AtomicUsize::new(0),
            failed: AtomicBool::new(false),
            oversized: AtomicBool::new(false),
        });
        let app = Router::new()
            .route("/keys", get(keys))
            .route("/.well-known/openid-configuration", get(discovery))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { state, task }
    }
    fn config(&self) -> IssuerConfig {
        IssuerConfig {
            issuer: self.state.url.clone(),
            audience: "scion-tests".into(),
            jwks_url: None,
            algorithms: vec![Algorithm::EdDSA],
            cache_seconds: 30,
            leeway_seconds: 0,
            allow_http_loopback: true,
        }
    }
    fn claims(&self) -> Value {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        json!({"iss":self.state.url, "sub":"stable-subject", "aud":"scion-tests", "exp":now+300, "nbf":now-1})
    }
    fn policy(&self) -> Policy {
        Policy {
            issuers: vec![self.config()],
            principals: vec![Principal {
                name: "alice".into(),
                identities: vec![IdentityBinding {
                    issuer: self.state.url.clone(),
                    subject: "stable-subject".into(),
                }],
                repositories: BTreeMap::from([(
                    "demo".into(),
                    Permissions {
                        read: true,
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            }],
        }
    }
}
async fn keys(State(state): State<Arc<ProviderState>>) -> axum::response::Response {
    state.requests.fetch_add(1, Ordering::SeqCst);
    if state.failed.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if state.oversized.load(Ordering::SeqCst) {
        return "x".repeat(MAX_DOCUMENT + 1).into_response();
    }
    axum::Json(state.keys.read().unwrap().clone()).into_response()
}
async fn discovery(State(state): State<Arc<ProviderState>>) -> impl IntoResponse {
    state.discoveries.fetch_add(1, Ordering::SeqCst);
    axum::Json(json!({"issuer":state.url,"jwks_uri":format!("{}/keys",state.url)}))
}
fn fixture(index: usize) -> Value {
    serde_json::from_str(match index {
        0 => include_str!("../tests/fixtures/ed25519-0.json"),
        _ => include_str!("../tests/fixtures/ed25519-1.json"),
    })
    .unwrap()
}
fn jwk(index: usize, kid: &str) -> Value {
    json!({"kty":"OKP","crv":"Ed25519","use":"sig","key_ops":["verify"],"alg":"EdDSA","kid":kid,"x":fixture(index)["x"]})
}
fn token(index: usize, kid: &str, claims: &Value) -> String {
    let der = STANDARD
        .decode(fixture(index)["pkcs8"].as_str().unwrap())
        .unwrap();
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some(kid.into());
    // These untrusted token URLs must never change the configured key source.
    header.jku = Some("http://192.0.2.1/token-controlled".into());
    encode(&header, claims, &EncodingKey::from_ed_der(&der)).unwrap()
}
fn headers(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {value}").parse().unwrap());
    headers
}
async fn age(verifier: &Verifier) {
    for provider in verifier.providers.values() {
        let mut cache = provider.cache.lock().await;
        cache.fetched = Some(Instant::now() - Duration::from_secs(3601));
        cache.attempted = Some(Instant::now() - MIN_REFRESH);
    }
}

#[tokio::test]
async fn signed_identity_is_bound_to_exact_subject_and_repository() {
    let fixture = Fixture::start().await;
    let store = PolicyStore::new(fixture.policy()).unwrap();
    let proof = store
        .authenticate(&headers(&token(0, "old", &fixture.claims())), false)
        .await
        .unwrap();
    assert_eq!(proof.principal(), "alice");
    let access = store.authorize(&proof, "demo").unwrap();
    assert!(access.permissions.read);
    assert!(!access.permissions.write);
    assert!(store.authorize(&proof, "other").is_err());
    let mut claims = fixture.claims();
    claims["sub"] = json!("unmapped");
    assert!(
        store
            .authenticate(&headers(&token(0, "old", &claims)), false)
            .await
            .is_err()
    );
    assert_eq!(fixture.state.requests.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.state.discoveries.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn wrong_claims_signature_algorithm_and_unknown_issuer_are_rejected() {
    let fixture = Fixture::start().await;
    let verifier = Verifier::new(&[fixture.config()]).unwrap();
    for (field, value) in [
        ("iss", json!("https://attacker.invalid")),
        ("aud", json!("other")),
        ("sub", json!("")),
        ("exp", json!(1)),
        ("nbf", json!(u64::MAX)),
    ] {
        let mut claims = fixture.claims();
        claims[field] = value;
        assert!(
            verifier.verify(&token(0, "old", &claims)).await.is_err(),
            "accepted invalid {field}"
        );
    }
    for field in ["iss", "aud", "sub", "exp"] {
        let mut claims = fixture.claims();
        claims.as_object_mut().unwrap().remove(field);
        assert!(
            verifier.verify(&token(0, "old", &claims)).await.is_err(),
            "accepted missing {field}"
        );
    }
    assert!(
        verifier
            .verify(&token(1, "old", &fixture.claims()))
            .await
            .is_err()
    );
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some("old".into());
    let confused = encode(
        &header,
        &fixture.claims(),
        &EncodingKey::from_secret(b"public key is not a secret"),
    )
    .unwrap();
    assert!(verifier.verify(&confused).await.is_err());
    assert!(verifier.verify("malformed.jwt").await.is_err());
    assert!(verifier.verify(&"x".repeat(16 * 1024 + 1)).await.is_err());
    // Invalid issuer/algorithm cannot cause another provider request.
    assert_eq!(fixture.state.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn key_fetches_coalesce_and_rotation_does_not_trust_expired_cache() {
    let fixture = Fixture::start().await;
    let verifier = Arc::new(Verifier::new(&[fixture.config()]).unwrap());
    let valid = token(0, "old", &fixture.claims());
    let checks = (0..24).map(|_| {
        let verifier = verifier.clone();
        let valid = valid.clone();
        tokio::spawn(async move {
            verifier.verify(&valid).await.unwrap();
        })
    });
    for result in futures::future::join_all(checks).await {
        result.unwrap();
    }
    assert_eq!(fixture.state.requests.load(Ordering::SeqCst), 1);
    *fixture.state.keys.write().unwrap() = json!({"keys":[jwk(1,"new")]});
    let rotated = token(1, "new", &fixture.claims());
    for _ in 0..12 {
        assert!(verifier.verify(&rotated).await.is_err());
    }
    assert_eq!(
        fixture.state.requests.load(Ordering::SeqCst),
        1,
        "unknown kid amplified refreshes"
    );
    age(&verifier).await;
    verifier.verify(&rotated).await.unwrap();
    assert!(verifier.verify(&valid).await.is_err());
    fixture.state.failed.store(true, Ordering::SeqCst);
    age(&verifier).await;
    assert!(
        verifier.verify(&rotated).await.is_err(),
        "expired cache used during outage"
    );
    let requests = fixture.state.requests.load(Ordering::SeqCst);
    assert!(verifier.verify(&rotated).await.is_err());
    assert_eq!(
        fixture.state.requests.load(Ordering::SeqCst),
        requests,
        "failed refresh was not throttled"
    );
}

#[tokio::test]
async fn bounded_jwks_and_unique_key_identifiers_are_enforced() {
    let fixture = Fixture::start().await;
    let verifier = Verifier::new(&[fixture.config()]).unwrap();
    *fixture.state.keys.write().unwrap() = json!({"keys":[jwk(0,"old"),jwk(1,"old")]});
    let valid = token(0, "old", &fixture.claims());
    assert!(verifier.verify(&valid).await.is_err());
    age(&verifier).await;
    fixture.state.oversized.store(true, Ordering::SeqCst);
    assert!(verifier.verify(&valid).await.is_err());
    fixture.state.oversized.store(false, Ordering::SeqCst);
    *fixture.state.keys.write().unwrap() = json!({"keys":[jwk(0,"old")]});
    age(&verifier).await;
    verifier.verify(&valid).await.unwrap();
}

#[test]
fn provider_trust_rejects_unbounded_or_insecure_configuration() {
    let good: IssuerConfig =
        serde_json::from_value(json!({"issuer":"https://issuer.example","audience":"scion"}))
            .unwrap();
    good.validate().unwrap();
    for url in [
        "http://issuer.example",
        "https://user:password@issuer.example",
        "https://issuer.example/#fragment",
        "file:///tmp/key",
    ] {
        let mut bad = good.clone();
        bad.issuer = url.into();
        assert!(bad.validate().is_err());
    }
    let mut bad = good.clone();
    bad.algorithms = vec![Algorithm::HS256];
    assert!(bad.validate().is_err());
    let mut bad = good.clone();
    bad.cache_seconds = 0;
    assert!(bad.validate().is_err());
    let mut bad = good.clone();
    bad.leeway_seconds = 121;
    assert!(bad.validate().is_err());
    let mut bad = good;
    bad.allow_http_loopback = true;
    bad.issuer = "http://192.0.2.1".into();
    assert!(bad.validate().is_err());
}

#[tokio::test]
async fn rsa_default_algorithm_is_verified_and_ambiguous_key_algorithms_fail() {
    let fixture = Fixture::start().await;
    let key: Value = serde_json::from_str(include_str!("../tests/fixtures/rsa.json")).unwrap();
    let signing =
        EncodingKey::from_rsa_der(&STANDARD.decode(key["der"].as_str().unwrap()).unwrap());
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("rsa".into());
    let valid = encode(&header, &fixture.claims(), &signing).unwrap();
    let public = json!({"kty":"RSA","kid":"rsa","n":key["n"],"e":key["e"]});
    *fixture.state.keys.write().unwrap() = json!({"keys":[public.clone()]});
    let mut config = fixture.config();
    config.algorithms = vec![Algorithm::RS256];
    Verifier::new(&[config.clone()])
        .unwrap()
        .verify(&valid)
        .await
        .unwrap();
    config.algorithms.push(Algorithm::RS384);
    assert!(
        Verifier::new(&[config.clone()])
            .unwrap()
            .verify(&valid)
            .await
            .is_err()
    );
    let mut public = public;
    public["alg"] = json!("RS256");
    *fixture.state.keys.write().unwrap() = json!({"keys":[public]});
    let verifier = Verifier::new(&[config]).unwrap();
    verifier.verify(&valid).await.unwrap();
    header.crit = Some(vec!["unsupported-extension".into()]);
    let critical = encode(&header, &fixture.claims(), &signing).unwrap();
    assert!(verifier.verify(&critical).await.is_err());
    header.crit = None;
    header.extras.insert("b64", json!(false));
    let unencoded = encode(&header, &fixture.claims(), &signing).unwrap();
    assert!(verifier.verify(&unencoded).await.is_err());
}
