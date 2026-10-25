# Scope and performance implementation pass

This pass builds on the initial implementation merged in PR #1. Each completed
capability must include a concrete API, operational documentation, regression
coverage and passing package/build checks before merging.

## Implementation objectives

- Recursively synthesize virtual merge bases for criss-cross histories, retain
  explicit unresolved conflicts, and verify symmetry and ancestry behavior.
- Reduce working-copy status and materialization costs without trusting stale
  filesystem metadata; extend on-demand access with a writable overlay and
  explicit commit integration.
- Add reusable cloud object-storage adapters with authoritative metadata kept
  transactional, explicit configuration, integrity checks and emulator tests.
- Exercise remaining deployment and identity boundaries through concrete
  implementations as each dependency becomes available.

## Git comparison contract

Performance claims must identify the operation, dataset, machine, compiler,
backend, durability, concurrency and cache state. Compare equivalent results,
not a durable Git operation against a non-durable Scion operation. Report both
Scion and Git latency distributions, sample counts and speed ratios. Include
costs of process startup or exclude them explicitly on both sides. Preserve raw
results and executable benchmark code, including cases where Git is faster.

Prioritize small edits in large repositories, ancestry and merge-base queries,
unchanged and changed working-copy status, sparse materialization and streamed
object access. Optimize measured bottlenecks, and add deterministic operation
count regressions where timing noise would make a CI gate unreliable.

The performance target is substantial improvement for the online large-repository
workloads that motivate Scion. A universal speedup over every Git workload must
not be asserted from a selected subset of measurements.
