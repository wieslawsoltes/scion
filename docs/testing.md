# Testing and performance

## Correctness

The main workspace test command includes library, binary, integration, and documentation tests:

```sh
cargo test --workspace --all-features --locked
```

Storage tests should use fresh temporary directories or isolated databases. A backend is conforming only when immutable insertion is idempotent, acknowledged data survives restart, reference updates compare the old value atomically, and change-log insertion deduplicates retries.

Repository tests exercise DAG ancestry, tree updates, merges, rebases, path history, and line attribution. Client tests must exercise dirty and untracked files, sparse selection, and the distinction between absent files and files that have not been materialized. Service tests must verify authorization at both control and object endpoints.

Passing unit tests establishes the behavior those tests cover. It does not establish availability under region failures or throughput for an arbitrary production workload.

## Fuzzing

The independent `fuzz` workspace is development tooling and is not a product crate. It uses [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html) and libFuzzer with three targets:

| Target | Oracle |
|---|---|
| `paths` | Constructor and JSON deserializer agree; accepted paths preserve traversal and metadata-directory invariants |
| `canonical` | Parsed trees and changesets round-trip to stable bytes, IDs, and validation results |
| `envelope` | Compression round-trips, domain/ID mismatches fail, arbitrary envelope decoding never accepts mismatched content |

```sh
rustup toolchain install nightly
cargo install cargo-fuzz --locked
cargo +nightly fuzz run paths -- -max_total_time=60
cargo +nightly fuzz run canonical -- -max_total_time=60
cargo +nightly fuzz run envelope -- -max_total_time=60 -max_len=65536
```

CI runs short fuzz sessions on relevant pull requests and longer weekly sessions. A failing input is uploaded as an artifact. Reproduce it locally with `cargo +nightly fuzz run TARGET path/to/artifact`; reduce it with `cargo +nightly fuzz tmin TARGET path/to/artifact`, then add a focused regression test before fixing the implementation.

### Initial sanitizer smoke

On October 25, 2026, all three targets completed with AddressSanitizer and LeakSanitizer enabled, using a five-second fuzzing budget per target and a 65,536-byte input limit:

| Target | Successful executions |
|---|---:|
| `paths` | 168,670 |
| `canonical` | 298,292 |
| `envelope` | 7,975 |
| **Total** | **474,937** |

All targets exited successfully without a reported invariant violation or sanitizer finding. These short smoke runs verify that the harnesses execute and exercise their oracles; they are not exhaustive fuzz campaigns or proof that every input is safe.

## Measuring performance

Measure independent dimensions instead of reporting one aggregate source-control number:

- Commit latency versus changed path count, path depth, and directory fan-out.
- Tree objects read and written for a single-file edit in a large hierarchy.
- Ancestry and merge-base latency versus history length, merge density, and cache state.
- Sparse checkout bytes and wall time for cold and warm cache tiers.
- Blob throughput, compression ratio, cache hit rate, and object size distribution.
- Peak memory under concurrent requests and large diffs.
- Repeated Git ingestion memory usage and restart cost.

Use separate cold-cache and warm-cache runs. Record commit, target architecture, compiler version, CPU, memory, backend configuration, repository generator parameters, concurrency, and raw results. Report p50, p95, and p99 for service latency. Avoid extrapolating from a small local fixture to a production fleet.

The performance ratios in Anthropic's article belong to its own infrastructure and workload. This project does not adopt those ratios as measured results.

The repository's own [engine benchmark results](performance.md) include the command, workload sizes, sample counts, and limits of the measurements. Raw rows are emitted by `cargo bench -p scion-engine --bench scaling` for comparison against future changes.
