use super::*;
use crate::{LocalStore, PreparedObject, Storage};
use scion_types::{Tree, TreeEntry};
use std::collections::BTreeMap;

fn tree() -> Vec<u8> {
    Tree {
        entries: (0u32..1000)
            .map(|index| {
                (
                    format!("dir{index:04}"),
                    TreeEntry::Directory {
                        id: ObjectId::hash(ObjectKind::Tree, &index.to_le_bytes()),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
    }
    .canonical_bytes()
    .unwrap()
}

fn reset() {
    CODECS.with(|codecs| codecs.borrow_mut().metadata_cache.clear());
}

fn legacy_decode(encoded: &[u8], size: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    zstd::stream::read::Decoder::new(&encoded[HEADER..])
        .unwrap()
        .take(size as u64 + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

#[test]
fn cold_and_admitted_frames_preserve_complete_envelopes_ids_and_legacy_decoding() {
    reset();
    let bytes = tree();
    let id = ObjectId::hash(ObjectKind::Tree, &bytes);
    let cold = encode_object(ObjectKind::Tree, &bytes).unwrap();
    assert_eq!(&cold[HEADER..], zstd::bulk::compress(&bytes, 1).unwrap());
    let admitted = encode_object(ObjectKind::Tree, &bytes).unwrap();
    let repeat = PreparedObject::new(ObjectKind::Tree, bytes.clone())
        .unwrap()
        .encode()
        .unwrap();
    assert_ne!(cold, admitted);
    assert_eq!(admitted, repeat);
    assert_eq!(&cold[..HEADER], &admitted[..HEADER]);
    for encoded in [&cold, &admitted] {
        assert_eq!(decode_object(ObjectKind::Tree, id, encoded).unwrap(), bytes);
        assert_eq!(legacy_decode(encoded, bytes.len()), bytes);
    }

    for cut in [HEADER, admitted.len() / 2, admitted.len() - 1] {
        assert!(decode_object(ObjectKind::Tree, id, &admitted[..cut]).is_err());
    }
    let mut wrong_hash = admitted.clone();
    wrong_hash[17] ^= 1;
    assert!(decode_object(ObjectKind::Tree, id, &wrong_hash).is_err());
    let mut wrong_length = admitted.clone();
    wrong_length[9..17].copy_from_slice(&(bytes.len() as u64 - 1).to_le_bytes());
    assert!(decode_object(ObjectKind::Tree, id, &wrong_length).is_err());
    let mut trailing = admitted.clone();
    trailing.push(0xff);
    assert!(decode_object(ObjectKind::Tree, id, &trailing).is_err());

    // The compressed body is a valid alternate object. Only complete decoded
    // hash checking can reject it when the expected envelope is substituted.
    let mut changed = Tree::from_json(&bytes).unwrap();
    changed
        .entries
        .insert("dir0000".into(), TreeEntry::Directory { id });
    let changed_bytes = changed.canonical_bytes().unwrap();
    assert_eq!(changed_bytes.len(), bytes.len());
    let mut wrong_content = encode_object(ObjectKind::Tree, &changed_bytes).unwrap();
    wrong_content[..HEADER].copy_from_slice(&admitted[..HEADER]);
    assert!(decode_object(ObjectKind::Tree, id, &wrong_content).is_err());
}

#[test]
fn blobs_small_metadata_streams_and_decode_only_threads_keep_their_existing_paths() {
    std::thread::spawn(|| {
        let bytes = tree();
        let id = ObjectId::hash(ObjectKind::Tree, &bytes);
        let mut old = Vec::new();
        old.extend_from_slice(MAGIC);
        old.push(tag(ObjectKind::Tree));
        old.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        old.extend_from_slice(&id.0);
        zstd::stream::copy_encode(&bytes[..], &mut old, 3).unwrap();
        assert_eq!(decode_object(ObjectKind::Tree, id, &old).unwrap(), bytes);
        CODECS.with(|codecs| {
            let codecs = codecs.borrow();
            assert_eq!(codecs.metadata_cache.retained_test_bytes(), 0);
        });
        let blob = encode_object(ObjectKind::Blob, &bytes).unwrap();
        assert_eq!(&blob[HEADER..], zstd::bulk::compress(&bytes, 3).unwrap());
        let small = encode_object(ObjectKind::Changeset, b"small metadata").unwrap();
        assert_eq!(
            &small[HEADER..],
            zstd::bulk::compress(b"small metadata", 1).unwrap()
        );
        CODECS.with(|codecs| assert_eq!(codecs.borrow().metadata_cache.retained_test_bytes(), 0));

        encode_object(ObjectKind::Tree, &bytes).unwrap();
        encode_object(ObjectKind::Tree, &bytes).unwrap();
        let retained = CODECS.with(|codecs| codecs.borrow().metadata_cache.retained_test_bytes());
        assert!(retained > 0);
        let large = vec![b'x'; BULK_LIMIT + 1];
        let large_id = ObjectId::hash(ObjectKind::Derived, &large);
        let encoded = encode_object(ObjectKind::Derived, &large).unwrap();
        assert_eq!(
            decode_object(ObjectKind::Derived, large_id, &encoded).unwrap(),
            large
        );
        encode_object(ObjectKind::Blob, &bytes).unwrap();
        encode_object(ObjectKind::Changeset, b"small metadata").unwrap();
        assert_eq!(
            CODECS.with(|codecs| codecs.borrow().metadata_cache.retained_test_bytes()),
            retained
        );
        reset();
        assert_eq!(
            CODECS.with(|codecs| codecs.borrow().metadata_cache.retained_test_bytes()),
            0
        );
    })
    .join()
    .unwrap();
}

#[test]
fn alternate_frame_publication_accepts_valid_duplicates_and_rejects_stored_corruption() {
    reset();
    let bytes = tree();
    let object = PreparedObject::new(ObjectKind::Tree, bytes.clone()).unwrap();
    let whole = object.encode().unwrap();
    let framed = object.encode().unwrap();
    assert_ne!(whole, framed);
    let temporary = tempfile::tempdir().unwrap();
    let store = LocalStore::open(temporary.path()).unwrap();
    store
        .put_encoded(ObjectKind::Tree, object.id(), &whole)
        .unwrap();
    store
        .put_encoded(ObjectKind::Tree, object.id(), &framed)
        .unwrap();
    let first = ObjectId::hash(ObjectKind::Changeset, b"first publication");
    store
        .publish_commit_with_objects("repo", first, std::slice::from_ref(&object), &[])
        .unwrap();
    assert_eq!(
        store.get_encoded(ObjectKind::Tree, object.id()).unwrap(),
        whole
    );
    assert_eq!(
        store.get_object(ObjectKind::Tree, object.id()).unwrap(),
        bytes
    );
    assert_eq!(
        store
            .try_get_small_object(ObjectKind::Tree, object.id(), bytes.len())
            .unwrap(),
        Some(bytes.clone())
    );
    let framed_store = LocalStore::open(temporary.path().join("framed")).unwrap();
    framed_store
        .put_encoded(ObjectKind::Tree, object.id(), &framed)
        .unwrap();
    framed_store
        .put_encoded(ObjectKind::Tree, object.id(), &whole)
        .unwrap();
    assert_eq!(
        framed_store
            .get_encoded(ObjectKind::Tree, object.id())
            .unwrap(),
        framed
    );
    assert_eq!(
        framed_store
            .try_get_small_object(ObjectKind::Tree, object.id(), bytes.len())
            .unwrap(),
        Some(bytes)
    );

    let database = rusqlite::Connection::open(temporary.path().join("metadata.sqlite3")).unwrap();
    let mut corrupt = whole;
    corrupt[0] ^= 1;
    database
        .execute(
            "UPDATE inline_objects SET encoded=?1 WHERE kind='tree' AND id=?2",
            rusqlite::params![corrupt, object.id().0.as_slice()],
        )
        .unwrap();
    let second = ObjectId::hash(ObjectKind::Changeset, b"must not publish");
    assert!(
        store
            .publish_commit_with_objects("repo", second, &[object], &[])
            .is_err()
    );
    assert_eq!(store.read_log("repo", 0, 10).unwrap().len(), 1);
    assert!(
        store
            .get_record("repo/repo/changesets", &second.to_string())
            .unwrap()
            .is_none()
    );
}
