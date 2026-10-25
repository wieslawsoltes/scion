//! Deterministic recursive merge-base synthesis without publishing virtual commits.
//!
//! A virtual node's ancestry is the union of its real head ancestries. Therefore
//! its common ancestors with another virtual node are the maximal elements of
//! the union of their pairwise real merge bases. This avoids inserting temporary
//! graph nodes or relying on a chosen first parent for the synthesized ancestor.
use crate::Repository;
use scion_graph::{GraphAccess, GraphNode};
use scion_types::{Error, MergeConflict, ObjectId, ObjectKind, RepoPath, Result, Tree, canonical};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
};

const MAX_DEPTH: usize = 64;
const MAX_HEADS: usize = 256;
const MAX_OPERATIONS: usize = 4096;
const MAX_GRAPH_RECORDS: usize = 100_000;

#[derive(Clone)]
pub(crate) enum Outcome {
    Tree(ObjectId),
    Conflicts(Vec<MergeConflict>),
}

/// Query-local graph cache both avoids repeated database reads across recursive
/// DAG subproblems and limits memory/graph work for adversarial criss-cross DAGs.
struct Graph<'a> {
    repository: &'a Repository,
    records: RefCell<BTreeMap<ObjectId, GraphNode>>,
}
impl GraphAccess for Graph<'_> {
    fn node(&self, id: ObjectId) -> Result<GraphNode> {
        if let Some(node) = self.records.borrow().get(&id) {
            return Ok(node.clone());
        }
        if self.records.borrow().len() >= MAX_GRAPH_RECORDS {
            return Err(Error::Limit(
                "virtual merge base exceeds 100000 graph records".into(),
            ));
        }
        let node = self.repository.node(id)?;
        self.records.borrow_mut().insert(id, node.clone());
        Ok(node)
    }
}

pub(crate) struct Resolver<'a> {
    repository: &'a Repository,
    graph: Graph<'a>,
    roots: BTreeMap<ObjectId, ObjectId>,
    resolved: BTreeMap<Vec<ObjectId>, Outcome>,
    pairs: BTreeMap<(ObjectId, ObjectId), Vec<ObjectId>>,
    ancestry: BTreeMap<(ObjectId, ObjectId), bool>,
    operations: usize,
}

impl<'a> Resolver<'a> {
    pub(crate) fn new(repository: &'a Repository) -> Self {
        Self {
            repository,
            graph: Graph {
                repository,
                records: RefCell::new(BTreeMap::new()),
            },
            roots: BTreeMap::new(),
            resolved: BTreeMap::new(),
            pairs: BTreeMap::new(),
            ancestry: BTreeMap::new(),
            operations: 0,
        }
    }

    pub(crate) fn resolve(&mut self, heads: &[ObjectId]) -> Result<Outcome> {
        self.resolve_at(heads, 0)
    }

    fn spend(&mut self) -> Result<()> {
        if self.operations >= MAX_OPERATIONS {
            return Err(Error::Limit(
                "virtual merge base exceeds 4096 synthesis operations".into(),
            ));
        }
        self.operations += 1;
        Ok(())
    }

    fn root(&mut self, head: ObjectId) -> Result<ObjectId> {
        if let Some(root) = self.roots.get(&head) {
            return Ok(*root);
        }
        let root = self.repository.root_tree(head)?;
        self.roots.insert(head, root);
        Ok(root)
    }

    fn resolve_at(&mut self, heads: &[ObjectId], depth: usize) -> Result<Outcome> {
        let mut key = heads.to_vec();
        key.sort_unstable();
        key.dedup();
        if let Some(outcome) = self.resolved.get(&key) {
            return Ok(outcome.clone());
        }
        if depth >= MAX_DEPTH || key.len() > MAX_HEADS {
            return Err(Error::Limit(
                "virtual merge base exceeds 64 recursive levels or 256 heads".into(),
            ));
        }
        self.spend()?;
        let result = match key.as_slice() {
            [] => Outcome::Tree(
                self.repository
                    .store()
                    .put_object(ObjectKind::Tree, &canonical(&Tree::default())?)?,
            ),
            [head] => Outcome::Tree(self.root(*head)?),
            _ => {
                let roots = key
                    .iter()
                    .map(|head| self.root(*head))
                    .collect::<Result<BTreeSet<_>>>()?;
                if roots.len() == 1 {
                    Outcome::Tree(
                        *roots
                            .first()
                            .ok_or_else(|| Error::Corrupt("missing merge-base tree".into()))?,
                    )
                } else {
                    // Sorted, balanced grouping makes synthesis independent of
                    // caller parent order and bounds width-induced stack depth.
                    let (left, right) = key.split_at(key.len() / 2);
                    let left_root = self.resolve_at(left, depth + 1)?;
                    let right_root = self.resolve_at(right, depth + 1)?;
                    match (left_root, right_root) {
                        (Outcome::Tree(ours), Outcome::Tree(theirs)) => {
                            let common = self.common_bases(left, right)?;
                            match self.resolve_at(&common, depth + 1)? {
                                Outcome::Tree(base) => self.merge_roots(base, ours, theirs)?,
                                conflict => conflict,
                            }
                        }
                        (Outcome::Conflicts(mut left), Outcome::Conflicts(right)) => {
                            left.extend(right);
                            Outcome::Conflicts(normalize_conflicts(left)?)
                        }
                        (Outcome::Conflicts(conflicts), _) | (_, Outcome::Conflicts(conflicts)) => {
                            Outcome::Conflicts(conflicts)
                        }
                    }
                }
            }
        };
        self.resolved.insert(key, result.clone());
        Ok(result)
    }

    fn pair(&mut self, a: ObjectId, b: ObjectId) -> Result<Vec<ObjectId>> {
        let key = if a <= b { (a, b) } else { (b, a) };
        if let Some(bases) = self.pairs.get(&key) {
            return Ok(bases.clone());
        }
        self.spend()?;
        let bases = scion_graph::merge_bases(&self.graph, key.0, key.1)?;
        self.pairs.insert(key, bases.clone());
        Ok(bases)
    }

    fn is_ancestor(&mut self, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
        let key = (ancestor, descendant);
        if let Some(result) = self.ancestry.get(&key) {
            return Ok(*result);
        }
        self.spend()?;
        let result = scion_graph::is_ancestor(&self.graph, ancestor, descendant)?;
        self.ancestry.insert(key, result);
        Ok(result)
    }

    fn common_bases(&mut self, left: &[ObjectId], right: &[ObjectId]) -> Result<Vec<ObjectId>> {
        let mut union = BTreeSet::new();
        for a in left {
            for b in right {
                union.extend(self.pair(*a, *b)?);
                if union.len() > MAX_HEADS {
                    return Err(Error::Limit(
                        "virtual merge base has more than 256 ancestor candidates".into(),
                    ));
                }
            }
        }
        let mut ordered = union
            .into_iter()
            .map(|id| Ok((self.graph.node(id)?.generation, id)))
            .collect::<Result<Vec<_>>>()?;
        ordered.sort_unstable_by(|a, b| b.cmp(a));
        let mut maximal = Vec::new();
        for (_, candidate) in ordered {
            let mut dominated = false;
            for descendant in &maximal {
                if self.is_ancestor(candidate, *descendant)? {
                    dominated = true;
                    break;
                }
            }
            if !dominated {
                maximal.push(candidate);
            }
        }
        maximal.sort_unstable();
        Ok(maximal)
    }

    fn merge_roots(&self, base: ObjectId, ours: ObjectId, theirs: ObjectId) -> Result<Outcome> {
        let (changes, conflicts) = self.repository.merge_trees(base, ours, theirs)?;
        if !conflicts.is_empty() {
            return Ok(Outcome::Conflicts(normalize_conflicts(conflicts)?));
        }
        match self.repository.apply_tree(Some(ours), &changes) {
            Ok(root) => Ok(Outcome::Tree(root)),
            Err(Error::Conflict(reason)) | Err(Error::Invalid(reason)) => {
                Ok(Outcome::Conflicts(normalize_conflicts(vec![
                    MergeConflict {
                        path: RepoPath::root(),
                        reason,
                        base: None,
                        ours: None,
                        theirs: None,
                    },
                ])?))
            }
            Err(error) => Err(error),
        }
    }
}

fn normalize_conflicts(conflicts: Vec<MergeConflict>) -> Result<Vec<MergeConflict>> {
    let mut unique = BTreeMap::new();
    for mut conflict in conflicts {
        if !conflict.reason.starts_with("virtual merge base: ") {
            conflict.reason = format!("virtual merge base: {}", conflict.reason);
        }
        unique.insert(canonical(&conflict)?, conflict);
    }
    let mut conflicts: Vec<_> = unique.into_values().collect();
    conflicts.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.reason.cmp(&b.reason)));
    Ok(conflicts)
}
