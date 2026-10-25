use parking_lot::Mutex;
use scion_types::{ObjectId, Tree, TreeEntry};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

/// Accounting for the repository's bounded, immutable decoded-tree cache.
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
    entries: HashMap<ObjectId, Entry>,
    lru: BTreeSet<(u64, ObjectId)>,
    bytes: usize,
    clock: u64,
    hits: u64,
    misses: u64,
}
struct Entry {
    value: Value,
    bytes: usize,
    stamp: u64,
}
enum Value {
    Tree(Arc<Tree>),
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
    pub(crate) fn entry(&self, id: ObjectId, name: &str) -> Option<Option<TreeEntry>> {
        let mut state = self.state.lock();
        let stamp = Self::tick(&mut state);
        let found = state.entries.get_mut(&id).and_then(|entry| {
            let value = match &entry.value {
                Value::Tree(tree) => Some(tree.entries.get(name).cloned()),
                Value::Lookups(entries) => entries.get(name).cloned(),
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
        self.state.lock().entries.get(&id).is_some_and(
            |entry| matches!(&entry.value, Value::Lookups(entries) if !entries.is_empty()),
        )
    }
    pub(crate) fn insert_entry(&self, id: ObjectId, name: &str, value: Option<TreeEntry>) {
        let mut state = self.state.lock();
        if state
            .entries
            .get(&id)
            .is_some_and(|entry| match &entry.value {
                Value::Tree(_) => true,
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
    fn insert_value(&self, state: &mut State, id: ObjectId, value: Value, bytes: usize) {
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
}
