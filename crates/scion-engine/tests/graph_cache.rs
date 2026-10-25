use scion_engine::{CommitRefUpdate, Repository};
use scion_graph::{GraphAccess, GraphNode};
use scion_store::{ImmutableRecord, MemoryStore, Storage};
use scion_types::{Changeset, Error, LogEntry, ObjectId, ObjectKind, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

#[derive(Default)]
struct ObservedStore {
    inner: MemoryStore,
    graph_reads: AtomicUsize,
    member_reads: AtomicUsize,
    fail_graph_reads: AtomicBool,
    fail_publication: AtomicBool,
}

impl Storage for ObservedStore {
    fn put_object(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectId> {
        self.inner.put_object(kind, bytes)
    }
    fn get_object(&self, kind: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        self.inner.get_object(kind, id)
    }
    fn get_encoded(&self, kind: ObjectKind, id: ObjectId) -> Result<Vec<u8>> {
        self.inner.get_encoded(kind, id)
    }
    fn put_encoded(&self, kind: ObjectKind, id: ObjectId, bytes: &[u8]) -> Result<()> {
        self.inner.put_encoded(kind, id, bytes)
    }
    fn get_record(&self, namespace: &str, key: &str) -> Result<Option<Vec<u8>>> {
        if namespace.ends_with("/changesets") {
            self.member_reads.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.get_record(namespace, key)
    }
    fn get_record_pair(
        &self,
        first: (&str, &str),
        second: (&str, &str),
    ) -> Result<[Option<Vec<u8>>; 2]> {
        if second.0.ends_with("/graph") {
            self.graph_reads.fetch_add(1, Ordering::Relaxed);
            if self.fail_graph_reads.load(Ordering::Relaxed) {
                return Err(Error::Storage("injected graph read failure".into()));
            }
        }
        self.inner.get_record_pair(first, second)
    }
    fn put_record(&self, namespace: &str, key: &str, value: &[u8]) -> Result<()> {
        self.inner.put_record(namespace, key, value)
    }
    fn compare_and_swap(
        &self,
        namespace: &str,
        key: &str,
        expected: Option<&[u8]>,
        value: Option<&[u8]>,
    ) -> Result<bool> {
        self.inner.compare_and_swap(namespace, key, expected, value)
    }
    fn append_log(&self, repository: &str, id: ObjectId) -> Result<u64> {
        self.inner.append_log(repository, id)
    }
    fn publish_commit(
        &self,
        repository: &str,
        id: ObjectId,
        records: &[ImmutableRecord],
    ) -> Result<u64> {
        if self.fail_publication.load(Ordering::Relaxed) {
            return Err(Error::Storage("injected publication failure".into()));
        }
        self.inner.publish_commit(repository, id, records)
    }
    fn read_log(&self, repository: &str, after: u64, limit: usize) -> Result<Vec<LogEntry>> {
        self.inner.read_log(repository, after, limit)
    }
    fn scan_records(
        &self,
        namespace: &str,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        self.inner.scan_records(namespace, prefix, limit)
    }
}

fn request(parents: &[ObjectId], message: &str) -> Changeset {
    Changeset {
        parents: parents.to_vec(),
        author: "Graph cache test".into(),
        committer: None,
        timestamp_ms: 0,
        message: message.into(),
        changes: BTreeMap::new(),
        metadata: BTreeMap::new(),
    }
}

fn commit(repository: &Repository, parents: &[ObjectId], message: &str) -> ObjectId {
    repository
        .create_commit(request(parents, message))
        .unwrap()
        .id()
        .unwrap()
}

fn fixture() -> (Arc<ObservedStore>, Repository) {
    let store = Arc::new(ObservedStore::default());
    let repository = Repository::new("repo", store.clone()).unwrap();
    (store, repository)
}

fn replace_record(store: &ObservedStore, namespace: &str, id: ObjectId, value: Option<&[u8]>) {
    let key = id.to_string();
    let old = store.inner.get_record(namespace, &key).unwrap();
    assert!(
        store
            .inner
            .compare_and_swap(namespace, &key, old.as_deref(), value)
            .unwrap()
    );
}

#[test]
fn repeated_queries_and_clones_reuse_guarded_reads_without_reusing_owned_results() {
    let (store, repository) = fixture();
    let root = commit(&repository, &[], "root");
    let side = commit(&repository, &[root], "side");
    let mut tip = root;
    for number in 0..255 {
        tip = commit(&repository, &[tip], &format!("linear {number}"));
    }
    let left = commit(&repository, &[tip], "left");
    let right = commit(&repository, &[tip], "right");
    repository.clear_tree_cache();
    store.graph_reads.store(0, Ordering::Relaxed);
    let query = |repository: &Repository| {
        assert!(repository.is_ancestor(root, tip).unwrap());
        assert!(!repository.is_ancestor(side, tip).unwrap());
        assert_eq!(repository.merge_bases(left, right).unwrap(), vec![tip]);
        assert_eq!(repository.nth_first_parent(tip, 255).unwrap(), Some(root));
    };
    query(&repository);
    let cold_reads = store.graph_reads.load(Ordering::Relaxed);
    assert!(
        cold_reads > 2,
        "cold traversal must consult stored graph nodes"
    );
    for _ in 0..16 {
        query(&repository.clone());
    }
    assert_eq!(store.graph_reads.load(Ordering::Relaxed), cold_reads);

    let expected = repository.node(tip).unwrap();
    let mut owned = repository.node(tip).unwrap();
    owned.parents.clear();
    owned.jumps.clear();
    owned.linear_jumps.clear();
    owned.generation = u64::MAX;
    assert_eq!(repository.clone().node(tip).unwrap(), expected);
    assert_eq!(store.graph_reads.load(Ordering::Relaxed), cold_reads);

    repository.clone().clear_tree_cache();
    assert_eq!(repository.tree_cache_stats().entries, 0);
    query(&repository);
    assert_eq!(store.graph_reads.load(Ordering::Relaxed), 2 * cold_reads);
}

#[test]
fn membership_proofs_do_not_cross_repository_or_storage_scope_or_cache_absence() {
    let (store, repository) = fixture();
    let revision = commit(&repository, &[], "root");
    let expected = repository.node(revision).unwrap();
    let foreign = Repository::new("foreign", store.clone()).unwrap();
    let key = revision.to_string();
    let bytes = store
        .inner
        .get_record("repo/repo/graph", &key)
        .unwrap()
        .unwrap();
    store
        .inner
        .put_record("repo/foreign/graph", &key, &bytes)
        .unwrap();
    store.graph_reads.store(0, Ordering::Relaxed);
    for _ in 0..2 {
        assert!(matches!(foreign.node(revision), Err(Error::NotFound(_))));
    }
    assert_eq!(store.graph_reads.load(Ordering::Relaxed), 2);
    assert_eq!(foreign.tree_cache_stats().entries, 0);
    store.inner.append_log("foreign", revision).unwrap();
    assert_eq!(foreign.node(revision).unwrap(), expected);
    assert_eq!(store.graph_reads.load(Ordering::Relaxed), 3);

    let independent = Repository::new("repo", Arc::new(MemoryStore::new())).unwrap();
    assert!(matches!(
        independent.node(revision),
        Err(Error::NotFound(_))
    ));
    assert_eq!(repository.node(revision).unwrap(), expected);
}

#[test]
fn unknown_equal_or_distinct_query_endpoints_always_fail() {
    let (_, repository) = fixture();
    let known = commit(&repository, &[], "known");
    let unknown = ObjectId::hash(ObjectKind::Changeset, b"unknown endpoint");
    for (left, right) in [(unknown, unknown), (known, unknown), (unknown, known)] {
        assert!(matches!(
            repository.is_ancestor(left, right),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            repository.merge_bases(left, right),
            Err(Error::NotFound(_))
        ));
    }
    for distance in [0, 1, u64::MAX] {
        assert!(matches!(
            repository.nth_first_parent(unknown, distance),
            Err(Error::NotFound(_))
        ));
    }
}

#[test]
fn missing_corrupt_or_failed_reads_are_not_retained_and_legacy_records_remain_valid() {
    let (store, repository) = fixture();
    let root = commit(&repository, &[], "root");
    let tip = commit(&repository, &[root], "tip");
    let key = tip.to_string();
    let original = store
        .inner
        .get_record("repo/repo/graph", &key)
        .unwrap()
        .unwrap();
    repository.clear_tree_cache();
    store.graph_reads.store(0, Ordering::Relaxed);
    replace_record(&store, "repo/repo/graph", tip, None);
    for _ in 0..2 {
        assert!(matches!(repository.node(tip), Err(Error::Corrupt(_))));
    }
    assert_eq!(repository.tree_cache_stats().entries, 0);
    replace_record(&store, "repo/repo/graph", tip, Some(b"{malformed"));
    for _ in 0..2 {
        assert!(repository.node(tip).is_err());
    }
    assert_eq!(repository.tree_cache_stats().entries, 0);
    replace_record(&store, "repo/repo/graph", tip, Some(&original));
    store.fail_graph_reads.store(true, Ordering::Relaxed);
    for _ in 0..2 {
        assert!(matches!(repository.node(tip), Err(Error::Storage(_))));
    }
    assert_eq!(repository.tree_cache_stats().entries, 0);
    store.fail_graph_reads.store(false, Ordering::Relaxed);
    let expected: GraphNode = serde_json::from_slice(&original).unwrap();
    assert_eq!(repository.node(tip).unwrap(), expected);
    assert_eq!(store.graph_reads.load(Ordering::Relaxed), 7);
    assert_eq!(repository.node(tip).unwrap(), expected);
    assert_eq!(store.graph_reads.load(Ordering::Relaxed), 7);

    // Old graph records omit the merge-safe jump table. Cache decoding retains
    // the existing default/fallback semantics rather than changing their bytes.
    let mut legacy: serde_json::Value = serde_json::from_slice(&original).unwrap();
    legacy.as_object_mut().unwrap().remove("linear_jumps");
    replace_record(
        &store,
        "repo/repo/graph",
        tip,
        Some(&serde_json::to_vec(&legacy).unwrap()),
    );
    repository.clear_tree_cache();
    assert!(repository.node(tip).unwrap().linear_jumps.is_empty());
    assert!(repository.is_ancestor(root, tip).unwrap());
    assert_eq!(repository.nth_first_parent(tip, 1).unwrap(), Some(root));
}

#[test]
fn clear_and_disabled_caches_repeat_storage_validation_and_direct_parent_checks_remain_live() {
    let (store, repository) = fixture();
    let revision = commit(&repository, &[], "root");
    let expected = repository.node(revision).unwrap();
    let key = revision.to_string();
    let original = store
        .inner
        .get_record("repo/repo/graph", &key)
        .unwrap()
        .unwrap();
    replace_record(&store, "repo/repo/graph", revision, Some(b"corrupt"));
    assert_eq!(repository.node(revision).unwrap(), expected);
    assert_eq!(
        store.get_record("repo/repo/graph", &key).unwrap().unwrap(),
        b"corrupt"
    );
    repository.clear_tree_cache();
    assert!(repository.node(revision).is_err());
    replace_record(&store, "repo/repo/graph", revision, Some(&original));
    assert_eq!(repository.node(revision).unwrap(), expected);

    let disabled = Repository::new("repo", store.clone())
        .unwrap()
        .with_tree_cache_capacity(0);
    store.graph_reads.store(0, Ordering::Relaxed);
    for _ in 0..2 {
        assert_eq!(disabled.node(revision).unwrap(), expected);
    }
    assert_eq!(store.graph_reads.load(Ordering::Relaxed), 2);
    assert_eq!(disabled.tree_cache_stats().entries, 0);
    replace_record(&store, "repo/repo/graph", revision, Some(b"corrupt"));
    assert!(disabled.node(revision).is_err());
    assert_eq!(store.graph_reads.load(Ordering::Relaxed), 3);
    replace_record(&store, "repo/repo/graph", revision, Some(&original));

    // Low-level deletion violates immutable publication, but must still be
    // observed by the existing direct commit-parent validation and on a miss.
    replace_record(&store, "repo/repo/changesets", revision, None);
    assert_eq!(repository.node(revision).unwrap(), expected);
    store.member_reads.store(0, Ordering::Relaxed);
    assert!(matches!(
        repository.create_commit(request(&[revision], "child")),
        Err(Error::NotFound(_))
    ));
    assert_eq!(store.member_reads.load(Ordering::Relaxed), 1);
    assert!(matches!(
        repository.get_changeset(revision),
        Err(Error::NotFound(_))
    ));
    repository.clear_tree_cache();
    assert!(matches!(repository.node(revision), Err(Error::NotFound(_))));
    store.inner.append_log("repo", revision).unwrap();
    assert_eq!(repository.node(revision).unwrap(), expected);
}

#[test]
fn graph_values_are_seeded_only_after_acknowledged_publication_including_stale_cas() {
    let (store, repository) = fixture();
    let change = request(&[], "publication");
    let revision = change.id().unwrap();
    store.fail_publication.store(true, Ordering::Relaxed);
    assert!(matches!(
        repository.create_commit(change.clone()),
        Err(Error::Storage(_))
    ));
    assert!(matches!(repository.node(revision), Err(Error::NotFound(_))));
    assert_eq!(repository.tree_cache_stats().entries, 0);
    assert!(store.inner.read_log("repo", 0, 10).unwrap().is_empty());

    store.fail_publication.store(false, Ordering::Relaxed);
    let publication = repository
        .create_commit_with_blobs(
            change,
            Vec::new(),
            Some(CommitRefUpdate {
                name: "main".into(),
                expected: Some(ObjectId::hash(ObjectKind::Changeset, b"stale ref")),
            }),
        )
        .unwrap();
    assert_eq!(publication.reference_updated, Some(false));
    store.graph_reads.store(0, Ordering::Relaxed);
    assert!(repository.node(revision).unwrap().parents.is_empty());
    assert_eq!(store.graph_reads.load(Ordering::Relaxed), 0);
    assert_eq!(store.inner.read_log("repo", 0, 10).unwrap().len(), 1);
    assert_eq!(repository.resolve_ref("main").unwrap(), None);
}

#[test]
fn cached_and_uncached_graphs_match_an_independent_multi_parent_oracle() {
    let (store, repository) = fixture();
    let definitions: &[&[usize]] = &[
        &[],
        &[],
        &[0],
        &[0],
        &[2, 3],
        &[3, 2],
        &[4, 1],
        &[4],
        &[5],
        &[6, 8],
        &[7, 9],
    ];
    let mut ids = Vec::new();
    let mut ancestry = Vec::<BTreeSet<ObjectId>>::new();
    for (number, parents) in definitions.iter().enumerate() {
        let parents = parents
            .iter()
            .map(|parent| ids[*parent])
            .collect::<Vec<_>>();
        let id = commit(&repository, &parents, &format!("node {number}"));
        let mut ancestors = BTreeSet::from([id]);
        for parent in definitions[number] {
            ancestors.extend(&ancestry[*parent]);
        }
        ids.push(id);
        ancestry.push(ancestors);
    }
    repository.clear_tree_cache();
    let uncached = Repository::new("repo", store)
        .unwrap()
        .with_tree_cache_capacity(0);
    let clone = repository.clone();
    for graph in [&repository, &clone, &uncached] {
        for (left_index, left) in ids.iter().enumerate() {
            for (right_index, right) in ids.iter().enumerate() {
                assert_eq!(
                    graph.is_ancestor(*left, *right).unwrap(),
                    ancestry[right_index].contains(left)
                );
                let common = ancestry[left_index]
                    .intersection(&ancestry[right_index])
                    .copied()
                    .collect::<BTreeSet<_>>();
                let expected = common
                    .iter()
                    .filter(|candidate| {
                        !common.iter().any(|other| {
                            other != *candidate
                                && ancestry[ids.iter().position(|id| id == other).unwrap()]
                                    .contains(candidate)
                        })
                    })
                    .copied()
                    .collect::<Vec<_>>();
                assert_eq!(graph.merge_bases(*left, *right).unwrap(), expected);
            }
        }
    }
    assert_eq!(uncached.tree_cache_stats().entries, 0);
}
