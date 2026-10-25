# Testing and performance

## Correctness

The workspace test commands cover library, binary, integration, example, benchmark-target, and documentation tests:

```sh
cargo test --workspace --all-targets --all-features --locked
cargo test --workspace --doc --all-features --locked
```

Storage tests should use fresh temporary directories or isolated databases. A backend is conforming only when immutable insertion is idempotent, acknowledged data survives restart, reference updates compare the old value atomically, and change-log insertion deduplicates retries.

Repository tests exercise DAG ancestry, tree updates, merges, rebases, path history, and line attribution. Client tests exercise dirty and untracked files, sparse selection, absent lazy files, status fingerprints and timestamp races, checkout recovery, and explicit file/directory replacements involving virtual entries. Portable overlay tests cover namespace transactions, writes/truncation/holes, rename/unlink identity, Windows lifetime pins, cache accounting, dirty-ledger reset, synchronization, sparse guards, and orphan recovery. Service tests verify authorization at both control and object endpoints, and admission remains reserved while response bodies or their data frames are retained by slow consumers.

Passing unit tests establishes the behavior those tests cover. It does not establish availability under region failures or throughput for an arbitrary production workload.

## Recorded integration evidence

All six workflows below completed successfully for published commit
[`3b3110c`](https://github.com/wieslawsoltes/scion/commit/3b3110cf4f9cbc286a64289917707342da8d11e2)
on October 25, 2026. This records eighteen completed jobs at that source revision;
new commits must pass their own applicable checks before merge.

| Gate | Completed run | What it establishes |
|---|---|---|
| Main CI, eleven jobs | [40223535451](https://github.com/wieslawsoltes/scion/actions/runs/40223535451) | Linux/macOS/Windows all-target/all-feature builds and tests; native Linux x64/ARM64 static-CRT builds, tests and linkage verification; Rust 1.88; formatting, Clippy and rustdoc; all thirteen crate archives; dependency checks; PostgreSQL and running three-tier containers |
| Linux FUSE | [40223535437](https://github.com/wieslawsoltes/scion/actions/runs/40223535437) | Real kernel mount, lazy reads, create/write/append/holes/truncate, rename, symlinks/modes, fsync, mmap publication, two authority commits, open/unlink and `O_PATH` lifetimes, cleanup, and reopen |
| Cloud storage | [40223535466](https://github.com/wieslawsoltes/scion/actions/runs/40223535466) | Actual S3/GCS/Azure SDK HTTP contracts and pinned official MinIO integration with conditional writes, integrity checks, concurrency, and metadata restart |
| Spanner transactions | [40223535425](https://github.com/wieslawsoltes/scion/actions/runs/40223535425) | Official emulator 1.5.59 through the official Rust SDK: two clients, concurrent CAS/publication, committed cursor reads, immutable conflicts, rollback, and restart |
| Fuzz, three jobs | [40223535495](https://github.com/wieslawsoltes/scion/actions/runs/40223535495) | Sanitizer execution of generated and seeded path, canonical-object, and envelope inputs |
| Git comparison | [40223535401](https://github.com/wieslawsoltes/scion/actions/runs/40223535401) | Release comparison workers, equivalent query results, and retained raw timing data; no noisy shared-runner speed threshold |

The Windows run includes the regression for multiple writable handles protected
by separate lifetime-pin files. Linux FUSE success is an actual mount result,
not only a feature compilation check. Its [fixture](../scripts/smoke_fuse.py)
fails when `/dev/fuse` or the required mount capability is missing.

Identity tests use temporary loopback discovery/JWKS services and public test
keys. They exercise asymmetric signatures, issuer/audience/expiry checks,
algorithm confusion, key rotation, document bounds, coalescing, provider outages,
and atomic policy revocation. Authority integration additionally checks that an
unauthorized large upload is rejected before its body is sent. See
[identity verification](identity.md#verification).

### Run backend and filesystem fixtures

```sh
# PostgreSQL, with a disposable database supplied by the caller:
SCION_TEST_DATABASE_URL=postgres://localhost/scion_test \
  cargo test -p scion-store --features postgres --test postgres --test publication --locked
# MinIO binary built from the pinned official release:
python3 scripts/test-s3.py --minio /path/to/minio
# Official Spanner emulator exposed on loopback, or --gateway /path/to/gateway_main:
python3 scripts/test-spanner.py
# FUSE-capable Linux host:
cargo build --release -p scion-cli -p scion-server --features scion-cli/fuse --locked
python3 scripts/smoke_fuse.py
```

The [cloud](cloud-storage.md) and [Spanner](spanner.md) guides specify fixture
versions, credential isolation, schema setup, and the distinction between an
emulator contract and production service behavior. Process-exit/reopen tests
exercise durable publication without relying on Rust destructors; they do not
simulate physical power loss, filesystem damage, or a regional outage.

The storage suite also checks that the linked bundled SQLite includes the upstream [WAL-reset corruption fix](https://sqlite.org/releaselog/3_51_3.html). The recorded CI snapshot includes the SQLite dependency update and its regression tests; later commits must pass their own applicable checks.

## Fuzzing

The independent `fuzz` workspace is development tooling and is not a product crate. It uses [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html) and libFuzzer with three targets:

| Target | Oracle |
|---|---|
| `paths` | Constructor and JSON deserializer agree; accepted paths preserve traversal and metadata-directory invariants |
| `canonical` | Parsed trees and changesets round-trip to stable bytes, IDs, and validation results; optimized tree parsing matches the original derived decoder's acceptance and normalized values |
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

The [durable Git comparison](git-comparison.md) records equivalent repository fixtures, durability settings, process/cache state, median and tail latencies, and raw samples, including operations where Git wins. The repository's separate [engine benchmark results](performance.md) include the command, workload sizes, sample counts, and limits of the in-memory measurements. Raw rows are emitted by `cargo bench -p scion-engine --bench scaling` for comparison against future changes.
