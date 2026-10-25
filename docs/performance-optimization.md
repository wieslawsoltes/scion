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

## Work in progress

1. Reduce local-store opening, object decoding, and SQL dispatch costs.
2. Avoid allocating complete directory trees for a single path lookup.
3. Use first-parent changeset information to accelerate equivalent diffs.
4. Reduce graph-index traversal and record-decoding costs.
5. Provide atomic prepared publication where blobs, commit metadata, and an
   optional expected-value reference update can share one durability boundary.
6. Measure optimized release code and preserve all performance losses alongside
   wins; source changes must pass the existing build, test, packaging, and
   integration gates before merge.

Results will be added after the implementation and independent comparison are
complete. Existing measurements remain in [Git comparison](git-comparison.md).
