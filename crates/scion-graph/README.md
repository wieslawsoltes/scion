# scion-graph

Storage-independent directed acyclic commit graph algorithms. `GraphAccess`
supplies immutable `GraphNode` records; `build_node` constructs generation and
binary-lifting indexes incrementally. Query functions preserve every merge parent.

The crate provides first-parent jumps, complete ancestry, all maximal merge bases,
topological history, and balanced bisection candidates. Inputs are content-addressed
Scion identifiers, but no database, network, runtime, or working copy is required.
