use rusqlite::Connection;
use scion_store::{LocalStore, LocalStoreOptions, PreparedObject, Storage};
use scion_types::{ObjectId, ObjectKind};

#[test]
fn default_close_keeps_durable_wal_and_explicit_checkpoint_cleans_it() {
    let dir = tempfile::tempdir().unwrap();
    let wal = dir.path().join("metadata.sqlite3-wal");
    let object =
        PreparedObject::new(ObjectKind::Changeset, b"durable in retained WAL".to_vec()).unwrap();
    {
        let writer = LocalStore::open(dir.path()).unwrap();
        assert_eq!(writer.options(), LocalStoreOptions::default());
        writer
            .publish_commit_with_objects("r", object.id(), std::slice::from_ref(&object), &[])
            .unwrap();
    }
    assert!(wal.is_file(), "default close must retain the durable WAL");
    {
        let reader = LocalStore::open_read_only(dir.path()).unwrap();
        assert_eq!(
            reader.get_object(object.kind(), object.id()).unwrap(),
            object.bytes()
        );
        assert_eq!(reader.read_log("r", 0, 10).unwrap()[0].id, object.id());
    }
    assert!(
        wal.is_file(),
        "read-only close must not discard WAL coordination state"
    );
    {
        let writer = LocalStore::open_with_options(
            dir.path(),
            LocalStoreOptions {
                mmap_size: 0,
                checkpoint_on_close: true,
            },
        )
        .unwrap();
        writer
            .put_record("options", "checkpoint", b"enabled")
            .unwrap();
    }
    assert!(
        !wal.exists(),
        "explicit checkpoint-on-close must preserve SQLite's cleanup policy"
    );
    let reader = LocalStore::open_read_only_with_options(
        dir.path(),
        LocalStoreOptions {
            mmap_size: 0,
            checkpoint_on_close: false,
        },
    )
    .unwrap();
    assert_eq!(
        reader.get_object(object.kind(), object.id()).unwrap(),
        object.bytes()
    );
    assert_eq!(
        reader.get_record("options", "checkpoint").unwrap(),
        Some(b"enabled".to_vec())
    );
}

#[test]
fn mapped_and_unmapped_readers_observe_uncheckpointed_wal_updates() {
    let dir = tempfile::tempdir().unwrap();
    let writer = LocalStore::open(dir.path()).unwrap();
    writer.put_record("options", "first", b"immutable").unwrap();
    writer
        .compare_and_swap("refs", "main", None, Some(b"old"))
        .unwrap();
    let raw = Connection::open(dir.path().join("metadata.sqlite3")).unwrap();
    raw.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let readers: Vec<_> = [0, 256 * 1024 * 1024]
        .into_iter()
        .map(|mmap_size| {
            let reader = LocalStore::open_read_only_with_options(
                dir.path(),
                LocalStoreOptions {
                    mmap_size,
                    checkpoint_on_close: false,
                },
            )
            .unwrap();
            assert_eq!(
                reader.get_record("refs", "main").unwrap(),
                Some(b"old".to_vec())
            );
            reader
        })
        .collect();
    assert!(
        writer
            .compare_and_swap("refs", "main", Some(b"old"), Some(b"new"))
            .unwrap()
    );
    writer.put_record("options", "second", b"fresh").unwrap();
    for reader in readers {
        assert_eq!(
            reader
                .get_record_pair(("refs", "main"), ("options", "second"))
                .unwrap(),
            [Some(b"new".to_vec()), Some(b"fresh".to_vec())]
        );
        assert_eq!(reader.scan_records("options", "", 10).unwrap().len(), 2);
    }
}

#[test]
fn invalid_options_do_not_create_a_store() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent");
    let invalid = LocalStoreOptions {
        mmap_size: u64::MAX,
        checkpoint_on_close: false,
    };
    assert!(LocalStore::open_with_options(&missing, invalid).is_err());
    assert!(LocalStore::open_read_only_with_options(&missing, invalid).is_err());
    assert!(!missing.exists());
}

#[test]
fn mapped_and_unmapped_queries_report_corrupt_database_contents() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("metadata.sqlite3"),
        b"not a SQLite database".repeat(256),
    )
    .unwrap();
    for mmap_size in [0, 256 * 1024 * 1024] {
        let reader = LocalStore::open_read_only_with_options(
            dir.path(),
            LocalStoreOptions {
                mmap_size,
                checkpoint_on_close: false,
            },
        )
        .unwrap();
        assert!(reader.get_record("namespace", "key").is_err());
        assert!(
            reader
                .get_record_pair(("namespace", "key"), ("other", "key"))
                .is_err()
        );
        assert!(
            reader
                .get_object(ObjectKind::Blob, ObjectId([1; 32]))
                .is_err()
        );
        assert!(reader.read_log("r", 0, 10).is_err());
        assert!(reader.scan_records("namespace", "", 10).is_err());
    }
}
