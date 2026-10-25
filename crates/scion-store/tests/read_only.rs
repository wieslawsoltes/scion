use rusqlite::Connection;
use scion_store::{ImmutableRecord, LocalStore, PreparedObject, Storage, encode_object};
use scion_types::{ObjectId, ObjectKind};
use std::fs;

#[test]
fn read_only_open_never_initializes_missing_stores() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("missing");
    assert!(LocalStore::open_read_only(&absent).is_err());
    assert!(!absent.exists());
    assert!(LocalStore::open_read_only(dir.path()).is_err());
    assert!(!dir.path().join("metadata.sqlite3").exists());
}

#[test]
fn reader_observes_live_wal_publication_and_refs() {
    let dir = tempfile::tempdir().unwrap();
    let writer = LocalStore::open(dir.path()).unwrap();
    let reader = LocalStore::open_read_only(dir.path()).unwrap();
    let object = PreparedObject::new(ObjectKind::Changeset, b"live WAL contents".to_vec()).unwrap();
    assert!(reader.get_object(object.kind(), object.id()).is_err());
    let records = [ImmutableRecord {
        namespace: "reader/index".into(),
        key: "first".into(),
        value: b"index contents".to_vec(),
    }];
    writer
        .publish_commit_with_objects(
            "reader",
            object.id(),
            std::slice::from_ref(&object),
            &records,
        )
        .unwrap();
    writer
        .compare_and_swap("refs", "main", None, Some(b"one"))
        .unwrap();
    assert_eq!(
        reader.get_object(object.kind(), object.id()).unwrap(),
        object.bytes()
    );
    assert_eq!(
        reader.get_record("reader/index", "first").unwrap(),
        Some(b"index contents".to_vec())
    );
    assert_eq!(
        reader.scan_records("reader/index", "", 10).unwrap().len(),
        1
    );
    assert_eq!(reader.read_log("reader", 0, 10).unwrap()[0].id, object.id());
    assert_eq!(
        reader.get_record("refs", "main").unwrap(),
        Some(b"one".to_vec())
    );
    writer
        .compare_and_swap("refs", "main", Some(b"one"), Some(b"two"))
        .unwrap();
    assert_eq!(
        reader.get_record("refs", "main").unwrap(),
        Some(b"two".to_vec())
    );
}

#[test]
fn every_reader_mutation_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let writer = LocalStore::open(dir.path()).unwrap();
    let id = writer.put_object(ObjectKind::Blob, b"existing").unwrap();
    let reader = LocalStore::open_read_only(dir.path()).unwrap();
    let prepared = PreparedObject::new(ObjectKind::Changeset, b"new".to_vec()).unwrap();
    // Even an idempotent mutation is rejected, rather than sometimes appearing
    // writable depending on whether a particular object already exists.
    assert!(reader.put_object(ObjectKind::Blob, b"existing").is_err());
    assert!(reader.put_object(ObjectKind::Blob, b"new").is_err());
    assert!(
        reader
            .put_encoded(
                ObjectKind::Blob,
                id,
                &encode_object(ObjectKind::Blob, b"existing").unwrap()
            )
            .is_err()
    );
    assert!(reader.put_record("readonly", "key", b"value").is_err());
    assert!(
        reader
            .compare_and_swap("refs", "main", None, Some(b"new"))
            .is_err()
    );
    assert!(reader.append_log("readonly", id).is_err());
    assert!(reader.publish_commit("readonly", id, &[]).is_err());
    assert!(
        reader
            .publish_commit_with_objects("readonly", prepared.id(), &[prepared], &[])
            .is_err()
    );
    assert!(reader.read_log("readonly", 0, 10).unwrap().is_empty());
    assert_eq!(reader.get_record("readonly", "key").unwrap(), None);
    assert_eq!(reader.get_record("refs", "main").unwrap(), None);
}

#[test]
fn legacy_reader_does_not_migrate_but_observes_a_writer_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    drop(LocalStore::open(dir.path()).unwrap());
    let db = Connection::open(dir.path().join("metadata.sqlite3")).unwrap();
    db.execute_batch("DROP TABLE inline_objects").unwrap();
    let bytes = b"legacy loose contents";
    let id = ObjectId::hash(ObjectKind::Blob, bytes);
    let hex = id.to_string();
    let path = dir
        .path()
        .join("objects")
        .join("blob")
        .join(&hex[..2])
        .join(&hex[2..]);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, encode_object(ObjectKind::Blob, bytes).unwrap()).unwrap();
    let reader = LocalStore::open_read_only(dir.path()).unwrap();
    assert_eq!(reader.get_object(ObjectKind::Blob, id).unwrap(), bytes);
    let table_count: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='inline_objects'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table_count, 0);
    let writer = LocalStore::open(dir.path()).unwrap();
    let fresh = writer
        .put_object(ObjectKind::Blob, b"new inline contents")
        .unwrap();
    assert_eq!(
        reader.get_object(ObjectKind::Blob, fresh).unwrap(),
        b"new inline contents"
    );
    assert_eq!(reader.get_object(ObjectKind::Blob, id).unwrap(), bytes);
}
