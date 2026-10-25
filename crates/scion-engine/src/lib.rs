//! Server-side source control independent of transport, runtime and working copy.
//! Changesets are immutable first-parent file deltas; refs are separate CAS rows.
use scion_graph::{GraphAccess, GraphNode};
use scion_store::{ImmutableRecord, PreparedObject, RecordCas, SharedStore};
use scion_types::*;
use std::collections::BTreeMap;

mod derived;
mod history;
mod merge;
mod tree;
mod tree_cache;
mod virtual_base;
pub use derived::{DerivedData, DerivedPolicy};
pub use merge::RebaseResult;
pub use tree_cache::TreeCacheStats;

/// Optional compare-and-swap reference movement accompanying a new changeset.
/// The reference's new value is always the changeset being published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitRefUpdate {
    pub name: String,
    pub expected: Option<ObjectId>,
}

/// Durable commit publication, including the outcome of an optional reference
/// update. A stale reference never invalidates or hides the immutable commit.
#[derive(Clone, Debug)]
pub struct CommitPublication {
    pub changeset: Changeset,
    pub sequence: u64,
    pub reference_updated: Option<bool>,
}

/// A namespaced repository over an independently reusable storage backend.
#[derive(Clone)]
pub struct Repository {
    name: String,
    store: SharedStore,
    flights: std::sync::Arc<derived::Flights>,
    trees: std::sync::Arc<tree_cache::TreeCache>,
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
            trees: Default::default(),
        })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn store(&self) -> &SharedStore {
        &self.store
    }
    /// Configure decoded immutable-tree retention; zero disables it. The default
    /// is 8 MiB of conservative accounting, shared by clones of this repository.
    pub fn with_tree_cache_capacity(mut self, bytes: usize) -> Self {
        self.trees = std::sync::Arc::new(tree_cache::TreeCache::new(bytes));
        self
    }
    /// Discard retained trees; existing readers retain their immutable snapshots.
    pub fn clear_tree_cache(&self) {
        self.trees.clear();
    }
    pub fn tree_cache_stats(&self) -> TreeCacheStats {
        self.trees.stats()
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
        Ok(self
            .create_commit_with_blobs(request, Vec::new(), None)?
            .changeset)
    }

    /// Validate and publish newly prepared blobs, their changeset and derived
    /// metadata, and optionally compare-and-swap a reference to the new commit.
    ///
    /// Staged blobs need no separate durable upload or verification reread. All
    /// acknowledged content is durable before membership is visible. Backends
    /// may combine publication and the CAS in one transaction; the portable
    /// storage fallback publishes first and then performs CAS. A stale expected
    /// reference returns `Some(false)` while retaining the immutable commit.
    /// Previously uploaded blobs preserve their independent durability contract.
    pub fn create_commit_with_blobs(
        &self,
        request: CommitRequest,
        blobs: Vec<PreparedObject>,
        reference: Option<CommitRefUpdate>,
    ) -> Result<CommitPublication> {
        request.validate()?;
        if let Some(reference) = &reference {
            validate_name(&reference.name)?;
        }
        let mut staged = std::collections::HashMap::with_capacity(blobs.len());
        for blob in &blobs {
            if blob.kind() != ObjectKind::Blob {
                return Err(Error::Invalid(
                    "staged commit content must contain only blobs".into(),
                ));
            }
            staged.insert(blob.id(), blob.bytes());
        }
        for parent in &request.parents {
            self.ensure_member(*parent)?;
        }
        for (path, change) in &request.changes {
            if path.components().count() > 256 {
                return Err(Error::Limit("path nesting exceeds 256 directories".into()));
            }
            if let FileChange::Put { entry, copy_from } = change {
                let size = match staged.get(&entry.id) {
                    Some(blob) => blob.len(),
                    None => self.store.get_object(ObjectKind::Blob, entry.id)?.len(),
                };
                if size as u64 != entry.size {
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
        let mut prepared = self.prepare_tree(parent_root, &request.changes)?;
        let graph = scion_graph::build_node(self, &request.parents)?;
        let object = PreparedObject::new(ObjectKind::Changeset, canonical(&request)?)?;
        let id = object.id();
        prepared.objects.push(object);
        prepared.objects.extend(blobs);
        let cas = reference
            .map(|reference| {
                Ok::<_, Error>(RecordCas {
                    namespace: self.namespace("refs"),
                    key: reference.name,
                    expected: reference.expected.as_ref().map(canonical).transpose()?,
                    value: Some(canonical(&id)?),
                })
            })
            .transpose()?;
        // Backends may batch immutable content and metadata into one durable
        // transaction; the portable fallback writes all content before indexes,
        // membership and the log. No staged tree enters the cache before success.
        let publication = self.store.publish_commit_with_objects_and_cas(
            &self.name,
            id,
            &prepared.objects,
            &[
                ImmutableRecord {
                    namespace: self.namespace("derived/tree/1"),
                    key: id.to_string(),
                    value: canonical(&prepared.root)?,
                },
                ImmutableRecord {
                    namespace: self.namespace("graph"),
                    key: id.to_string(),
                    value: canonical(&graph)?,
                },
            ],
            cas.as_ref(),
        )?;
        for (id, tree) in prepared.decoded {
            self.trees.insert(id, tree);
        }
        Ok(CommitPublication {
            changeset: request,
            sequence: publication.sequence,
            reference_updated: publication.cas_applied,
        })
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
        let key = id.to_string();
        let [member, record] = self.store.get_record_pair(
            (&self.namespace("changesets"), &key),
            (&self.namespace("graph"), &key),
        )?;
        if member.is_none() {
            return Err(Error::NotFound(format!("changeset {id} in {}", self.name)));
        }
        match record {
            Some(bytes) => Ok(serde_json::from_slice(&bytes)?),
            None => Err(Error::Corrupt(format!("missing graph record for {id}"))),
        }
    }
}
