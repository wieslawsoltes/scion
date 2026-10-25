use rusqlite::{Connection, params};
use scion_store::{LocalStore, MAX_OBJECT_SIZE, MemoryStore, ShardedStore, Storage, encode_object};
use scion_types::{Error, ObjectId, ObjectKind};
use std::{fs, path::Path, sync::Arc};

fn write_loose(root: &Path, kind: ObjectKind, id: ObjectId, encoded: &[u8]) {
    let hex = id.to_string();
    let path = root
        .join("objects")
        .join(kind.as_str())
        .join(&hex[..2])
        .join(&hex[2..]);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, encoded).unwrap();
}

fn replace_inline(root: &Path, kind: ObjectKind, id: ObjectId, encoded: &[u8]) {
    let db = Connection::open(root.join("metadata.sqlite3")).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE inline_objects SET encoded=?3 WHERE kind=?1 AND id=?2",
            params![kind.as_str(), id.0.as_slice(), encoded]
        )
        .unwrap(),
        1
    );
}

#[test]
fn verified_small_reads_match_full_reads_at_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let memory = MemoryStore::new();
    let local = LocalStore::open(dir.path()).unwrap();
    let sharded = ShardedStore::new(
        Arc::new(MemoryStore::new()),
        vec![Arc::new(MemoryStore::new())],
    )
    .unwrap();
    for store in [&memory as &dyn Storage, &local, &sharded] {
        for length in [0, 1, 128, 8192, 8193, 65536] {
            let bytes: Vec<_> = (0..length).map(|i| (i % 251) as u8).collect();
            let id = store.put_object(ObjectKind::Changeset, &bytes).unwrap();
            let small = store
                .try_get_small_object(ObjectKind::Changeset, id, 8192)
                .unwrap();
            assert_eq!(small, (length <= 8192).then(|| bytes.clone()));
            assert_eq!(
                store
                    .try_get_small_object(ObjectKind::Changeset, id, usize::MAX)
                    .unwrap(),
                Some(bytes.clone())
            );
            assert_eq!(store.get_object(ObjectKind::Changeset, id).unwrap(), bytes);
        }
        let empty = store.put_object(ObjectKind::Blob, b"").unwrap();
        assert_eq!(
            store
                .try_get_small_object(ObjectKind::Blob, empty, 0)
                .unwrap(),
            Some(Vec::new())
        );
        assert!(matches!(
            store.try_get_small_object(ObjectKind::Blob, ObjectId([0xff; 32]), 8192),
            Err(Error::NotFound(_))
        ));
    }
}

#[test]
fn legacy_loose_reads_are_bounded_and_keep_header_validation() {
    let dir = tempfile::tempdir().unwrap();
    drop(LocalStore::open(dir.path()).unwrap());
    let db = Connection::open(dir.path().join("metadata.sqlite3")).unwrap();
    db.execute_batch("DROP TABLE inline_objects").unwrap();
    drop(db);
    let bytes = b"legacy small delta";
    let id = ObjectId::hash(ObjectKind::Changeset, bytes);
    let original = encode_object(ObjectKind::Changeset, bytes).unwrap();
    write_loose(dir.path(), ObjectKind::Changeset, id, &original);
    let reader = LocalStore::open_read_only(dir.path()).unwrap();
    assert_eq!(
        reader
            .try_get_small_object(ObjectKind::Changeset, id, bytes.len())
            .unwrap(),
        Some(bytes.to_vec())
    );
    assert_eq!(
        reader
            .try_get_small_object(ObjectKind::Changeset, id, bytes.len() - 1)
            .unwrap(),
        None
    );
    // Oversized declared data must fall back without decoding its payload.
    let mut oversized = original.clone();
    oversized[9..17].copy_from_slice(&(MAX_OBJECT_SIZE as u64).to_le_bytes());
    oversized[49..].fill(0xff);
    write_loose(dir.path(), ObjectKind::Changeset, id, &oversized);
    assert_eq!(
        reader
            .try_get_small_object(ObjectKind::Changeset, id, 8192)
            .unwrap(),
        None
    );
    assert!(reader.get_object(ObjectKind::Changeset, id).is_err());
    // An oversized result must still reject the wrong kind or ID in its header.
    oversized[8] = 1;
    write_loose(dir.path(), ObjectKind::Changeset, id, &oversized);
    assert!(matches!(
        reader.try_get_small_object(ObjectKind::Changeset, id, 8192),
        Err(Error::Corrupt(_))
    ));
    for end in [0, 8, 48, 49] {
        write_loose(dir.path(), ObjectKind::Changeset, id, &original[..end]);
        assert!(
            reader
                .try_get_small_object(ObjectKind::Changeset, id, 8192)
                .is_err()
        );
    }
}

#[test]
fn large_valid_skippable_representations_fall_back_without_changing_full_reads() {
    let dir = tempfile::tempdir().unwrap();
    let local = LocalStore::open(dir.path()).unwrap();
    let memory = MemoryStore::new();
    let bytes = b"small content with a large legal compressed representation";
    let id = ObjectId::hash(ObjectKind::Blob, bytes);
    let original = encode_object(ObjectKind::Blob, bytes).unwrap();
    let mut encoded = original[..49].to_vec();
    encoded.extend_from_slice(&0x184d2a50_u32.to_le_bytes());
    encoded.extend_from_slice(&16_384u32.to_le_bytes());
    encoded.extend_from_slice(&vec![0; 16_384]);
    encoded.extend_from_slice(&original[49..]);
    for store in [&memory as &dyn Storage, &local] {
        store.put_encoded(ObjectKind::Blob, id, &encoded).unwrap();
        assert_eq!(
            store
                .try_get_small_object(ObjectKind::Blob, id, 8192)
                .unwrap(),
            None
        );
        assert_eq!(store.get_object(ObjectKind::Blob, id).unwrap(), bytes);
        assert_eq!(
            store
                .try_get_small_object(ObjectKind::Blob, id, 32_768)
                .unwrap(),
            Some(bytes.to_vec())
        );
    }
    let loose_dir = tempfile::tempdir().unwrap();
    let loose = LocalStore::open(loose_dir.path()).unwrap();
    write_loose(loose_dir.path(), ObjectKind::Blob, id, &encoded);
    assert_eq!(
        loose
            .try_get_small_object(ObjectKind::Blob, id, 8192)
            .unwrap(),
        None
    );
    assert_eq!(loose.get_object(ObjectKind::Blob, id).unwrap(), bytes);
}

#[test]
fn inline_small_reads_reject_inspected_corruption_and_global_size_violations() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path()).unwrap();
    let bytes = b"verified small delta";
    let kind = ObjectKind::Changeset;
    let id = store.put_object(kind, bytes).unwrap();
    let original = encode_object(kind, bytes).unwrap();
    let mut variants = vec![Vec::new(), original[..49].to_vec()];
    let mut wrong_kind = original.clone();
    wrong_kind[8] = 1;
    variants.push(wrong_kind);
    let mut wrong_id = original.clone();
    wrong_id[17] ^= 1;
    variants.push(wrong_id);
    let mut wrong_size = original.clone();
    wrong_size[9..17].copy_from_slice(&((MAX_OBJECT_SIZE + 1) as u64).to_le_bytes());
    variants.push(wrong_size);
    let mut wrong_body = original.clone();
    wrong_body[49..].fill(0xff);
    variants.push(wrong_body);
    let mut mismatched_hash = original[..49].to_vec();
    mismatched_hash
        .extend_from_slice(&encode_object(kind, &vec![b'x'; bytes.len()]).unwrap()[49..]);
    variants.push(mismatched_hash);
    for encoded in variants {
        replace_inline(dir.path(), kind, id, &encoded);
        assert!(store.try_get_small_object(kind, id, 8192).is_err());
    }
    replace_inline(dir.path(), kind, id, &original);
    assert_eq!(
        store.try_get_small_object(kind, id, 8192).unwrap(),
        Some(bytes.to_vec())
    );
}
