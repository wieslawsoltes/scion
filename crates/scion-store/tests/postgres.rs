#![cfg(feature = "postgres")]
use scion_store::{PostgresStore, Storage};
use scion_types::{ObjectId, ObjectKind};
use std::sync::Arc;

#[tokio::test]
async fn pool_can_be_destroyed_by_an_async_owner() {
    let Ok(url) = std::env::var("SCION_TEST_DATABASE_URL") else {
        return;
    };
    let store =
        tokio::task::spawn_blocking(move || Arc::new(PostgresStore::connect(&url, 2).unwrap()))
            .await
            .unwrap();
    let query = store.clone();
    tokio::task::spawn_blocking(move || query.get_record("health", "probe").unwrap())
        .await
        .unwrap();
    // The final Arc can be released by an HTTP task without nested-runtime panic.
    drop(store);
}

#[test]
fn shared_backend_concurrent_publication_cas_and_reopen() {
    let Ok(url) = std::env::var("SCION_TEST_DATABASE_URL") else {
        eprintln!("SCION_TEST_DATABASE_URL unset; PostgreSQL runtime test requires the CI service");
        return;
    };
    let repo = format!(
        "conformance-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let a = Arc::new(PostgresStore::connect(&url, 4).unwrap());
    let b = Arc::new(PostgresStore::connect(&url, 4).unwrap());
    let ns = format!("test/{repo}/refs");
    let mut tasks = Vec::new();
    for i in 0..16u64 {
        let store = if i % 2 == 0 { a.clone() } else { b.clone() };
        let repo = repo.clone();
        let ns = ns.clone();
        tasks.push(std::thread::spawn(move || {
            let id = store
                .put_object(ObjectKind::Blob, &i.to_le_bytes())
                .unwrap();
            let seq = store.append_log(&repo, id).unwrap();
            assert_eq!(seq, store.append_log(&repo, id).unwrap());
            assert_eq!(
                store
                    .get_record(&format!("repo/{repo}/changesets"), &id.to_string())
                    .unwrap(),
                Some(b"1".to_vec())
            );
            store
                .compare_and_swap(&ns, "main", None, Some(&i.to_le_bytes()))
                .unwrap()
        }));
    }
    assert_eq!(
        tasks
            .into_iter()
            .map(|t| usize::from(t.join().unwrap()))
            .sum::<usize>(),
        1
    );
    let log = a.read_log(&repo, 0, 100).unwrap();
    assert_eq!(log.len(), 16);
    assert!(log.windows(2).all(|w| w[0].sequence < w[1].sequence));
    assert!(
        a.read_log(&repo, log.last().unwrap().sequence, 100)
            .unwrap()
            .is_empty()
    );
    drop(a);
    drop(b);
    let reopened = PostgresStore::connect(&url, 2).unwrap();
    for i in 0..16u64 {
        let id = ObjectId::hash(ObjectKind::Blob, &i.to_le_bytes());
        assert_eq!(
            reopened.get_object(ObjectKind::Blob, id).unwrap(),
            i.to_le_bytes()
        );
        assert_eq!(
            reopened
                .get_record_pair(
                    (&format!("repo/{repo}/changesets"), &id.to_string()),
                    (&ns, "absent"),
                )
                .unwrap(),
            [Some(b"1".to_vec()), None],
        );
    }
    let old = reopened.get_record(&ns, "main").unwrap().unwrap();
    assert!(
        !reopened
            .compare_and_swap(&ns, "main", Some(b"incorrect"), Some(b"next"))
            .unwrap()
    );
    assert!(
        reopened
            .compare_and_swap(&ns, "main", Some(&old), Some(b"next"))
            .unwrap()
    );
    assert!(
        reopened
            .compare_and_swap(&ns, "main", Some(b"next"), None)
            .unwrap()
    );
}
