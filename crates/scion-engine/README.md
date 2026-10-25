# scion-engine

Reusable server-side source control over a `scion-store::Storage` backend.
`Repository` creates immutable first-parent changesets, derives persistent trees,
and provides diff, complete-DAG ancestry, merge bases, merge, rebase, revert,
history, blame, copies, path search and bisection without a working copy.

References use an independent compare-and-swap transaction. `DerivedData` allows
applications to register deterministic, versioned recursive indexes. Persistent
directory updates rewrite only the directories on changed paths.

## Bounded immutable read cache

Repository clones share an 8 MiB cache for validated revision roots, complete
trees, selected directory entries and verified blob contents of at most 64 KiB.
Every retained value competes for the same byte budget and 4,096-entry limit;
blob accounting includes spare allocation capacity. Large blobs are read directly
without retaining another full-size copy. Repeated file reads and text diffs can
reuse verified bytes while repository membership, path lookup and diff permission
filtering still precede content access. Repository never caches mutable references;
reference reads follow the storage backend's live or snapshot semantics.

`with_tree_cache_capacity(bytes)` sets this shared budget, including blobs; zero
disables retention. `tree_cache_stats()` includes all retained kinds, and
`clear_tree_cache()` clears them for all clones sharing the cache. In-progress
readers may continue using their immutable snapshots after eviction or clearing.

Cache misses verify stored object content before retention; failures are never
cached. Cache hits return the previously verified bytes. Physical corruption or
deletion after a successful read becomes visible after eviction or clearing, or
through a direct `Storage::get_object` call. Use direct storage reads for integrity
audits; those reads and commit content validation remain uncached. Published
changes use new content IDs, so cached older revisions do not hide later commits
or reference changes.

Edits can take ownership of a fully cached parent tree to reuse its map and name
allocations. If another reader holds an `Arc<Tree>`, the edit clones it before
changing anything; the reader's snapshot remains immutable. The old tree's cache
entry is evicted even if an edit later fails. Revisiting that older tree may
therefore require a verified storage read. Partial directory lookups remain
usable for their original immutable tree. New tree values are cached only after
their storage write succeeds; values retained in a publication batch wait for
that publication to succeed. Stored parent trees and content IDs never change.
New trees use `Tree::canonical_bytes()` with the established canonical bytes.

## Merge semantics

Merging criss-cross history recursively synthesizes a virtual tree from every
maximal merge base. Canonical ordering and memoized DAG queries make synthesis
deterministic when callers exchange the two merge operands. Virtual trees and
blobs remain ordinary immutable objects; synthesis does not publish artificial
commits, append log entries or move references.

Unresolved content, mode, deletion or file/directory conflicts in a virtual base
are returned through `MergeResult.conflicts`, with a `virtual merge base: ` reason
prefix and `changeset: None`. These conflicts describe the base operands and are
reported even when the requested tips happen to have identical trees. Synthesis
has explicit recursion, head-count, graph-cache and work budgets; exhaustion
returns `Error::Limit` without publishing a changeset. Ordinary single-base merge,
rebase and revert conflict behavior is unchanged.

## Frozen local graph exports

`export_local_graph_snapshot(data_path, repository_name, destination, limits)`
opens a consistent read-only publication view, checks membership and full
changeset hashes, and rebuilds a closed graph with `scion-graph`. The output is
an explicit frozen analysis artifact: it does not change live repository
queries or resolve mutable refs. `GraphSnapshotExportLimits` bounds collected
log entries; the default and maximum are ten million commits. Destination paths
must remain outside the source store and avoid source-file aliases.

The returned metadata includes repository, source identity, publication sequence,
node count, format, and storage size. See the
[frozen graph guide](https://github.com/wieslawsoltes/scion/blob/main/docs/graph-snapshots.md)
for the reusable graph reader, atomic replacement, limits, CLI, and preparation
costs. Local `scn graph-snapshot` commands use this same public exporter.
