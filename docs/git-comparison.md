# Reproducible comparison with Git

`scripts/compare_git.py` measures durable local Scion operations against Git on the
same generated repository. It checks every query result and saves individual
samples, summary statistics, configurations, tool versions and source hashes.
The comparison uses `LocalStore`, including SQLite and filesystem durability.

These measurements describe the specified workload and machine. They do not
establish a general performance advantage over Git, distributed server throughput,
or behavior under cold caches, concurrent writers or real monorepo contents.

## Run it

From the repository root, with Rust, Python 3 and Git on `PATH`:

```sh
CARGO_INCREMENTAL=0 python3 scripts/compare_git.py --quick
CARGO_INCREMENTAL=0 python3 scripts/compare_git.py \
  --output target/git-comparison/full
```

The script builds the release example automatically. `--offline` asks Cargo to
use cached dependencies. `--no-build` uses an existing executable, whose SHA-256
is recorded. `--driver PATH` selects another build. A default run uses:

| Parameter | Full profile | Quick profile |
| --- | ---: | ---: |
| Files | 100,000 | 1,000 |
| Files per directory | 100 | 100 |
| Trunk commits | 10,000 | 1,000 |
| Commits on each divergent arm | 1,000 | 100 |
| Samples per measured operation and implementation | 31 | 7 |
| Warmup operations | 3 | 3 |
| Initial distinct blob contents | 128 | 128 |
| File content length | 1,024 bytes | 1,024 bytes |

Override individual dimensions with `--files`, `--commits`, `--branch-commits`,
`--samples`, `--warmup`, `--fanout`, `--variants` and `--blob-bytes`.
`--work-root PATH` selects the filesystem containing both repositories.
`--keep-workdir` retains the fixture, identifiers and worker logs for investigation.
The temporary fixture is removed otherwise. Output consists of a JSON report,
a CSV containing every sample and a separate summary CSV.

The files share a bounded pool of deterministic text bodies. This deliberately
isolates metadata, tree traversal and small-change costs; it is not a claim about
the compression ratio or content distribution of a production repository. Both
implementations receive the same paths, bytes, authors, messages, timestamps and
logical parent relationships. Their native object hashes and formats differ.

## What each measurement includes

### Prepared durable commits: resident processes

One Scion comparison worker and one `git fast-import` process remain alive for
the write measurements. Both receive a prepared one-file delta with new content.
Each sample measures the coordinator's wall time from writing the request until
receiving its completion acknowledgement. Python payload construction happens
before the timer. There is no filesystem discovery, working-copy scan or network
transport in this operation.

Scion writes the blob, creates and publishes the immutable changeset and its
derived metadata, then advances a reference with compare-and-swap. Its local
backend uses SQLite WAL with `synchronous=FULL`, and syncs new object files and
their parent directories on Unix. The reply follows successful reference update.

Git receives a commit on `refs/heads/bench`, then `checkpoint` and `get-mark`.
The checkpoint closes the pack and saves references; the subsequent mark response
provides a sequential completion acknowledgement. The Git documentation describes
this checkpoint boundary and recommends a following response-producing command
when a frontend needs to observe completion.
[Git fast-import documentation](https://git-scm.com/docs/git-fast-import/2.51.1).

The harness explicitly configures Git with:

```ini
[core]
    fsync = all
    fsyncMethod = fsync
    commitGraph = true
[feature]
    manyFiles = true
[fastimport]
    unpackLimit = 0
[gc]
    auto = 0
[maintenance]
    auto = false
```

`core.fsync=all` requests hardening of Git's supported repository components,
including objects, metadata and references. `fsyncMethod=fsync` selects the system
fsync primitive. Retaining small fast-import packs avoids spawning an unpacking
process at every checkpoint. These are documented Git settings, not modifications
to the Git binary. Global/system Git configuration is excluded from the fixture.
[Git configuration documentation](https://git-scm.com/docs/git-config/2.51.1).

Both systems perform their documented local durability operations before the
acknowledgement. This is a comparison of those software contracts; no power-loss
test or verification of the host's physical storage cache was performed. A
checkpoint per commit is a prepared-delta publication workload. Bulk import with
infrequent checkpoints is a separate workload and is not represented by this row.

A resident no-op row reports pipe/request overhead separately. No-op timing is
never subtracted from measured operations. The Scion worker also returns an
internal operation duration as a diagnostic; ratios use the coordinator's full
wall time for both implementations.

### Reads and graph queries: fresh processes

Each query starts a new Scion example process or Git CLI process. Both include
process creation, repository opening, computation, output and process shutdown.
The Scion executable calls the local `Repository` API directly; it does not run an
HTTP server. Git likewise operates on local repository storage.

Before these measurements, the harness closes both writers, repacks Git's objects
and runs `git commit-graph write --reachable --changed-paths`. Git therefore has
its native persisted graph acceleration available, while Scion uses the graph
records it derives during commit creation.
[Git commit-graph documentation](https://git-scm.com/docs/git-commit-graph).

The measured operations are:

| Operation | Scion API | Git command / equivalent |
| --- | --- | --- |
| Positive ancestry | `is_ancestor(root, trunk)` | `merge-base --is-ancestor` |
| Negative ancestry | `is_ancestor(side, trunk)` | `merge-base --is-ancestor` |
| Ancestor merge base | `merge_bases(root, trunk)` | `merge-base --all` |
| Divergent merge bases | `merge_bases(left, right)` | `merge-base --all` |
| One-file text diff | `diff(previous, tip)` | `diff --no-ext-diff --no-textconv --no-renames --unified=3` |
| File content read | `read_file(tip, path)` | `show tip:path` |
| Repository-open control | Open `LocalStore`, then no-op | `rev-parse --is-bare-repository` |
| Process-startup control | Driver `--version` | `git --version` |

Scion outputs JSON while Git uses its command's native output. Output byte counts
are recorded for the fresh-process rows. The text-diff comparison includes patch
generation on both sides; the harness validates the one changed path. File-read
contents must match byte for byte. Boolean ancestry and merge-base results are
checked against the constructed DAG, rather than treating successful process
exit as proof of a correct answer.

The parent chains used for graph queries contain empty deltas and reuse the
initial tree. A short separate branch from the root provides the negative ancestry
query. The two long divergent branches start at the trunk tip; their unique merge
base is that tip. Setup durations are retained for diagnosis but are not compared:
Scion publishes setup commits individually, while Git imports each setup chain in
a batch before checkpointing.

### Resident file reads

An additional row uses a resident Scion worker and a resident
`git cat-file --batch` process. Each measurement includes one request and its
complete content response. It resolves a commit and repository path on both
sides; Git does not receive a pre-resolved blob ID. The repositories have already
been prepared as above, including Git repacking.

Paths are spread deterministically through the fixture with index
`(sample * 104729) % file_count`. Before recording samples, every selected path is
read once per warmup pass by each implementation. This intentionally measures
warm process and filesystem caches, which matter for long-lived services. The
raw report records the exact selected indices. Every returned body must match its
expected bytes. The original baseline predates this additional row, so it has no
before/after value for resident spread reads.

## Cache, sampling and reproducibility

The benchmark uses warm operating-system caches, performs warmups and alternates
which implementation runs first on each sample. It does not require privileged
kernel-cache manipulation. Every read/query sample still uses a fresh process;
resident process timings are labeled separately. The median and nearest-rank
95th percentile are reported without removing outliers. Shared-runner CPU and I/O
contention can affect both, particularly tail latency.

The JSON records the Git and compiler versions, OS, CPU, filesystem, dimensions,
effective Git configuration, source commit, dirty status, tracked-diff hash,
individual source-file hashes and executable hash. A source change detected during
an automatic build is marked in the report. For a publication-grade repeat, use a
clean checkout of the desired commit and run on an otherwise idle host with the
same storage and toolchain. Keep raw samples alongside any summarized claim.

## Continuous integration

`.github/workflows/performance.yml` runs the quick profile on relevant pull
requests, supports manual quick/full runs and schedules the full profile weekly.
It uploads raw JSON and CSV artifacts. CI fails for incorrect results or harness
errors; it deliberately has no timing threshold on shared hosted runners.

The example executable avoids registering another Cargo benchmark target, so
`cargo test --all-targets` does not accidentally execute the comparison workload.

## Recorded runs

### Baseline, 25 October 2026

The full profile ran on Linux x86_64, an AMD EPYC 9V74 host exposing nine logical
CPUs, about 9.7 GiB RAM and an overlay filesystem. The compiler was Rust 1.99.1;
Git was 2.51.1. The core source revision was `f1e9967`, with the uncommitted
comparison harness and other workspace changes recorded by status and file hash
in the report. The release executable's hash is also recorded. This was a shared
development container, not an isolated performance laboratory.

Raw data: [JSON report](benchmarks/git-comparison-2026-10-25.json),
[all samples](benchmarks/git-comparison-2026-10-25.csv), and
[summary CSV](benchmarks/git-comparison-2026-10-25-summary.csv).

| Operation | Scope | Scion median | Git median |
| --- | --- | ---: | ---: |
| Prepared durable commit | Resident processes | 6.839 ms | 3.313 ms |
| Positive ancestry, 10,000 commits | Fresh processes | 2.339 ms | 3.784 ms |
| Negative ancestry | Fresh processes | 2.464 ms | 3.715 ms |
| Ancestor merge base | Fresh processes | 2.177 ms | 3.502 ms |
| Divergent merge base, 1,000 commits per arm | Fresh processes | 92.135 ms | 2.767 ms |
| One-file text diff | Fresh processes | 3.655 ms | 1.892 ms |
| File read, 1 KiB | Fresh processes | 2.682 ms | 1.601 ms |
| Repository-open control | Fresh processes | 1.598 ms | 1.136 ms |
| Process-startup control | Fresh processes | 1.022 ms | 0.987 ms |
| No-op control | Resident processes | 0.020 ms | 0.015 ms |

Scion's binary-lifting queries were about 1.5–1.6 times faster for the measured
ancestry and ancestor-base cases. Git was about twice as fast for prepared
commits and small diffs, and about 33 times as fast for the divergent merge-base
case. This baseline does **not** support a general claim that Scion is faster than
Git. Fresh-process startup accounts for a substantial part of the short queries.

The divergent result identified a concrete implementation cost: the DAG frontier
read every node on two linear arms through SQLite, even though merge-safe jump
tables already existed. Subsequent optimization and measurements must retain this
baseline and report the same workload and settings, including any remaining
losses. Setup numbers in the raw report use different batching policies and must
not be interpreted as a comparison of bulk-import performance.

### After graph and local-cache optimization, 25 October 2026

The same full profile was repeated after adding merge-safe paired jump reduction,
a bounded decoded-tree cache and caching of proven durable directory/object
state. Both generated repositories produced **exactly the same commit identifiers
and fixture contents as the baseline**, confirming that these changes preserved
the object formats and logical workload. The report records source revision
`859907c` plus its dirty working-tree status and source hashes. The release driver
was rebuilt immediately before the run; its executable hash is recorded.

Raw data: [JSON report](benchmarks/git-comparison-2026-10-25-optimized.json),
[all samples](benchmarks/git-comparison-2026-10-25-optimized.csv), and
[summary CSV](benchmarks/git-comparison-2026-10-25-optimized-summary.csv).

| Operation | Scope | Scion median | Git median | Within-run comparison |
| --- | --- | ---: | ---: | --- |
| Prepared durable commit | Resident | 6.433 ms | 3.345 ms | Git 1.92× faster |
| Positive ancestry | Fresh process | 1.895 ms | 3.917 ms | Scion 2.07× faster |
| Negative ancestry | Fresh process | 2.846 ms | 5.166 ms | Scion 1.82× faster |
| Ancestor merge base | Fresh process | 2.336 ms | 4.341 ms | Scion 1.86× faster |
| Divergent merge base | Fresh process | 2.572 ms | 2.191 ms | Git 1.17× faster |
| One-file text diff | Fresh process | 3.659 ms | 2.362 ms | Git 1.55× faster |
| File read, 1 KiB | Fresh process | 2.651 ms | 1.749 ms | Git 1.52× faster |
| File reads across warmed paths | Resident | 0.0509 ms | 0.0472 ms | Git 1.08× faster |

The divergent merge-base median improved from 92.135 ms to 2.572 ms, about 35.8×
within Scion, while retaining every merge edge when the optimization reaches a
merge node. Counted-I/O tests independently verify fewer than 100 graph reads for
1,024-commit equal and unequal forks, including arms ending at criss-cross merges;
an exhaustive oracle checks all 16,384 ordered node pairs in a generated DAG.

This improvement removed the large graph-query regression without reversing all
of the other results. Durable prepared commits still took about twice Git's time,
and fresh-process file reads and diffs remained slower. The warmed resident read
row was close to Git, but is a different process/cache scope from the fresh-process
row and must not replace it in a headline comparison. It had no earlier baseline.

Tail latency remains relevant: prepared-commit p95 was 16.522 ms for Scion and
4.441 ms for Git, while divergent-base p95 was 3.604 ms and 2.997 ms respectively.
All samples, including these tails, are retained. Other agents' compilation was
paused for the second measurement window, but the host was still shared; this is
not an isolated experiment attributing each timing change to one patch.

The measured evidence supports substantially improved Scion graph traversal and
faster ancestry operations on this fixture. It does **not** support claiming that
the full system is uniformly faster than Git. Future work should measure durable
write batching and tree-format/traversal costs with the same safeguards and retain
any workloads where Git remains faster.
