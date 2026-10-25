# scion-engine

Reusable server-side source control over a `scion-store::Storage` backend.
`Repository` creates immutable first-parent changesets, derives persistent trees,
and provides diff, complete-DAG ancestry, merge bases, merge, rebase, revert,
history, blame, copies, path search and bisection without a working copy.

References use an independent compare-and-swap transaction. `DerivedData` allows
applications to register deterministic, versioned recursive indexes. Persistent
directory updates rewrite only the directories on changed paths.
