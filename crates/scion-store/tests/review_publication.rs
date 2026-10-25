use rusqlite::{Connection, params};
use scion_store::{ImmutableRecord, LocalStore, PreparedObject, Storage, encode_object};
use scion_types::{ObjectId, ObjectKind};
use std::{fs, path::Path};

fn replace_inline(root: &Path, kind: ObjectKind, id: ObjectId, encoded: &[u8]) {
    let db = Connection::open(root.join("metadata.sqlite3")).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE inline_objects SET encoded=?3 WHERE kind=?1 AND id=?2",
            params![kind.as_str(), id.0.as_slice(), encoded],
        )
        .unwrap(),
        1
    );
}

#[test]
fn corrupt_inline_duplicate_put_object_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let id = store
        .put_object(ObjectKind::Blob, b"verified object")
        .unwrap();
    replace_inline(dir.path(), ObjectKind::Blob, id, b"broken envelope");
    assert!(
        store
            .put_object(ObjectKind::Blob, b"verified object")
            .is_err()
    );
}

#[test]
fn corrupt_inline_duplicate_put_encoded_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let id = store
        .put_object(ObjectKind::Blob, b"verified object")
        .unwrap();
    let encoded = encode_object(ObjectKind::Blob, b"verified object").unwrap();
    replace_inline(dir.path(), ObjectKind::Blob, id, b"broken envelope");
    assert!(store.put_encoded(ObjectKind::Blob, id, &encoded).is_err());
}

#[test]
fn corrupt_duplicate_rolls_back_final_publication_batch() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let existing = PreparedObject::new(ObjectKind::Tree, b"existing tree".to_vec()).unwrap();
    store.put_object(existing.kind(), existing.bytes()).unwrap();
    replace_inline(
        dir.path(),
        existing.kind(),
        existing.id(),
        b"broken envelope",
    );
    let fresh = PreparedObject::new(ObjectKind::Tree, b"fresh tree".to_vec()).unwrap();
    let changeset =
        PreparedObject::new(ObjectKind::Changeset, b"fresh changeset".to_vec()).unwrap();
    let id = changeset.id();
    let records = [ImmutableRecord {
        namespace: "review/index".into(),
        key: "fresh".into(),
        value: b"fresh index".to_vec(),
    }];
    let objects = [fresh.clone(), changeset.clone(), existing];
    assert!(
        store
            .publish_commit_with_objects("review", id, &objects, &records)
            .is_err()
    );
    assert!(store.get_object(fresh.kind(), fresh.id()).is_err());
    assert!(store.get_object(changeset.kind(), changeset.id()).is_err());
    assert_eq!(store.get_record("review/index", "fresh").unwrap(), None);
    assert_eq!(
        store
            .get_record("repo/review/changesets", &id.to_string())
            .unwrap(),
        None
    );
    assert!(store.read_log("review", 0, 10).unwrap().is_empty());
    // A rolled-back insertion must not leave a positive in-process existence
    // cache that suppresses a later real write.
    assert_eq!(
        store.put_object(fresh.kind(), fresh.bytes()).unwrap(),
        fresh.id()
    );
    assert_eq!(
        store.get_object(fresh.kind(), fresh.id()).unwrap(),
        fresh.bytes()
    );
}

#[test]
fn alternate_valid_compression_is_accepted_without_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let bytes = b"same immutable contents repeated same immutable contents repeated".repeat(1024);
    let object = PreparedObject::new(ObjectKind::Blob, bytes).unwrap();
    let original = encode_object(object.kind(), object.bytes()).unwrap();
    let mut alternate = original[..49].to_vec();
    zstd::stream::copy_encode(object.bytes(), &mut alternate, 19).unwrap();
    store
        .put_encoded(object.kind(), object.id(), &alternate)
        .unwrap();
    assert_eq!(
        store.put_object(object.kind(), object.bytes()).unwrap(),
        object.id()
    );
    store
        .put_encoded(object.kind(), object.id(), &original)
        .unwrap();
    store
        .publish_commit_with_objects("review", object.id(), std::slice::from_ref(&object), &[])
        .unwrap();
    assert_eq!(
        store.get_encoded(object.kind(), object.id()).unwrap(),
        alternate
    );
    assert_eq!(
        store.get_object(object.kind(), object.id()).unwrap(),
        object.bytes()
    );
}

#[test]
fn reopened_corrupt_loose_duplicate_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    drop(LocalStore::open(dir.path()).unwrap());
    let kind = ObjectKind::Blob;
    let bytes = b"old loose object";
    let id = ObjectId::hash(kind, bytes);
    let hex = id.to_string();
    let path = dir
        .path()
        .join("objects")
        .join(kind.as_str())
        .join(&hex[..2])
        .join(&hex[2..]);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"broken envelope").unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    assert!(store.put_object(kind, bytes).is_err());
    assert!(
        store
            .put_encoded(kind, id, &encode_object(kind, bytes).unwrap())
            .is_err()
    );
    let object = PreparedObject::new(kind, bytes.to_vec()).unwrap();
    assert!(
        store
            .publish_commit_with_objects("review", id, &[object], &[])
            .is_err()
    );
    assert!(store.read_log("review", 0, 10).unwrap().is_empty());
    assert_eq!(fs::read(path).unwrap(), b"broken envelope");
}
