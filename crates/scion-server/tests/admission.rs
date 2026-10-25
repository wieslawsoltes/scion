use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use scion_protocol::{ObjectRequest, ObjectTicket, RpcRequest, now_ms};
use scion_server::{ServerConfig, ServerState, router};
use scion_store::{MemoryStore, Storage};
use scion_types::{ObjectKind, RepoPath};
use std::sync::Arc;
use tower::ServiceExt;

fn request(path: &str, body: Vec<u8>) -> Request<Body> {
    Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn response_data_retains_admission_until_its_last_owner_is_dropped() {
    let store = Arc::new(MemoryStore::new());
    let id = store
        .put_object(ObjectKind::Blob, b"source content")
        .unwrap();
    let key = [7; 32];
    let app = router(
        ServerState::new(
            store,
            ServerConfig {
                development: true,
                capability_key: key,
                max_concurrent: 1,
                ..ServerConfig::default()
            },
        )
        .unwrap(),
    );
    let mut ticket = ObjectTicket {
        repo: "demo".into(),
        commit: id,
        path: RepoPath::new("source.txt").unwrap(),
        kind: ObjectKind::Blob,
        id,
        expires_ms: now_ms() + 60_000,
        signature: String::new(),
    };
    ticket.sign(&key).unwrap();
    let rpc_path = "/v1/repos/demo/rpc";
    let rpc = serde_json::to_vec(&RpcRequest::ListRefs).unwrap();
    let objects = serde_json::to_vec(&ObjectRequest { ticket }).unwrap();
    for (path, body) in [(rpc_path, rpc.clone()), ("/v1/repos/demo/objects", objects)] {
        let response = app.clone().oneshot(request(path, body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let rejected = app
            .clone()
            .oneshot(request(rpc_path, rpc.clone()))
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);

        // Taking a body frame out of the response must not release its budget.
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let held = bytes.clone();
        drop(bytes);
        let rejected = app
            .clone()
            .oneshot(request(rpc_path, rpc.clone()))
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
        drop(held);

        let accepted = app
            .clone()
            .oneshot(request(rpc_path, rpc.clone()))
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        drop(accepted);
    }
}
