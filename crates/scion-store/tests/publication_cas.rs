use rusqlite::Connection;
use scion_store::{ImmutableRecord, LocalStore, MemoryStore, PreparedObject, RecordCas, Storage};
use scion_types::ObjectKind;
use std::sync::{Arc, Barrier};

fn prepared(bytes: &[u8]) -> PreparedObject {
    PreparedObject::new(ObjectKind::Changeset, bytes.to_vec()).unwrap()
}

fn cas(expected: Option<&[u8]>, value: Option<&[u8]>) -> RecordCas {
    RecordCas {
        namespace: "repo/r/refs".into(),
        key: "main".into(),
        expected: expected.map(<[u8]>::to_vec),
        value: value.map(<[u8]>::to_vec),
    }
}

fn exercise_publication(store: &dyn Storage) {
    let object = prepared(b"new commit");
    let id = object.id();
    let records = [ImmutableRecord {
        namespace: "index".into(),
        key: "commit".into(),
        value: b"root".to_vec(),
    }];
    let first = store
        .publish_commit_with_objects_and_cas(
            "r",
            id,
            std::slice::from_ref(&object),
            &records,
            Some(&cas(None, Some(b"one"))),
        )
        .unwrap();
    assert_eq!(first.cas_applied, Some(true));
    let repeated = store
        .publish_commit_with_objects_and_cas(
            "r",
            id,
            std::slice::from_ref(&object),
            &records,
            Some(&cas(None, Some(b"two"))),
        )
        .unwrap();
    assert_eq!(repeated.sequence, first.sequence);
    assert_eq!(repeated.cas_applied, Some(false));
    assert_eq!(
        store.get_record("repo/r/refs", "main").unwrap(),
        Some(b"one".to_vec())
    );
    assert_eq!(store.get_object(object.kind(), id).unwrap(), object.bytes());
    assert_eq!(
        store
            .get_record_pair(("repo/r/changesets", &id.to_string()), ("index", "commit"))
            .unwrap(),
        [Some(b"1".to_vec()), Some(b"root".to_vec())]
    );
    assert_eq!(store.read_log("r", 0, 10).unwrap().len(), 1);

    let stale = prepared(b"commit with stale expected ref");
    let result = store
        .publish_commit_with_objects_and_cas(
            "r",
            stale.id(),
            std::slice::from_ref(&stale),
            &[],
            Some(&cas(Some(b"stale"), Some(b"three"))),
        )
        .unwrap();
    assert_eq!(result.cas_applied, Some(false));
    assert_eq!(
        store
            .get_record("repo/r/changesets", &stale.id().to_string())
            .unwrap(),
        Some(b"1".to_vec())
    );
    assert_eq!(
        store.get_object(stale.kind(), stale.id()).unwrap(),
        stale.bytes()
    );
    assert_eq!(store.read_log("r", 0, 10).unwrap().len(), 2);
}

#[test]
fn local_and_portable_publication_agree_on_cas_miss_and_retry() {
    exercise_publication(&MemoryStore::new());
    let dir = tempfile::tempdir().unwrap();
    exercise_publication(&LocalStore::open(dir.path()).unwrap());
    let reopened = LocalStore::open_read_only(dir.path()).unwrap();
    assert_eq!(reopened.read_log("r", 0, 10).unwrap().len(), 2);
    assert_eq!(
        reopened.get_record("repo/r/refs", "main").unwrap(),
        Some(b"one".to_vec())
    );
}

#[test]
fn conditional_statement_cas_preserves_all_transition_cases() {
    let dir = tempfile::tempdir().unwrap();
    for store in [
        &MemoryStore::new() as &dyn Storage,
        &LocalStore::open(dir.path()).unwrap(),
    ] {
        assert!(store.compare_and_swap("refs", "main", None, None).unwrap());
        assert!(
            !store
                .compare_and_swap("refs", "main", Some(b""), None)
                .unwrap()
        );
        assert!(
            store
                .compare_and_swap("refs", "main", None, Some(b""))
                .unwrap()
        );
        assert!(
            !store
                .compare_and_swap("refs", "main", None, Some(b"x"))
                .unwrap()
        );
        assert!(!store.compare_and_swap("refs", "main", None, None).unwrap());
        assert!(
            store
                .compare_and_swap("refs", "main", Some(b""), Some(b""))
                .unwrap()
        );
        assert!(
            store
                .compare_and_swap("refs", "main", Some(b""), Some(b"new"))
                .unwrap()
        );
        assert!(
            !store
                .compare_and_swap("refs", "main", Some(b"stale"), None)
                .unwrap()
        );
        assert!(
            store
                .compare_and_swap("refs", "main", Some(b"new"), None)
                .unwrap()
        );
        assert_eq!(store.get_record("refs", "main").unwrap(), None);
    }
}

#[test]
fn concurrent_publications_have_one_ref_winner_and_all_commits_durable() {
    let dir = tempfile::tempdir().unwrap();
    let stores: Vec<_> = (0..8)
        .map(|_| LocalStore::open(dir.path()).unwrap())
        .collect();
    let barrier = Arc::new(Barrier::new(stores.len()));
    let threads: Vec<_> = stores
        .into_iter()
        .enumerate()
        .map(|(index, store)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let object = prepared(format!("concurrent commit {index}").as_bytes());
                let value = object.id().to_string();
                barrier.wait();
                let result = store
                    .publish_commit_with_objects_and_cas(
                        "r",
                        object.id(),
                        std::slice::from_ref(&object),
                        &[],
                        Some(&cas(None, Some(value.as_bytes()))),
                    )
                    .unwrap();
                (object, result)
            })
        })
        .collect();
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(
        results
            .iter()
            .filter(|(_, result)| result.cas_applied == Some(true))
            .count(),
        1
    );
    let reopened = LocalStore::open_read_only(dir.path()).unwrap();
    assert_eq!(reopened.read_log("r", 0, 100).unwrap().len(), 8);
    for (object, result) in results {
        assert_eq!(
            reopened.get_object(object.kind(), object.id()).unwrap(),
            object.bytes()
        );
        assert!(
            reopened
                .get_record("repo/r/changesets", &object.id().to_string())
                .unwrap()
                .is_some()
        );
        if result.cas_applied == Some(true) {
            assert_eq!(
                reopened.get_record("repo/r/refs", "main").unwrap(),
                Some(object.id().to_string().into_bytes())
            );
        }
    }
}

#[test]
fn cas_sql_error_rolls_back_local_objects_indexes_and_membership() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    assert!(
        store
            .compare_and_swap("repo/r/refs", "main", None, Some(b"old"))
            .unwrap()
    );
    let connection = Connection::open(dir.path().join("metadata.sqlite3")).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_ref_update BEFORE UPDATE ON records WHEN OLD.namespace='repo/r/refs' BEGIN SELECT RAISE(ABORT,'injected CAS failure'); END;").unwrap();
    let object = prepared(b"rolled back publication");
    let records = [ImmutableRecord {
        namespace: "index".into(),
        key: "failed".into(),
        value: b"new".to_vec(),
    }];
    assert!(
        store
            .publish_commit_with_objects_and_cas(
                "r",
                object.id(),
                std::slice::from_ref(&object),
                &records,
                Some(&cas(Some(b"old"), Some(b"new")))
            )
            .is_err()
    );
    assert!(store.get_object(object.kind(), object.id()).is_err());
    assert_eq!(store.get_record("index", "failed").unwrap(), None);
    assert_eq!(
        store
            .get_record("repo/r/changesets", &object.id().to_string())
            .unwrap(),
        None
    );
    assert!(store.read_log("r", 0, 10).unwrap().is_empty());
    assert_eq!(
        store.get_record("repo/r/refs", "main").unwrap(),
        Some(b"old".to_vec())
    );
}

#[test]
fn immutable_cas_targets_are_rejected_before_any_publication() {
    let dir = tempfile::tempdir().unwrap();
    let memory = MemoryStore::new();
    let local = LocalStore::open(dir.path()).unwrap();
    for store in [&memory as &dyn Storage, &local] {
        let object = prepared(b"must not be published");
        let records = [ImmutableRecord {
            namespace: "index".into(),
            key: "immutable".into(),
            value: b"value".to_vec(),
        }];
        for (namespace, key) in [
            ("repo/r/changesets", object.id().to_string()),
            ("index", "immutable".to_owned()),
        ] {
            let cas = RecordCas {
                namespace: namespace.into(),
                key,
                expected: None,
                value: Some(b"bad".to_vec()),
            };
            assert!(
                store
                    .publish_commit_with_objects_and_cas(
                        "r",
                        object.id(),
                        std::slice::from_ref(&object),
                        &records,
                        Some(&cas)
                    )
                    .is_err()
            );
            assert!(store.get_object(object.kind(), object.id()).is_err());
            assert!(store.read_log("r", 0, 10).unwrap().is_empty());
        }
    }
}

#[test]
fn readonly_rejects_atomic_publication_and_pair_queries_follow_live_wal() {
    let dir = tempfile::tempdir().unwrap();
    let writer = LocalStore::open(dir.path()).unwrap();
    let reader = LocalStore::open_read_only(dir.path()).unwrap();
    let object = prepared(b"paired WAL records");
    let id = object.id().to_string();
    assert_eq!(
        reader
            .get_record_pair(("repo/r/changesets", &id), ("index", "root"))
            .unwrap(),
        [None, None]
    );
    assert!(
        reader
            .publish_commit_with_objects_and_cas(
                "r",
                object.id(),
                std::slice::from_ref(&object),
                &[],
                Some(&cas(None, Some(b"new")))
            )
            .is_err()
    );
    writer
        .publish_commit_with_objects_and_cas(
            "r",
            object.id(),
            std::slice::from_ref(&object),
            &[ImmutableRecord {
                namespace: "index".into(),
                key: "root".into(),
                value: b"value".to_vec(),
            }],
            None,
        )
        .unwrap();
    assert_eq!(
        reader
            .get_record_pair(("repo/r/changesets", &id), ("index", "root"))
            .unwrap(),
        [Some(b"1".to_vec()), Some(b"value".to_vec())]
    );
}

#[test]
fn acknowledged_combined_publication_child() {
    let Some(root) = std::env::var_os("SCION_TEST_COMBINED_EXIT_STORE") else {
        return;
    };
    let store = LocalStore::open(root).unwrap();
    let object = prepared(b"publication survives immediate process exit");
    let result = store
        .publish_commit_with_objects_and_cas(
            "r",
            object.id(),
            std::slice::from_ref(&object),
            &[],
            Some(&cas(None, Some(object.id().to_string().as_bytes()))),
        )
        .unwrap();
    assert_eq!(result.cas_applied, Some(true));
    // Deliberately skip Rust drops and SQLite connection cleanup/checkpointing.
    std::process::exit(0);
}

#[test]
fn combined_publication_acknowledgement_survives_exit_without_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "acknowledged_combined_publication_child",
            "--nocapture",
        ])
        .env("SCION_TEST_COMBINED_EXIT_STORE", dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let object = prepared(b"publication survives immediate process exit");
    let reopened = LocalStore::open_read_only(dir.path()).unwrap();
    assert_eq!(
        reopened.get_object(object.kind(), object.id()).unwrap(),
        object.bytes()
    );
    assert_eq!(
        reopened.get_record("repo/r/refs", "main").unwrap(),
        Some(object.id().to_string().into_bytes())
    );
    assert_eq!(
        reopened
            .get_record("repo/r/changesets", &object.id().to_string())
            .unwrap(),
        Some(b"1".to_vec())
    );
    assert_eq!(reopened.read_log("r", 0, 10).unwrap()[0].id, object.id());
}

#[test]
fn publication_cas_rejects_prior_immutable_records_and_other_repositories() {
    let dir = tempfile::tempdir().unwrap();
    let memory = MemoryStore::new();
    let local = LocalStore::open(dir.path()).unwrap();
    for store in [&memory as &dyn Storage, &local] {
        let old = prepared(b"previously published commit");
        store
            .publish_commit_with_objects("r", old.id(), std::slice::from_ref(&old), &[])
            .unwrap();
        let fresh = prepared(b"must not publish with an invalid ref target");
        for (namespace, key, existing) in [
            ("repo/r/changesets", old.id().to_string(), b"1".as_slice()),
            ("repo/r/graph", old.id().to_string(), b"graph".as_slice()),
            (
                "repo/r/derived/tree/1",
                old.id().to_string(),
                b"tree".as_slice(),
            ),
            (
                "repo/other/refs",
                "main".to_owned(),
                b"other ref".as_slice(),
            ),
            (
                "repo/r/refs/child",
                "main".to_owned(),
                b"nested namespace".as_slice(),
            ),
        ] {
            store.put_record(namespace, &key, existing).unwrap();
            let cas = RecordCas {
                namespace: namespace.into(),
                key: key.clone(),
                expected: Some(existing.to_vec()),
                value: None,
            };
            assert!(
                store
                    .publish_commit_with_objects_and_cas(
                        "r",
                        fresh.id(),
                        std::slice::from_ref(&fresh),
                        &[],
                        Some(&cas),
                    )
                    .is_err(),
                "accepted forbidden namespace {namespace}"
            );
            assert_eq!(
                store.get_record(namespace, &key).unwrap(),
                Some(existing.to_vec())
            );
            assert!(store.get_object(fresh.kind(), fresh.id()).is_err());
            assert_eq!(
                store
                    .get_record("repo/r/changesets", &fresh.id().to_string())
                    .unwrap(),
                None
            );
            assert_eq!(store.read_log("r", 0, 10).unwrap().len(), 1);
        }
        for key in [
            "",
            "../main",
            "/main",
            "main/",
            "feature//name",
            "white space",
            "control\nname",
        ] {
            let cas = RecordCas {
                key: key.into(),
                ..cas(None, Some(b"target"))
            };
            assert!(
                store
                    .publish_commit_with_objects_and_cas(
                        "r",
                        fresh.id(),
                        std::slice::from_ref(&fresh),
                        &[],
                        Some(&cas),
                    )
                    .is_err(),
                "accepted invalid ref name {key:?}"
            );
            assert!(store.get_object(fresh.kind(), fresh.id()).is_err());
            assert_eq!(store.read_log("r", 0, 10).unwrap().len(), 1);
        }
    }
}
