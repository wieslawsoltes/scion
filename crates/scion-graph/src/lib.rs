//! Storage-independent commit DAG algorithms. First-parent jumps accelerate linear
//! traversal without discarding merge edges from general ancestry queries.
use scion_types::{Error, ObjectId, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::rc::Rc;

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
    // The first-parent and linear jump tables often consult exactly the same
    // ancestors. Read each immutable record once rather than repeating backend
    // membership checks, database round trips and decoding for each table.
    let mut records = BTreeMap::<ObjectId, Rc<GraphNode>>::new();
    let mut read = |id| -> Result<Rc<GraphNode>> {
        match records.entry(id) {
            std::collections::btree_map::Entry::Occupied(entry) => Ok(entry.get().clone()),
            std::collections::btree_map::Entry::Vacant(entry) => {
                Ok(entry.insert(Rc::new(graph.node(id)?)).clone())
            }
        }
    };
    let mut generation = 0;
    for parent in parents {
        generation = generation.max(
            read(*parent)?
                .generation
                .checked_add(1)
                .ok_or_else(|| Error::Limit("graph generation overflow".into()))?,
        );
    }
    let mut jumps = Vec::new();
    let mut first_parent_depth = 0;
    if let Some(first) = parents.first() {
        first_parent_depth = read(*first)?
            .first_parent_depth
            .checked_add(1)
            .ok_or_else(|| Error::Limit("first-parent depth overflow".into()))?;
        jumps.push(*first);
        let mut level = 0;
        while let Some(ancestor) = read(jumps[level])?.jumps.get(level).copied() {
            jumps.push(ancestor);
            level += 1;
        }
    }
    let mut linear_jumps = Vec::new();
    if parents.len() == 1 {
        linear_jumps.push(parents[0]);
        let mut level = 0;
        while let Some(ancestor) = read(linear_jumps[level])?.linear_jumps.get(level).copied() {
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
    id: ObjectId,
    n: u64,
) -> Result<Option<ObjectId>> {
    nth_first_parent_from(graph, id, &graph.node(id)?, n)
}

/// Continue from a record the caller already loaded. The jump destination is
/// only loaded if another jump is required; neither endpoint is decoded twice.
fn nth_first_parent_from(
    graph: &impl GraphAccess,
    id: ObjectId,
    node: &GraphNode,
    mut n: u64,
) -> Result<Option<ObjectId>> {
    if n > node.first_parent_depth {
        return Ok(None);
    }
    if n == 0 {
        return Ok(Some(id));
    }
    let level = n.trailing_zeros() as usize;
    let mut id = *node
        .jumps
        .get(level)
        .ok_or_else(|| Error::Corrupt("incomplete first-parent jump table".into()))?;
    n &= n - 1;
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
    if ancestor == descendant {
        return Ok(true);
    }
    let mut descendant = descendant;
    let mut start = graph.node(descendant)?;
    if target.generation >= start.generation {
        return Ok(false);
    }
    // A single-parent span has no alternative path to explore. Follow it once,
    // by generation, and stop as soon as its known destination generation rules
    // the target in or out. In particular a negative linear query must not
    // traverse the same prefix speculatively through first-parent jumps and then
    // again through the all-parent search.
    let mut prefix_visits = 0;
    while !start.linear_jumps.is_empty() {
        prefix_visits += 1;
        if prefix_visits > MAX_VISITED {
            return Err(Error::Limit("graph traversal budget exceeded".into()));
        }
        let distance = start.generation - target.generation;
        let level = (63 - distance.leading_zeros() as usize).min(start.linear_jumps.len() - 1);
        let generation = start.generation - (1_u64 << level);
        descendant = start.linear_jumps[level];
        if descendant == ancestor {
            return Ok(true);
        }
        if generation == target.generation {
            return Ok(false);
        }
        start = graph.node(descendant)?;
        if start.generation != generation {
            return Err(Error::Corrupt("inconsistent linear jump generation".into()));
        }
    }
    if start.first_parent_depth >= target.first_parent_depth
        && nth_first_parent_from(
            graph,
            descendant,
            &start,
            start.first_parent_depth - target.first_parent_depth,
        )? == Some(ancestor)
    {
        return Ok(true);
    }
    let mut pending = BinaryHeap::from([(start.generation, descendant)]);
    let mut initial = Some(start);
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
        if prefix_visits + seen.len() > MAX_VISITED {
            return Err(Error::Limit("graph traversal budget exceeded".into()));
        }
        if generation == target.generation {
            continue;
        }
        let node = if id == descendant {
            initial
                .take()
                .expect("the initial frontier is visited once")
        } else {
            graph.node(id)?
        };
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
    let (mut a, mut b) = (a, b);
    let mut a_node = graph.node(a)?;
    if a == b {
        return Ok(vec![a]);
    }
    let mut b_node = graph.node(b)?;
    // An endpoint on the other endpoint's first-parent chain is necessarily
    // their unique maximal common ancestor. Prove that in logarithmic reads
    // before painting what could be millions of intervening linear commits.
    if a_node.generation < b_node.generation
        && a_node.first_parent_depth < b_node.first_parent_depth
        && nth_first_parent_from(
            graph,
            b,
            &b_node,
            b_node.first_parent_depth - a_node.first_parent_depth,
        )? == Some(a)
    {
        return Ok(vec![a]);
    }
    if b_node.generation < a_node.generation
        && b_node.first_parent_depth < a_node.first_parent_depth
        && nth_first_parent_from(
            graph,
            a,
            &a_node,
            a_node.first_parent_depth - b_node.first_parent_depth,
        )? == Some(b)
    {
        return Ok(vec![b]);
    }
    // Remove provably disjoint, single-parent prefixes before painting the DAG.
    // At different generations, discarded nodes are newer than the other head
    // and cannot be its ancestors. At equal generations, two merge-safe spans
    // ending at different nodes cannot meet internally: a unique-parent chain
    // cannot split again after meeting. Thus every reduction preserves the
    // complete common-ancestor set, including second-parent/criss-cross bases.
    // The optimization stops at merge nodes; the full DAG algorithm below then
    // sees all their parents. A bounded loop also handles partial jump indexes.
    for _ in 0..256 {
        if a == b {
            return Ok(vec![a]);
        }
        match a_node.generation.cmp(&b_node.generation) {
            std::cmp::Ordering::Greater => {
                if a_node.linear_jumps.is_empty() {
                    break;
                }
                let distance = a_node.generation - b_node.generation;
                let level =
                    (63 - distance.leading_zeros() as usize).min(a_node.linear_jumps.len() - 1);
                a = a_node.linear_jumps[level];
                if a == b {
                    return Ok(vec![a]);
                }
                a_node = graph.node(a)?;
            }
            std::cmp::Ordering::Less => {
                if b_node.linear_jumps.is_empty() {
                    break;
                }
                let distance = b_node.generation - a_node.generation;
                let level =
                    (63 - distance.leading_zeros() as usize).min(b_node.linear_jumps.len() - 1);
                b = b_node.linear_jumps[level];
                if a == b {
                    return Ok(vec![a]);
                }
                b_node = graph.node(b)?;
            }
            std::cmp::Ordering::Equal => {
                let levels = a_node.linear_jumps.len().min(b_node.linear_jumps.len());
                if levels == 0 {
                    break;
                }
                if a_node.linear_jumps[0] == b_node.linear_jumps[0] {
                    return Ok(vec![a_node.linear_jumps[0]]);
                }
                let level = (0..levels)
                    .rev()
                    .find(|level| a_node.linear_jumps[*level] != b_node.linear_jumps[*level])
                    .unwrap_or(0);
                a = a_node.linear_jumps[level];
                b = b_node.linear_jumps[level];
                a_node = graph.node(a)?;
                b_node = graph.node(b)?;
            }
        }
    }
    // Paint both frontiers in descending generation. Once a node is common,
    // its ancestors cannot be maximal and need not be expanded along that route.
    // This avoids reading the entire shared history for every nearby merge.
    let mut colors = BTreeMap::new();
    colors.insert(a, 1_u8);
    *colors.entry(b).or_default() |= 2;
    let mut pending = BinaryHeap::from([(a_node.generation, a), (b_node.generation, b)]);
    let mut visited = BTreeSet::new();
    let mut common = Vec::new();
    while let Some((_, id)) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        if visited.len() > MAX_VISITED {
            return Err(Error::Limit("merge-base traversal budget exceeded".into()));
        }
        let color = colors[&id];
        if color == 3 {
            common.push(id);
            continue;
        }
        for parent in graph.node(id)?.parents {
            *colors.entry(parent).or_default() |= color;
            pending.push((graph.node(parent)?.generation, parent));
        }
    }
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

/// Choose a balanced commit in the unknown good..bad DAG. Small ranges evaluate
/// every candidate; large ranges score 65 evenly spaced candidates to bound work.
/// Graph records are loaded once, never once per candidate. Skipped revisions
/// remain in the search space but cannot be selected.
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
    let mut unknown: Vec<_> = ancestors(graph, bad, MAX_VISITED + 1)?
        .into_iter()
        .filter(|id| !known_good.contains(id))
        .collect();
    unknown.reverse();
    let positions: BTreeMap<_, _> = unknown.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let links = unknown
        .iter()
        .map(|id| {
            Ok(graph
                .node(*id)?
                .parents
                .into_iter()
                .filter_map(|parent| positions.get(&parent).copied())
                .collect::<Vec<_>>())
        })
        .collect::<Result<Vec<_>>>()?;
    let skipped: BTreeSet<_> = skipped.iter().copied().collect();
    let eligible: Vec<_> = unknown
        .iter()
        .enumerate()
        .filter(|(_, id)| **id != bad && !skipped.contains(id))
        .map(|(i, _)| i)
        .collect();
    if eligible.is_empty() {
        return Ok(None);
    }
    let candidates = if eligible.len() <= 512 {
        eligible
    } else {
        (0..=64)
            .map(|i| eligible[i * (eligible.len() - 1) / 64])
            .collect()
    };
    let mut marks = vec![0_usize; unknown.len()];
    let mut best = None;
    for (iteration, candidate) in candidates.into_iter().enumerate() {
        let mark = iteration + 1;
        let mut queue = vec![candidate];
        let mut count = 0;
        while let Some(index) = queue.pop() {
            if marks[index] == mark {
                continue;
            }
            marks[index] = mark;
            count += 1;
            queue.extend(links[index].iter().copied());
        }
        let score = (count.min(unknown.len() - count), unknown[candidate]);
        if best.is_none_or(|current| score > current) {
            best = Some(score);
        }
    }
    Ok(best.map(|(_, id)| id))
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
        let left = g.add("left", &[tip]);
        let right = g.add("right", &[tip]);
        let measured = Measured {
            graph: &g,
            reads: std::cell::Cell::new(0),
        };
        assert_eq!(merge_bases(&measured, left, right).unwrap(), vec![tip]);
        assert!(
            measured.reads.get() < 20,
            "merge base scanned common history"
        );
        measured.reads.set(0);
        assert_eq!(merge_bases(&measured, root, tip).unwrap(), vec![root]);
        assert!(
            measured.reads.get() < 20,
            "ancestor merge base scanned a linear chain"
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
    fn divergent_linear_arms_use_bounded_reads_and_stop_at_merge_edges() {
        let mut graph = Graph::default();
        let root = graph.add("root", &[]);
        let a = graph.add("a", &[root]);
        let b = graph.add("b", &[root]);
        let x = graph.add("x", &[a, b]);
        let y = graph.add("y", &[b, a]);
        let mut left = x;
        let mut right = x;
        let mut cross = y;
        for i in 0..1024 {
            left = graph.add(&format!("left{i}"), &[left]);
            right = graph.add(&format!("right{i}"), &[right]);
            cross = graph.add(&format!("cross{i}"), &[cross]);
        }
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
            graph: &graph,
            reads: std::cell::Cell::new(0),
        };
        assert_eq!(merge_bases(&measured, left, right).unwrap(), vec![x]);
        assert!(
            measured.reads.get() < 100,
            "linear fork read {} nodes",
            measured.reads.get()
        );
        measured.reads.set(0);
        let mut bases = vec![a, b];
        bases.sort_unstable();
        assert_eq!(merge_bases(&measured, left, cross).unwrap(), bases);
        assert!(
            measured.reads.get() < 100,
            "criss-cross prefixes read {} nodes",
            measured.reads.get()
        );
        let shorter = nth_first_parent(&graph, right, 537).unwrap().unwrap();
        measured.reads.set(0);
        assert_eq!(merge_bases(&measured, left, shorter).unwrap(), vec![x]);
        assert!(
            measured.reads.get() < 100,
            "unequal fork read {} nodes",
            measured.reads.get()
        );
    }
    #[test]
    fn building_jump_tables_reads_each_backend_record_once() {
        let mut graph = Graph::default();
        let root = graph.add("root", &[]);
        let mut tip = root;
        for i in 0..1024 {
            tip = graph.add(&format!("linear{i}"), &[tip]);
        }
        struct Measured<'a> {
            graph: &'a Graph,
            reads: std::cell::RefCell<BTreeMap<ObjectId, usize>>,
        }
        impl GraphAccess for Measured<'_> {
            fn node(&self, id: ObjectId) -> Result<GraphNode> {
                *self.reads.borrow_mut().entry(id).or_default() += 1;
                self.graph.node(id)
            }
        }
        let measured = Measured {
            graph: &graph,
            reads: Default::default(),
        };
        let node = build_node(&measured, &[tip]).unwrap();
        assert_eq!(node.first_parent_depth, 1025);
        assert_eq!(node.jumps, node.linear_jumps);
        let reads = measured.reads.borrow();
        assert!(
            reads.len() <= 11,
            "building one node scanned too much history"
        );
        assert!(
            reads.values().all(|reads| *reads == 1),
            "duplicate backend reads: {reads:?}"
        );
    }
    #[test]
    fn linear_reachability_never_reads_a_record_twice() {
        let mut graph = Graph::default();
        let root = graph.add("root", &[]);
        let first = graph.add("first", &[root]);
        let side = graph.add("side", &[root]);
        let mut tip = first;
        for i in 0..1023 {
            tip = graph.add(&format!("linear{i}"), &[tip]);
        }
        struct Measured<'a> {
            graph: &'a Graph,
            reads: std::cell::RefCell<BTreeMap<ObjectId, usize>>,
        }
        impl GraphAccess for Measured<'_> {
            fn node(&self, id: ObjectId) -> Result<GraphNode> {
                *self.reads.borrow_mut().entry(id).or_default() += 1;
                self.graph.node(id)
            }
        }
        let measured = Measured {
            graph: &graph,
            reads: Default::default(),
        };
        for (ancestor, expected) in [(first, true), (side, false)] {
            assert_eq!(is_ancestor(&measured, ancestor, tip).unwrap(), expected);
            let reads = measured.reads.borrow();
            // 1023 = 2^10 - 1: ten jumps plus the separately validated target.
            assert!(reads.len() <= 11, "unnecessary graph reads: {reads:?}");
            assert!(reads.values().all(|count| *count == 1));
            drop(reads);
            measured.reads.borrow_mut().clear();
        }
        assert!(is_ancestor(&measured, tip, tip).unwrap());
        assert_eq!(measured.reads.borrow().get(&tip), Some(&1));
        measured.reads.borrow_mut().clear();
        assert_eq!(nth_first_parent(&measured, tip, 1023).unwrap(), Some(first));
        let reads = measured.reads.borrow();
        assert_eq!(reads.len(), 10);
        assert!(reads.values().all(|count| *count == 1));
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

    #[test]
    fn optimized_dag_queries_match_an_exhaustive_reachability_oracle() {
        let mut graph = Graph::default();
        let mut ids = Vec::new();
        let mut reach = BTreeMap::<ObjectId, BTreeSet<ObjectId>>::new();
        let mut seed = 0x1234_5678_u64;
        let mut random = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            seed
        };
        for i in 0..128 {
            let count = if i == 0 { 0 } else { (random() % 4) as usize };
            let mut parents = BTreeSet::new();
            for _ in 0..count {
                parents.insert(ids[(random() as usize) % ids.len()]);
            }
            let parents = parents.into_iter().collect::<Vec<_>>();
            let id = graph.add(&format!("random{i}"), &parents);
            let mut ancestors = BTreeSet::from([id]);
            for parent in parents {
                ancestors.extend(reach[&parent].iter().copied());
            }
            reach.insert(id, ancestors);
            ids.push(id);
        }
        for &a in &ids {
            for &b in &ids {
                assert_eq!(is_ancestor(&graph, a, b).unwrap(), reach[&b].contains(&a));
                let common = reach[&a]
                    .intersection(&reach[&b])
                    .copied()
                    .collect::<Vec<_>>();
                let expected = common
                    .iter()
                    .filter(|candidate| {
                        !common
                            .iter()
                            .any(|other| other != *candidate && reach[other].contains(*candidate))
                    })
                    .copied()
                    .collect::<Vec<_>>();
                assert_eq!(merge_bases(&graph, a, b).unwrap(), expected);
            }
        }
    }
}
