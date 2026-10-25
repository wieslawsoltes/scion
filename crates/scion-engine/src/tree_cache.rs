use parking_lot::Mutex;
use scion_types::{ObjectId, Tree, TreeEntry};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

/// Accounting for retained trees, entries, revision roots and verified small blobs.
#[derive(Clone, Copy, Debug, Default)]
pub struct TreeCacheStats {
    pub entries: usize,
    pub estimated_bytes: usize,
    pub hits: u64,
    pub misses: u64,
}

pub(crate) struct TreeCache {
    capacity: usize,
    state: Mutex<State>,
}
#[derive(Default)]
struct State {
    entries: HashMap<Key, Entry>,
    lru: BTreeSet<(u64, Key)>,
    bytes: usize,
    clock: u64,
    hits: u64,
    misses: u64,
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Key {
    Tree(ObjectId),
    Root(ObjectId),
    Blob(ObjectId),
}
struct Entry {
    value: Value,
    bytes: usize,
    stamp: u64,
}
enum Value {
    Tree(Arc<Tree>),
    Root(ObjectId),
    Blob(Arc<Vec<u8>>),
    /// Cold point lookups retain only selected entries. Missing names are cached
    /// as well; the immutable object's full payload was validated before insert.
    Lookups(BTreeMap<String, Option<TreeEntry>>),
}
impl Default for TreeCache {
    fn default() -> Self {
        Self::new(8 * 1024 * 1024)
    }
}
impl TreeCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(State::default()),
        }
    }
    fn tick(state: &mut State) -> u64 {
        if state.clock == u64::MAX {
            state.entries.clear();
            state.lru.clear();
            state.bytes = 0;
            state.clock = 0;
        }
        state.clock += 1;
        state.clock
    }
    pub(crate) fn get(&self, id: ObjectId) -> Option<Arc<Tree>> {
        let id = Key::Tree(id);
        let mut state = self.state.lock();
        let stamp = Self::tick(&mut state);
        if let Some(entry) = state.entries.get_mut(&id)
            && let Value::Tree(tree) = &entry.value
        {
            let old = entry.stamp;
            entry.stamp = stamp;
            let tree = tree.clone();
            state.lru.remove(&(old, id));
            state.lru.insert((stamp, id));
            state.hits = state.hits.saturating_add(1);
            Some(tree)
        } else {
            state.misses = state.misses.saturating_add(1);
            None
        }
    }
    /// Transfer the cache's ownership to an editor. Other readers keep their
    /// Arc snapshots; the editor must unwrap-or-clone before making any change.
    /// Partial directory lookups cannot provide an owned tree and remain cached.
    pub(crate) fn take_tree(&self, object: ObjectId) -> Option<Arc<Tree>> {
        let id = Key::Tree(object);
        let mut state = self.state.lock();
        if !state
            .entries
            .get(&id)
            .is_some_and(|entry| matches!(entry.value, Value::Tree(_)))
        {
            state.misses = state.misses.saturating_add(1);
            return None;
        }
        let entry = state.entries.remove(&id).expect("checked full tree");
        state.lru.remove(&(entry.stamp, id));
        state.bytes -= entry.bytes;
        state.hits = state.hits.saturating_add(1);
        let Value::Tree(tree) = entry.value else {
            unreachable!("checked full tree")
        };
        Some(tree)
    }
    pub(crate) fn entry(&self, id: ObjectId, name: &str) -> Option<Option<TreeEntry>> {
        let id = Key::Tree(id);
        let mut state = self.state.lock();
        let stamp = Self::tick(&mut state);
        let found = state.entries.get_mut(&id).and_then(|entry| {
            let value = match &entry.value {
                Value::Tree(tree) => Some(tree.entries.get(name).cloned()),
                Value::Lookups(entries) => entries.get(name).cloned(),
                Value::Root(_) | Value::Blob(_) => None,
            }?;
            let old = std::mem::replace(&mut entry.stamp, stamp);
            Some((old, value))
        });
        if let Some((old, entry)) = found {
            state.lru.remove(&(old, id));
            state.lru.insert((stamp, id));
            state.hits = state.hits.saturating_add(1);
            Some(entry)
        } else {
            state.misses = state.misses.saturating_add(1);
            None
        }
    }
    pub(crate) fn should_promote(&self, id: ObjectId) -> bool {
        self.state.lock().entries.get(&Key::Tree(id)).is_some_and(
            |entry| matches!(&entry.value, Value::Lookups(entries) if !entries.is_empty()),
        )
    }
    pub(crate) fn insert_entry(&self, id: ObjectId, name: &str, value: Option<TreeEntry>) {
        let id = Key::Tree(id);
        let mut state = self.state.lock();
        if state
            .entries
            .get(&id)
            .is_some_and(|entry| match &entry.value {
                Value::Tree(_) | Value::Root(_) | Value::Blob(_) => true,
                Value::Lookups(entries) => entries.contains_key(name),
            })
        {
            return;
        }
        let (mut entries, old_bytes) = match state.entries.remove(&id) {
            Some(Entry {
                value: Value::Lookups(entries),
                bytes,
                stamp,
            }) => {
                state.lru.remove(&(stamp, id));
                state.bytes -= bytes;
                (entries, bytes)
            }
            None => (BTreeMap::new(), 512),
            Some(_) => unreachable!("full trees were handled above"),
        };
        let name = name.to_owned();
        let bytes = old_bytes
            .saturating_add(name.capacity())
            .saturating_add(192);
        if bytes > self.capacity {
            return;
        }
        entries.insert(name, value);
        self.insert_value(&mut state, id, Value::Lookups(entries), bytes);
    }
    pub(crate) fn insert(&self, id: ObjectId, tree: Arc<Tree>) {
        let id = Key::Tree(id);
        // Conservative per-entry allowance includes B-tree nodes, key capacity,
        // enum values, Arc and the cache's two indexing structures.
        let bytes = tree.entries.keys().fold(512usize, |size, key| {
            size.saturating_add(key.capacity()).saturating_add(192)
        });
        if bytes > self.capacity {
            return;
        }
        let mut state = self.state.lock();
        if state
            .entries
            .get(&id)
            .is_some_and(|entry| matches!(entry.value, Value::Tree(_)))
        {
            return;
        }
        if let Some(entry) = state.entries.remove(&id) {
            state.lru.remove(&(entry.stamp, id));
            state.bytes -= entry.bytes;
        }
        self.insert_value(&mut state, id, Value::Tree(tree), bytes);
    }
    pub(crate) fn root(&self, revision: ObjectId) -> Option<ObjectId> {
        let id = Key::Root(revision);
        let mut state = self.state.lock();
        let stamp = Self::tick(&mut state);
        if let Some(entry) = state.entries.get_mut(&id)
            && let Value::Root(root) = entry.value
        {
            let old = std::mem::replace(&mut entry.stamp, stamp);
            state.lru.remove(&(old, id));
            state.lru.insert((stamp, id));
            state.hits = state.hits.saturating_add(1);
            Some(root)
        } else {
            state.misses = state.misses.saturating_add(1);
            None
        }
    }
    /// Call only after guarded membership lookup or acknowledged publication.
    /// Membership and tree indexes are immutable within a repository namespace.
    pub(crate) fn insert_root(&self, revision: ObjectId, root: ObjectId) {
        const BYTES: usize = 256;
        if BYTES > self.capacity {
            return;
        }
        let id = Key::Root(revision);
        let mut state = self.state.lock();
        if state.entries.contains_key(&id) {
            return;
        }
        self.insert_value(&mut state, id, Value::Root(root), BYTES);
    }
    pub(crate) fn blob(&self, object: ObjectId) -> Option<Arc<Vec<u8>>> {
        let id = Key::Blob(object);
        let mut state = self.state.lock();
        let stamp = Self::tick(&mut state);
        if let Some(entry) = state.entries.get_mut(&id)
            && let Value::Blob(bytes) = &entry.value
        {
            let old = std::mem::replace(&mut entry.stamp, stamp);
            let bytes = bytes.clone();
            state.lru.remove(&(old, id));
            state.lru.insert((stamp, id));
            state.hits = state.hits.saturating_add(1);
            Some(bytes)
        } else {
            state.misses = state.misses.saturating_add(1);
            None
        }
    }
    /// Retain only successfully verified blob reads. The actual allocation,
    /// including spare Vec capacity, participates in the shared cache budget.
    pub(crate) fn insert_blob(&self, object: ObjectId, value: Arc<Vec<u8>>) {
        const MAX_BLOB_BYTES: usize = 64 * 1024;
        let bytes = value.capacity().saturating_add(256);
        if value.len() > MAX_BLOB_BYTES || bytes > self.capacity {
            return;
        }
        let id = Key::Blob(object);
        let mut state = self.state.lock();
        if state.entries.contains_key(&id) {
            return;
        }
        self.insert_value(&mut state, id, Value::Blob(value), bytes);
    }
    fn insert_value(&self, state: &mut State, id: Key, value: Value, bytes: usize) {
        while state.bytes.saturating_add(bytes) > self.capacity || state.entries.len() >= 4096 {
            let Some((_, oldest)) = state.lru.pop_first() else {
                break;
            };
            if let Some(entry) = state.entries.remove(&oldest) {
                state.bytes -= entry.bytes;
            }
        }
        let stamp = Self::tick(state);
        state.bytes += bytes;
        state.lru.insert((stamp, id));
        state.entries.insert(
            id,
            Entry {
                value,
                bytes,
                stamp,
            },
        );
    }
    pub(crate) fn clear(&self) {
        let mut state = self.state.lock();
        state.entries.clear();
        state.lru.clear();
        state.bytes = 0;
    }
    pub(crate) fn stats(&self) -> TreeCacheStats {
        let state = self.state.lock();
        TreeCacheStats {
            entries: state.entries.len(),
            estimated_bytes: state.bytes,
            hits: state.hits,
            misses: state.misses,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_types::ObjectKind;
    #[test]
    fn capacity_and_lru_do_not_invalidate_live_readers() {
        let cache = TreeCache::new(1024);
        let a = ObjectId::hash(ObjectKind::Tree, b"a");
        let b = ObjectId::hash(ObjectKind::Tree, b"b");
        let c = ObjectId::hash(ObjectKind::Tree, b"c");
        cache.insert(a, Arc::new(Tree::default()));
        cache.insert(b, Arc::new(Tree::default()));
        let held = cache.get(a).unwrap();
        cache.insert(c, Arc::new(Tree::default()));
        assert!(cache.get(b).is_none());
        assert_eq!(cache.stats().estimated_bytes, 1024);
        assert_eq!(cache.stats().entries, 2);
        cache.clear();
        assert!(held.entries.is_empty());
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn taking_full_trees_transfers_ownership_and_preserves_other_cache_entries() {
        let cache = TreeCache::new(4096);
        let id = ObjectId::hash(ObjectKind::Tree, b"editable");
        let partial = ObjectId::hash(ObjectKind::Tree, b"partial");
        let tree = Tree {
            entries: BTreeMap::from([("a".into(), TreeEntry::Directory { id })]),
        };
        let original = Arc::new(tree.clone());
        cache.insert(id, original.clone());
        cache.insert_root(id, partial);
        cache.insert_blob(id, Arc::new(vec![1]));
        cache.insert_entry(partial, "known", None);
        let before = cache.stats();
        let taken = cache.take_tree(id).unwrap();
        assert!(Arc::ptr_eq(&taken, &original));
        assert_eq!(cache.stats().entries, before.entries - 1);
        assert_eq!(cache.stats().estimated_bytes, before.estimated_bytes - 705);
        assert!(cache.get(id).is_none());
        assert_eq!(cache.root(id), Some(partial));
        assert_eq!(cache.blob(id).unwrap().as_slice(), &[1]);
        let before_partial = cache.stats().estimated_bytes;
        assert!(cache.take_tree(partial).is_none());
        assert_eq!(cache.entry(partial, "known"), Some(None));
        assert_eq!(cache.stats().estimated_bytes, before_partial);
        let mut edited = Arc::unwrap_or_clone(taken);
        edited.entries.clear();
        assert_eq!(*original, tree);
        cache.clear();
        assert_eq!(cache.stats().estimated_bytes, 0);
        assert_eq!(cache.stats().entries, 0);

        let unique = Arc::new(tree);
        let weak = Arc::downgrade(&unique);
        cache.insert(id, unique);
        let taken = cache.take_tree(id).unwrap();
        assert_eq!(Arc::strong_count(&taken), 1);
        let owned = Arc::unwrap_or_clone(taken);
        assert!(weak.upgrade().is_none());
        assert_eq!(owned.entries.len(), 1);
        assert_eq!(cache.stats().estimated_bytes, 0);
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn selected_entries_share_the_tree_budget_and_promote_to_complete_trees() {
        let cache = TreeCache::new(2048);
        let a = ObjectId::hash(ObjectKind::Tree, b"a");
        let b = ObjectId::hash(ObjectKind::Tree, b"b");
        cache.insert_entry(a, "missing", None);
        assert_eq!(cache.entry(a, "missing"), Some(None));
        assert!(cache.entry(a, "other").is_none());
        assert!(cache.get(a).is_none());
        cache.insert_entry(a, "other", None);
        assert_eq!(cache.stats().entries, 1);
        cache.insert_entry(b, "missing", None);
        assert!(cache.stats().estimated_bytes <= 2048);
        cache.insert(a, Arc::new(Tree::default()));
        assert!(cache.get(a).is_some());
        assert_eq!(cache.entry(a, "anything"), Some(None));
        assert!(cache.stats().estimated_bytes <= 2048);
        cache.clear();
        assert_eq!(cache.stats().estimated_bytes, 0);
        assert!(cache.entry(a, "missing").is_none());

        let disabled = TreeCache::new(0);
        disabled.insert_entry(a, "name", None);
        assert_eq!(disabled.stats().entries, 0);
    }

    #[test]
    fn validated_roots_share_the_budget_and_cannot_alias_tree_keys() {
        let cache = TreeCache::new(1024);
        let id = ObjectId::hash(ObjectKind::Changeset, b"revision");
        let root = ObjectId::hash(ObjectKind::Tree, b"root");
        cache.insert(id, Arc::new(Tree::default()));
        cache.insert_root(id, root);
        assert!(cache.get(id).is_some());
        assert_eq!(cache.root(id), Some(root));
        assert_eq!(cache.stats().estimated_bytes, 768);
        for index in 0u8..32 {
            cache.insert_root(ObjectId::hash(ObjectKind::Changeset, &[index]), root);
            assert!(cache.stats().estimated_bytes <= 1024);
        }
        assert_eq!(cache.stats().entries, 4);
        cache.clear();
        assert_eq!(cache.root(id), None);
        assert_eq!(cache.stats().estimated_bytes, 0);
        let disabled = TreeCache::new(0);
        disabled.insert_root(id, root);
        assert_eq!(disabled.root(id), None);
    }

    #[test]
    fn blobs_share_tree_and_root_budget_and_survive_eviction_for_active_readers() {
        let cache = TreeCache::new(1024);
        let id = ObjectId::hash(ObjectKind::Blob, b"content");
        let root = ObjectId::hash(ObjectKind::Tree, b"root");
        let bytes = Arc::new(vec![7; 256]);
        cache.insert(id, Arc::new(Tree::default()));
        cache.insert_blob(id, bytes.clone());
        assert!(cache.get(id).is_some());
        let held = cache.blob(id).unwrap();
        assert_eq!(cache.stats().estimated_bytes, 1024);
        // The root competes with both object kinds and evicts the older tree.
        cache.insert_root(id, root);
        assert!(cache.get(id).is_none());
        assert_eq!(cache.blob(id).as_deref(), Some(bytes.as_ref()));
        assert_eq!(cache.root(id), Some(root));
        let other = ObjectId::hash(ObjectKind::Blob, b"other");
        cache.insert_blob(other, Arc::new(vec![8; 256]));
        assert!(cache.blob(id).is_none());
        assert!(cache.stats().estimated_bytes <= 1024);
        cache.clear();
        assert_eq!(held.as_slice(), &[7; 256]);
        assert_eq!(cache.stats().entries, 0);
        assert_eq!(cache.stats().estimated_bytes, 0);
    }

    #[test]
    fn blob_retention_bounds_actual_length_allocation_and_entry_count() {
        const CAPACITY: usize = 8 * 1024 * 1024;
        let cache = TreeCache::new(CAPACITY);
        let id = ObjectId::hash(ObjectKind::Blob, b"limit");
        cache.insert_blob(id, Arc::new(vec![0; 64 * 1024 + 1]));
        assert!(cache.blob(id).is_none());
        let mut oversized_allocation = Vec::with_capacity(CAPACITY + 1);
        oversized_allocation.push(0);
        cache.insert_blob(id, Arc::new(oversized_allocation));
        assert!(cache.blob(id).is_none());
        cache.insert_blob(id, Arc::new(vec![0; 64 * 1024]));
        assert_eq!(cache.blob(id).unwrap().len(), 64 * 1024);
        assert_eq!(cache.stats().estimated_bytes, 64 * 1024 + 256);
        cache.clear();
        for number in 0u32..4097 {
            let id = ObjectId::hash(ObjectKind::Blob, &number.to_le_bytes());
            cache.insert_blob(id, Arc::new(Vec::new()));
        }
        assert_eq!(cache.stats().entries, 4096);
        assert!(cache.stats().estimated_bytes <= CAPACITY);
        let first = ObjectId::hash(ObjectKind::Blob, &0u32.to_le_bytes());
        assert!(cache.blob(first).is_none());
        let disabled = TreeCache::new(0);
        disabled.insert_blob(id, Arc::new(Vec::new()));
        assert!(disabled.blob(id).is_none());
        assert_eq!(disabled.stats().entries, 0);
    }
}
