use scion_engine::Repository;
use scion_store::MemoryStore;
use scion_types::*;
use std::{collections::BTreeMap, sync::Arc};

fn repository() -> Repository {
    Repository::new("virtual-bases", Arc::new(MemoryStore::new())).unwrap()
}

fn path(value: &str) -> RepoPath {
    RepoPath::new(value).unwrap()
}

fn commit(
    repository: &Repository,
    parents: &[ObjectId],
    files: &[(&str, &[u8])],
    message: &str,
) -> ObjectId {
    let changes = files
        .iter()
        .map(|(name, bytes)| {
            (
                path(name),
                FileChange::put(FileEntry {
                    id: repository.put_blob(bytes).unwrap(),
                    size: bytes.len() as u64,
                    kind: FileKind::Regular,
                }),
            )
        })
        .collect();
    repository
        .create_commit(Changeset {
            parents: parents.to_vec(),
            author: "A <a@example.com>".into(),
            committer: None,
            timestamp_ms: 1,
            message: message.into(),
            changes,
            metadata: BTreeMap::new(),
        })
        .unwrap()
        .id()
        .unwrap()
}

fn merge(repository: &Repository, ours: ObjectId, theirs: ObjectId) -> MergeResult {
    repository
        .merge(ours, theirs, "A".into(), "merge".into(), 2)
        .unwrap()
}

fn published(repository: &Repository) -> usize {
    repository
        .store()
        .read_log(repository.name(), 0, 1000)
        .unwrap()
        .len()
}

#[test]
fn criss_cross_synthesizes_the_combined_text_base_symmetrically() {
    let repository = repository();
    let root = commit(&repository, &[], &[("f", b"a\nb\nc\nd\ne\n")], "root");
    let a = commit(&repository, &[root], &[("f", b"A\nb\nc\nd\ne\n")], "a");
    let b = commit(&repository, &[root], &[("f", b"a\nb\nc\nd\nE\n")], "b");
    let x = commit(&repository, &[a, b], &[("f", b"A\nb\nc\nd\nE\n")], "x");
    let y = commit(&repository, &[b, a], &[("f", b"A\nb\nc\nd\nE\n")], "y");
    let left = commit(&repository, &[x], &[("f", b"AA\nb\nc\nd\nE\n")], "left");
    let right = commit(&repository, &[y], &[("f", b"A\nb\nc\nd\nEE\n")], "right");
    let mut expected_bases = vec![a, b];
    expected_bases.sort_unstable();
    assert_eq!(repository.merge_bases(left, right).unwrap(), expected_bases);
    repository.update_ref("main", None, Some(left)).unwrap();
    let before = published(&repository);

    let result = merge(&repository, left, right);
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    let merged = result.changeset.unwrap();
    assert_eq!(
        repository.read_file(merged, &path("f")).unwrap(),
        b"AA\nb\nc\nd\nEE\n"
    );
    assert_eq!(
        repository.get_changeset(merged).unwrap().parents,
        vec![left, right]
    );
    assert_eq!(
        published(&repository),
        before + 1,
        "virtual commits must not enter the public log"
    );
    assert_eq!(repository.resolve_ref("main").unwrap(), Some(left));

    let reversed = merge(&repository, right, left);
    assert!(reversed.conflicts.is_empty());
    assert_eq!(
        repository.root_tree(reversed.changeset.unwrap()).unwrap(),
        repository.root_tree(merged).unwrap()
    );
    assert_eq!(published(&repository), before + 2);
    assert_eq!(merge(&repository, left, right).changeset, Some(merged));
    assert_eq!(
        published(&repository),
        before + 2,
        "repeated synthesis must be idempotent"
    );
}

#[test]
fn recursively_nested_criss_cross_bases_preserve_all_independent_changes() {
    let repository = repository();
    let root = commit(&repository, &[], &[], "root");
    let a = commit(&repository, &[root], &[("a", b"a")], "a");
    let b = commit(&repository, &[root], &[("b", b"b")], "b");
    let x = commit(&repository, &[a, b], &[("b", b"b"), ("x", b"x")], "x");
    let y = commit(&repository, &[b, a], &[("a", b"a"), ("y", b"y")], "y");
    let left = commit(
        &repository,
        &[x, y],
        &[("y", b"y"), ("left", b"left")],
        "left",
    );
    let right = commit(
        &repository,
        &[y, x],
        &[("x", b"x"), ("right", b"right")],
        "right",
    );
    let before = published(&repository);

    let result = merge(&repository, left, right);
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    let merged = result.changeset.unwrap();
    let names: Vec<_> = repository
        .list_files(merged, &RepoPath::root())
        .unwrap()
        .into_keys()
        .collect();
    assert_eq!(names, ["a", "b", "left", "right", "x", "y"].map(path));
    assert_eq!(published(&repository), before + 1);
    assert_eq!(
        repository
            .root_tree(merge(&repository, right, left).changeset.unwrap())
            .unwrap(),
        repository.root_tree(merged).unwrap()
    );
}

#[test]
fn three_maximal_bases_are_combined_in_a_canonical_order() {
    let repository = repository();
    let root = commit(&repository, &[], &[], "root");
    let a = commit(&repository, &[root], &[("a", b"a")], "a");
    let b = commit(&repository, &[root], &[("b", b"b")], "b");
    let c = commit(&repository, &[root], &[("c", b"c")], "c");
    let left = commit(
        &repository,
        &[a, b, c],
        &[("b", b"b"), ("c", b"c"), ("left", b"left")],
        "left",
    );
    let right = commit(
        &repository,
        &[c, a, b],
        &[("a", b"a"), ("b", b"b"), ("right", b"right")],
        "right",
    );
    assert_eq!(repository.merge_bases(left, right).unwrap().len(), 3);

    let result = merge(&repository, left, right);
    assert!(result.conflicts.is_empty(), "{:?}", result.conflicts);
    let merged = result.changeset.unwrap();
    let names: Vec<_> = repository
        .list_files(merged, &RepoPath::root())
        .unwrap()
        .into_keys()
        .collect();
    assert_eq!(names, ["a", "b", "c", "left", "right"].map(path));
    assert_eq!(
        repository
            .root_tree(merge(&repository, right, left).changeset.unwrap())
            .unwrap(),
        repository.root_tree(merged).unwrap()
    );
}

#[test]
fn ambiguous_virtual_content_is_reported_even_when_tip_trees_agree() {
    let repository = repository();
    let root = commit(&repository, &[], &[("f", b"base\n")], "root");
    let a = commit(&repository, &[root], &[("f", b"one\n")], "a");
    let b = commit(&repository, &[root], &[("f", b"two\n")], "b");
    let left = commit(&repository, &[a, b], &[("f", b"resolution\n")], "left");
    let right = commit(&repository, &[b, a], &[("f", b"resolution\n")], "right");
    let before = published(&repository);

    let result = merge(&repository, left, right);
    assert!(result.changeset.is_none());
    assert_eq!(result.conflicts.len(), 1);
    assert_eq!(result.conflicts[0].path, path("f"));
    assert!(
        result.conflicts[0]
            .reason
            .starts_with("virtual merge base: ")
    );
    assert_eq!(
        result.conflicts[0].base,
        repository.file(root, &path("f")).unwrap()
    );
    let mut actual_entries = [
        result.conflicts[0].ours.as_ref().unwrap().id,
        result.conflicts[0].theirs.as_ref().unwrap().id,
    ];
    let mut base_entries = [
        repository.file(a, &path("f")).unwrap().unwrap().id,
        repository.file(b, &path("f")).unwrap().unwrap().id,
    ];
    actual_entries.sort_unstable();
    base_entries.sort_unstable();
    assert_eq!(actual_entries, base_entries);
    assert_eq!(merge(&repository, right, left).conflicts, result.conflicts);
    assert_eq!(published(&repository), before);
}

#[test]
fn virtual_file_directory_collisions_are_structured_conflicts() {
    let repository = repository();
    let root = commit(&repository, &[], &[], "root");
    let a = commit(&repository, &[root], &[("src", b"file")], "a");
    let b = commit(&repository, &[root], &[("src/lib.rs", b"nested")], "b");
    let left = commit(&repository, &[a, b], &[], "left");
    let right = commit(&repository, &[b, a], &[], "right");
    let before = published(&repository);

    let result = merge(&repository, left, right);
    assert!(result.changeset.is_none());
    assert_eq!(result.conflicts.len(), 1);
    assert!(
        result.conflicts[0]
            .reason
            .starts_with("virtual merge base: ")
    );
    assert_eq!(merge(&repository, right, left).conflicts, result.conflicts);
    assert_eq!(published(&repository), before);
}

#[test]
fn excessively_nested_virtual_bases_stop_without_publishing() {
    let repository = repository();
    let root = commit(&repository, &[], &[], "root");
    let mut a = commit(&repository, &[root], &[("a0", b"a")], "a0");
    let mut b = commit(&repository, &[root], &[("b0", b"b")], "b0");
    for level in 1..66 {
        let previous_a = format!("a{}", level - 1);
        let previous_b = format!("b{}", level - 1);
        let next_a = format!("a{level}");
        let next_b = format!("b{level}");
        let x = commit(
            &repository,
            &[a, b],
            &[(&previous_b, b"b"), (&next_a, b"a")],
            &next_a,
        );
        let y = commit(
            &repository,
            &[b, a],
            &[(&previous_a, b"a"), (&next_b, b"b")],
            &next_b,
        );
        (a, b) = (x, y);
    }
    let before = published(&repository);
    let result = repository.merge(a, b, "A".into(), "too deep".into(), 2);
    assert!(matches!(result, Err(Error::Limit(_))), "{result:?}");
    assert_eq!(published(&repository), before);
}
