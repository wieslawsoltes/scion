use scion_store::{ImmutableRecord, LocalStore, MemoryStore, Storage};
use scion_types::{ObjectId, ObjectKind};

fn verify_publication(store: &dyn Storage, repo: &str) {
    let id = ObjectId::hash(ObjectKind::Changeset, repo.as_bytes());
    let namespace = format!("publication/{repo}");
    store
        .put_record(&namespace, "collision", b"original")
        .unwrap();
    let mut records = vec![
        ImmutableRecord {
            namespace: namespace.clone(),
            key: "tree".into(),
            value: b"root".to_vec(),
        },
        ImmutableRecord {
            namespace: namespace.clone(),
            key: "collision".into(),
            value: b"different".to_vec(),
        },
    ];
    assert!(store.publish_commit(repo, id, &records).is_err());
    assert_eq!(store.get_record(&namespace, "tree").unwrap(), None);
    assert_eq!(
        store
            .get_record(&format!("repo/{repo}/changesets"), &id.to_string())
            .unwrap(),
        None
    );
    assert!(store.read_log(repo, 0, 10).unwrap().is_empty());
    records[1].value = b"original".to_vec();
    let sequence = store.publish_commit(repo, id, &records).unwrap();
    assert_eq!(store.publish_commit(repo, id, &records).unwrap(), sequence);
    assert_eq!(
        store.get_record(&namespace, "tree").unwrap(),
        Some(b"root".to_vec())
    );
    assert_eq!(store.read_log(repo, 0, 10).unwrap().len(), 1);

    let other = ObjectId::hash(ObjectKind::Changeset, b"duplicate batch");
    let duplicate = vec![
        ImmutableRecord {
            namespace: namespace.clone(),
            key: "duplicate".into(),
            value: vec![1],
        },
        ImmutableRecord {
            namespace: namespace.clone(),
            key: "duplicate".into(),
            value: vec![2],
        },
    ];
    assert!(store.publish_commit(repo, other, &duplicate).is_err());
    assert_eq!(store.get_record(&namespace, "duplicate").unwrap(), None);
    assert_eq!(store.read_log(repo, 0, 10).unwrap().len(), 1);
    let reserved = vec![ImmutableRecord {
        namespace: format!("repo/{repo}/changesets"),
        key: other.to_string(),
        value: b"1".to_vec(),
    }];
    assert!(store.publish_commit(repo, other, &reserved).is_err());
    assert_eq!(
        store
            .get_record(&reserved[0].namespace, &reserved[0].key)
            .unwrap(),
        None
    );
}

#[test]
fn memory_publication_is_atomic_and_idempotent() {
    verify_publication(&MemoryStore::new(), "memory");
}

#[test]
fn local_publication_is_atomic_durable_and_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temp.path()).unwrap();
    verify_publication(&store, "local");
    drop(store);
    let reopened = LocalStore::open(temp.path()).unwrap();
    assert_eq!(reopened.read_log("local", 0, 10).unwrap().len(), 1);
    assert_eq!(
        reopened.get_record("publication/local", "tree").unwrap(),
        Some(b"root".to_vec())
    );
}

#[cfg(feature = "postgres")]
#[test]
fn postgres_publication_is_atomic_and_idempotent() {
    let Ok(url) = std::env::var("SCION_TEST_DATABASE_URL") else {
        return;
    };
    let repo = format!(
        "publication-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let store = scion_store::PostgresStore::connect(&url, 2).unwrap();
    verify_publication(&store, &repo);
}
