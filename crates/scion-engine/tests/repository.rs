use scion_engine::{DerivedData, Repository};
use scion_store::{MemoryStore, Storage};
use scion_types::*;
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn repo() -> Repository {
    Repository::new("test", Arc::new(MemoryStore::new())).unwrap()
}
fn path(p: &str) -> RepoPath {
    RepoPath::new(p).unwrap()
}
fn entry(repo: &Repository, text: &[u8]) -> FileEntry {
    FileEntry {
        id: repo.put_blob(text).unwrap(),
        size: text.len() as u64,
        kind: FileKind::Regular,
    }
}
fn commit(
    repo: &Repository,
    parents: &[ObjectId],
    files: &[(&str, Option<&[u8]>)],
    message: &str,
) -> ObjectId {
    let changes = files
        .iter()
        .map(|(p, text)| {
            (
                path(p),
                match text {
                    Some(text) => FileChange::put(entry(repo, text)),
                    None => FileChange::Delete,
                },
            )
        })
        .collect();
    let commit = Changeset {
        parents: parents.to_vec(),
        author: "A <a@example.com>".into(),
        committer: None,
        timestamp_ms: 1000,
        message: message.into(),
        changes,
        metadata: BTreeMap::new(),
    };
    repo.create_commit(commit).unwrap().id().unwrap()
}

#[test]
fn immutable_commit_is_independent_of_refs_and_idempotent() {
    let repo = repo();
    let a = commit(&repo, &[], &[("src/lib.rs", Some(b"hello\n"))], "root");
    assert!(repo.list_refs().unwrap().is_empty());
    assert!(repo.update_ref("main", None, Some(a)).unwrap());
    let b = commit(&repo, &[a], &[("src/lib.rs", Some(b"changed\n"))], "next");
    assert!(!repo.update_ref("main", None, Some(b)).unwrap());
    assert_eq!(repo.resolve_ref("main").unwrap(), Some(a));
    assert!(repo.update_ref("main", Some(a), Some(b)).unwrap());
    repo.create_commit(repo.get_changeset(b).unwrap()).unwrap();
    assert_eq!(repo.store().read_log("test", 0, 10).unwrap().len(), 2);
    assert_eq!(repo.read_file(a, &path("src/lib.rs")).unwrap(), b"hello\n");
    assert_eq!(
        repo.read_file(b, &path("src/lib.rs")).unwrap(),
        b"changed\n"
    );
}

#[test]
fn repository_membership_prevents_cross_repository_reads_and_parent_injection() {
    let store = Arc::new(MemoryStore::new());
    let a = Repository::new("a", store.clone()).unwrap();
    let b = Repository::new("b", store).unwrap();
    let id = commit(&a, &[], &[], "a");
    assert!(matches!(b.get_changeset(id), Err(Error::NotFound(_))));
    let mut injected = a.get_changeset(id).unwrap();
    injected.parents.push(id);
    assert!(b.create_commit(injected).is_err());
}

#[test]
fn file_directory_replacements_are_explicit_and_lossless() {
    let repo = repo();
    let a = commit(&repo, &[], &[("a", Some(b"old"))], "file");
    let b = commit(
        &repo,
        &[a],
        &[("a", None), ("a/b", Some(b"nested"))],
        "directory",
    );
    assert_eq!(repo.read_file(b, &path("a/b")).unwrap(), b"nested");
    let c = commit(
        &repo,
        &[b],
        &[("a", Some(b"new")), ("a/b", None)],
        "file again",
    );
    assert_eq!(repo.read_file(c, &path("a")).unwrap(), b"new");
    assert_eq!(repo.history(c, &path("a"), 10).unwrap(), vec![c, b, a]);
    assert_eq!(repo.history(c, &path("a/b"), 10).unwrap(), vec![c, b]);
    let request = Changeset {
        parents: vec![b],
        author: "a".into(),
        committer: None,
        timestamp_ms: 0,
        message: "unsafe".into(),
        changes: BTreeMap::from([(path("a"), FileChange::put(entry(&repo, b"overwrite")))]),
        metadata: BTreeMap::new(),
    };
    assert!(matches!(
        repo.create_commit(request),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn dag_history_merge_bases_and_second_parent_ancestry() {
    let repo = repo();
    let root = commit(&repo, &[], &[], "root");
    let a = commit(&repo, &[root], &[("a", Some(b"a"))], "a");
    let b = commit(&repo, &[root], &[("b", Some(b"b"))], "b");
    let merged = repo
        .merge(a, b, "a".into(), "merge".into(), 0)
        .unwrap()
        .changeset
        .unwrap();
    assert!(repo.is_ancestor(b, merged).unwrap());
    assert_eq!(repo.merge_bases(a, b).unwrap(), vec![root]);
    assert_eq!(repo.list_files(merged, &RepoPath::root()).unwrap().len(), 2);
    assert_eq!(repo.log(merged, 10).unwrap().len(), 4);
}

#[test]
fn three_way_text_merge_preserves_both_sides_and_modes() {
    let repo = repo();
    let root = commit(&repo, &[], &[("f", Some(b"a\nb\nc\nd\ne\n"))], "root");
    let a = commit(&repo, &[root], &[("f", Some(b"A\nb\nc\nd\ne\n"))], "a");
    let b = commit(&repo, &[root], &[("f", Some(b"a\nb\nc\nd\nE\n"))], "b");
    let merged = repo.merge(a, b, "a".into(), "merge".into(), 0).unwrap();
    assert!(merged.conflicts.is_empty());
    assert_eq!(
        repo.read_file(merged.changeset.unwrap(), &path("f"))
            .unwrap(),
        b"A\nb\nc\nd\nE\n"
    );
}

#[test]
fn conflicts_never_publish_a_changeset() {
    let repo = repo();
    let root = commit(&repo, &[], &[("f", Some(b"base\n"))], "root");
    let a = commit(&repo, &[root], &[("f", Some(b"ours\n"))], "a");
    let b = commit(&repo, &[root], &[("f", Some(b"theirs\n"))], "b");
    let before = repo.store().read_log("test", 0, 100).unwrap().len();
    let merged = repo.merge(a, b, "a".into(), "conflict".into(), 0).unwrap();
    assert!(merged.changeset.is_none());
    assert_eq!(merged.conflicts.len(), 1);
    assert_eq!(merged.conflicts[0].path, path("f"));
    assert_eq!(repo.store().read_log("test", 0, 100).unwrap().len(), before);
    let deleted = commit(&repo, &[root], &[("f", None)], "delete");
    assert!(
        !repo
            .merge(a, deleted, "a".into(), "conflict".into(), 0)
            .unwrap()
            .conflicts
            .is_empty()
    );
}

#[test]
fn rebase_and_revert_apply_deltas_without_losing_unrelated_work() {
    let repo = repo();
    let root = commit(&repo, &[], &[("f", Some(b"base"))], "root");
    let a = commit(&repo, &[root], &[("f", Some(b"changed"))], "change");
    let b = commit(&repo, &[root], &[("g", Some(b"unrelated"))], "side");
    let rebased = repo.rebase(a, b).unwrap().changeset.unwrap();
    assert_eq!(repo.get_changeset(rebased).unwrap().parents, vec![b]);
    assert_eq!(repo.read_file(rebased, &path("g")).unwrap(), b"unrelated");
    let reverted = repo
        .revert(a, rebased, 0, "a".into(), "revert".into(), 0)
        .unwrap()
        .changeset
        .unwrap();
    assert_eq!(repo.read_file(reverted, &path("f")).unwrap(), b"base");
    assert_eq!(repo.read_file(reverted, &path("g")).unwrap(), b"unrelated");
}

#[test]
fn blame_tracks_unchanged_lines_and_explicit_copies() {
    let repo = repo();
    let a = commit(&repo, &[], &[("f", Some(b"one\ntwo\nthree\n"))], "a");
    let b = commit(&repo, &[a], &[("f", Some(b"one\nTWO\nthree\n"))], "b");
    let blame = repo.blame(b, &path("f")).unwrap();
    assert_eq!(
        blame.iter().map(|l| l.changeset).collect::<Vec<_>>(),
        vec![a, b, a]
    );
    assert_eq!(blame[2].original_line, 3);
    let request = Changeset {
        parents: vec![b],
        author: "a".into(),
        committer: None,
        timestamp_ms: 0,
        message: "copy".into(),
        changes: BTreeMap::from([(
            path("copied"),
            FileChange::Put {
                entry: repo.file(b, &path("f")).unwrap().unwrap(),
                copy_from: Some(CopySource {
                    changeset: b,
                    path: path("f"),
                }),
            },
        )]),
        metadata: BTreeMap::new(),
    };
    let c = repo.create_commit(request).unwrap().id().unwrap();
    assert_eq!(repo.blame(c, &path("copied")).unwrap(), blame);
    assert_eq!(repo.copies(c).unwrap()[&path("copied")].path, path("f"));
}

#[test]
fn search_is_component_scoped_and_diff_is_binary_safe() {
    let repo = repo();
    let a = commit(
        &repo,
        &[],
        &[("src/a.rs", Some(b"a")), ("src-extra/b.rs", Some(b"b"))],
        "a",
    );
    assert_eq!(
        repo.search(a, &path("src"), ".rs", 10).unwrap(),
        vec![path("src/a.rs")]
    );
    let b = commit(&repo, &[a], &[("binary", Some(b"\0\xff"))], "binary");
    let diff = repo.diff(a, b).unwrap();
    assert_eq!(diff.len(), 1);
    assert!(diff[0].patch.is_none());
}

struct CountIndex;
impl DerivedData for CountIndex {
    fn name(&self) -> &str {
        "test_count"
    }
    fn version(&self) -> u32 {
        1
    }
    fn derive(&self, _: &Repository, _: &Changeset, parents: &[Vec<u8>]) -> Result<Vec<u8>> {
        let total = parents
            .iter()
            .map(|p| serde_json::from_slice::<u64>(p).unwrap())
            .sum::<u64>()
            + 1;
        canonical(&total)
    }
}

#[test]
fn recursive_derived_data_is_memoized_deterministic_and_stack_safe() {
    let repo = repo();
    let mut tip = commit(&repo, &[], &[], "root");
    for i in 0..512 {
        tip = commit(&repo, &[tip], &[], &format!("commit {i}"));
    }
    let value = repo.derive(tip, &CountIndex).unwrap();
    assert_eq!(
        serde_json::from_slice::<u64>(
            &repo.store().get_object(ObjectKind::Derived, value).unwrap()
        )
        .unwrap(),
        513
    );
    assert_eq!(repo.derive(tip, &CountIndex).unwrap(), value);
}

#[test]
fn concurrent_commits_do_not_move_refs_and_cas_has_one_winner() {
    let repo = repo();
    let root = commit(&repo, &[], &[], "root");
    assert!(repo.update_ref("main", None, Some(root)).unwrap());
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let repo = repo.clone();
            std::thread::spawn(move || {
                let id = commit(&repo, &[root], &[], &format!("parallel {i}"));
                repo.update_ref("main", Some(root), Some(id)).unwrap()
            })
        })
        .collect();
    let wins = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|won| *won)
        .count();
    assert_eq!(wins, 1);
    assert_eq!(repo.store().read_log("test", 0, 100).unwrap().len(), 9);
}

#[derive(Default)]
struct CountingStore {
    inner: MemoryStore,
    tree_reads: AtomicUsize,
    tree_writes: AtomicUsize,
    blob_reads: AtomicUsize,
}
impl Storage for CountingStore {
    fn put_object(&self, k: ObjectKind, b: &[u8]) -> Result<ObjectId> {
        if k == ObjectKind::Tree {
            self.tree_writes.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.put_object(k, b)
    }
    fn get_object(&self, k: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        if k == ObjectKind::Tree {
            self.tree_reads.fetch_add(1, Ordering::Relaxed);
        }
        if k == ObjectKind::Blob {
            self.blob_reads.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.get_object(k, id)
    }
    fn get_encoded(&self, k: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        self.inner.get_encoded(k, id)
    }
    fn put_encoded(&self, k: ObjectKind, id: ObjectId, b: &[u8]) -> Result<()> {
        self.inner.put_encoded(k, id, b)
    }
    fn get_record(&self, n: &str, k: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get_record(n, k)
    }
    fn put_record(&self, n: &str, k: &str, v: &[u8]) -> Result<()> {
        self.inner.put_record(n, k, v)
    }
    fn compare_and_swap(
        &self,
        n: &str,
        k: &str,
        e: Option<&[u8]>,
        v: Option<&[u8]>,
    ) -> Result<bool> {
        self.inner.compare_and_swap(n, k, e, v)
    }
    fn append_log(&self, r: &str, id: ObjectId) -> Result<u64> {
        self.inner.append_log(r, id)
    }
    fn read_log(&self, r: &str, a: u64, l: usize) -> Result<Vec<LogEntry>> {
        self.inner.read_log(r, a, l)
    }
    fn scan_records(&self, n: &str, p: &str, l: usize) -> Result<Vec<(String, Vec<u8>)>> {
        self.inner.scan_records(n, p, l)
    }
}

#[test]
fn one_file_commit_does_not_read_or_rewrite_unrelated_subtrees() {
    let store = Arc::new(CountingStore::default());
    let repo = Repository::new("test", store.clone()).unwrap();
    let initial = Changeset {
        parents: vec![],
        author: "a".into(),
        committer: None,
        timestamp_ms: 0,
        message: "large tree".into(),
        changes: (0..1000)
            .map(|i| {
                (
                    path(&format!("dir{i}/f")),
                    FileChange::put(entry(&repo, b"same")),
                )
            })
            .collect(),
        metadata: BTreeMap::new(),
    };
    let root = repo.create_commit(initial).unwrap().id().unwrap();
    repo.clear_tree_cache();
    store.tree_reads.store(0, Ordering::Relaxed);
    store.tree_writes.store(0, Ordering::Relaxed);
    let edited = commit(
        &repo,
        &[root],
        &[("dir500/f", Some(b"changed"))],
        "small edit",
    );
    assert_eq!(store.tree_reads.load(Ordering::Relaxed), 2);
    assert_eq!(store.tree_writes.load(Ordering::Relaxed), 2);
    repo.clear_tree_cache();
    store.tree_reads.store(0, Ordering::Relaxed);
    let diff = repo.diff(root, edited).unwrap();
    assert_eq!(diff.len(), 1);
    assert_eq!(diff[0].path, path("dir500/f"));
    assert_eq!(store.tree_reads.load(Ordering::Relaxed), 4);

    store.tree_reads.store(0, Ordering::Relaxed);
    store.tree_writes.store(0, Ordering::Relaxed);
    let empty = commit(&repo, &[edited], &[], "metadata only");
    assert_eq!(
        repo.root_tree(empty).unwrap(),
        repo.root_tree(edited).unwrap()
    );
    assert_eq!(store.tree_reads.load(Ordering::Relaxed), 0);
    assert_eq!(store.tree_writes.load(Ordering::Relaxed), 0);
}

#[test]
fn persistent_backend_reopens_with_history_refs_and_derived_data() {
    let temp = tempfile::tempdir().unwrap();
    let id;
    {
        let repo = Repository::new(
            "test",
            Arc::new(scion_store::LocalStore::open(temp.path()).unwrap()),
        )
        .unwrap();
        id = commit(&repo, &[], &[("f", Some(b"durable\n"))], "persist");
        repo.update_ref("main", None, Some(id)).unwrap();
        repo.blame(id, &path("f")).unwrap();
    }
    let repo = Repository::new(
        "test",
        Arc::new(scion_store::LocalStore::open(temp.path()).unwrap()),
    )
    .unwrap();
    assert_eq!(repo.resolve("main").unwrap(), id);
    assert_eq!(repo.read_file(id, &path("f")).unwrap(), b"durable\n");
    assert_eq!(repo.blame(id, &path("f")).unwrap()[0].changeset, id);
}

#[test]
fn paginated_listing_is_lexical_complete_and_exclusive() {
    let repo = repo();
    let names = ["a/b", "a-", "a0", "a/b-", "a/b/x", "z"];
    // a/b cannot be both a file and directory; retain the tricky sibling keys.
    let names = names
        .into_iter()
        .filter(|p| *p != "a/b")
        .collect::<Vec<_>>();
    let files = names
        .iter()
        .map(|p| (*p, Some(b"x".as_slice())))
        .collect::<Vec<_>>();
    let id = commit(&repo, &[], &files, "pages");
    let expected = repo.list_files(id, &RepoPath::root()).unwrap();
    let mut actual = BTreeMap::new();
    let mut cursor = None;
    loop {
        let (page, next) = repo
            .list_files_page(id, &RepoPath::root(), cursor.as_ref(), 1)
            .unwrap();
        assert_eq!(page.len(), 1);
        if let Some(old) = &cursor {
            assert!(page.first_key_value().unwrap().0 > old);
        }
        actual.extend(page);
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(actual, expected);
    let (page, next) = repo
        .list_files_page(id, &path("a"), Some(&path("a/b-")), 10)
        .unwrap();
    assert_eq!(
        page.keys().cloned().collect::<Vec<_>>(),
        vec![path("a/b/x")]
    );
    assert!(next.is_none());
    assert!(
        repo.list_files_page(id, &RepoPath::root(), None, 0)
            .is_err()
    );
}

#[test]
fn filtered_diff_never_reads_denied_file_contents() {
    let store = Arc::new(CountingStore::default());
    let repo = Repository::new("test", store.clone()).unwrap();
    let a = commit(
        &repo,
        &[],
        &[("public/a", Some(b"public")), ("secret/a", Some(b"secret"))],
        "root",
    );
    let b = commit(
        &repo,
        &[a],
        &[("secret/a", Some(b"changed secret"))],
        "secret",
    );
    store.blob_reads.store(0, Ordering::Relaxed);
    assert!(
        repo.diff_filtered(a, b, |p| path("public").contains(p))
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.blob_reads.load(Ordering::Relaxed), 0);
}

#[test]
fn directory_history_skips_unrelated_changes_and_retains_deleted_paths() {
    let repo = repo();
    let a = commit(&repo, &[], &[("dir/a", Some(b"a"))], "a");
    let b = commit(&repo, &[a], &[("elsewhere", Some(b"b"))], "b");
    let c = commit(&repo, &[b], &[("dir/a", None)], "c");
    assert_eq!(repo.history(c, &path("dir"), 10).unwrap(), vec![c, a]);
    assert_eq!(
        repo.history(c, &RepoPath::root(), 10).unwrap(),
        vec![c, b, a]
    );
}

#[test]
fn concurrent_derivation_coalesces_and_reentrant_indexes_fail_without_deadlock() {
    struct Count(AtomicUsize);
    impl DerivedData for Count {
        fn name(&self) -> &str {
            "coalesced"
        }
        fn version(&self) -> u32 {
            1
        }
        fn derive(&self, _: &Repository, _: &Changeset, _: &[Vec<u8>]) -> Result<Vec<u8>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(vec![1])
        }
    }
    let repo = repo();
    let mut tip = commit(&repo, &[], &[], "root");
    for i in 0..128 {
        tip = commit(&repo, &[tip], &[], &format!("{i}"));
    }
    let index = Arc::new(Count(AtomicUsize::new(0)));
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let handles = (0..8)
        .map(|_| {
            let repo = repo.clone();
            let index = index.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                repo.derive(tip, index.as_ref()).unwrap()
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(index.0.load(Ordering::Relaxed), 129);
    struct Reenter;
    impl DerivedData for Reenter {
        fn name(&self) -> &str {
            "reenter"
        }
        fn version(&self) -> u32 {
            1
        }
        fn derive(&self, repo: &Repository, c: &Changeset, _: &[Vec<u8>]) -> Result<Vec<u8>> {
            repo.derive(c.id()?, self)?;
            Ok(vec![])
        }
    }
    let root = repo.nth_first_parent(tip, 128).unwrap().unwrap();
    assert!(matches!(
        repo.derive(root, &Reenter),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn merge_combines_executable_mode_with_text_but_rejects_type_change_edit() {
    let repo = repo();
    let root = commit(&repo, &[], &[("f", Some(b"target"))], "root");
    let edited = commit(&repo, &[root], &[("f", Some(b"new target"))], "edit");
    let change_mode = |kind: FileKind| {
        let mut file = repo.file(root, &path("f")).unwrap().unwrap();
        file.kind = kind;
        repo.create_commit(Changeset {
            parents: vec![root],
            author: "a".into(),
            committer: None,
            timestamp_ms: 0,
            message: format!("mode{kind:?}"),
            changes: BTreeMap::from([(path("f"), FileChange::put(file))]),
            metadata: BTreeMap::new(),
        })
        .unwrap()
        .id()
        .unwrap()
    };
    let executable = change_mode(FileKind::Executable);
    let merged = repo
        .merge(executable, edited, "a".into(), "merge".into(), 0)
        .unwrap()
        .changeset
        .unwrap();
    assert_eq!(
        repo.file(merged, &path("f")).unwrap().unwrap().kind,
        FileKind::Executable
    );
    assert_eq!(repo.read_file(merged, &path("f")).unwrap(), b"new target");
    let symlink = change_mode(FileKind::Symlink);
    assert_eq!(
        repo.merge(symlink, edited, "a".into(), "merge".into(), 0)
            .unwrap()
            .conflicts
            .len(),
        1
    );
}
