use rusqlite::Connection;
use scion_store::{
    LocalStore, LocalStoreOptions, PreparedObject, RecordCas, Storage, encode_object,
};
use scion_types::{Error, ObjectKind};
use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

fn publish(store: &dyn Storage, bytes: &[u8], expected: Option<&[u8]>) -> PreparedObject {
    let object = PreparedObject::new(ObjectKind::Changeset, bytes.to_vec()).unwrap();
    let result = store
        .publish_commit_with_objects_and_cas(
            "r",
            object.id(),
            std::slice::from_ref(&object),
            &[],
            Some(&RecordCas {
                namespace: "repo/r/refs".into(),
                key: "main".into(),
                expected: expected.map(<[u8]>::to_vec),
                value: Some(bytes.to_vec()),
            }),
        )
        .unwrap();
    assert_eq!(result.cas_applied, Some(true));
    object
}

#[test]
fn snapshot_binds_at_first_query_and_wal_writers_continue() {
    for mmap_size in [0, 256 * 1024 * 1024] {
        let dir = tempfile::tempdir().unwrap();
        let writer = Arc::new(LocalStore::open(dir.path()).unwrap());
        let first = publish(writer.as_ref(), b"first", None);
        let snapshot = LocalStore::open_read_snapshot_with_options(
            dir.path(),
            LocalStoreOptions {
                mmap_size,
                checkpoint_on_close: true,
            },
        )
        .unwrap();
        let live = LocalStore::open_read_only(dir.path()).unwrap();
        // BEGIN DEFERRED alone must not pin an older snapshot.
        let second = publish(writer.as_ref(), b"second", Some(b"first"));
        assert_eq!(
            snapshot
                .try_get_small_object(second.kind(), second.id(), 8192)
                .unwrap(),
            Some(second.bytes().to_vec())
        );
        assert_eq!(snapshot.read_log("r", 0, 10).unwrap().len(), 2);
        let (send, receive) = mpsc::channel();
        let worker = {
            let writer = writer.clone();
            std::thread::spawn(move || {
                let third = publish(writer.as_ref(), b"third", Some(b"second"));
                send.send(third).unwrap();
            })
        };
        let third = receive
            .recv_timeout(Duration::from_secs(5))
            .expect("WAL writer blocked by read snapshot");
        worker.join().unwrap();
        assert_eq!(
            snapshot.get_record("repo/r/refs", "main").unwrap(),
            Some(b"second".to_vec())
        );
        assert_eq!(snapshot.read_log("r", 0, 10).unwrap().len(), 2);
        assert!(matches!(
            snapshot.get_object(third.kind(), third.id()),
            Err(Error::NotFound(_))
        ));
        assert_eq!(
            snapshot
                .get_record_pair(
                    ("repo/r/changesets", &first.id().to_string()),
                    ("repo/r/changesets", &third.id().to_string())
                )
                .unwrap(),
            [Some(b"1".to_vec()), None]
        );
        assert_eq!(
            live.get_record("repo/r/refs", "main").unwrap(),
            Some(b"third".to_vec())
        );
        assert_eq!(live.read_log("r", 0, 10).unwrap().len(), 3);
        let fresh = LocalStore::open_read_snapshot(dir.path()).unwrap();
        assert_eq!(
            fresh.get_record("repo/r/refs", "main").unwrap(),
            Some(b"third".to_vec())
        );
        assert_eq!(
            fresh.get_object(third.kind(), third.id()).unwrap(),
            third.bytes()
        );
    }
}

#[test]
fn snapshot_rejects_every_mutation_including_idempotent_operations() {
    let dir = tempfile::tempdir().unwrap();
    let writer = LocalStore::open(dir.path()).unwrap();
    let object = publish(&writer, b"existing", None);
    let snapshot = LocalStore::open_read_snapshot(dir.path()).unwrap();
    assert!(snapshot.put_object(object.kind(), object.bytes()).is_err());
    assert!(
        snapshot
            .put_encoded(
                object.kind(),
                object.id(),
                &encode_object(object.kind(), object.bytes()).unwrap()
            )
            .is_err()
    );
    assert!(snapshot.put_record("r", "key", b"value").is_err());
    assert!(
        snapshot
            .compare_and_swap("repo/r/refs", "main", Some(b"existing"), Some(b"existing"))
            .is_err()
    );
    assert!(snapshot.append_log("r", object.id()).is_err());
    assert!(snapshot.migrate_records_to_rowid().is_err());
    assert!(snapshot.publish_commit("r", object.id(), &[]).is_err());
    assert!(
        snapshot
            .publish_commit_with_objects("r", object.id(), std::slice::from_ref(&object), &[])
            .is_err()
    );
    assert!(
        snapshot
            .publish_commit_with_objects_and_cas(
                "r",
                object.id(),
                std::slice::from_ref(&object),
                &[],
                None
            )
            .is_err()
    );
    assert_eq!(
        snapshot.get_object(object.kind(), object.id()).unwrap(),
        object.bytes()
    );
}

#[test]
fn snapshot_supports_legacy_loose_objects_without_schema_migration() {
    let dir = tempfile::tempdir().unwrap();
    drop(LocalStore::open(dir.path()).unwrap());
    let db = Connection::open(dir.path().join("metadata.sqlite3")).unwrap();
    db.execute_batch("DROP TABLE inline_objects").unwrap();
    let bytes = b"legacy loose snapshot object";
    let id = scion_types::ObjectId::hash(ObjectKind::Blob, bytes);
    let hex = id.to_string();
    let path = dir
        .path()
        .join("objects")
        .join("blob")
        .join(&hex[..2])
        .join(&hex[2..]);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, encode_object(ObjectKind::Blob, bytes).unwrap()).unwrap();
    let snapshot = LocalStore::open_read_snapshot(dir.path()).unwrap();
    assert_eq!(
        snapshot
            .try_get_small_object(ObjectKind::Blob, id, 8192)
            .unwrap(),
        Some(bytes.to_vec())
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='inline_objects'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    let writer = LocalStore::open(dir.path()).unwrap();
    let fresh = writer
        .put_object(ObjectKind::Blob, b"new after legacy snapshot")
        .unwrap();
    assert!(snapshot.get_object(ObjectKind::Blob, fresh).is_err());
    assert_eq!(
        LocalStore::open_read_snapshot(dir.path())
            .unwrap()
            .get_object(ObjectKind::Blob, fresh)
            .unwrap(),
        b"new after legacy snapshot"
    );
}

#[test]
fn snapshot_preserves_missing_and_corrupt_database_errors() {
    let dir = tempfile::tempdir().unwrap();
    assert!(LocalStore::open_read_snapshot(dir.path().join("missing")).is_err());
    std::fs::write(
        dir.path().join("metadata.sqlite3"),
        b"not a SQLite database".repeat(256),
    )
    .unwrap();
    let snapshot = LocalStore::open_read_snapshot(dir.path()).unwrap();
    assert!(snapshot.get_record("r", "key").is_err());
    assert!(snapshot.read_log("r", 0, 10).is_err());
}
