use scion_engine::{GraphSnapshotExportLimits, Repository, export_local_graph_snapshot};
use scion_graph::{GraphAccess, GraphSnapshot, MAX_SNAPSHOT_NODES};
use scion_store::{LocalStore, Storage};
use scion_types::{Changeset, Error, ObjectId, ObjectKind, canonical};
use std::{collections::BTreeMap, fs, sync::Arc};

fn request(parents: &[ObjectId], message: &str) -> Changeset {
    Changeset {
        parents: parents.to_vec(),
        author: "Snapshot test".into(),
        committer: None,
        timestamp_ms: 0,
        message: message.into(),
        changes: BTreeMap::new(),
        metadata: BTreeMap::new(),
    }
}

fn fixture() -> (tempfile::TempDir, Arc<LocalStore>, Repository) {
    let temporary = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalStore::open(temporary.path().join("data")).unwrap());
    let repository = Repository::new("example", store.clone()).unwrap();
    (temporary, store, repository)
}

#[test]
fn export_pins_committed_wal_membership_and_rebuilds_indexes() {
    let (temporary, store, repository) = fixture();
    let root = repository
        .create_commit(request(&[], "root"))
        .unwrap()
        .id()
        .unwrap();
    let foreign = Repository::new("foreign", store.clone()).unwrap();
    let foreign_id = foreign
        .create_commit(request(&[], "foreign"))
        .unwrap()
        .id()
        .unwrap();
    let tip = repository
        .create_commit(request(&[root], "tip"))
        .unwrap()
        .id()
        .unwrap();
    assert!(
        store
            .root()
            .join("metadata.sqlite3-wal")
            .metadata()
            .unwrap()
            .len()
            > 0
    );
    let output = temporary.path().join("graph.scng");
    let metadata =
        export_local_graph_snapshot(store.root(), "example", &output, Default::default()).unwrap();
    assert_eq!(metadata.node_count, 2);
    assert_eq!(metadata.source.sequence, Some(3));
    let snapshot = GraphSnapshot::open(&output, "example", metadata.source.identity).unwrap();
    assert!(scion_graph::is_ancestor(&snapshot, root, tip).unwrap());
    assert_eq!(
        scion_graph::merge_bases(&snapshot, root, tip).unwrap(),
        vec![root]
    );
    assert_eq!(
        scion_graph::nth_first_parent(&snapshot, tip, 1).unwrap(),
        Some(root)
    );
    assert!(matches!(snapshot.node(foreign_id), Err(Error::NotFound(_))));
    assert!(GraphSnapshot::open(&output, "foreign", metadata.source.identity).is_err());

    let later = repository
        .create_commit(request(&[tip], "later"))
        .unwrap()
        .id()
        .unwrap();
    assert!(matches!(snapshot.node(later), Err(Error::NotFound(_))));
    assert!(repository.get_changeset(later).is_ok());

    // Derived graph bytes are not part of the export's trust boundary.
    let key = root.to_string();
    let old = store
        .get_record("repo/example/graph", &key)
        .unwrap()
        .unwrap();
    assert!(
        store
            .compare_and_swap(
                "repo/example/graph",
                &key,
                Some(&old),
                Some(b"invalid derived graph")
            )
            .unwrap()
    );
    let next =
        export_local_graph_snapshot(store.root(), "example", &output, Default::default()).unwrap();
    assert_eq!(next.node_count, 3);
    assert_ne!(next.source.identity, metadata.source.identity);
    let latest = GraphSnapshot::open(&output, "example", next.source.identity).unwrap();
    assert!(scion_graph::is_ancestor(&latest, root, later).unwrap());
    // Atomic replacement preserves the old open handle's frozen membership.
    assert!(matches!(snapshot.node(later), Err(Error::NotFound(_))));
    assert!(scion_graph::is_ancestor(&snapshot, root, tip).unwrap());
}

#[test]
fn corrupt_changeset_export_preserves_an_existing_artifact() {
    let (temporary, store, repository) = fixture();
    let root = repository
        .create_commit(request(&[], "root"))
        .unwrap()
        .id()
        .unwrap();
    let output = temporary.path().join("graph.scng");
    export_local_graph_snapshot(store.root(), "example", &output, Default::default()).unwrap();
    let before = fs::read(&output).unwrap();
    let mut encoded = store.get_encoded(ObjectKind::Changeset, root).unwrap();
    encoded[0] ^= 1;
    let database = rusqlite::Connection::open(store.root().join("metadata.sqlite3")).unwrap();
    database
        .execute(
            "UPDATE inline_objects SET encoded=?1 WHERE kind='changeset' AND id=?2",
            rusqlite::params![encoded, root.0.as_slice()],
        )
        .unwrap();
    assert!(
        export_local_graph_snapshot(store.root(), "example", &output, Default::default()).is_err()
    );
    assert_eq!(fs::read(output).unwrap(), before);
}

#[test]
fn invalid_changesets_membership_and_parent_closure_are_rejected() {
    for failure in [
        "validation",
        "membership",
        "foreign_parent",
        "missing_parent",
    ] {
        let (temporary, store, repository) = fixture();
        let root = repository
            .create_commit(request(&[], "root"))
            .unwrap()
            .id()
            .unwrap();
        let output = temporary.path().join("graph.scng");
        export_local_graph_snapshot(store.root(), "example", &output, Default::default()).unwrap();
        let before = fs::read(&output).unwrap();
        match failure {
            "membership" => {
                assert!(
                    store
                        .compare_and_swap(
                            "repo/example/changesets",
                            &root.to_string(),
                            Some(b"1"),
                            None
                        )
                        .unwrap()
                );
            }
            "validation" => {
                let mut invalid = request(&[root], "invalid");
                invalid.author.clear();
                let id = store
                    .put_object(ObjectKind::Changeset, &canonical(&invalid).unwrap())
                    .unwrap();
                store.append_log("example", id).unwrap();
            }
            parent_failure => {
                let parent = if parent_failure == "foreign_parent" {
                    Repository::new("foreign", store.clone())
                        .unwrap()
                        .create_commit(request(&[], "foreign"))
                        .unwrap()
                        .id()
                        .unwrap()
                } else {
                    ObjectId::hash(ObjectKind::Changeset, b"missing")
                };
                let invalid = request(&[parent], "invalid parent");
                let id = store
                    .put_object(ObjectKind::Changeset, &canonical(&invalid).unwrap())
                    .unwrap();
                store.append_log("example", id).unwrap();
            }
        }
        assert!(
            export_local_graph_snapshot(store.root(), "example", &output, Default::default())
                .is_err(),
            "{failure}"
        );
        assert_eq!(fs::read(output).unwrap(), before, "{failure}");
    }
}

#[test]
fn empty_graph_and_export_limits_are_explicit() {
    let (temporary, store, repository) = fixture();
    let output = temporary.path().join("empty.scng");
    let zero = GraphSnapshotExportLimits { max_commits: 0 };
    let metadata = export_local_graph_snapshot(store.root(), "example", &output, zero).unwrap();
    assert_eq!(metadata.node_count, 0);
    assert_eq!(metadata.source.sequence, Some(0));
    let before = fs::read(&output).unwrap();
    repository.create_commit(request(&[], "root")).unwrap();
    assert!(matches!(
        export_local_graph_snapshot(store.root(), "example", &output, zero),
        Err(Error::Limit(_))
    ));
    assert_eq!(fs::read(&output).unwrap(), before);
    assert!(matches!(
        export_local_graph_snapshot(
            store.root(),
            "example",
            &output,
            GraphSnapshotExportLimits {
                max_commits: MAX_SNAPSHOT_NODES + 1
            }
        ),
        Err(Error::Invalid(_))
    ));
    assert!(
        export_local_graph_snapshot(store.root(), "repo/escape", &output, Default::default())
            .is_err()
    );
    assert_eq!(fs::read(output).unwrap(), before);
}

#[test]
fn export_cannot_overwrite_store_files_or_hard_link_aliases() {
    let (temporary, store, repository) = fixture();
    let root = repository
        .create_commit(request(&[], "root"))
        .unwrap()
        .id()
        .unwrap();
    for output in [
        store.root().join("metadata.sqlite3"),
        store.root().join("new.scng"),
    ] {
        assert!(matches!(
            export_local_graph_snapshot(store.root(), "example", output, Default::default()),
            Err(Error::Invalid(_))
        ));
    }
    let loose = store.root().join("objects/blob/ab/example");
    fs::create_dir_all(loose.parent().unwrap()).unwrap();
    fs::write(&loose, b"a protected loose object").unwrap();
    for source in [store.root().join("metadata.sqlite3"), loose] {
        let output = temporary.path().join("alias.scng");
        fs::hard_link(&source, &output).unwrap();
        let before = fs::read(&source).unwrap();
        assert!(matches!(
            export_local_graph_snapshot(store.root(), "example", &output, Default::default()),
            Err(Error::Invalid(_))
        ));
        assert_eq!(fs::read(&source).unwrap(), before);
        assert_eq!(fs::read(&output).unwrap(), before);
        fs::remove_file(output).unwrap();
    }
    assert!(repository.get_changeset(root).is_ok());
    assert!(!store.root().join("new.scng").exists());
}

#[cfg(unix)]
#[test]
fn symlink_destinations_and_store_routes_are_rejected() {
    use std::os::unix::fs::symlink;
    let (temporary, store, repository) = fixture();
    repository.create_commit(request(&[], "root")).unwrap();
    let link = temporary.path().join("alias.scng");
    symlink(store.root().join("metadata.sqlite3"), &link).unwrap();
    assert!(
        export_local_graph_snapshot(store.root(), "example", &link, Default::default()).is_err()
    );
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let directory = temporary.path().join("store-alias");
    symlink(store.root(), &directory).unwrap();
    assert!(
        export_local_graph_snapshot(
            store.root(),
            "example",
            directory.join("graph.scng"),
            Default::default()
        )
        .is_err()
    );
    let external = temporary.path().join("external");
    fs::create_dir(&external).unwrap();
    let escaped = store.root().join("objects-escape");
    symlink(&external, &escaped).unwrap();
    assert!(
        export_local_graph_snapshot(
            store.root(),
            "example",
            escaped.join("graph.scng"),
            Default::default()
        )
        .is_err()
    );
    assert!(!external.join("graph.scng").exists());
}
