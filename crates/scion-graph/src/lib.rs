//! Storage-independent commit DAG algorithms. First-parent jumps accelerate linear
//! traversal without discarding merge edges from general ancestry queries.
use scion_types::{Error, ObjectId, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

const MAX_VISITED: usize = 10_000_000;

/// Immutable derived graph record; `jumps[k]` is the 2^k-th first parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNode {
    pub parents: Vec<ObjectId>,
    pub generation: u64,
    pub first_parent_depth: u64,
    pub jumps: Vec<ObjectId>,
    /// Jumps whose interior contains no merge node. General ancestry can skip
    /// these spans without dropping side-parent edges.
    #[serde(default)]
    pub linear_jumps: Vec<ObjectId>,
}

/// A read interface for graph algorithms, independent of storage implementation.
pub trait GraphAccess {
    fn node(&self, id: ObjectId) -> Result<GraphNode>;
}

/// Derive one node using O(number of parents + log first-parent depth) reads.
pub fn build_node(graph: &impl GraphAccess, parents: &[ObjectId]) -> Result<GraphNode> {
    if parents.iter().collect::<BTreeSet<_>>().len() != parents.len() {
        return Err(Error::Invalid("duplicate graph parents".into()));
    }
    let mut generation = 0;
    for parent in parents {
        generation = generation.max(
            graph
                .node(*parent)?
                .generation
                .checked_add(1)
                .ok_or_else(|| Error::Limit("graph generation overflow".into()))?,
        );
    }
    let mut jumps = Vec::new();
    let mut first_parent_depth = 0;
    if let Some(first) = parents.first() {
        first_parent_depth = graph
            .node(*first)?
            .first_parent_depth
            .checked_add(1)
            .ok_or_else(|| Error::Limit("first-parent depth overflow".into()))?;
        jumps.push(*first);
        let mut level = 0;
        while let Some(ancestor) = graph.node(jumps[level])?.jumps.get(level).copied() {
            jumps.push(ancestor);
            level += 1;
        }
    }
    let mut linear_jumps = Vec::new();
    if parents.len() == 1 {
        linear_jumps.push(parents[0]);
        let mut level = 0;
        while let Some(ancestor) = graph
            .node(linear_jumps[level])?
            .linear_jumps
            .get(level)
            .copied()
        {
            linear_jumps.push(ancestor);
            level += 1;
        }
    }
    Ok(GraphNode {
        parents: parents.to_vec(),
        generation,
        first_parent_depth,
        jumps,
        linear_jumps,
    })
}

/// Return the N-th first parent in logarithmic reads, or None beyond a root.
pub fn nth_first_parent(
    graph: &impl GraphAccess,
    mut id: ObjectId,
    mut n: u64,
) -> Result<Option<ObjectId>> {
    if n > graph.node(id)?.first_parent_depth {
        return Ok(None);
    }
    while n > 0 {
        let level = n.trailing_zeros() as usize;
        let node = graph.node(id)?;
        let Some(next) = node.jumps.get(level) else {
            return Err(Error::Corrupt("incomplete first-parent jump table".into()));
        };
        id = *next;
        n &= n - 1;
    }
    Ok(Some(id))
}

/// Test reachability through all parents. Generations prune impossible branches.
pub fn is_ancestor(
    graph: &impl GraphAccess,
    ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<bool> {
    let target = graph.node(ancestor)?;
    let start = graph.node(descendant)?;
    if ancestor == descendant {
        return Ok(true);
    }
    if target.generation >= start.generation {
        return Ok(false);
    }
    if start.first_parent_depth >= target.first_parent_depth
        && nth_first_parent(
            graph,
            descendant,
            start.first_parent_depth - target.first_parent_depth,
        )? == Some(ancestor)
    {
        return Ok(true);
    }
    let mut pending = BinaryHeap::from([(start.generation, descendant)]);
    let mut seen = BTreeSet::new();
    while let Some((generation, id)) = pending.pop() {
        if generation < target.generation {
            break;
        }
        if id == ancestor {
            return Ok(true);
        }
        if !seen.insert(id) {
            continue;
        }
        if seen.len() > MAX_VISITED {
            return Err(Error::Limit("graph traversal budget exceeded".into()));
        }
        if generation == target.generation {
            continue;
        }
        let node = graph.node(id)?;
        if !node.linear_jumps.is_empty() {
            let distance = generation - target.generation;
            let level = (63 - distance.leading_zeros() as usize).min(node.linear_jumps.len() - 1);
            pending.push((generation - (1_u64 << level), node.linear_jumps[level]));
        } else {
            for parent in node.parents {
                let node = graph.node(parent)?;
                if node.generation >= target.generation {
                    pending.push((node.generation, parent));
                }
            }
        }
    }
    Ok(false)
}

/// Visit ancestry in descending generation; ties are deterministic by object ID.
pub fn ancestors(graph: &impl GraphAccess, start: ObjectId, limit: usize) -> Result<Vec<ObjectId>> {
    let mut queue = BinaryHeap::from([(graph.node(start)?.generation, start)]);
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    while let Some((_, id)) = queue.pop() {
        if !seen.insert(id) {
            continue;
        }
        if result.len() >= limit {
            break;
        }
        if seen.len() > MAX_VISITED {
            return Err(Error::Limit("graph traversal budget exceeded".into()));
        }
        result.push(id);
        for parent in graph.node(id)?.parents {
            queue.push((graph.node(parent)?.generation, parent));
        }
    }
    Ok(result)
}

/// All maximal common ancestors, including multiple criss-cross merge bases.
pub fn merge_bases(graph: &impl GraphAccess, a: ObjectId, b: ObjectId) -> Result<Vec<ObjectId>> {
    let left: BTreeSet<_> = ancestors(graph, a, MAX_VISITED + 1)?.into_iter().collect();
    let common: Vec<_> = ancestors(graph, b, MAX_VISITED + 1)?
        .into_iter()
        .filter(|id| left.contains(id))
        .collect();
    let mut bases = Vec::new();
    for candidate in common {
        let mut dominated = false;
        for base in &bases {
            if is_ancestor(graph, candidate, *base)? {
                dominated = true;
                break;
            }
        }
        if !dominated {
            bases.push(candidate);
        }
    }
    bases.sort();
    Ok(bases)
}

/// Choose a commit which most evenly divides the unknown good..bad DAG.
/// Skipped revisions remain part of the search space but cannot be selected.
pub fn bisect_candidate(
    graph: &impl GraphAccess,
    good: ObjectId,
    bad: ObjectId,
    skipped: &[ObjectId],
) -> Result<Option<ObjectId>> {
    if !is_ancestor(graph, good, bad)? {
        return Err(Error::Invalid("good must be an ancestor of bad".into()));
    }
    let known_good: BTreeSet<_> = ancestors(graph, good, MAX_VISITED + 1)?
        .into_iter()
        .collect();
    let unknown: BTreeSet<_> = ancestors(graph, bad, MAX_VISITED + 1)?
        .into_iter()
        .filter(|id| !known_good.contains(id))
        .collect();
    let skipped: BTreeSet<_> = skipped.iter().copied().collect();
    let mut counts = BTreeMap::new();
    for candidate in &unknown {
        if *candidate == bad || skipped.contains(candidate) {
            continue;
        }
        let count = ancestors(graph, *candidate, MAX_VISITED + 1)?
            .into_iter()
            .filter(|id| unknown.contains(id))
            .count();
        counts.insert(*candidate, count.min(unknown.len().saturating_sub(count)));
    }
    Ok(counts
        .into_iter()
        .max_by_key(|(id, balance)| (*balance, *id))
        .map(|(id, _)| id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_types::ObjectKind;
    #[derive(Default)]
    struct Graph(BTreeMap<ObjectId, GraphNode>);
    impl GraphAccess for Graph {
        fn node(&self, id: ObjectId) -> Result<GraphNode> {
            self.0
                .get(&id)
                .cloned()
                .ok_or_else(|| Error::NotFound(id.to_string()))
        }
    }
    impl Graph {
        fn add(&mut self, name: &str, parents: &[ObjectId]) -> ObjectId {
            let id = ObjectId::hash(ObjectKind::Changeset, name.as_bytes());
            self.0.insert(id, build_node(self, parents).unwrap());
            id
        }
    }
    #[test]
    fn binary_lifting_and_second_parent_reachability() {
        let mut g = Graph::default();
        let root = g.add("root", &[]);
        let mut tip = root;
        for i in 1..=1024 {
            tip = g.add(&format!("n{i}"), &[tip]);
        }
        assert_eq!(nth_first_parent(&g, tip, 1024).unwrap(), Some(root));
        assert_eq!(nth_first_parent(&g, tip, 1025).unwrap(), None);
        let side = g.add("side", &[root]);
        let merged = g.add("merged", &[tip, side]);
        assert!(is_ancestor(&g, side, merged).unwrap());
        assert!(!is_ancestor(&g, side, tip).unwrap());
        struct Measured<'a> {
            graph: &'a Graph,
            reads: std::cell::Cell<usize>,
        }
        impl GraphAccess for Measured<'_> {
            fn node(&self, id: ObjectId) -> Result<GraphNode> {
                self.reads.set(self.reads.get() + 1);
                self.graph.node(id)
            }
        }
        let measured = Measured {
            graph: &g,
            reads: std::cell::Cell::new(0),
        };
        assert!(!is_ancestor(&measured, side, tip).unwrap());
        assert!(
            measured.reads.get() < 100,
            "linear negative ancestry read {} nodes",
            measured.reads.get()
        );
    }
    #[test]
    fn criss_cross_returns_all_maximal_bases() {
        let mut g = Graph::default();
        let root = g.add("root", &[]);
        let a = g.add("a", &[root]);
        let b = g.add("b", &[root]);
        let x = g.add("x", &[a, b]);
        let y = g.add("y", &[b, a]);
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(merge_bases(&g, x, y).unwrap(), expected);
        let unrelated = g.add("u", &[]);
        assert!(merge_bases(&g, x, unrelated).unwrap().is_empty());
    }
    #[test]
    fn bisect_chooses_middle() {
        let mut g = Graph::default();
        let a = g.add("a", &[]);
        let b = g.add("b", &[a]);
        let c = g.add("c", &[b]);
        let d = g.add("d", &[c]);
        let e = g.add("e", &[d]);
        assert_eq!(bisect_candidate(&g, a, e, &[]).unwrap(), Some(c));
    }
}
