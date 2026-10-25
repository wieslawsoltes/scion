use rusqlite::Connection;
use scion_store::{ImmutableRecord, LocalStore, PreparedObject, Storage};
use scion_types::{Error, ObjectKind};
use std::{
    path::Path,
    sync::{Arc, mpsc},
    time::Duration,
};

fn legacy(root: &Path) {
    let db = Connection::open(root.join("metadata.sqlite3")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE records(namespace TEXT NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL, PRIMARY KEY(namespace,key)) WITHOUT ROWID;").unwrap();
}

fn without_rowid(root: &Path, table: &str) -> bool {
    Connection::open(root.join("metadata.sqlite3"))
        .unwrap()
        .query_row(
            "SELECT wr FROM pragma_table_list WHERE schema='main' AND name=?1",
            [table],
            |row| row.get(0),
        )
        .unwrap()
}

fn record_storage(root: &Path) -> Vec<[String; 6]> {
    let db = Connection::open(root.join("metadata.sqlite3")).unwrap();
    let mut statement = db.prepare("SELECT typeof(namespace),hex(namespace),typeof(key),hex(key),typeof(value),hex(value) FROM records ORDER BY namespace,key").unwrap();
    statement
        .query_map([], |row| {
            Ok([
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ])
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn new_stores_use_rowid_records_and_legacy_open_never_migrates_implicitly() {
    let fresh = tempfile::tempdir().unwrap();
    let store = LocalStore::open(fresh.path()).unwrap();
    assert!(!without_rowid(fresh.path(), "records"));
    assert!(without_rowid(fresh.path(), "inline_objects"));
    assert!(!store.migrate_records_to_rowid().unwrap());
    let old = tempfile::tempdir().unwrap();
    legacy(old.path());
    let store = LocalStore::open(old.path()).unwrap();
    store.put_record("old", "key", b"still legacy").unwrap();
    assert!(without_rowid(old.path(), "records"));
    assert_eq!(
        LocalStore::open_read_only(old.path())
            .unwrap()
            .get_record("old", "key")
            .unwrap(),
        Some(b"still legacy".to_vec())
    );
    assert!(store.migrate_records_to_rowid().unwrap());
    assert!(!without_rowid(old.path(), "records"));
    assert!(without_rowid(old.path(), "inline_objects"));
}

#[test]
fn migration_preserves_bytes_logs_objects_and_cached_statement_behavior() {
    let dir = tempfile::tempdir().unwrap();
    legacy(dir.path());
    let store = LocalStore::open(dir.path()).unwrap();
    for (namespace, key, value) in [
        ("n", "ascii", b"value".as_slice()),
        ("n", "日本語", b"binary\x00\xff".as_slice()),
        ("other", "same", b"".as_slice()),
    ] {
        store.put_record(namespace, key, value).unwrap();
    }
    let object = PreparedObject::new(ObjectKind::Changeset, b"preserved commit".to_vec()).unwrap();
    let records = [ImmutableRecord {
        namespace: "index".into(),
        key: object.id().to_string(),
        value: vec![0xa5; 65536],
    }];
    let sequence = store
        .publish_commit_with_objects("r", object.id(), std::slice::from_ref(&object), &records)
        .unwrap();
    assert!(
        store
            .compare_and_swap("repo/r/refs", "main", None, Some(b"old"))
            .unwrap()
    );
    {
        let db = Connection::open(dir.path().join("metadata.sqlite3")).unwrap();
        db.execute_batch("INSERT INTO records(namespace,key,value) VALUES ('classes','blob',X'00ff'),('classes','text','000123'),('classes','integer',17),('classes','real',1.25)").unwrap();
    }
    let classes_before = record_storage(dir.path());
    let old = store.scan_records("n", "", 100).unwrap();
    let live = LocalStore::open_read_only(dir.path()).unwrap();
    assert_eq!(
        live.get_record("repo/r/refs", "main").unwrap(),
        Some(b"old".to_vec())
    );
    assert!(store.migrate_records_to_rowid().unwrap());
    assert!(!store.migrate_records_to_rowid().unwrap());
    assert_eq!(record_storage(dir.path()), classes_before);
    assert_eq!(store.scan_records("n", "", 100).unwrap(), old);
    assert_eq!(store.get_record("other", "same").unwrap(), Some(Vec::new()));
    assert_eq!(
        store.get_record("index", &object.id().to_string()).unwrap(),
        Some(vec![0xa5; 65536])
    );
    assert_eq!(
        store.get_object(object.kind(), object.id()).unwrap(),
        object.bytes()
    );
    assert_eq!(store.read_log("r", 0, 10).unwrap()[0].sequence, sequence);
    assert!(
        store
            .compare_and_swap("repo/r/refs", "main", Some(b"old"), Some(b"new"))
            .unwrap()
    );
    assert_eq!(
        live.get_record("repo/r/refs", "main").unwrap(),
        Some(b"new".to_vec())
    );
    drop(store);
    let reopened = LocalStore::open(dir.path()).unwrap();
    assert_eq!(reopened.scan_records("n", "", 100).unwrap(), old);
    assert_eq!(
        reopened.get_record("repo/r/refs", "main").unwrap(),
        Some(b"new".to_vec())
    );
}

#[test]
fn existing_wal_snapshot_survives_migration_and_later_updates() {
    let dir = tempfile::tempdir().unwrap();
    legacy(dir.path());
    let store = Arc::new(LocalStore::open(dir.path()).unwrap());
    store
        .compare_and_swap("refs", "main", None, Some(b"old"))
        .unwrap();
    let snapshot = LocalStore::open_read_snapshot(dir.path()).unwrap();
    assert_eq!(
        snapshot.get_record("refs", "main").unwrap(),
        Some(b"old".to_vec())
    );
    let (send, receive) = mpsc::channel();
    let worker = {
        let store = store.clone();
        std::thread::spawn(move || {
            assert!(store.migrate_records_to_rowid().unwrap());
            assert!(
                store
                    .compare_and_swap("refs", "main", Some(b"old"), Some(b"new"))
                    .unwrap()
            );
            send.send(()).unwrap();
        })
    };
    receive
        .recv_timeout(Duration::from_secs(5))
        .expect("WAL snapshot blocked atomic metadata migration");
    worker.join().unwrap();
    assert_eq!(
        snapshot.get_record("refs", "main").unwrap(),
        Some(b"old".to_vec())
    );
    assert_eq!(
        LocalStore::open_read_snapshot(dir.path())
            .unwrap()
            .get_record("refs", "main")
            .unwrap(),
        Some(b"new".to_vec())
    );
}

#[test]
fn migration_rejects_custom_schema_without_discarding_it() {
    for custom in [
        "CREATE INDEX custom_records_value ON records(value)",
        "CREATE TRIGGER custom_records_insert AFTER INSERT ON records BEGIN SELECT 1; END",
        "CREATE VIEW custom_records_view AS SELECT namespace,key FROM records",
        "CREATE TABLE record_owner(namespace TEXT, key TEXT, FOREIGN KEY(namespace,key) REFERENCES records(namespace,key))",
    ] {
        let dir = tempfile::tempdir().unwrap();
        legacy(dir.path());
        let store = LocalStore::open(dir.path()).unwrap();
        store.put_record("custom", "key", b"preserved").unwrap();
        let db = Connection::open(dir.path().join("metadata.sqlite3")).unwrap();
        db.execute_batch(custom).unwrap();
        assert!(
            store.migrate_records_to_rowid().is_err(),
            "discarded custom schema: {custom}"
        );
        assert!(without_rowid(dir.path(), "records"));
        assert_eq!(
            store.get_record("custom", "key").unwrap(),
            Some(b"preserved".to_vec())
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='scion_records_rowid_migration'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
}

#[test]
fn migration_rejects_custom_constraints_and_read_only_connections() {
    let dir = tempfile::tempdir().unwrap();
    let db = Connection::open(dir.path().join("metadata.sqlite3")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE records(namespace TEXT NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL CHECK(length(value)>0), PRIMARY KEY(namespace,key)) WITHOUT ROWID;").unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    assert!(store.migrate_records_to_rowid().is_err());
    assert!(
        LocalStore::open_read_only(dir.path())
            .unwrap()
            .migrate_records_to_rowid()
            .is_err()
    );
    assert!(
        LocalStore::open_read_snapshot(dir.path())
            .unwrap()
            .migrate_records_to_rowid()
            .is_err()
    );
    assert!(without_rowid(dir.path(), "records"));
}

#[test]
fn quoted_declared_types_cannot_masquerade_as_the_standard_schema() {
    for columns in [
        r#"namespace "T E X T" NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL"#,
        r#"namespace TEXT NOT NULL, key TEXT NOT NULL, value "BLOB NOT NULL""#,
        r#"namespace TEXT NOT NULL, key "TEXT NOT NULL", value BLOB NOT NULL"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = Connection::open(dir.path().join("metadata.sqlite3")).unwrap();
        db.execute_batch(&format!("PRAGMA journal_mode=WAL; CREATE TABLE records({columns}, PRIMARY KEY(namespace,key)) WITHOUT ROWID;")).unwrap();
        db.execute_batch("INSERT INTO records(namespace,key,value) VALUES ('000123','first',X'00ff'),('text','second','000456')").unwrap();
        let before = record_storage(dir.path());
        let schema: String = db
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name='records'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let store = LocalStore::open(dir.path()).unwrap();
        assert!(
            matches!(store.migrate_records_to_rowid(), Err(Error::Invalid(_))),
            "accepted masquerading declared type: {columns}"
        );
        assert_eq!(record_storage(dir.path()), before);
        assert!(without_rowid(dir.path(), "records"));
        assert_eq!(
            db.query_row(
                "SELECT sql FROM sqlite_schema WHERE name='records'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            schema
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='scion_records_rowid_migration'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
}
