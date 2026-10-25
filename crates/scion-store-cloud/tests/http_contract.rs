#![cfg(any(feature = "aws", feature = "gcp", feature = "azure"))]

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{Method, StatusCode},
    response::Response,
    routing::any,
};
use object_store::{ClientOptions, ObjectStore, RetryConfig};
use scion_store::{MemoryStore, Storage};
use scion_store_cloud::{CloudLimits, CloudStore};
use scion_types::{Error, ObjectId, ObjectKind};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Fixture {
    objects: BTreeMap<String, Bytes>,
    creates: usize,
    duplicates: usize,
    signed: usize,
}
type SharedFixture = Arc<Mutex<Fixture>>;

fn response(status: StatusCode, bytes: Bytes) -> Response {
    Response::builder()
        .status(status)
        .header("content-length", bytes.len())
        .header("etag", "\"contract-etag\"")
        .header("last-modified", "Sun, 25 Oct 2026 00:00:00 GMT")
        .header("x-goog-generation", "1")
        .header("x-ms-request-id", "contract-request")
        .body(Body::from(bytes))
        .unwrap()
}

async fn object_endpoint(State(state): State<SharedFixture>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    let signed = request.headers().contains_key("authorization");
    let conditional = request
        .headers()
        .get("if-none-match")
        .is_some_and(|h| h == "*")
        || request
            .headers()
            .get("x-goog-if-generation-match")
            .is_some_and(|h| h == "0");
    match method {
        Method::PUT => {
            if !signed || !conditional {
                return response(
                    StatusCode::BAD_REQUEST,
                    Bytes::from_static(b"missing signature or immutable precondition"),
                );
            }
            let body = to_bytes(request.into_body(), 4096).await.unwrap();
            let mut state = state.lock().unwrap();
            state.signed += 1;
            if state.objects.contains_key(&path) {
                state.duplicates += 1;
                return response(StatusCode::PRECONDITION_FAILED, Bytes::from_static(b"<Error><Code>PreconditionFailed</Code><Message>Already exists</Message></Error>"));
            }
            state.objects.insert(path, body);
            state.creates += 1;
            response(StatusCode::CREATED, Bytes::new())
        }
        Method::GET => {
            if !signed {
                return response(StatusCode::FORBIDDEN, Bytes::new());
            }
            let mut state = state.lock().unwrap();
            state.signed += 1;
            match state.objects.get(&path) {
                Some(bytes) => response(StatusCode::OK, bytes.clone()),
                None => response(
                    StatusCode::NOT_FOUND,
                    Bytes::from_static(b"<Error><Code>NoSuchKey</Code></Error>"),
                ),
            }
        }
        _ => response(StatusCode::METHOD_NOT_ALLOWED, Bytes::new()),
    }
}

async fn exercise_provider(build: impl FnOnce(&str) -> Arc<dyn ObjectStore>) {
    let fixture = SharedFixture::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let service = tokio::spawn(
        axum::serve(
            listener,
            Router::new()
                .route("/{*key}", any(object_endpoint))
                .with_state(fixture.clone()),
        )
        .into_future(),
    );
    let cloud = Arc::new(
        CloudStore::with_object_store(
            Arc::new(MemoryStore::new()),
            build(&endpoint),
            "contract",
            CloudLimits {
                max_encoded_size: 1024,
                operation_timeout: Duration::from_secs(5),
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let task_store = cloud.clone();
    let id = tokio::task::spawn_blocking(move || {
        let id = task_store
            .put_object(ObjectKind::Blob, b"over actual HTTP")
            .unwrap();
        assert_eq!(
            task_store
                .put_object(ObjectKind::Blob, b"over actual HTTP")
                .unwrap(),
            id
        );
        assert_eq!(
            task_store.get_object(ObjectKind::Blob, id).unwrap(),
            b"over actual HTTP"
        );
        assert!(matches!(
            task_store.get_encoded(ObjectKind::Blob, ObjectId([0; 32])),
            Err(Error::NotFound(_))
        ));
        id
    })
    .await
    .unwrap();
    {
        let mut state = fixture.lock().unwrap();
        assert_eq!(state.creates, 1);
        assert_eq!(state.duplicates, 1);
        assert!(state.signed >= 5);
        state
            .objects
            .values_mut()
            .for_each(|bytes| *bytes = Bytes::from(vec![0; 1025]));
    }
    let task_store = cloud.clone();
    tokio::task::spawn_blocking(move || {
        assert!(matches!(
            task_store.get_encoded(ObjectKind::Blob, id),
            Err(Error::Limit(_))
        ));
        assert!(matches!(
            task_store.put_object(ObjectKind::Blob, b"over actual HTTP"),
            Err(Error::Limit(_))
        ));
    })
    .await
    .unwrap();
    drop(cloud);
    service.abort();
}

fn options() -> ClientOptions {
    ClientOptions::new()
        .with_allow_http(true)
        .with_timeout(Duration::from_secs(3))
}
fn retry() -> RetryConfig {
    RetryConfig {
        max_retries: 0,
        ..Default::default()
    }
}

#[cfg(feature = "aws")]
#[tokio::test]
async fn s3_signed_conditional_put_get_and_body_limits() {
    exercise_provider(|endpoint| {
        Arc::new(
            object_store::aws::AmazonS3Builder::new()
                .with_bucket_name("bucket")
                .with_region("us-east-1")
                .with_access_key_id("contract-key")
                .with_secret_access_key("contract-secret")
                .with_endpoint(endpoint)
                .with_client_options(options())
                .with_retry(retry())
                .build()
                .unwrap(),
        )
    })
    .await;
}

#[cfg(feature = "gcp")]
#[tokio::test]
async fn gcs_signed_generation_precondition_get_and_body_limits() {
    exercise_provider(|endpoint| {
        Arc::new(
            object_store::gcp::GoogleCloudStorageBuilder::new()
                .with_bucket_name("bucket")
                .with_bearer_token("contract-token")
                .with_base_url(endpoint)
                .with_client_options(options())
                .with_retry(retry())
                .build()
                .unwrap(),
        )
    })
    .await;
}

#[cfg(feature = "azure")]
#[tokio::test]
async fn azure_signed_conditional_put_get_and_body_limits() {
    exercise_provider(|endpoint| {
        Arc::new(
            object_store::azure::MicrosoftAzureBuilder::new()
                .with_account("account")
                .with_container_name("bucket")
                .with_access_key("Y29udHJhY3Qta2V5")
                .with_endpoint(endpoint.to_owned())
                .with_client_options(options())
                .with_retry(retry())
                .build()
                .unwrap(),
        )
    })
    .await;
}
