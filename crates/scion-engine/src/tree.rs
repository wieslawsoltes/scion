use crate::Repository;
use scion_types::*;
use std::collections::BTreeMap;

#[derive(Default)]
struct DeltaNode<'a> {
    change: Option<&'a FileChange>,
    children: BTreeMap<&'a str, DeltaNode<'a>>,
}

impl Repository {
    pub fn tree(&self, id: ObjectId) -> Result<Tree> {
        let result: Tree = serde_json::from_slice(&self.store().get_object(ObjectKind::Tree, id)?)?;
        result.validate()?;
        Ok(result)
    }

    pub fn root_tree(&self, revision: ObjectId) -> Result<ObjectId> {
        self.ensure_member(revision)?;
        if let Some(bytes) = self
            .store()
            .get_record(&self.namespace("derived/tree/1"), &revision.to_string())?
        {
            return Ok(serde_json::from_slice(&bytes)?);
        }
        // The general derived evaluator reconstructs evicted/rebuildable tree indexes.
        let object = self.derive(revision, &crate::derived::TreeIndex)?;
        Ok(serde_json::from_slice(
            &self.store().get_object(ObjectKind::Derived, object)?,
        )?)
    }

    pub(crate) fn apply_tree(
        &self,
        base: Option<ObjectId>,
        changes: &BTreeMap<RepoPath, FileChange>,
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
            if let Some(entry) = self.apply_node(root.entries.remove(*name), change, name)? {
                root.entries.insert((*name).to_owned(), entry);
            }
        }
        self.store()
            .put_object(ObjectKind::Tree, &canonical(&root)?)
    }

    fn apply_node(
        &self,
        old: Option<TreeEntry>,
        delta: &DeltaNode<'_>,
        path: &str,
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
            id: self
                .store()
                .put_object(ObjectKind::Tree, &canonical(&directory)?)?,
        }))
    }

    /// Look up a tree entry by traversing only path ancestors.
    pub fn entry(&self, revision: ObjectId, path: &RepoPath) -> Result<Option<TreeEntry>> {
        let root = self.root_tree(revision)?;
        if path.is_root() {
            return Ok(Some(TreeEntry::Directory { id: root }));
        }
        let mut current = root;
        let parts: Vec<_> = path.components().collect();
        for (i, part) in parts.iter().enumerate() {
            let entry = self.tree(current)?.entries.remove(*part);
            if i + 1 == parts.len() {
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
        Ok(match self.entry(revision, path)? {
            Some(TreeEntry::File { entry }) => Some(entry),
            _ => None,
        })
    }
    pub fn read_file(&self, revision: ObjectId, path: &RepoPath) -> Result<Vec<u8>> {
        let file = self
            .file(revision, path)?
            .ok_or_else(|| Error::NotFound(path.to_string()))?;
        self.store().get_object(ObjectKind::Blob, file.id)
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
        self.diff_roots(self.root_tree(base)?, self.root_tree(target)?, true)
    }
    /// Apply path authorization before reading file bodies or computing patches.
    /// Tree metadata is traversed internally, but denied file content is untouched.
    pub fn diff_filtered(
        &self,
        base: ObjectId,
        target: ObjectId,
        allowed: impl Fn(&RepoPath) -> bool,
    ) -> Result<Vec<FileDiff>> {
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
            let old_dir = match &before {
                Some(TreeEntry::Directory { id }) => self.tree(*id)?.entries,
                _ => BTreeMap::new(),
            };
            let new_dir = match &after {
                Some(TreeEntry::Directory { id }) => self.tree(*id)?.entries,
                _ => BTreeMap::new(),
            };
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
            let mut old = old_dir.into_iter().peekable();
            let mut new = new_dir.into_iter().peekable();
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
                    pending.push((path.join(&name)?, before, after));
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
            .map(|e| self.store().get_object(ObjectKind::Blob, e.id))
            .transpose()?
            .unwrap_or_default();
        let new = after
            .map(|e| self.store().get_object(ObjectKind::Blob, e.id))
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
