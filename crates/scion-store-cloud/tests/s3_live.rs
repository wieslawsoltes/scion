#![cfg(feature = "aws")]
//! Set SCION_TEST_S3_ENDPOINT and AWS credentials to exercise a real disposable
//! S3-compatible service. CI supplies MinIO and a pre-created bucket.
use scion_store::{LocalStore, Storage};
use scion_store_cloud::{CloudConfig, CloudProvider, CloudStore};
use scion_types::{ObjectId, ObjectKind};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[test]
fn s3_service_concurrent_create_restart_and_metadata_cas() {
    let Ok(endpoint) = std::env::var("SCION_TEST_S3_ENDPOINT") else {
        eprintln!("S3 service fixture absent; set SCION_TEST_S3_ENDPOINT to run");
        return;
    };
    let bucket = std::env::var("SCION_TEST_S3_BUCKET").unwrap_or_else(|_| "scion-test".into());
    let mut config = CloudConfig::new(CloudProvider::S3, bucket);
    config.region = Some("us-east-1".into());
    config.allow_http = endpoint.starts_with("http://");
    config.endpoint = Some(endpoint);
    config.prefix = format!(
        "integration/{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    config.limits.operation_timeout = Duration::from_secs(10);
    let directory = tempfile::tempdir().unwrap();
    let metadata = Arc::new(LocalStore::open(directory.path()).unwrap());
    let store = Arc::new(CloudStore::connect(metadata.clone(), config.clone()).unwrap());
    let input = b"Scion on a real S3-compatible service\n";
    let id = ObjectId::hash(ObjectKind::Blob, input);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let store = store.clone();
            scope.spawn(move || assert_eq!(store.put_object(ObjectKind::Blob, input).unwrap(), id));
        }
    });
    assert!(
        store
            .compare_and_swap("refs", "main", None, Some(&id.0))
            .unwrap()
    );
    assert!(
        !store
            .compare_and_swap("refs", "main", None, Some(&id.0))
            .unwrap()
    );
    assert_eq!(store.get_object(ObjectKind::Blob, id).unwrap(), input);
    let encoded = store.get_encoded(ObjectKind::Blob, id).unwrap();
    store.put_encoded(ObjectKind::Blob, id, &encoded).unwrap();
    drop(store);
    drop(metadata);
    let reopened = CloudStore::connect(
        Arc::new(LocalStore::open(directory.path()).unwrap()),
        config,
    )
    .unwrap();
    assert_eq!(reopened.get_object(ObjectKind::Blob, id).unwrap(), input);
    assert_eq!(
        reopened.get_record("refs", "main").unwrap(),
        Some(id.0.to_vec())
    );
}
