use crate::Repository;
use scion_types::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Result of replaying a first-parent stack; completed rewrites remain immutable
/// when a later changeset conflicts, enabling explicit continuation by callers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebaseResult {
    pub rewritten: BTreeMap<ObjectId, ObjectId>,
    pub tip: ObjectId,
    pub conflicts: Vec<MergeConflict>,
}

impl Repository {
    /// Three-way merge. All maximal merge bases are considered; ambiguous
    /// criss-cross bases are reported instead of silently selecting a base.
    pub fn merge(
        &self,
        ours: ObjectId,
        theirs: ObjectId,
        author: String,
        message: String,
        timestamp_ms: i64,
    ) -> Result<MergeResult> {
        self.get_changeset(ours)?;
        self.get_changeset(theirs)?;
        if ours == theirs {
            return Ok(MergeResult {
                changeset: Some(ours),
                conflicts: Vec::new(),
            });
        }
        let bases = self.merge_bases(ours, theirs)?;
        let base_root = match bases.as_slice() {
            [] => self
                .store()
                .put_object(ObjectKind::Tree, &canonical(&Tree::default())?)?,
            [base] => self.root_tree(*base)?,
            _ => {
                // Identical base trees are semantically unambiguous even with several DAG bases.
                let roots = bases
                    .iter()
                    .map(|id| self.root_tree(*id))
                    .collect::<Result<BTreeSet<_>>>()?;
                if roots.len() == 1 {
                    *roots
                        .first()
                        .ok_or_else(|| Error::Corrupt("missing base tree".into()))?
                } else {
                    return Ok(MergeResult { changeset: None,conflicts: vec![MergeConflict {
                        path: RepoPath::root(),reason: "multiple merge bases have different trees; resolve a virtual base explicitly".into(),base: None,ours: None,theirs: None,
                    }] });
                }
            }
        };
        let (changes, conflicts) =
            self.merge_trees(base_root, self.root_tree(ours)?, self.root_tree(theirs)?)?;
        self.finish_merge(
            vec![ours, theirs],
            changes,
            conflicts,
            None,
            author,
            message,
            timestamp_ms,
            BTreeMap::new(),
        )
    }

    /// Replay one first-parent delta onto `onto`, preserving author and message.
    /// Merge changesets are flattened intentionally; use merge for two-parent history.
    pub fn rebase(&self, revision: ObjectId, onto: ObjectId) -> Result<MergeResult> {
        let original = self.get_changeset(revision)?;
        let base = match original.parents.first() {
            Some(parent) => self.root_tree(*parent)?,
            None => self
                .store()
                .put_object(ObjectKind::Tree, &canonical(&Tree::default())?)?,
        };
        let (mut changes, conflicts) =
            self.merge_trees(base, self.root_tree(onto)?, self.root_tree(revision)?)?;
        for (path, change) in &mut changes {
            if let FileChange::Put { copy_from, .. } = change
                && let Some(FileChange::Put {
                    copy_from: source, ..
                }) = original.changes.get(path)
            {
                *copy_from = source.clone();
            }
        }
        let mut metadata = original.metadata;
        metadata.insert("scion.rebased_from".into(), revision.to_string());
        self.finish_merge(
            vec![onto],
            changes,
            conflicts,
            original.committer,
            original.author,
            original.message,
            original.timestamp_ms,
            metadata,
        )
    }

    /// Replay the first-parent interval `(base, tip]` in order.
    pub fn rebase_stack(
        &self,
        tip: ObjectId,
        base: ObjectId,
        onto: ObjectId,
    ) -> Result<RebaseResult> {
        let mut original = tip;
        let mut stack = Vec::new();
        while original != base {
            if stack.len() >= 100_000 {
                return Err(Error::Limit("rebase stack exceeds 100000 commits".into()));
            }
            stack.push(original);
            original = self
                .get_changeset(original)?
                .parents
                .first()
                .copied()
                .ok_or_else(|| {
                    Error::Invalid("base is not on tip's first-parent history".into())
                })?;
        }
        let mut result = RebaseResult {
            rewritten: BTreeMap::new(),
            tip: onto,
            conflicts: Vec::new(),
        };
        for old in stack.into_iter().rev() {
            let replay = self.rebase(old, result.tip)?;
            if let Some(new) = replay.changeset {
                result.rewritten.insert(old, new);
                result.tip = new;
            } else {
                result.conflicts = replay.conflicts;
                break;
            }
        }
        Ok(result)
    }

    /// Apply the inverse delta of a changeset to another revision. `mainline` is
    /// the zero-based parent used for merges; roots have an empty-tree parent.
    pub fn revert(
        &self,
        revision: ObjectId,
        onto: ObjectId,
        mainline: usize,
        author: String,
        message: String,
        timestamp_ms: i64,
    ) -> Result<MergeResult> {
        let original = self.get_changeset(revision)?;
        let parent = if original.parents.is_empty() && mainline == 0 {
            self.store()
                .put_object(ObjectKind::Tree, &canonical(&Tree::default())?)?
        } else {
            self.root_tree(
                *original
                    .parents
                    .get(mainline)
                    .ok_or_else(|| Error::Invalid("mainline parent does not exist".into()))?,
            )?
        };
        let (changes, conflicts) =
            self.merge_trees(self.root_tree(revision)?, self.root_tree(onto)?, parent)?;
        let metadata = BTreeMap::from([("scion.reverts".into(), revision.to_string())]);
        self.finish_merge(
            vec![onto],
            changes,
            conflicts,
            None,
            author,
            message,
            timestamp_ms,
            metadata,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_merge(
        &self,
        parents: Vec<ObjectId>,
        changes: BTreeMap<RepoPath, FileChange>,
        conflicts: Vec<MergeConflict>,
        committer: Option<String>,
        author: String,
        message: String,
        timestamp_ms: i64,
        metadata: BTreeMap<String, String>,
    ) -> Result<MergeResult> {
        if !conflicts.is_empty() {
            return Ok(MergeResult {
                changeset: None,
                conflicts,
            });
        }
        let request = Changeset {
            parents,
            author,
            committer,
            timestamp_ms,
            message,
            changes,
            metadata,
        };
        match self.create_commit(request) {
            Ok(commit) => Ok(MergeResult {
                changeset: Some(commit.id()?),
                conflicts: Vec::new(),
            }),
            Err(Error::Conflict(reason)) | Err(Error::Invalid(reason)) => Ok(MergeResult {
                changeset: None,
                conflicts: vec![MergeConflict {
                    path: RepoPath::root(),
                    reason,
                    base: None,
                    ours: None,
                    theirs: None,
                }],
            }),
            Err(error) => Err(error),
        }
    }

    fn merge_trees(
        &self,
        base: ObjectId,
        ours: ObjectId,
        theirs: ObjectId,
    ) -> Result<(BTreeMap<RepoPath, FileChange>, Vec<MergeConflict>)> {
        let their_changes = self.diff_roots(base, theirs, false)?;
        let our_changes = self
            .diff_roots(base, ours, false)?
            .into_iter()
            .map(|d| (d.path, d.after))
            .collect::<BTreeMap<_, _>>();
        let mut result = BTreeMap::new();
        let mut conflicts = Vec::new();
        for change in their_changes {
            let our = our_changes
                .get(&change.path)
                .cloned()
                .unwrap_or_else(|| change.before.clone());
            let merged =
                self.merge_file(change.before.as_ref(), our.as_ref(), change.after.as_ref());
            match merged {
                Ok(entry) => {
                    if entry != our {
                        result.insert(
                            change.path,
                            match entry {
                                Some(entry) => FileChange::put(entry),
                                None => FileChange::Delete,
                            },
                        );
                    }
                }
                Err(Error::Conflict(reason)) => conflicts.push(MergeConflict {
                    path: change.path,
                    reason,
                    base: change.before,
                    ours: our,
                    theirs: change.after,
                }),
                Err(error) => return Err(error),
            }
        }
        Ok((result, conflicts))
    }

    fn merge_file(
        &self,
        base: Option<&FileEntry>,
        ours: Option<&FileEntry>,
        theirs: Option<&FileEntry>,
    ) -> Result<Option<FileEntry>> {
        if ours == theirs {
            return Ok(ours.cloned());
        }
        if ours == base {
            return Ok(theirs.cloned());
        }
        if theirs == base {
            return Ok(ours.cloned());
        }
        let (Some(base), Some(ours), Some(theirs)) = (base, ours, theirs) else {
            return Err(Error::Conflict(
                if base.is_none() {
                    "concurrent additions differ"
                } else {
                    "delete/modify conflict"
                }
                .into(),
            ));
        };
        let kind_class = |kind| match kind {
            FileKind::Regular | FileKind::Executable => 0,
            FileKind::Symlink => 1,
            FileKind::Submodule => 2,
        };
        if (kind_class(ours.kind) != kind_class(base.kind)
            && kind_class(theirs.kind) != kind_class(ours.kind))
            || (kind_class(theirs.kind) != kind_class(base.kind)
                && kind_class(ours.kind) != kind_class(theirs.kind))
        {
            return Err(Error::Conflict(
                "file type change conflicts with concurrent modification".into(),
            ));
        }
        let kind = if ours.kind == theirs.kind {
            ours.kind
        } else if ours.kind == base.kind {
            theirs.kind
        } else if theirs.kind == base.kind {
            ours.kind
        } else {
            return Err(Error::Conflict("incompatible file mode changes".into()));
        };
        let content = if ours.id == theirs.id || theirs.id == base.id {
            Some(ours)
        } else if ours.id == base.id {
            Some(theirs)
        } else {
            None
        };
        if let Some(content) = content {
            return Ok(Some(FileEntry {
                id: content.id,
                size: content.size,
                kind,
            }));
        }
        if !matches!(kind, FileKind::Regular | FileKind::Executable) {
            return Err(Error::Conflict(
                "concurrent symbolic-link/submodule changes".into(),
            ));
        }
        if base.size.max(ours.size).max(theirs.size) > 16 * 1024 * 1024 {
            return Err(Error::Conflict("text merge size limit exceeded".into()));
        }
        let base_bytes = self.store().get_object(ObjectKind::Blob, base.id)?;
        let our_bytes = self.store().get_object(ObjectKind::Blob, ours.id)?;
        let their_bytes = self.store().get_object(ObjectKind::Blob, theirs.id)?;
        if base_bytes.contains(&0) || our_bytes.contains(&0) || their_bytes.contains(&0) {
            return Err(Error::Conflict("concurrent binary changes".into()));
        }
        let (Ok(base), Ok(ours), Ok(theirs)) = (
            std::str::from_utf8(&base_bytes),
            std::str::from_utf8(&our_bytes),
            std::str::from_utf8(&their_bytes),
        ) else {
            return Err(Error::Conflict("concurrent non-UTF-8 changes".into()));
        };
        let merged = diffy::merge(base, ours, theirs)
            .map_err(|_| Error::Conflict("overlapping text changes".into()))?;
        let id = self.put_blob(merged.as_bytes())?;
        Ok(Some(FileEntry {
            id,
            size: merged.len() as u64,
            kind,
        }))
    }
}
