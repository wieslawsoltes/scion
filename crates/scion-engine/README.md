# scion-engine

Reusable server-side source control over a `scion-store::Storage` backend.
`Repository` creates immutable first-parent changesets, derives persistent trees,
and provides diff, complete-DAG ancestry, merge bases, merge, rebase, revert,
history, blame, copies, path search and bisection without a working copy.

References use an independent compare-and-swap transaction. `DerivedData` allows
applications to register deterministic, versioned recursive indexes. Persistent
directory updates rewrite only the directories on changed paths.

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
