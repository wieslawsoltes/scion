use crate::{DerivedData, Repository};
use scion_types::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Persistent path trie permits a historical terminal and children simultaneously,
/// retaining tombstones across file/directory replacements.
#[derive(Default, Serialize, Deserialize)]
struct PathNode {
    value: Option<ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    directory_history: Option<ObjectId>,
    children: BTreeMap<String, ObjectId>,
}
#[derive(Serialize, Deserialize)]
struct HistoryRecord {
    revision: ObjectId,
    previous: Option<ObjectId>,
    entry: Option<FileEntry>,
    copy_from: Option<CopySource>,
}

struct HistoryIndex;
impl DerivedData for HistoryIndex {
    fn name(&self) -> &str {
        "path_history"
    }
    fn version(&self) -> u32 {
        1
    }
    fn derive(
        &self,
        repo: &Repository,
        changeset: &Changeset,
        parents: &[Vec<u8>],
    ) -> Result<Vec<u8>> {
        let mut root = parents
            .first()
            .map(|p| serde_json::from_slice::<ObjectId>(p))
            .transpose()?;
        let revision = changeset.id()?;
        for (path, change) in &changeset.changes {
            let parts: Vec<_> = path.components().collect();
            let previous = path_lookup(repo, root, &parts)?;
            let record = HistoryRecord {
                revision,
                previous,
                entry: change.entry().cloned(),
                copy_from: match change {
                    FileChange::Put { copy_from, .. } => copy_from.clone(),
                    _ => None,
                },
            };
            let value = repo
                .store()
                .put_object(ObjectKind::Derived, &canonical(&record)?)?;
            root = Some(path_update(repo, root, &parts, value, Some(revision))?);
        }
        let root = match root {
            Some(id) => id,
            None => repo
                .store()
                .put_object(ObjectKind::Derived, &canonical(&PathNode::default())?)?,
        };
        canonical(&root)
    }
}

fn path_lookup(
    repo: &Repository,
    root: Option<ObjectId>,
    parts: &[&str],
) -> Result<Option<ObjectId>> {
    let mut current = root;
    for part in parts {
        let Some(id) = current else {
            return Ok(None);
        };
        let node: PathNode =
            serde_json::from_slice(&repo.store().get_object(ObjectKind::Derived, id)?)?;
        current = node.children.get(*part).copied();
    }
    match current {
        Some(id) => Ok(serde_json::from_slice::<PathNode>(
            &repo.store().get_object(ObjectKind::Derived, id)?,
        )?
        .value),
        None => Ok(None),
    }
}

fn path_update(
    repo: &Repository,
    root: Option<ObjectId>,
    parts: &[&str],
    value: ObjectId,
    revision: Option<ObjectId>,
) -> Result<ObjectId> {
    let mut ancestors = Vec::with_capacity(parts.len());
    let mut current = root;
    for part in parts {
        let node = match current {
            Some(id) => serde_json::from_slice::<PathNode>(
                &repo.store().get_object(ObjectKind::Derived, id)?,
            )?,
            None => PathNode::default(),
        };
        current = node.children.get(*part).copied();
        ancestors.push((node, *part));
    }
    let mut leaf = match current {
        Some(id) => {
            serde_json::from_slice::<PathNode>(&repo.store().get_object(ObjectKind::Derived, id)?)?
        }
        None => PathNode::default(),
    };
    leaf.value = Some(value);
    let mut id = repo
        .store()
        .put_object(ObjectKind::Derived, &canonical(&leaf)?)?;
    for (mut node, name) in ancestors.into_iter().rev() {
        node.children.insert(name.to_owned(), id);
        if let Some(revision) = revision {
            let already_recorded = match node.directory_history {
                Some(head) => {
                    serde_json::from_slice::<HistoryRecord>(
                        &repo.store().get_object(ObjectKind::Derived, head)?,
                    )?
                    .revision
                        == revision
                }
                None => false,
            };
            if !already_recorded {
                let record = HistoryRecord {
                    revision,
                    previous: node.directory_history,
                    entry: None,
                    copy_from: None,
                };
                node.directory_history = Some(
                    repo.store()
                        .put_object(ObjectKind::Derived, &canonical(&record)?)?,
                );
            }
        }
        id = repo
            .store()
            .put_object(ObjectKind::Derived, &canonical(&node)?)?;
    }
    Ok(id)
}

impl Repository {
    fn history_head(&self, revision: ObjectId, path: &RepoPath) -> Result<Option<ObjectId>> {
        let value = self.derive(revision, &HistoryIndex)?;
        let root = serde_json::from_slice::<ObjectId>(
            &self.store().get_object(ObjectKind::Derived, value)?,
        )?;
        path_lookup(self, Some(root), &path.components().collect::<Vec<_>>())
    }

    /// First-parent path history including descendants, deletion and explicit
    /// copies. Two linked indexes merge exact-path and directory-prefix changes,
    /// skipping revisions which did not touch the requested path.
    pub fn history(
        &self,
        revision: ObjectId,
        path: &RepoPath,
        limit: usize,
    ) -> Result<Vec<ObjectId>> {
        let value = self.derive(revision, &HistoryIndex)?;
        let mut root = Some(serde_json::from_slice::<ObjectId>(
            &self.store().get_object(ObjectKind::Derived, value)?,
        )?);
        for part in path.components() {
            let Some(id) = root else {
                return Ok(Vec::new());
            };
            let node: PathNode =
                serde_json::from_slice(&self.store().get_object(ObjectKind::Derived, id)?)?;
            root = node.children.get(part).copied();
        }
        let Some(root) = root else {
            return Ok(Vec::new());
        };
        let node: PathNode =
            serde_json::from_slice(&self.store().get_object(ObjectKind::Derived, root)?)?;
        let mut heads = [node.value, node.directory_history];
        let mut result = Vec::new();
        let mut seen = BTreeSet::new();
        while result.len() < limit.min(100_000) {
            let mut latest = None;
            for (slot, head) in heads.iter().enumerate() {
                if let Some(id) = head {
                    let record: HistoryRecord = serde_json::from_slice(
                        &self.store().get_object(ObjectKind::Derived, *id)?,
                    )?;
                    let generation =
                        scion_graph::GraphAccess::node(self, record.revision)?.generation;
                    if latest
                        .as_ref()
                        .is_none_or(|(_, best, _)| generation > *best)
                    {
                        latest = Some((slot, generation, record));
                    }
                }
            }
            let Some((slot, _, record)) = latest else {
                break;
            };
            heads[slot] = record.previous;
            if seen.insert(record.revision) {
                result.push(record.revision);
            }
        }
        Ok(result)
    }

    /// Cached line attribution; unchanged lines retain their original revision and
    /// line number. Explicit copies/renames inherit the source's attribution.
    pub fn blame(&self, revision: ObjectId, path: &RepoPath) -> Result<Vec<BlameLine>> {
        let file = self
            .file(revision, path)?
            .ok_or_else(|| Error::NotFound(path.to_string()))?;
        let bytes = self.store().get_object(ObjectKind::Blob, file.id)?;
        if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
            return Err(Error::Invalid(
                "blame requires UTF-8 text without NUL bytes".into(),
            ));
        }
        let head = self
            .history_head(revision, path)?
            .ok_or_else(|| Error::Corrupt(format!("missing history for {path}")))?;
        let namespace = self.namespace("blame/1");
        let mut pending = vec![(head, false)];
        let mut done = BTreeMap::<ObjectId, Vec<BlameLine>>::new();
        let mut active = BTreeSet::new();
        while let Some((id, ready)) = pending.pop() {
            if done.contains_key(&id) {
                continue;
            }
            if let Some(bytes) = self.store().get_record(&namespace, &id.to_string())? {
                let object: ObjectId = serde_json::from_slice(&bytes)?;
                done.insert(
                    id,
                    serde_json::from_slice(&self.store().get_object(ObjectKind::Derived, object)?)?,
                );
                continue;
            }
            let record: HistoryRecord =
                serde_json::from_slice(&self.store().get_object(ObjectKind::Derived, id)?)?;
            let dependency = if let Some(source) = &record.copy_from {
                self.history_head(source.changeset, &source.path)?
            } else {
                record.previous
            };
            if !ready {
                if !active.insert(id) {
                    return Err(Error::Corrupt("cycle in copy provenance".into()));
                }
                pending.push((id, true));
                if let Some(previous) = dependency
                    && !done.contains_key(&previous)
                {
                    pending.push((previous, false));
                }
                continue;
            }
            let before = dependency
                .and_then(|d| done.get(&d))
                .cloned()
                .unwrap_or_default();
            let mut lines = Vec::new();
            if let Some(entry) = record.entry {
                let bytes = self.store().get_object(ObjectKind::Blob, entry.id)?;
                if let Ok(text) = std::str::from_utf8(&bytes)
                    && !bytes.contains(&0)
                {
                    let old: String = before.iter().map(|line| line.text.as_str()).collect();
                    let diff = similar::TextDiff::from_lines(old.as_str(), text);
                    for change in diff.iter_all_changes() {
                        let Some(new_index) = change.new_index() else {
                            continue;
                        };
                        let inherited = if change.tag() == similar::ChangeTag::Equal {
                            change.old_index().and_then(|i| before.get(i))
                        } else {
                            None
                        };
                        lines.push(BlameLine {
                            line: new_index + 1,
                            changeset: inherited.map_or(record.revision, |line| line.changeset),
                            original_line: inherited
                                .map_or(new_index + 1, |line| line.original_line),
                            text: change.value().to_owned(),
                        });
                    }
                }
            }
            let object = self
                .store()
                .put_object(ObjectKind::Derived, &canonical(&lines)?)?;
            self.store()
                .put_record(&namespace, &id.to_string(), &canonical(&object)?)?;
            done.insert(id, lines);
            active.remove(&id);
        }
        done.remove(&head)
            .ok_or_else(|| Error::Corrupt("blame derivation failed".into()))
    }

    /// Path search walks persistent directory indexes, restricted to a component prefix.
    pub fn search(
        &self,
        revision: ObjectId,
        prefix: &RepoPath,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RepoPath>> {
        self.search_filtered(revision, prefix, query, limit, |_| true)
    }

    /// Apply authorization before counting results toward the requested limit.
    pub fn search_filtered(
        &self,
        revision: ObjectId,
        prefix: &RepoPath,
        query: &str,
        limit: usize,
        allowed: impl Fn(&RepoPath) -> bool,
    ) -> Result<Vec<RepoPath>> {
        let mut result = Vec::new();
        let mut pending = vec![(prefix.clone(), self.entry(revision, prefix)?)];
        while let Some((path, entry)) = pending.pop() {
            match entry {
                Some(TreeEntry::File { .. }) if path.as_str().contains(query) && allowed(&path) => {
                    if result.len() >= limit.min(100_000) {
                        break;
                    }
                    result.push(path);
                }
                Some(TreeEntry::Directory { id }) => {
                    for (name, entry) in self.tree(id)?.entries.into_iter().rev() {
                        pending.push((path.join(&name)?, Some(entry)));
                    }
                }
                _ => {}
            }
        }
        Ok(result)
    }

    /// Return explicit copy metadata plus exact-content copies from the first parent.
    /// The content index is lazily derived and reused across repeated requests.
    pub fn copies(&self, revision: ObjectId) -> Result<BTreeMap<RepoPath, CopySource>> {
        let changeset = self.get_changeset(revision)?;
        let mut copies = BTreeMap::new();
        let parent = changeset.parents.first().copied();
        let index = match parent {
            Some(id) => {
                let value = self.derive(id, &ContentIndex)?;
                Some(serde_json::from_slice::<ObjectId>(
                    &self.store().get_object(ObjectKind::Derived, value)?,
                )?)
            }
            None => None,
        };
        for (path, change) in changeset.changes {
            if let FileChange::Put { entry, copy_from } = change {
                if let Some(source) = copy_from {
                    copies.insert(path, source);
                } else if let (Some(root), Some(changeset)) = (index, parent) {
                    let candidates = content_paths(self, Some(root), entry.id)?;
                    if let Some(source) = candidates.into_iter().find(|p| *p != path) {
                        copies.insert(
                            path,
                            CopySource {
                                changeset,
                                path: source,
                            },
                        );
                    }
                }
            }
        }
        Ok(copies)
    }
}

// The reverse content index is a persistent 256-way hash trie. A small edit
// copies only touched hash paths, never a whole repository-sized reverse map.
struct ContentIndex;
fn hash_parts(text: &str) -> Vec<&str> {
    (0..text.len())
        .step_by(2)
        .map(|i| &text[i..i + 2])
        .collect()
}
fn content_paths(
    repo: &Repository,
    root: Option<ObjectId>,
    content: ObjectId,
) -> Result<Vec<RepoPath>> {
    let key = content.to_string();
    match path_lookup(repo, root, &hash_parts(&key))? {
        Some(id) => Ok(serde_json::from_slice(
            &repo.store().get_object(ObjectKind::Derived, id)?,
        )?),
        None => Ok(Vec::new()),
    }
}
impl DerivedData for ContentIndex {
    fn name(&self) -> &str {
        "content_paths"
    }
    fn version(&self) -> u32 {
        1
    }
    fn derive(
        &self,
        repo: &Repository,
        changeset: &Changeset,
        parents: &[Vec<u8>],
    ) -> Result<Vec<u8>> {
        let mut root = parents
            .first()
            .map(|p| serde_json::from_slice::<ObjectId>(p))
            .transpose()?;
        let mut updates = BTreeMap::<ObjectId, BTreeSet<RepoPath>>::new();
        for (path, change) in &changeset.changes {
            if let Some(parent) = changeset.parents.first()
                && let Some(old) = repo.file(*parent, path)?
            {
                if let std::collections::btree_map::Entry::Vacant(entry) = updates.entry(old.id) {
                    entry.insert(content_paths(repo, root, old.id)?.into_iter().collect());
                }
                if let Some(paths) = updates.get_mut(&old.id) {
                    paths.remove(path);
                }
            }
            if let Some(entry) = change.entry() {
                if let std::collections::btree_map::Entry::Vacant(slot) = updates.entry(entry.id) {
                    slot.insert(content_paths(repo, root, entry.id)?.into_iter().collect());
                }
                if let Some(paths) = updates.get_mut(&entry.id) {
                    paths.insert(path.clone());
                }
            }
        }
        for (content, paths) in updates {
            let key = content.to_string();
            let value = repo.store().put_object(
                ObjectKind::Derived,
                &canonical(&paths.into_iter().collect::<Vec<_>>())?,
            )?;
            root = Some(path_update(repo, root, &hash_parts(&key), value, None)?);
        }
        let root = match root {
            Some(id) => id,
            None => repo
                .store()
                .put_object(ObjectKind::Derived, &canonical(&PathNode::default())?)?,
        };
        canonical(&root)
    }
}
