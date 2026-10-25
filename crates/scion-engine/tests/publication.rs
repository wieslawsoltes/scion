use scion_engine::{CommitRefUpdate, Repository};
use scion_store::{LocalStore, MemoryStore, PreparedObject, SharedStore, Storage};
use scion_types::*;
use std::{collections::BTreeMap, sync::Arc};

fn staged(parent: Option<ObjectId>, content: &[u8], message: &str) -> (Changeset, PreparedObject) {
    let blob = PreparedObject::new(ObjectKind::Blob, content.to_vec()).unwrap();
    let request = Changeset {
        parents: parent.into_iter().collect(),
        author: "author".into(),
        committer: None,
        timestamp_ms: 0,
        message: message.into(),
        changes: BTreeMap::from([(
            RepoPath::new("dir/file").unwrap(),
            FileChange::put(FileEntry {
                id: blob.id(),
                size: content.len() as u64,
                kind: FileKind::Regular,
            }),
        )]),
        metadata: BTreeMap::new(),
    };
    (request, blob)
}

fn reference(expected: Option<ObjectId>) -> Option<CommitRefUpdate> {
    Some(CommitRefUpdate {
        name: "main".into(),
        expected,
    })
}

fn publication_contract(store: SharedStore) -> (ObjectId, ObjectId) {
    let repo = Repository::new("publication", store).unwrap();
    let (request, blob) = staged(None, b"first", "root");
    let first = repo
        .create_commit_with_blobs(request, vec![blob], reference(None))
        .unwrap();
    let first_id = first.changeset.id().unwrap();
    assert_eq!(first.reference_updated, Some(true));
    assert_eq!(repo.resolve_ref("main").unwrap(), Some(first_id));

    let (request, blob) = staged(Some(first_id), b"stale publication", "stale");
    let stale = repo
        .create_commit_with_blobs(request, vec![blob], reference(None))
        .unwrap();
    let stale_id = stale.changeset.id().unwrap();
    assert_eq!(stale.reference_updated, Some(false));
    assert!(stale.sequence > first.sequence);
    assert_eq!(repo.resolve_ref("main").unwrap(), Some(first_id));
    assert_eq!(
        repo.read_file(stale_id, &RepoPath::new("dir/file").unwrap())
            .unwrap(),
        b"stale publication"
    );

    let (request, blob) = staged(Some(stale_id), b"current", "current");
    let current = repo
        .create_commit_with_blobs(request, vec![blob], reference(Some(first_id)))
        .unwrap();
    let current_id = current.changeset.id().unwrap();
    assert_eq!(current.reference_updated, Some(true));
    assert!(current.sequence > stale.sequence);
    assert_eq!(repo.resolve_ref("main").unwrap(), Some(current_id));

    let (request, blob) = staged(Some(current_id), b"independent", "independent");
    let independent = repo
        .create_commit_with_blobs(request, vec![blob], None)
        .unwrap();
    assert_eq!(independent.reference_updated, None);
    assert_eq!(repo.resolve_ref("main").unwrap(), Some(current_id));
    assert_eq!(
        repo.store().read_log("publication", 0, 10).unwrap().len(),
        4
    );
    (current_id, stale_id)
}

#[test]
fn staged_publication_preserves_portable_fallback_and_local_reopen_semantics() {
    publication_contract(Arc::new(MemoryStore::new()));
    let directory = tempfile::tempdir().unwrap();
    let (current, stale) =
        publication_contract(Arc::new(LocalStore::open(directory.path()).unwrap()));
    let reader = Repository::new(
        "publication",
        Arc::new(LocalStore::open_read_only(directory.path()).unwrap()),
    )
    .unwrap();
    assert_eq!(reader.resolve_ref("main").unwrap(), Some(current));
    assert_eq!(
        reader
            .read_file(current, &RepoPath::new("dir/file").unwrap())
            .unwrap(),
        b"current"
    );
    assert_eq!(
        reader
            .read_file(stale, &RepoPath::new("dir/file").unwrap())
            .unwrap(),
        b"stale publication"
    );
    assert_eq!(
        reader.store().read_log("publication", 0, 10).unwrap().len(),
        4
    );
}

#[test]
fn invalid_staged_content_and_reference_fail_before_publication() {
    let store = Arc::new(MemoryStore::new());
    let repo = Repository::new("publication", store.clone()).unwrap();
    let (request, blob) = staged(None, b"content", "root");
    let blob_id = blob.id();
    let mut wrong_size = request.clone();
    if let FileChange::Put { entry, .. } = wrong_size.changes.values_mut().next().unwrap() {
        entry.size += 1;
    }
    assert!(
        repo.create_commit_with_blobs(wrong_size, vec![blob.clone()], None)
            .is_err()
    );
    assert!(
        repo.create_commit_with_blobs(
            request.clone(),
            vec![blob.clone()],
            Some(CommitRefUpdate {
                name: "../invalid".into(),
                expected: None
            })
        )
        .is_err()
    );
    let non_blob = PreparedObject::new(ObjectKind::Tree, b"{}".to_vec()).unwrap();
    assert!(
        repo.create_commit_with_blobs(request.clone(), vec![non_blob], None)
            .is_err()
    );
    assert!(
        repo.create_commit_with_blobs(request, Vec::new(), None)
            .is_err()
    );
    assert!(store.get_object(ObjectKind::Blob, blob_id).is_err());
    assert!(store.read_log("publication", 0, 10).unwrap().is_empty());
}

#[test]
fn concurrently_staged_commits_are_all_visible_with_one_reference_winner() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(directory.path()).unwrap());
    let repo = Repository::new("publication", store).unwrap();
    let (request, blob) = staged(None, b"root", "root");
    let root = repo
        .create_commit_with_blobs(request, vec![blob], reference(None))
        .unwrap()
        .changeset
        .id()
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|index| {
            let path = directory.path().to_owned();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let repo =
                    Repository::new("publication", Arc::new(LocalStore::open(path).unwrap()))
                        .unwrap();
                let text = format!("writer {index}");
                let (request, blob) = staged(Some(root), text.as_bytes(), &text);
                barrier.wait();
                repo.create_commit_with_blobs(request, vec![blob], reference(Some(root)))
                    .unwrap()
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
            .filter(|result| result.reference_updated == Some(true))
            .count(),
        1
    );
    assert_eq!(
        repo.store().read_log("publication", 0, 10).unwrap().len(),
        5
    );
    for result in results {
        let id = result.changeset.id().unwrap();
        assert_eq!(repo.get_changeset(id).unwrap(), result.changeset);
        assert!(
            repo.read_file(id, &RepoPath::new("dir/file").unwrap())
                .is_ok()
        );
    }
}
