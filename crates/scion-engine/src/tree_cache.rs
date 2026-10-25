use parking_lot::Mutex;
use scion_types::{ObjectId, Tree};
use std::{
    collections::{BTreeSet, HashMap},
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
    tree: Arc<Tree>,
    bytes: usize,
    stamp: u64,
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
        if let Some(entry) = state.entries.get_mut(&id) {
            let old = entry.stamp;
            entry.stamp = stamp;
            let tree = entry.tree.clone();
            state.lru.remove(&(old, id));
            state.lru.insert((stamp, id));
            state.hits = state.hits.saturating_add(1);
            Some(tree)
        } else {
            state.misses = state.misses.saturating_add(1);
            None
        }
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
        if state.entries.contains_key(&id) {
            return;
        }
        while state.bytes.saturating_add(bytes) > self.capacity || state.entries.len() >= 4096 {
            let Some((_, oldest)) = state.lru.pop_first() else {
                break;
            };
            if let Some(entry) = state.entries.remove(&oldest) {
                state.bytes -= entry.bytes;
            }
        }
        let stamp = Self::tick(&mut state);
        state.bytes += bytes;
        state.lru.insert((stamp, id));
        state.entries.insert(id, Entry { tree, bytes, stamp });
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
}
