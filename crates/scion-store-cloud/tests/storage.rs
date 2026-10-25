use bytes::Bytes;
use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory};
use scion_store::{ImmutableRecord, LocalStore, MemoryStore, Storage, encode_object};
use scion_store_cloud::{CloudLimits, CloudStore};
use scion_types::{Error, ObjectId, ObjectKind};
use std::sync::Arc;

fn store(objects: Arc<dyn ObjectStore>) -> CloudStore {
    CloudStore::with_object_store(
        Arc::new(MemoryStore::new()),
        objects,
        "tests/scion",
        CloudLimits::default(),
    )
    .unwrap()
}

#[test]
fn immutable_objects_are_idempotent_and_metadata_remains_transactional() {
    let directory = tempfile::tempdir().unwrap();
    let metadata = Arc::new(LocalStore::open(directory.path()).unwrap());
    let objects = Arc::new(InMemory::new());
    let cloud = CloudStore::with_object_store(
        metadata.clone(),
        objects.clone(),
        "team/repo",
        CloudLimits::default(),
    )
    .unwrap();
    let bytes = b"verified cloud source\n";
    let id = cloud.put_object(ObjectKind::Blob, bytes).unwrap();
    assert_eq!(cloud.put_object(ObjectKind::Blob, bytes).unwrap(), id);
    assert_eq!(cloud.get_object(ObjectKind::Blob, id).unwrap(), bytes);
    assert_eq!(
        cloud.get_encoded(ObjectKind::Blob, id).unwrap(),
        encode_object(ObjectKind::Blob, bytes).unwrap()
    );
    assert!(matches!(
        metadata.get_object(ObjectKind::Blob, id),
        Err(Error::NotFound(_))
    ));
    assert!(
        cloud
            .compare_and_swap("refs", "main", None, Some(&id.0))
            .unwrap()
    );
    assert!(
        !cloud
            .compare_and_swap("refs", "main", None, Some(&id.0))
            .unwrap()
    );
    let record = ImmutableRecord {
        namespace: "derived".into(),
        key: "root".into(),
        value: b"tree".to_vec(),
    };
    let seq = cloud
        .publish_commit("demo", id, std::slice::from_ref(&record))
        .unwrap();
    assert_eq!(cloud.publish_commit("demo", id, &[record]).unwrap(), seq);
    assert_eq!(cloud.read_log("demo", 0, 10).unwrap().len(), 1);
    drop(cloud);
    drop(metadata);
    let metadata = Arc::new(LocalStore::open(directory.path()).unwrap());
    let restored = CloudStore::with_object_store(
        metadata.clone(),
        objects,
        "team/repo",
        CloudLimits::default(),
    )
    .unwrap();
    assert_eq!(
        restored.get_record("refs", "main").unwrap(),
        Some(id.0.to_vec())
    );
    assert_eq!(
        restored.get_record("derived", "root").unwrap(),
        Some(b"tree".to_vec())
    );
    assert_eq!(restored.read_log("demo", 0, 10).unwrap()[0].sequence, seq);
    assert_eq!(restored.get_object(ObjectKind::Blob, id).unwrap(), bytes);
}

#[tokio::test(flavor = "current_thread")]
async fn corruption_is_detected_without_overwriting_existing_objects() {
    let objects = Arc::new(InMemory::new());
    let cloud = store(objects.clone());
    let bytes = b"valid";
    let id = cloud.put_object(ObjectKind::Blob, bytes).unwrap();
    let key = cloud.object_key(ObjectKind::Blob, id);
    let corrupted = Bytes::from_static(b"corrupt preexisting object");
    objects.put(&key, corrupted.clone().into()).await.unwrap();
    assert!(matches!(
        cloud.get_encoded(ObjectKind::Blob, id),
        Err(Error::Corrupt(_))
    ));
    assert!(matches!(
        cloud.put_object(ObjectKind::Blob, bytes),
        Err(Error::Corrupt(_))
    ));
    assert_eq!(
        objects.get(&key).await.unwrap().bytes().await.unwrap(),
        corrupted
    );
    assert!(
        cloud
            .put_encoded(
                ObjectKind::Blob,
                ObjectId([0; 32]),
                &encode_object(ObjectKind::Blob, bytes).unwrap()
            )
            .is_err()
    );
    drop(cloud); // Must not panic when dropping its private runtime in async code.
}

#[tokio::test]
async fn equivalent_compression_is_accepted_but_remote_size_is_bounded() {
    let objects = Arc::new(InMemory::new());
    let cloud = store(objects.clone());
    let bytes = b"same content repeated repeated repeated";
    let id = ObjectId::hash(ObjectKind::Blob, bytes);
    let mut other_encoding = encode_object(ObjectKind::Blob, bytes).unwrap()[..49].to_vec();
    zstd::stream::copy_encode(bytes.as_slice(), &mut other_encoding, 10).unwrap();
    objects
        .put(
            &cloud.object_key(ObjectKind::Blob, id),
            Bytes::from(other_encoding).into(),
        )
        .await
        .unwrap();
    assert_eq!(cloud.put_object(ObjectKind::Blob, bytes).unwrap(), id);
    let bounded = CloudStore::with_object_store(
        Arc::new(MemoryStore::new()),
        objects.clone(),
        "small",
        CloudLimits {
            max_encoded_size: 64,
            ..Default::default()
        },
    )
    .unwrap();
    objects
        .put(
            &bounded.object_key(ObjectKind::Blob, id),
            Bytes::from(vec![0; 65]).into(),
        )
        .await
        .unwrap();
    assert!(matches!(
        bounded.get_encoded(ObjectKind::Blob, id),
        Err(Error::Limit(_))
    ));
}

#[test]
fn concurrent_creates_have_one_immutable_value() {
    let cloud = Arc::new(store(Arc::new(InMemory::new())));
    let results = std::thread::scope(|scope| {
        (0..8)
            .map(|_| {
                let cloud = cloud.clone();
                scope.spawn(move || cloud.put_object(ObjectKind::Blob, b"same payload").unwrap())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|task| task.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(results.iter().all(|id| *id == results[0]));
    assert_eq!(
        cloud.get_object(ObjectKind::Blob, results[0]).unwrap(),
        b"same payload"
    );
}
