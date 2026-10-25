# Implementation map

The repository implements the public Scion architectural themes as thirteen
independently reusable Rust product crates. [Public design and coverage](public-design.md)
is the maintained requirement matrix; [architecture](architecture.md) defines the
independent formats, algorithms, and storage contracts.

| Area | Implementation |
|---|---|
| Immutable model | Canonical first-parent changesets, validated paths, BLAKE3 IDs, compressed envelopes |
| Durable publication | Membership/log publication independent of ref CAS; bounded prepared-object batches |
| Persistence | SQLite, PostgreSQL, Spanner metadata, hash-partitioned objects, S3/GCS/Azure adapters |
| Repository engine | Incremental directory trees; generic eager/lazy derived indexes; history, blame, copies, and path search |
| Graph and mutations | Skip indexes, maximal merge bases, recursive virtual bases, merge, rebase, revert, and bisect |
| Service | Typed checkout-free APIs, repository/directory authorization, identity federation, policy reload, bounds and telemetry |
| Client | Sparse/lazy checkouts, verified status caching, journaled recovery, writable overlays, Linux FUSE, notifications |
| Cache infrastructure | Identical immutable-object RPCs, strict daemon/cache/server tiering, coalescing, circuit breakers, snapshots |
| Interoperability | Streaming Git import/export, durable mappings, supported modes and SHA-256 Git repositories |
| Distribution | Cross-platform checks, fuzzing, real backend/filesystem integration, all-crate packaging, native releases |

[Commands](commands.md), [operations](operations.md), [identity](identity.md),
[cloud storage](cloud-storage.md), [Spanner](spanner.md), and
[working copies](working-copies.md) describe runnable configurations and workflows.
[Testing](testing.md) records concrete verification; [the Git comparison](git-comparison.md)
retains measurements and remaining performance limits.

The source article is an architectural description rather than a published
protocol specification. This project defines its own formats and API. Remaining
platform and production boundaries are documented in the
[coverage guide](public-design.md#deliberate-boundaries); they must not be hidden
behind a claim of private upstream compatibility or unmeasured fleet performance.
