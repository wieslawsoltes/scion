//! Server-side source control independent of transport, runtime and working copy.
//! Changesets are immutable first-parent file deltas; refs are separate CAS rows.
use scion_graph::{GraphAccess, GraphNode};
use scion_store::{ImmutableRecord, SharedStore};
use scion_types::*;
use std::collections::BTreeMap;

mod derived;
mod history;
mod merge;
mod tree;
mod virtual_base;
pub use derived::{DerivedData, DerivedPolicy};
pub use merge::RebaseResult;

/// A namespaced repository over an independently reusable storage backend.
#[derive(Clone)]
pub struct Repository {
    name: String,
    store: SharedStore,
    flights: std::sync::Arc<derived::Flights>,
}

impl Repository {
    pub fn new(name: impl Into<String>, store: SharedStore) -> Result<Self> {
        let name = name.into();
        validate_name(&name)?;
        // Slash-containing repository names would alias metadata namespaces.
        if name.contains('/') {
            return Err(Error::Invalid(
                "repository names cannot contain slashes".into(),
            ));
        }
        Ok(Self {
            name,
            store,
            flights: Default::default(),
        })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn store(&self) -> &SharedStore {
        &self.store
    }
    pub(crate) fn namespace(&self, suffix: &str) -> String {
        format!("repo/{}/{suffix}", self.name)
    }

    pub fn put_blob(&self, bytes: &[u8]) -> Result<ObjectId> {
        self.store.put_object(ObjectKind::Blob, bytes)
    }

    /// Create a durable immutable commit without moving any reference.
    /// All parent and content validation occurs before atomic log publication.
    pub fn create_commit(&self, request: CommitRequest) -> Result<Changeset> {
        request.validate()?;
        for parent in &request.parents {
            self.ensure_member(*parent)?;
        }
        for (path, change) in &request.changes {
            if path.components().count() > 256 {
                return Err(Error::Limit("path nesting exceeds 256 directories".into()));
            }
            if let FileChange::Put { entry, copy_from } = change {
                let blob = self.store.get_object(ObjectKind::Blob, entry.id)?;
                if blob.len() as u64 != entry.size {
                    return Err(Error::Invalid(format!("blob size mismatch for {path}")));
                }
                if let Some(source) = copy_from {
                    let original = self.file(source.changeset, &source.path)?.ok_or_else(|| {
                        Error::Invalid(format!("copy source {} does not exist", source.path))
                    })?;
                    if original.kind == FileKind::Submodule && entry.kind != FileKind::Submodule {
                        return Err(Error::Invalid(
                            "submodule copy source has incompatible mode".into(),
                        ));
                    }
                }
            }
        }
        let parent_root = match request.parents.first() {
            Some(id) => Some(self.root_tree(*id)?),
            None => None,
        };
        // Validation and persistent-tree construction touch only modified directories.
        let root = self.apply_tree(parent_root, &request.changes)?;
        let graph = scion_graph::build_node(self, &request.parents)?;
        let bytes = canonical(&request)?;
        let id = self.store.put_object(ObjectKind::Changeset, &bytes)?;
        // One metadata transaction persists indexes, membership and the log. The
        // immutable content above is durable before this visibility boundary.
        self.store.publish_commit(
            &self.name,
            id,
            &[
                ImmutableRecord {
                    namespace: self.namespace("derived/tree/1"),
                    key: id.to_string(),
                    value: canonical(&root)?,
                },
                ImmutableRecord {
                    namespace: self.namespace("graph"),
                    key: id.to_string(),
                    value: canonical(&graph)?,
                },
            ],
        )?;
        Ok(request)
    }

    /// Read a commit only if it belongs to this repository; knowing its ID is insufficient.
    pub fn get_changeset(&self, id: ObjectId) -> Result<Changeset> {
        self.ensure_member(id)?;
        let bytes = self.store.get_object(ObjectKind::Changeset, id)?;
        let changeset: Changeset = serde_json::from_slice(&bytes)?;
        changeset.validate()?;
        Ok(changeset)
    }

    pub(crate) fn ensure_member(&self, id: ObjectId) -> Result<()> {
        if self
            .store
            .get_record(&self.namespace("changesets"), &id.to_string())?
            .is_none()
        {
            return Err(Error::NotFound(format!("changeset {id} in {}", self.name)));
        }
        Ok(())
    }

    pub fn resolve_ref(&self, name: &str) -> Result<Option<ObjectId>> {
        validate_name(name)?;
        self.store
            .get_record(&self.namespace("refs"), name)?
            .map(|v| serde_json::from_slice(&v).map_err(Error::from))
            .transpose()
    }

    /// Atomically update or delete a ref, returning false for a stale expected value.
    pub fn update_ref(
        &self,
        name: &str,
        expected: Option<ObjectId>,
        target: Option<ObjectId>,
    ) -> Result<bool> {
        validate_name(name)?;
        if let Some(id) = target {
            self.get_changeset(id)?;
        }
        let old = expected.as_ref().map(canonical).transpose()?;
        let new = target.as_ref().map(canonical).transpose()?;
        self.store.compare_and_swap(
            &self.namespace("refs"),
            name,
            old.as_deref(),
            new.as_deref(),
        )
    }

    pub fn list_refs(&self) -> Result<BTreeMap<String, ObjectId>> {
        self.store
            .scan_records(&self.namespace("refs"), "", 1_000_000)?
            .into_iter()
            .map(|(name, data)| Ok((name, serde_json::from_slice(&data)?)))
            .collect()
    }

    pub fn is_ancestor(&self, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
        scion_graph::is_ancestor(self, ancestor, descendant)
    }
    pub fn merge_bases(&self, a: ObjectId, b: ObjectId) -> Result<Vec<ObjectId>> {
        scion_graph::merge_bases(self, a, b)
    }
    pub fn nth_first_parent(&self, id: ObjectId, distance: u64) -> Result<Option<ObjectId>> {
        scion_graph::nth_first_parent(self, id, distance)
    }
    pub fn log(&self, revision: ObjectId, limit: usize) -> Result<Vec<(ObjectId, Changeset)>> {
        scion_graph::ancestors(self, revision, limit.min(100_000))?
            .into_iter()
            .map(|id| Ok((id, self.get_changeset(id)?)))
            .collect()
    }
    pub fn bisect_candidate(
        &self,
        good: ObjectId,
        bad: ObjectId,
        skipped: &[ObjectId],
    ) -> Result<Option<ObjectId>> {
        scion_graph::bisect_candidate(self, good, bad, skipped)
    }

    /// Resolve a full ID or an explicitly named reference.
    pub fn resolve(&self, value: &str) -> Result<ObjectId> {
        if let Ok(id) = value.parse::<ObjectId>() {
            self.get_changeset(id)?;
            return Ok(id);
        }
        self.resolve_ref(value)?
            .ok_or_else(|| Error::NotFound(format!("reference {value}")))
    }
}

impl GraphAccess for Repository {
    fn node(&self, id: ObjectId) -> Result<GraphNode> {
        self.ensure_member(id)?;
        match self
            .store
            .get_record(&self.namespace("graph"), &id.to_string())?
        {
            Some(bytes) => Ok(serde_json::from_slice(&bytes)?),
            None => Err(Error::Corrupt(format!("missing graph record for {id}"))),
        }
    }
}
