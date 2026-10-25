use crate::Repository;
use scion_store::PreparedObject;
use scion_types::*;
use std::collections::BTreeMap;
use std::sync::Arc;

// Bound transient raw-tree retention independently of repository size. Beyond
// this budget trees are persisted immediately; membership remains unpublished.
const PREPARED_TREE_BYTES: usize = 32 * 1024 * 1024;
// An optional recent-edit shortcut must never decode an entire initial import.
const SMALL_CHANGESET_BYTES: usize = 8 * 1024;

pub(crate) struct PreparedTree {
    pub root: ObjectId,
    pub objects: Vec<PreparedObject>,
    pub decoded: Vec<(ObjectId, Arc<Tree>)>,
}

#[derive(Default)]
struct DeltaNode<'a> {
    change: Option<&'a FileChange>,
    children: BTreeMap<&'a str, DeltaNode<'a>>,
}

impl Repository {
    pub fn tree(&self, id: ObjectId) -> Result<Tree> {
        Ok((*self.shared_tree(id)?).clone())
    }

    /// A verified immutable tree shared without cloning sibling entries. Service
    /// authorization remains the caller's responsibility, as with `tree`.
    pub fn shared_tree(&self, id: ObjectId) -> Result<std::sync::Arc<Tree>> {
        if let Some(tree) = self.trees.get(id) {
            return Ok(tree);
        }
        let result = Tree::from_json(&self.store().get_object(ObjectKind::Tree, id)?)?;
        result.validate()?;
        let result = std::sync::Arc::new(result);
        self.trees.insert(id, result.clone());
        Ok(result)
    }

    fn store_tree(&self, tree: Tree) -> Result<ObjectId> {
        let id = self
            .store()
            .put_object(ObjectKind::Tree, &canonical(&tree)?)?;
        self.trees.insert(id, std::sync::Arc::new(tree));
        Ok(id)
    }

    pub fn root_tree(&self, revision: ObjectId) -> Result<ObjectId> {
        self.root_tree_for_lookup(revision).map(|(root, _)| root)
    }

    fn root_tree_for_lookup(&self, revision: ObjectId) -> Result<(ObjectId, bool)> {
        if let Some(root) = self.trees.root(revision) {
            return Ok((root, false));
        }
        let key = revision.to_string();
        let [member, tree] = self.store().get_record_pair(
            (&self.namespace("changesets"), &key),
            (&self.namespace("derived/tree/1"), &key),
        )?;
        if member.is_none() {
            return Err(Error::NotFound(format!(
                "changeset {revision} in {}",
                self.name()
            )));
        }
        if let Some(bytes) = tree {
            let root = serde_json::from_slice(&bytes)?;
            self.trees.insert_root(revision, root);
            return Ok((root, true));
        }
        // The general derived evaluator reconstructs evicted/rebuildable tree indexes.
        let object = self.derive(revision, &crate::derived::TreeIndex)?;
        let root = serde_json::from_slice(&self.store().get_object(ObjectKind::Derived, object)?)?;
        self.trees.insert_root(revision, root);
        Ok((root, true))
    }

    /// Call after validating membership. Storage may decline this optimization;
    /// oversized changesets are rejected before allocating their decoded data.
    fn small_changeset(&self, revision: ObjectId) -> Result<Option<Changeset>> {
        let Some(bytes) = self.store().try_get_small_object(
            ObjectKind::Changeset,
            revision,
            SMALL_CHANGESET_BYTES,
        )?
        else {
            return Ok(None);
        };
        if bytes.len() > SMALL_CHANGESET_BYTES {
            return Err(Error::Corrupt(
                "storage exceeded the bounded changeset read limit".into(),
            ));
        }
        let changeset: Changeset = serde_json::from_slice(&bytes)?;
        changeset.validate()?;
        Ok(Some(changeset))
    }

    pub(crate) fn apply_tree(
        &self,
        base: Option<ObjectId>,
        changes: &BTreeMap<RepoPath, FileChange>,
    ) -> Result<ObjectId> {
        self.apply_tree_with(base, changes, &mut |tree| self.store_tree(tree))
    }

    pub(crate) fn prepare_tree(
        &self,
        base: Option<ObjectId>,
        changes: &BTreeMap<RepoPath, FileChange>,
    ) -> Result<PreparedTree> {
        let mut objects = Vec::new();
        let mut decoded = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut bytes = 0;
        let root = self.apply_tree_with(base, changes, &mut |tree| {
            let object = PreparedObject::new(ObjectKind::Tree, canonical(&tree)?)?;
            let id = object.id();
            if seen.contains(&id) {
                return Ok(id);
            }
            if bytes + object.bytes().len() > PREPARED_TREE_BYTES {
                self.store().put_object(ObjectKind::Tree, object.bytes())?;
                self.trees.insert(id, Arc::new(tree));
            } else {
                bytes += object.bytes().len();
                seen.insert(id);
                objects.push(object);
                decoded.push((id, Arc::new(tree)));
            }
            Ok(id)
        })?;
        Ok(PreparedTree {
            root,
            objects,
            decoded,
        })
    }

    fn apply_tree_with(
        &self,
        base: Option<ObjectId>,
        changes: &BTreeMap<RepoPath, FileChange>,
        write: &mut impl FnMut(Tree) -> Result<ObjectId>,
    ) -> Result<ObjectId> {
        if changes.is_empty()
            && let Some(root) = base
        {
            return Ok(root);
        }
        let mut delta = DeltaNode::default();
        for (path, change) in changes {
            let mut node = &mut delta;
            for part in path.components() {
                node = node.children.entry(part).or_default();
            }
            node.change = Some(change);
        }
        let mut root = match base {
            Some(id) => self.tree(id)?,
            None => Tree::default(),
        };
        for (name, change) in &delta.children {
            if let Some(entry) = self.apply_node(root.entries.remove(*name), change, name, write)? {
                root.entries.insert((*name).to_owned(), entry);
            }
        }
        write(root)
    }

    fn apply_node(
        &self,
        old: Option<TreeEntry>,
        delta: &DeltaNode<'_>,
        path: &str,
        write: &mut impl FnMut(Tree) -> Result<ObjectId>,
    ) -> Result<Option<TreeEntry>> {
        let mut old = old;
        if matches!(delta.change, Some(FileChange::Delete)) {
            if matches!(old, Some(TreeEntry::Directory { .. })) {
                return Err(Error::Invalid(format!(
                    "{path} is a directory; changesets delete individual files"
                )));
            }
            old = None;
        }
        if delta.children.is_empty() {
            return match delta.change {
                Some(FileChange::Put { entry, .. }) => {
                    if matches!(old, Some(TreeEntry::Directory { .. })) {
                        return Err(Error::Conflict(format!(
                            "cannot replace nonempty directory {path} without deleting its files"
                        )));
                    }
                    Ok(Some(TreeEntry::File {
                        entry: entry.clone(),
                    }))
                }
                Some(FileChange::Delete) => Ok(None),
                None => Ok(old),
            };
        }
        let mut directory = match old {
            Some(TreeEntry::Directory { id }) => self.tree(id)?,
            Some(TreeEntry::File { .. }) => {
                return Err(Error::Conflict(format!("file blocks directory {path}")));
            }
            None => Tree::default(),
        };
        for (name, child) in &delta.children {
            if let Some(entry) = self.apply_node(
                directory.entries.remove(*name),
                child,
                &format!("{path}/{name}"),
                write,
            )? {
                directory.entries.insert((*name).to_owned(), entry);
            }
        }
        if let Some(FileChange::Put { entry, .. }) = delta.change {
            if !directory.entries.is_empty() {
                return Err(Error::Conflict(format!(
                    "file/directory collision at {path}"
                )));
            }
            return Ok(Some(TreeEntry::File {
                entry: entry.clone(),
            }));
        }
        if directory.entries.is_empty() {
            return Ok(None);
        }
        Ok(Some(TreeEntry::Directory {
            id: write(directory)?,
        }))
    }

    /// Look up a tree entry by traversing only path ancestors.
    pub fn entry(&self, revision: ObjectId, path: &RepoPath) -> Result<Option<TreeEntry>> {
        let root = self.root_tree(revision)?;
        self.entry_at_root(root, path)
    }

    fn entry_at_root(&self, root: ObjectId, path: &RepoPath) -> Result<Option<TreeEntry>> {
        if path.is_root() {
            return Ok(Some(TreeEntry::Directory { id: root }));
        }
        let mut current = root;
        let mut parts = path.components().peekable();
        while let Some(part) = parts.next() {
            let entry = match self.trees.entry(current, part) {
                Some(entry) => entry,
                None if self.trees.should_promote(current) => {
                    // A second distinct lookup demonstrates directory reuse.
                    // Decode it once rather than repeatedly scanning the same
                    // wide object for every name in a sequential access pattern.
                    let entry = self.shared_tree(current)?.entries.get(part).cloned();
                    // Oversized full trees may not fit the cache. Their selected
                    // entries can still be retained within the existing budget.
                    self.trees.insert_entry(current, part, entry.clone());
                    entry
                }
                None => {
                    let bytes = self.store().get_object(ObjectKind::Tree, current)?;
                    let entry = Tree::entry_from_json(&bytes, part)?;
                    self.trees.insert_entry(current, part, entry.clone());
                    entry
                }
            };
            if parts.peek().is_none() {
                return Ok(entry);
            }
            match entry {
                Some(TreeEntry::Directory { id }) => current = id,
                _ => return Ok(None),
            }
        }
        Ok(None)
    }

    pub fn file(&self, revision: ObjectId, path: &RepoPath) -> Result<Option<FileEntry>> {
        let (root, cold) = self.root_tree_for_lookup(revision)?;
        // Exact first-parent puts/deletes directly describe the revision's file
        // value, even when a deleted file became a directory. Only file lookup
        // uses this shortcut; entry() still resolves directory identities.
        if cold
            && let Some(changeset) = self.small_changeset(revision)?
            && let Some(change) = changeset.changes.get(path)
        {
            return Ok(change.entry().cloned());
        }
        Ok(match self.entry_at_root(root, path)? {
            Some(TreeEntry::File { entry }) => Some(entry),
            _ => None,
        })
    }
    pub fn read_file(&self, revision: ObjectId, path: &RepoPath) -> Result<Vec<u8>> {
        let file = self
            .file(revision, path)?
            .ok_or_else(|| Error::NotFound(path.to_string()))?;
        self.read_blob(file.id).map(Arc::unwrap_or_clone)
    }

    pub fn list_files(
        &self,
        revision: ObjectId,
        prefix: &RepoPath,
    ) -> Result<BTreeMap<RepoPath, FileEntry>> {
        let mut result = BTreeMap::<RepoPath, FileEntry>::new();
        match self.entry(revision, prefix)? {
            Some(TreeEntry::File { entry }) => {
                result.insert(prefix.clone(), entry);
            }
            Some(TreeEntry::Directory { id }) => {
                self.collect_files(id, prefix.clone(), &mut result)?
            }
            None => {}
        }
        Ok(result)
    }

    /// Bounded lexicographic listing with an exclusive path cursor. Whole
    /// subtrees before the cursor are skipped without reading their objects.
    /// The continuation is the last returned path when another page exists.
    pub fn list_files_page(
        &self,
        revision: ObjectId,
        prefix: &RepoPath,
        after: Option<&RepoPath>,
        limit: usize,
    ) -> Result<(BTreeMap<RepoPath, FileEntry>, Option<RepoPath>)> {
        if limit == 0 || limit > 10_000 {
            return Err(Error::Invalid("file page limit must be 1..10000".into()));
        }
        let mut result = BTreeMap::<RepoPath, FileEntry>::new();
        let mut pending = self
            .entry(revision, prefix)?
            .map(|entry| vec![(prefix.clone(), entry)])
            .unwrap_or_default();
        while let Some((path, entry)) = pending.pop() {
            match entry {
                TreeEntry::File { entry } => {
                    if after.is_some_and(|cursor| path <= *cursor) {
                        continue;
                    }
                    if result.len() == limit {
                        let next = result.last_key_value().map(|(path, _)| path.clone());
                        return Ok((result, next));
                    }
                    result.insert(path, entry);
                }
                TreeEntry::Directory { id } => {
                    // Directory sort keys include '/', matching full-file-path
                    // ordering even for siblings like `a-` and directory `a/`.
                    let key = if path.is_root() {
                        String::new()
                    } else {
                        format!("{path}/")
                    };
                    if after.is_some_and(|cursor| {
                        !path.contains(cursor) && key.as_str() < cursor.as_str()
                    }) {
                        continue;
                    }
                    let mut children = self
                        .tree(id)?
                        .entries
                        .into_iter()
                        .map(|(name, entry)| {
                            let path = path.join(&name)?;
                            let key = match entry {
                                TreeEntry::Directory { .. } => format!("{path}/"),
                                _ => path.to_string(),
                            };
                            Ok((key, path, entry))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    children.sort_by(|a, b| a.0.cmp(&b.0));
                    pending.extend(
                        children
                            .into_iter()
                            .rev()
                            .map(|(_, path, entry)| (path, entry)),
                    );
                }
            }
        }
        Ok((result, None))
    }
    fn collect_files(
        &self,
        id: ObjectId,
        prefix: RepoPath,
        output: &mut BTreeMap<RepoPath, FileEntry>,
    ) -> Result<()> {
        let mut pending = vec![(id, prefix)];
        while let Some((id, path)) = pending.pop() {
            for (name, entry) in self.tree(id)?.entries {
                let path = path.join(&name)?;
                match entry {
                    TreeEntry::File { entry } => {
                        output.insert(path, entry);
                    }
                    TreeEntry::Directory { id } => pending.push((id, path)),
                }
            }
        }
        Ok(())
    }

    /// Content-aware diff skips equal subtree IDs before reading directory objects.
    pub fn diff(&self, base: ObjectId, target: ObjectId) -> Result<Vec<FileDiff>> {
        self.diff_filtered(base, target, |_| true)
    }
    /// Apply path authorization before reading file bodies or computing patches.
    /// Tree metadata is traversed internally, but denied file content is untouched.
    pub fn diff_filtered(
        &self,
        base: ObjectId,
        target: ObjectId,
        allowed: impl Fn(&RepoPath) -> bool,
    ) -> Result<Vec<FileDiff>> {
        if base == target {
            self.ensure_member(base)?;
            return Ok(Vec::new());
        }
        // Inspect the compact graph record before loading a potentially very
        // large changeset. Nonadjacent diffs should not deserialize unrelated
        // initial-import deltas just to discover their parent relationship.
        let target_node = scion_graph::GraphAccess::node(self, target)?;
        if target_node.parents.first() == Some(&base) {
            let changeset = self.get_changeset(target)?;
            if changeset.parents != target_node.parents {
                return Err(Error::Corrupt(
                    "changeset parents disagree with its graph index".into(),
                ));
            }
            return self.diff_first_parent(base, &changeset, false, &allowed);
        }
        let base_node = scion_graph::GraphAccess::node(self, base)?;
        if base_node.parents.first() == Some(&target) {
            let changeset = self.get_changeset(base)?;
            if changeset.parents != base_node.parents {
                return Err(Error::Corrupt(
                    "changeset parents disagree with its graph index".into(),
                ));
            }
            return self.diff_first_parent(target, &changeset, true, &allowed);
        }
        self.diff_roots(self.root_tree(base)?, self.root_tree(target)?, false)?
            .into_iter()
            .filter(|diff| allowed(&diff.path))
            .map(|mut diff| {
                diff.patch =
                    self.text_patch(&diff.path, diff.before.as_ref(), diff.after.as_ref())?;
                Ok(diff)
            })
            .collect()
    }

    /// A changeset is an exact first-parent delta, including deletions needed for
    /// file/directory replacements. Point lookups into the parent are sufficient
    /// to recover its diff; decoding the target's full directories is redundant.
    fn diff_first_parent(
        &self,
        parent: ObjectId,
        changeset: &Changeset,
        reverse: bool,
        allowed: &impl Fn(&RepoPath) -> bool,
    ) -> Result<Vec<FileDiff>> {
        let (root, cold) = self.root_tree_for_lookup(parent)?;
        let previous_delta = if cold && !changeset.changes.is_empty() {
            self.small_changeset(parent)?
        } else {
            None
        };
        let mut output = Vec::with_capacity(changeset.changes.len().min(1024));
        for (path, change) in &changeset.changes {
            if !allowed(path) {
                continue;
            }
            let before = match previous_delta
                .as_ref()
                .and_then(|delta| delta.changes.get(path))
            {
                Some(change) => change.entry().cloned(),
                None => match self.entry_at_root(root, path)? {
                    Some(TreeEntry::File { entry }) => Some(entry),
                    _ => None,
                },
            };
            let after = change.entry().cloned();
            if before == after {
                continue;
            }
            let (before, after) = if reverse {
                (after, before)
            } else {
                (before, after)
            };
            output.push(FileDiff {
                patch: self.text_patch(path, before.as_ref(), after.as_ref())?,
                path: path.clone(),
                before,
                after,
            });
        }
        Ok(output)
    }

    pub(crate) fn diff_roots(
        &self,
        base: ObjectId,
        target: ObjectId,
        patches: bool,
    ) -> Result<Vec<FileDiff>> {
        let mut pending = vec![(
            RepoPath::root(),
            Some(TreeEntry::Directory { id: base }),
            Some(TreeEntry::Directory { id: target }),
        )];
        let mut changes = BTreeMap::<RepoPath, (Option<FileEntry>, Option<FileEntry>)>::new();
        while let Some((path, before, after)) = pending.pop() {
            if before == after {
                continue;
            }
            let old_tree = match &before {
                Some(TreeEntry::Directory { id }) => Some(self.shared_tree(*id)?),
                _ => None,
            };
            let new_tree = match &after {
                Some(TreeEntry::Directory { id }) => Some(self.shared_tree(*id)?),
                _ => None,
            };
            let empty = BTreeMap::new();
            let old_dir = old_tree
                .as_ref()
                .map(|tree| &tree.entries)
                .unwrap_or(&empty);
            let new_dir = new_tree
                .as_ref()
                .map(|tree| &tree.entries)
                .unwrap_or(&empty);
            let old_file = match before {
                Some(TreeEntry::File { entry }) => Some(entry),
                _ => None,
            };
            let new_file = match after {
                Some(TreeEntry::File { entry }) => Some(entry),
                _ => None,
            };
            if old_file.is_some() || new_file.is_some() {
                changes.insert(path.clone(), (old_file, new_file));
            }
            // Merge already-sorted maps in linear time. Equal entries require no
            // cloned name, full path allocation, tree read or traversal frame.
            let mut old = old_dir.iter().peekable();
            let mut new = new_dir.iter().peekable();
            loop {
                let entry = match (old.peek(), new.peek()) {
                    (Some((a, _)), Some((b, _))) => match a.cmp(b) {
                        std::cmp::Ordering::Less => {
                            old.next().map(|(name, entry)| (name, Some(entry), None))
                        }
                        std::cmp::Ordering::Greater => {
                            new.next().map(|(name, entry)| (name, None, Some(entry)))
                        }
                        std::cmp::Ordering::Equal => old
                            .next()
                            .zip(new.next())
                            .map(|((name, before), (_, after))| (name, Some(before), Some(after))),
                    },
                    (Some(_), None) => old.next().map(|(name, entry)| (name, Some(entry), None)),
                    (None, Some(_)) => new.next().map(|(name, entry)| (name, None, Some(entry))),
                    (None, None) => break,
                };
                if let Some((name, before, after)) = entry
                    && before != after
                {
                    pending.push((path.join(name)?, before.cloned(), after.cloned()));
                }
            }
        }
        changes
            .into_iter()
            .map(|(path, (before, after))| {
                let patch = if patches {
                    self.text_patch(&path, before.as_ref(), after.as_ref())?
                } else {
                    None
                };
                Ok(FileDiff {
                    path,
                    before,
                    after,
                    patch,
                })
            })
            .collect()
    }

    fn text_patch(
        &self,
        path: &RepoPath,
        before: Option<&FileEntry>,
        after: Option<&FileEntry>,
    ) -> Result<Option<String>> {
        const MAX_PATCH: u64 = 4 * 1024 * 1024;
        if before.is_some_and(|e| e.size > MAX_PATCH) || after.is_some_and(|e| e.size > MAX_PATCH) {
            return Ok(None);
        }
        let old = before
            .map(|e| self.read_blob(e.id))
            .transpose()?
            .unwrap_or_default();
        let new = after
            .map(|e| self.read_blob(e.id))
            .transpose()?
            .unwrap_or_default();
        if old.contains(&0) || new.contains(&0) {
            return Ok(None);
        }
        let (Ok(old), Ok(new)) = (std::str::from_utf8(&old), std::str::from_utf8(&new)) else {
            return Ok(None);
        };
        Ok(Some(
            similar::TextDiff::from_lines(old, new)
                .unified_diff()
                .header(&format!("a/{path}"), &format!("b/{path}"))
                .to_string(),
        ))
    }
}
