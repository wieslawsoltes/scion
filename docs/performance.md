# Performance and scaling

Scion's performance contract is measurable work proportional to the changed
directory paths and requested objects. The implementation does not copy a flat
repository manifest for each commit. These measurements describe this independent
implementation; they are not measurements of Anthropic's service or predictions
of fleet throughput.

The paired in-memory comparison below measures clean pre-optimization and
optimized builds on the unchanged original workload. All eleven rows exceed
2× in the observed ratio of median trial p50 values. For disk-durable
measurements against Git, see the separate [Git comparison](git-comparison.md).
Earlier standalone observations remain below as historical evidence.

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

## Paired comparison protocol

`scripts/compare_scaling.py` compares prebuilt copies of the original full Rust
workload. It pins the benchmark source hash, requires clean Git checkouts,
checks executable hashes, validates all eleven operation/scale/sample-count
rows, and alternates baseline/candidate order across five trials by default.
It records complete output and source identities for every trial. Optional
`--baseline-build-report` and `--candidate-build-report` inputs bind recorded
build commands, flags and toolchains to those identities; without them, build
provenance is explicitly unknown.

```sh
# Build in separate clean source trees and separate Cargo target directories:
CARGO_TARGET_DIR=/tmp/scion-baseline-target \
  cargo bench --locked --offline -p scion-engine --bench scaling --no-run
# Repeat in the candidate checkout with a different target directory, then:
python3 scripts/compare_scaling.py \
  --baseline-root /path/to/baseline --baseline-driver /path/to/baseline/scaling \
  --candidate-root /path/to/candidate --candidate-driver /path/to/candidate/scaling \
  --trials 5 --output target/scaling-comparison.json
```

Ratios divide the baseline median of per-trial p50 values by the candidate
median of per-trial p50 values. Individual operation samples are not available
from the original Rust harness and are not pooled or reconstructed. Each
initial-commit row is one observation per trial. Both build-provenance records
are embedded in the recorded paired reports below.

### Regression exposed by the first paired run

Clean candidate `f48eb8496717d4010d12add81de306c95238f54f` was compared with clean
pre-optimization source `6a11ff76052d10951e9f7d029f0313de7c78101d` in five
alternating trials. Initial commits improved 5.759–6.520×, one-file commits
2.275–2.907×, and ancestry 24.436–33.329×. However, the diff rows exposed a real
cache interaction:

| One-file diff scale | Baseline median of trial p50 | Candidate median of trial p50 | Baseline / candidate |
|---|---:|---:|---:|
| 1,000 files | 24.787 µs | 23.545 µs | 1.053× |
| 10,000 files | 25.349 µs | 37.286 µs | 0.680× |
| 100,000 files | 39.189 µs | 127.401 µs | 0.308× |

Taking ownership of a cached tree evicted the old complete directory, while its
revision-root proof remained cached. A subsequent diff reloaded the old directory
instead of using its small changed-path information. The follow-up retains only
affected original entries under the old immutable tree ID before mutation.
Operation-count regressions require zero old-tree reads for affected-path
inspection and forward/reverse adjacent diffs, while preserving held readers,
failed-publication behavior and bounded retention. The Rust benchmark workload
is unchanged.

The [complete first paired report](benchmarks/scaling-2026-10-25-before-diff-fix.json)
retains all measurements and both build identities. The same candidate's
[first durable default-profile report](benchmarks/git-comparison-2026-10-25-resident-first-default.json)
also remains available, including controls and rows below the 2× target. Later
measurements of the published fix are recorded in the next section; the earlier
reports remain unchanged.

## Paired results after the diff fix

On October 25, 2026, clean candidate
`991fdc9c36190104019840110c69a689edc9e5a1` was compared with clean preserved
pre-optimization source `6a11ff76052d10951e9f7d029f0313de7c78101d`.
The [complete optimized paired report](benchmarks/scaling-2026-10-25-optimized.json)
contains all five trials, their execution order, original stdout and stderr,
parsed p50/p95 rows, and both verified build-provenance records.

Both executables used `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, `cargo 1.99.1`,
the workspace's optimized bench profile, and
`cargo bench --locked --offline -p scion-engine --bench scaling --no-run`.
They were built in separate target directories with `CARGO_INCREMENTAL=0`.
The recorded source trees were clean before and after building and measuring;
source commits, tree/archive hashes, executable hashes, build commands and
environments are retained in the report. The unchanged Rust benchmark source
has SHA-256
`a3aabcb63f820fe2ac566c7f9b36856ec45612cda2c3604e22986ccc3ec93a00`.

Each trial ran the complete original harness in a fresh process for each build.
Order alternated baseline/candidate and candidate/baseline. No `--quick` profile,
sample removal, workload edits or discarded trials were used. The harness
creates 14-byte source files in directories of 100 files, measures 50 edits and
diffs at each repository size, and runs each ancestry query 100 times against a
10,000-commit chain.

| Operation | Repository or graph size | Samples per trial | Baseline median of trial p50 (µs) | Candidate median of trial p50 (µs) | Baseline / candidate |
| --- | ---: | ---: | ---: | ---: | ---: |
| Initial commit | 1,000 files | 1 | 10,810.965 | 1,816.514 | 5.951× |
| One-file commit | 1,000 files | 50 | 157.517 | 57.406 | 2.744× |
| One-file diff | 1,000 files | 50 | 24.196 | 5.959 | 4.060× |
| Initial commit | 10,000 files | 1 | 100,669.522 | 17,194.423 | 5.855× |
| One-file commit | 10,000 files | 50 | 204.918 | 92.419 | 2.217× |
| One-file diff | 10,000 files | 50 | 24.216 | 6.180 | 3.918× |
| Initial commit | 100,000 files | 1 | 1,032,659.463 | 187,847.659 | 5.497× |
| One-file commit | 100,000 files | 50 | 785.332 | 343.687 | 2.285× |
| One-file diff | 100,000 files | 50 | 37.596 | 7.982 | 4.710× |
| Positive ancestry | 10,000-commit chain | 100 | 22.994 | 0.931 | 24.698× |
| Negative ancestry | 10,000-commit chain | 100 | 49.384 | 1.502 | 32.879× |

The ratios divide the two medians of the five per-trial p50 values. Original
within-trial p95 values remain in the report; individual operation samples are
unavailable and have not been pooled. Each initial commit has exactly one
observation per trial, so its original p50 and p95 describe the same observation.
There are five initial-commit observations at each scale.

The fixed resident edit-to-diff path improves all three diff rows by
3.918–4.710× over the preserved baseline. Retaining affected original entries
avoids reloading the parent directory after an edit takes ownership of a cached
tree; it preserves the bounded cache and immutable stored trees. The
[pre-fix report](benchmarks/scaling-2026-10-25-before-diff-fix.json) remains linked
so the regression and its measured correction can be inspected together.

These observations describe the compressed `MemoryStore` backend in one shared
Linux x86_64 development container. They include validation, hashing,
serialization, compression and in-process cache behavior. They exclude disk
durability, network requests and concurrent deployment load. Hardware and host
load were not controlled. The 2.217–32.879× range is an observed result for this
workload and these builds; it does not establish a universal speedup or a Git
comparison.

## Historical standalone run

The following earlier standalone observations are retained unchanged. They are
separate from the paired comparison and are not its baseline denominator.

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
