# Query and publication performance targets

This follow-up starts from merged commit `6a11ff76052d10951e9f7d029f0313de7c78101d`.
The target is to beat Git on every currently losing functional comparison and
reduce Scion latency by at least two times on the remaining comparisons.
Process startup, repository opening, and resident no-op controls remain visible
so that improvements cannot be attributed to hidden setup costs.

## Measurement contract

- Preserve the existing workloads, correctness checks, logical history, warmup,
  sample count, alternating order, and fully consumed responses.
- Retain FULL-synchronous publication, immutable-object integrity verification,
  committed-WAL visibility, and compare-and-swap reference semantics. Fresh
  single-operation queries may use a consistent snapshot; resident readers keep
  observing later commits between requests.
- Compare the baseline and candidate binaries on identical logical fixtures;
  use independently constructed stores when measuring a changed physical layout,
  and identify shared-fixture query replays explicitly.
- Use the same current Git binary and prepare its packs and commit graph before
  query measurements. Publish source and executable hashes, medians, p95s,
  controls, raw observations, and any target that remains unmet.
- Production APIs and shared implementation paths must carry the improvements.
  The comparison worker may adopt a new reusable API without changing the
  logical work or durability promised by its acknowledgement.

## Implemented changes under measurement

Prepared publication now accepts validated staged blobs and an explicit optional
reference CAS. LocalStore places small objects, derived records, membership, log,
and CAS in one FULL-synchronous transaction. A CAS mismatch still publishes the
immutable commit and returns the mismatch; the portable backend fallback retains
publication-before-CAS ordering. Ordinary `create_commit` still leaves refs alone.

Cold tree traversal validates canonical bytes without allocating a complete
directory or reconstructing unused identifiers. Unsupported shapes retain the
existing Serde compatibility path. A second distinct lookup promotes a directory
to its complete decoded tree. Positive revision-root proofs share the same
bounded cache and are cleared or disabled with it; missing membership is never
cached. Adjacent diffs use first-parent deltas with equivalent tree-based results.

Small object compression/decompression reuses per-thread Zstandard contexts.
Graph traversal avoids repeated node reads, and guarded metadata lookups can
share a database query. The worker buffers each complete JSON response before
flushing; end-to-end measurements still include serialization, every response
byte, and the acknowledgement.

Fresh single-operation queries now use the reusable `open_read_snapshot` API.
It pins the committed WAL view at the first database query and reuses a read
transaction for the operation. Resident workers continue using live read-only
connections. New stores use a rowid metadata table with a narrow composite-key
index so comparisons do not traverse wide values on overflow pages. Existing
stores retain their layout; an explicit atomic migration is available and tested
for concurrent snapshots, byte preservation, and rollback on storage failure.

Recent changed-file reads and adjacent diffs can inspect bounded, verified small
changesets before traversing trees. The probe decodes at most 8 KiB and retains
the regular tree path for large changesets, unrelated paths, or unsupported
storage implementations. Repository membership, object identity, and changeset
validation remain required. Repeated unrelated lookups do not re-probe the same
revision.

Verified blobs of at most 64 KiB now share the existing bounded repository cache.
Repeated file reads and text diffs can reuse their content after membership and
path checks. Capacity, clearing, and entry limits include every retained kind;
direct storage reads and commit validation remain uncached. Cache hits use bytes
verified on insertion, with later physical corruption observable on a fresh
storage read or after clearing/eviction.

## Measurement-driven decisions

SQLite mapping is configurable, but the default remains disabled. The initial
256 MiB and 1 GiB mapping experiments slowed fresh-process graph queries enough
to outweigh their small resident-read improvement. Checkpoint-on-close retains
the original default behavior. These choices preserve FULL synchronization and
normal WAL auto-checkpointing.

The comparison harness can include the previous Scion executable using
`--baseline-driver` and `--baseline-report`. It verifies the executable hash
against its own clean-source report and records that provenance separately from
the candidate. All three implementations rotate through execution order. Full
runs use independent stores and verify identical Scion changeset IDs; query
replays use the same retained logical fixture. The previous two-way mode remains
available.

## Recorded result

The [clean three-way default-profile run](git-comparison.md#optimized-three-way-comparison-25-october-2026)
uses public commit `3b3110cf4f9cbc286a64289917707342da8d11e2`, independent baseline
and candidate stores, Git 2.56.0, and verified static C runtime linkage. All eight
functional medians beat Git. Against the preserved Scion baseline, prepared
commits improve 2.009×, fresh reads 2.107×, resident reads 2.232×, and diffs 2.733×.
Graph rows improve 1.297–1.574× and remain below the requested 2× target. Controls
remain visible and below 2×; the resident no-op is effectively equal to Git.

The final CPU optimizations use a byte-equivalent canonical tree writer, reuse
verified prepared-object IDs during encoding, use reusable level-one Zstandard
contexts for metadata (blobs retain level three), and take unique cached tree
ownership during edits. Object IDs, canonical bytes, immutable validation,
publication atomicity, and durable acknowledgements remain unchanged. Tests
compare the new writer with the original canonical oracle and exercise held
readers, failures, and cache bounds. Envelope decoders continue accepting prior
standard Zstandard frames.

## Frozen graph follow-up

The public graph snapshot API and local CLI remove SQLite opening from explicitly
prepared graph queries. Both full comparison profiles beat Git in every
functional median, and all eight graph measurements (four operations at two
history sizes) improve by at least 2× over the preserved Scion baseline. Export
costs, artifact size, frozen membership, raw samples, and all controls are in the
[frozen graph comparison](git-comparison.md#frozen-graph-comparisons-25-october-2026).

The Windows held-reader replacement regression exposed `tempfile::persist`'s
platform limitation. Publication now uses the standard library's atomic rename
with a guarded temporary-file lifetime; the old reader stays open throughout the
regression. Failure cleanup and Windows normal-file attributes are also checked.

## Resident-cache and edit-diff result

Decoded graph nodes now share the bounded repository cache with trees, root
proofs, path lookups, and small blobs. Membership checks remain repository-scoped;
misses and errors are not cached. Cold inline-object reads prepare their actual
lookup directly, with a guarded legacy fallback only when the inline table is
confirmed absent. Storage errors and corruption still propagate.

Taking ownership of an old decoded tree had removed entries needed by the next
adjacent diff. The fix retains only affected names and verified absence under
the old immutable tree ID before mutation. Partial entries never stand in for a
complete editable directory. Tests prove zero old-tree reads for affected parent
lookups and forward/reverse adjacent diffs, including failure and branching cases.

The [five-trial original MemoryStore workload](performance.md) now exceeds 2× in
all eleven cases (2.217–32.879×). The [complete durable default run and large query
replay](git-comparison.md#resident-cache-and-edit-diff-follow-up-25-october-2026)
beat Git in every measured functional median. All seven large-profile query cases
improve by at least 2× over the preserved Scion build. The default run reaches 2×
in seven of eight functional cases; its prepared commit improves 1.833×.

## Remaining investigation

1. Reduce durable prepared-commit latency with sufficient margin: the latest
   complete run records 1.833× improvement, while previous runs range above and
   below 2×. Preserve FULL synchronization and validate any change against the
   same independent baseline and Git workloads.
2. Keep transport, startup, and repository-open controls visible. They remain
   below 2× and must not be represented as completed targets.
3. Retain every measurement and failure that informs a change. Local engine and
   durable-query results do not establish arbitrary-workload, cold-cache,
   concurrent-writer, or distributed-service speedups.
