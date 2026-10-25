use crate::Repository;
use parking_lot::Mutex;
use scion_types::*;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Weak},
};

#[derive(Default)]
pub(crate) struct Flights {
    jobs: Mutex<BTreeMap<String, Weak<Mutex<()>>>>,
}
thread_local! { static ACTIVE: RefCell<BTreeSet<String>> = const { RefCell::new(BTreeSet::new()) }; }
struct ActiveGuard(String);
impl ActiveGuard {
    fn enter(key: String) -> Result<Self> {
        if !ACTIVE.with(|active| active.borrow_mut().insert(key.clone())) {
            return Err(Error::Invalid(
                "recursive request for the same derived index".into(),
            ));
        }
        Ok(Self(key))
    }
}
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            active.borrow_mut().remove(&self.0);
        });
    }
}

/// A deterministic recursive index. Values are content-addressed opaque bytes.
/// Parent values are supplied in changeset parent order, allowing any new index
/// to reuse the same iterative evaluator without introducing a protocol change.
pub trait DerivedData: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> u32;
    fn derive(
        &self,
        repository: &Repository,
        changeset: &Changeset,
        parent_values: &[Vec<u8>],
    ) -> Result<Vec<u8>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivedPolicy {
    Eager,
    Lazy,
}

impl Repository {
    /// Materialize an arbitrary derived index in parent order without recursive
    /// call stacks or global locks. Racing writers converge through immutable CAS.
    pub fn derive(&self, revision: ObjectId, index: &dyn DerivedData) -> Result<ObjectId> {
        validate_name(index.name())?;
        let key = format!("{}/{}/{revision}", index.name(), index.version());
        let _active = ActiveGuard::enter(format!("{:p}/{key}", Arc::as_ptr(&self.flights)))?;
        let flight = {
            let mut jobs = self.flights.jobs.lock();
            if let Some(flight) = jobs.get(&key).and_then(Weak::upgrade) {
                flight
            } else {
                if jobs.len() >= 1024 {
                    jobs.retain(|_, value| value.strong_count() > 0);
                }
                if jobs.len() >= 1024 {
                    return Err(Error::Limit(
                        "too many concurrent derived computations".into(),
                    ));
                }
                let flight = Arc::new(Mutex::new(()));
                jobs.insert(key, Arc::downgrade(&flight));
                flight
            }
        };
        let _guard = flight.lock();
        self.derive_inner(revision, index)
    }

    fn derive_inner(&self, revision: ObjectId, index: &dyn DerivedData) -> Result<ObjectId> {
        let namespace = self.namespace(&format!("index/{}/{}", index.name(), index.version()));
        let mut stack = vec![(revision, false)];
        let mut done = BTreeMap::<ObjectId, ObjectId>::new();
        let mut active = BTreeSet::new();
        while let Some((id, ready)) = stack.pop() {
            if done.contains_key(&id) {
                continue;
            }
            if done.len() >= 1_000_000 {
                return Err(Error::Limit(
                    "derived traversal exceeds one million revisions".into(),
                ));
            }
            if let Some(bytes) = self.store().get_record(&namespace, &id.to_string())? {
                done.insert(id, serde_json::from_slice(&bytes)?);
                continue;
            }
            let changeset = self.get_changeset(id)?;
            if !ready {
                if !active.insert(id) {
                    return Err(Error::Corrupt("cycle in changeset ancestry".into()));
                }
                stack.push((id, true));
                for parent in changeset.parents.iter().rev() {
                    if !done.contains_key(parent) {
                        stack.push((*parent, false));
                    }
                }
                continue;
            }
            let parent_values = changeset
                .parents
                .iter()
                .map(|parent| {
                    let value_id = done
                        .get(parent)
                        .ok_or_else(|| Error::Corrupt("missing derived parent".into()))?;
                    self.store().get_object(ObjectKind::Derived, *value_id)
                })
                .collect::<Result<Vec<_>>>()?;
            let value = index.derive(self, &changeset, &parent_values)?;
            let value_id = self.store().put_object(ObjectKind::Derived, &value)?;
            self.store()
                .put_record(&namespace, &id.to_string(), &canonical(&value_id)?)?;
            done.insert(id, value_id);
            active.remove(&id);
        }
        done.get(&revision)
            .copied()
            .ok_or_else(|| Error::Corrupt("derived evaluator produced no result".into()))
    }

    /// Apply eager or lazy scheduling for a set of application-defined indexes.
    pub fn prepare_derived(
        &self,
        revision: ObjectId,
        indexes: &[(&dyn DerivedData, DerivedPolicy)],
    ) -> Result<()> {
        for (index, policy) in indexes {
            if *policy == DerivedPolicy::Eager {
                self.derive(revision, *index)?;
            }
        }
        Ok(())
    }
}

pub(crate) struct TreeIndex;
impl DerivedData for TreeIndex {
    fn name(&self) -> &str {
        "tree"
    }
    fn version(&self) -> u32 {
        1
    }
    fn derive(
        &self,
        repository: &Repository,
        changeset: &Changeset,
        parents: &[Vec<u8>],
    ) -> Result<Vec<u8>> {
        let base = parents
            .first()
            .map(|v| serde_json::from_slice(v))
            .transpose()?;
        let root = repository.apply_tree(base, &changeset.changes)?;
        repository.store().put_record(
            &repository.namespace("derived/tree/1"),
            &changeset.id()?.to_string(),
            &canonical(&root)?,
        )?;
        canonical(&root)
    }
}
