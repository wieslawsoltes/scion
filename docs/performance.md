# Performance and scaling

Scion's performance contract is measurable work proportional to the changed
directory paths and requested objects. The implementation does not copy a flat
repository manifest for each commit. These measurements describe this independent
implementation; they are not measurements of Anthropic's service or predictions
of fleet throughput.

For disk-durable measurements against Git, see the separate [Git comparison](git-comparison.md). The historical in-memory results below isolate algorithm and object-encoding costs and are retained as a baseline; they are not a substitute for the durable comparison or a claim that the latest implementation has these exact timings.

## Reproduce

```sh
cargo bench -p scion-engine --bench scaling
# Smaller development profile:
cargo bench -p scion-engine --bench scaling -- --quick
```

The benchmark is a standalone Rust harness with no benchmarking-service dependency.
It reports CSV rows containing operation, scale, sample count, p50 and p95 latency
in microseconds. The source is
[`crates/scion-engine/benches/scaling.rs`](../crates/scion-engine/benches/scaling.rs).

## Observed results

Run on October 25, 2026, in a shared Linux x86_64 development container using
`rustc 1.99.1 (58cf8f9c3 2026-10-13)`, the workspace's optimized bench profile,
and the compressed `MemoryStore` backend. Each source file contains 14 bytes and
directories contain 100 files. This is a single-process algorithm/storage benchmark;
it includes hashing, serialization and compression but excludes network latency,
disk durability, PostgreSQL, concurrent load, operating-system filesystem mounts,
and cache warming across machines. Hardware and host load were not controlled.

| Operation | Repository or graph size | Samples | p50 | p95 |
| --- | ---: | ---: | ---: | ---: |
| One-file commit | 1,000 files | 50 | 358.109 µs | 408.995 µs |
| One-file commit | 10,000 files | 50 | 527.103 µs | 588.366 µs |
| One-file commit | 100,000 files | 50 | 2,040.171 µs | 2,357.999 µs |
| One-file diff | 1,000 files | 50 | 269.847 µs | 321.775 µs |
| One-file diff | 10,000 files | 50 | 420.042 µs | 480.884 µs |
| One-file diff | 100,000 files | 50 | 1,832.959 µs | 2,093.412 µs |
| Positive ancestry | 10,000 commits | 100 | 52.218 µs | 68.043 µs |
| Negative ancestry | 10,000 commits | 100 | 111.487 µs | 170.647 µs |

Initial commits were measured once each: 11.615 ms for 1,000 files, 111.529 ms
for 10,000 files, and 1,158.708 ms for 100,000 files. A single observation does
not establish a latency distribution.

The one-file commit does not scale linearly with total file count. It still gets
slower as the immediate root-directory fanout grows. A tree object contains the
complete entries of one directory; updating it must read and serialize those
immediate entries. In this benchmark the root grows from 10 to 1,000 directories.
The current bound is therefore the number and fanout of affected directory nodes,
plus changed content size. Extremely wide directories remain a tuning target.

## Deterministic complexity regressions

Timing varies by machine. Tests additionally assert structural work bounds:

- A one-file edit in a repository with 1,000 unrelated top-level directories
  performs exactly **two tree reads and two tree writes**: its containing directory
  and the root. Unrelated subtrees are reused by content ID.
- Negative ancestry across 1,024 linear commits requires fewer than 100 graph
  reads, using skip spans that contain no interior merge node.
- Two children above 1,024 shared commits find their merge base in fewer than
  20 graph reads. Frontier painting stops at common nodes instead of reading the
  entire shared ancestry.
- Filtered diffs read **zero denied file blobs** when all changes fall outside
  authorized paths.
- Concurrent requests for the same derived index/revision compute the index
  once within a shared `Repository` instance; later requests use immutable cached
  results.

Run these checks with:

```sh
cargo test -p scion-engine -p scion-graph
```

## Algorithm and resource limits

| Subsystem | Current behavior |
| --- | --- |
| Changesets | Validate changed blobs; create a canonical delta; publish immutable content and the log. Prepared publication can bundle an explicit reference CAS; ordinary commit creation leaves refs unchanged. |
| Trees | Batch path-copy updates; content-identical subtrees are shared. Directory fanout contributes to node cost. |
| First-parent history | Binary lifting provides logarithmic jumps. |
| General ancestry | Generation pruning and linear skip spans retain every merge-parent edge; positive first-parent proofs use binary lifting. |
| Merge bases | Paired linear-span skips and descending-generation frontier painting; returns all maximal bases. |
| Merge/diff | Traverse changed subtrees; merge only changed-path maps. No full flattened manifest is required. |
| Path history | Persistent path trie and linked records skip unrelated changes; deletion records remain queryable. |
| Copy lookup | Persistent 256-way content-hash trie, with only changed paths updated. |
| Derived indexes | Versioned immutable memoization; iterative dependency evaluation; up to 1,024 coalesced in-flight keys and one million visited revisions per evaluation. |
| Listing | Exclusive-path pagination; skips subtrees entirely before the cursor; at most 10,000 files per page. |
| Text patches | Files larger than 4 MiB and binary/non-UTF-8 files return metadata with no text patch. |
| Automatic text merge | Concurrent changes larger than 16 MiB return a structured conflict; binary, symbolic-link and submodule conflicts require explicit resolution. |
| Bisection | Loads graph records once; scores every eligible candidate for small ranges and 65 spaced candidates for ranges above 512 eligible revisions. |

The local backend batches small objects and metadata into durable SQLite publication transactions and serializes writers. Shared deployments use PostgreSQL or Spanner and can partition immutable objects with `ShardedStore` or use S3/GCS/Azure adapters.
No benchmark here establishes shared-database throughput, replication behavior, cluster
availability, regional-cache hit rates or production SLOs. Those require a workload
and deployment-specific load and failure test.

## Deployment measurements

Before selecting capacity, measure representative blob sizes and compressibility,
directory fanout, commit fanout, concurrent branch structure, sparse checkout paths,
and cold/warm cache behavior. Capture server latency, object bytes, cache hits and
misses, upstream errors, admission rejection, database transaction latency and disk
space. Exercise a regional cache outage to verify that clients do not bypass the
configured cache tier and overload the authority.
