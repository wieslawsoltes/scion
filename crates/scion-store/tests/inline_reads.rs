use rusqlite::{Connection, params};
use scion_store::{LocalStore, Storage, encode_object};
use scion_types::{Error, ObjectId, ObjectKind, Result};
use std::{fs, path::Path};

const CONTENT: &[u8] = b"verified inline and loose content\n";

fn write_loose(root: &Path, kind: ObjectKind, id: ObjectId, bytes: &[u8]) {
    let hex = id.to_string();
    let path = root
        .join("objects")
        .join(kind.as_str())
        .join(&hex[..2])
        .join(&hex[2..]);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, encode_object(kind, bytes).unwrap()).unwrap();
}

fn read(store: &LocalStore, kind: ObjectKind, id: ObjectId, bounded: bool) -> Result<Vec<u8>> {
    if bounded {
        store
            .try_get_small_object(kind, id, 8192)?
            .ok_or_else(|| Error::Limit("unexpected small-object fallback".into()))
    } else {
        store.get_object(kind, id)
    }
}

#[test]
fn cold_inline_reads_verify_complete_objects_and_fall_back_only_for_absent_rows() {
    let dir = tempfile::tempdir().unwrap();
    let writer = LocalStore::open(dir.path()).unwrap();
    let inline = writer.put_object(ObjectKind::Changeset, CONTENT).unwrap();
    let loose_bytes = b"a legacy loose object alongside current inline data";
    let loose = ObjectId::hash(ObjectKind::Changeset, loose_bytes);
    write_loose(dir.path(), ObjectKind::Changeset, loose, loose_bytes);
    for bounded in [false, true] {
        let reader = LocalStore::open_read_only(dir.path()).unwrap();
        assert_eq!(
            read(&reader, ObjectKind::Changeset, inline, bounded).unwrap(),
            CONTENT
        );
        assert_eq!(
            read(&reader, ObjectKind::Changeset, loose, bounded).unwrap(),
            loose_bytes
        );
        assert!(matches!(
            read(
                &reader,
                ObjectKind::Changeset,
                ObjectId([0xff; 32]),
                bounded
            ),
            Err(Error::NotFound(_))
        ));
    }
    let snapshot = LocalStore::open_read_snapshot(dir.path()).unwrap();
    assert_eq!(
        read(&snapshot, ObjectKind::Changeset, inline, true).unwrap(),
        CONTENT
    );
    assert_eq!(
        snapshot
            .try_get_small_object(ObjectKind::Changeset, inline, CONTENT.len() - 1)
            .unwrap(),
        None
    );
}

#[test]
fn bounded_legacy_reads_observe_live_schema_upgrades_without_changing_pinned_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    drop(LocalStore::open(dir.path()).unwrap());
    let database = Connection::open(dir.path().join("metadata.sqlite3")).unwrap();
    database.execute_batch("DROP TABLE inline_objects").unwrap();
    let legacy = ObjectId::hash(ObjectKind::Blob, CONTENT);
    write_loose(dir.path(), ObjectKind::Blob, legacy, CONTENT);
    let live = LocalStore::open_read_only(dir.path()).unwrap();
    let snapshot = LocalStore::open_read_snapshot(dir.path()).unwrap();
    for reader in [&live, &snapshot] {
        assert_eq!(
            read(reader, ObjectKind::Blob, legacy, true).unwrap(),
            CONTENT
        );
    }
    let writer = LocalStore::open(dir.path()).unwrap();
    let fresh_bytes = b"new object after the inline schema upgrade";
    let fresh = writer.put_object(ObjectKind::Blob, fresh_bytes).unwrap();
    assert_eq!(
        read(&live, ObjectKind::Blob, fresh, true).unwrap(),
        fresh_bytes
    );
    assert_eq!(
        read(&live, ObjectKind::Blob, fresh, false).unwrap(),
        fresh_bytes
    );
    assert!(matches!(
        read(&snapshot, ObjectKind::Blob, fresh, true),
        Err(Error::NotFound(_))
    ));
    assert_eq!(
        read(&snapshot, ObjectKind::Blob, legacy, true).unwrap(),
        CONTENT
    );
    let latest = LocalStore::open_read_snapshot(dir.path()).unwrap();
    assert_eq!(
        read(&latest, ObjectKind::Blob, fresh, true).unwrap(),
        fresh_bytes
    );
}

#[test]
fn incompatible_existing_schemas_never_hide_errors_behind_valid_loose_objects() {
    for schema in [
        "CREATE TABLE inline_objects(kind TEXT, id BLOB)",
        "CREATE TABLE INLINE_OBJECTS(kind TEXT, id BLOB)",
        "CREATE VIEW inline_objects AS SELECT * FROM absent_underlying_table",
        "CREATE INDEX inline_objects ON records(key)",
    ] {
        let dir = tempfile::tempdir().unwrap();
        drop(LocalStore::open(dir.path()).unwrap());
        let database = Connection::open(dir.path().join("metadata.sqlite3")).unwrap();
        database.execute_batch("DROP TABLE inline_objects").unwrap();
        database.execute_batch(schema).unwrap();
        let id = ObjectId::hash(ObjectKind::Blob, CONTENT);
        write_loose(dir.path(), ObjectKind::Blob, id, CONTENT);
        for bounded in [false, true] {
            let reader = LocalStore::open_read_only(dir.path()).unwrap();
            assert!(
                matches!(
                    read(&reader, ObjectKind::Blob, id, bounded),
                    Err(Error::Storage(_))
                ),
                "{schema}; bounded={bounded}"
            );
        }
    }
}

#[test]
fn inline_execution_and_integrity_errors_are_not_reinterpreted_as_absence() {
    let dir = tempfile::tempdir().unwrap();
    let writer = LocalStore::open(dir.path()).unwrap();
    let id = writer.put_object(ObjectKind::Blob, CONTENT).unwrap();
    write_loose(dir.path(), ObjectKind::Blob, id, CONTENT);
    let database = Connection::open(dir.path().join("metadata.sqlite3")).unwrap();
    database
        .execute(
            "UPDATE inline_objects SET encoded='wrong SQL value type' WHERE id=?1",
            params![id.0.as_slice()],
        )
        .unwrap();
    for bounded in [false, true] {
        let reader = LocalStore::open_read_only(dir.path()).unwrap();
        assert!(matches!(
            read(&reader, ObjectKind::Blob, id, bounded),
            Err(Error::Storage(_))
        ));
    }
    let mut corrupt = encode_object(ObjectKind::Blob, CONTENT).unwrap();
    corrupt[0] ^= 1;
    database
        .execute(
            "UPDATE inline_objects SET encoded=?1 WHERE id=?2",
            params![corrupt, id.0.as_slice()],
        )
        .unwrap();
    for bounded in [false, true] {
        let reader = LocalStore::open_read_only(dir.path()).unwrap();
        assert!(matches!(
            read(&reader, ObjectKind::Blob, id, bounded),
            Err(Error::Corrupt(_))
        ));
    }
    let correct = encode_object(ObjectKind::Blob, CONTENT).unwrap();
    database
        .execute(
            "UPDATE inline_objects SET encoded=?1 WHERE id=?2",
            params![correct, id.0.as_slice()],
        )
        .unwrap();
    let reader = LocalStore::open_read_only(dir.path()).unwrap();
    assert_eq!(read(&reader, ObjectKind::Blob, id, true).unwrap(), CONTENT);
    database.execute_batch("DROP TABLE inline_objects").unwrap();
    // A previously observed table disappearing is not a legacy store. Do not
    // silently serve its loose duplicate after a schema failure.
    for bounded in [false, true] {
        assert!(matches!(
            read(&reader, ObjectKind::Blob, id, bounded),
            Err(Error::Storage(_))
        ));
    }
}

#[test]
fn corrupt_database_errors_are_not_hidden_by_a_readable_loose_object() {
    let dir = tempfile::tempdir().unwrap();
    drop(LocalStore::open(dir.path()).unwrap());
    let id = ObjectId::hash(ObjectKind::Blob, CONTENT);
    write_loose(dir.path(), ObjectKind::Blob, id, CONTENT);
    fs::write(
        dir.path().join("metadata.sqlite3"),
        b"not a SQLite database",
    )
    .unwrap();
    for bounded in [false, true] {
        let reader = LocalStore::open_read_only(dir.path()).unwrap();
        assert!(matches!(
            read(&reader, ObjectKind::Blob, id, bounded),
            Err(Error::Storage(_))
        ));
    }
}
