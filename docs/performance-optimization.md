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
  live-WAL visibility, and compare-and-swap reference semantics.
- Compare the baseline and candidate binaries on identical retained fixtures;
  measure fresh durable writes separately with equivalent fixture construction.
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

## Remaining investigation

1. Reduce SQLite index/overflow-page amplification for wide metadata records.
2. Answer small recent changes directly from verified changeset deltas.
3. Reduce fresh-process and request/response overhead without omitting work.
4. Measure optimized release code and preserve all performance losses alongside
   wins; source changes must pass the existing build, test, packaging, and
   integration gates before merge.

Results will be added after the implementation and independent comparison are
complete. Existing measurements remain in [Git comparison](git-comparison.md).
