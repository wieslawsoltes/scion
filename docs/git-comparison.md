# Reproducible comparison with Git

`scripts/compare_git.py` measures durable local Scion operations against Git on the
same generated repository. It checks every query result and saves individual
samples, summary statistics, configurations, tool versions and source hashes.
The comparison uses `LocalStore`, including SQLite and filesystem durability.
The latest complete default run and large query replay are the
[resident-cache follow-up](#resident-cache-and-edit-diff-follow-up-25-october-2026).
The [non-snapshot three-way comparison](#optimized-three-way-comparison-25-october-2026)
retains the ordinary SQLite graph path and its original observations.
The [Git 2.56.0 version follow-up](#git-2560-follow-up-25-october-2026) retains the
earlier default and 100,000-commit histories as an optimization record.

These measurements describe the specified workload and machine. They do not
establish a general performance advantage over Git, distributed server throughput,
or behavior under cold caches, concurrent writers or real monorepo contents.

## Resident cache and edit-diff follow-up: 25 October 2026

These measurements use clean public source
[`991fdc9`](https://github.com/wieslawsoltes/scion/commit/991fdc9c36190104019840110c69a689edc9e5a1),
Git 2.56.0, the same preserved Scion baseline, verified static-CRT GNU/Linux
release linkage, 100,000 file paths, 31 samples, and three warmups. All result,
fixture, source, and executable checks passed. Candidate and baseline stores
remain independent; all three execution orders rotate through six permutations.

The changes retain decoded graph nodes within the existing shared cache budget,
avoid an extra schema query before inline object reads, and preserve affected
parent-tree entries when reusing a decoded tree for an edit. The latter fixes a
measured edit-to-diff regression. The separate [five-trial MemoryStore comparison](performance.md)
records 2.217–32.879× improvements across all eleven original scaling cases.

### Default profile: complete run

The complete run constructs new independent stores with a 10,000-commit trunk
and 1,000 commits per divergent arm. Prepared commits are measured before history
extension, exactly as in the earlier profiles. All eight functional medians beat
Git; seven exceed 2× improvement over the preserved Scion binary. The prepared
commit result is **1.833×**, so the durable-commit 2× target is still unmet in this
run despite higher ratios in earlier observations.

| Operation | Scion p50 / p95, µs | Baseline p50, µs | Git p50, µs | Baseline / Scion | Git / Scion |
|---|---:|---:|---:|---:|---:|
| Prepared durable commit | 1,066.705 / 1,313.326 | 1,954.782 | 2,796.568 | 1.833× | 2.622× |
| Positive ancestry | 693.353 / 910.990 | 1,532.865 | 3,207.614 | 2.211× | 4.626× |
| Negative ancestry | 663.148 / 845.882 | 1,791.654 | 3,241.334 | 2.702× | 4.888× |
| Ancestor merge base | 671.330 / 895.357 | 1,517.522 | 2,975.053 | 2.260× | 4.432× |
| Divergent merge base | 674.744 / 766.733 | 1,724.723 | 1,517.202 | 2.556× | 2.249× |
| Fresh file read, 1 KiB | 1,079.142 / 2,158.907 | 2,225.186 | 1,397.171 | 2.062× | 1.295× |
| Fresh one-file text diff | 1,084.851 / 1,282.899 | 3,086.992 | 1,549.060 | 2.846× | 1.428× |
| Resident spread file read | 16.996 / 28.874 | 37.646 | 40.090 | 2.215× | 2.359× |

| Operation | Scion p50 / p95, µs | Baseline p50, µs | Git p50, µs | Baseline / Scion | Git / Scion |
|---|---:|---:|---:|---:|---:|
| Process startup control | 575.375 / 754.765 | 909.878 | 934.946 | 1.581× | 1.625× |
| Repository-open control | 775.576 / 1,240.566 | 1,216.729 | 1,032.572 | 1.569× | 1.331× |
| Resident no-op control | 19.539 / 38.288 | 20.391 | 19.769 | 1.044× | 1.012× |

Controls remain below 2× improvement over the baseline. Their complete samples
and tails are retained. These wall-clock results include transport, process, and
filesystem variability; a passing functional median does not imply a universal
2× improvement or a better latency tail on every run.

[Raw JSON](benchmarks/git-comparison-2026-10-25-resident-default.json), [individual samples](benchmarks/git-comparison-2026-10-25-resident-default.csv), [summary CSV](benchmarks/git-comparison-2026-10-25-resident-default-summary.csv).

### Large profile: queries-only replay

This replay uses the retained, independently built 100,000-commit trunk and
10,000-commit arms from the earlier complete large run. It checks the fixture
marker, keeps the baseline database separate, prepares a new verified graph
snapshot, and measures only queries. **No new prepared-commit or resident no-op
measurement is included.** All seven functional query medians beat Git and exceed
2× over the preserved baseline.

| Operation | Scion p50 / p95, µs | Baseline p50, µs | Git p50, µs | Baseline / Scion | Git / Scion |
|---|---:|---:|---:|---:|---:|
| Positive ancestry | 916.008 / 1,230.751 | 2,081.001 | 45,267.094 | 2.272× | 49.418× |
| Negative ancestry | 843.399 / 1,064.321 | 2,675.205 | 35,694.792 | 3.172× | 42.323× |
| Ancestor merge base | 814.796 / 1,001.076 | 2,056.404 | 30,717.345 | 2.524× | 37.699× |
| Divergent merge base | 744.280 / 1,165.713 | 2,069.233 | 6,060.676 | 2.780× | 8.143× |
| Fresh file read, 1 KiB | 1,022.428 / 1,391.543 | 2,234.622 | 1,273.366 | 2.186× | 1.245× |
| Fresh one-file text diff | 1,172.253 / 1,427.738 | 3,251.571 | 1,616.331 | 2.774× | 1.379× |
| Resident spread file read | 17.076 / 26.640 | 37.917 | 40.541 | 2.220× | 2.374× |

| Operation | Scion p50 / p95, µs | Baseline p50, µs | Git p50, µs | Baseline / Scion | Git / Scion |
|---|---:|---:|---:|---:|---:|
| Process startup control | 560.483 / 704.119 | 913.074 | 902.898 | 1.629× | 1.611× |
| Repository-open control | 752.082 / 913.755 | 1,297.177 | 1,027.656 | 1.725× | 1.366× |

[Raw JSON](benchmarks/git-comparison-2026-10-25-resident-large-queries.json), [individual samples](benchmarks/git-comparison-2026-10-25-resident-large-queries.csv), [summary CSV](benchmarks/git-comparison-2026-10-25-resident-large-queries-summary.csv).

### Preparation costs and retained evidence

| Profile | Graph nodes | Artifact bytes | Export wall time |
|---|---:|---:|---:|
| Complete default run | 12,036 | 8,923,168 | 0.511 s |
| Large query replay | 120,036 | 96,467,328 | 7.764 s |

Only candidate ancestry and merge-base operations use the explicit frozen graph
artifact. File reads, diffs, writes, and resident requests use ordinary LocalStore
APIs. Graph preparation is excluded from query latency, alongside Git's native
pack and commit-graph preparation; both costs are present in each JSON report.
The snapshot's membership is frozen and must be exported again to include later
commits. Full query timings include opening, integrity and identity checks,
response bytes, and process shutdown.

The [first resident run](benchmarks/git-comparison-2026-10-25-resident-first-default.json)
remains available: its resident-read and ancestor-merge-base improvements were
1.845× and 1.932×. The [pre-fix scaling report](benchmarks/scaling-2026-10-25-before-diff-fix.json)
retains the larger edit-to-diff regression that prompted the affected-entry fix.
Earlier comparisons below remain intact. These observations are part of the
performance record, including cases below the requested threshold.

## Frozen graph comparisons: 25 October 2026

These complete three-way runs use the public frozen graph API for candidate
ancestry and merge-base queries. The default profile uses clean source
[`2b88497`](https://github.com/wieslawsoltes/scion/commit/2b88497a69af85aa4a941c00b3a21cdeb4356ee9);
the large profile uses
[`43146c2`](https://github.com/wieslawsoltes/scion/commit/43146c205c6bdd2fadfec99f3333761a8ec58677).
The later commit fixes atomic replacement with an open reader on Windows; the
measured queries and Linux fixture semantics are unchanged. Both use verified
static-CRT GNU/Linux release builds, Git 2.56.0, the same preserved baseline
executable, independent Scion stores, 100,000 paths, 31 samples, and three warmups.
All correctness and source/binary provenance checks finished successfully.

### Explicit preparation and scope

`--graph-snapshot` calls `export_local_graph_snapshot` before timing, verifies
every source changeset, and rebuilds a closed graph. Git retains its existing
pack and commit-graph preparation. Candidate graph queries open that frozen
artifact in a fresh process, without opening SQLite. Their timings include
startup, header/identity checks, every graph lookup and checksum, response bytes,
and shutdown. The baseline keeps its previous repository API; candidate file
reads, diffs, writes, and resident workers keep the ordinary storage APIs.

| Profile | Trunk / each arm | Included nodes | Artifact bytes | Export wall time |
|---|---:|---:|---:|---:|
| Default | 10,000 / 1,000 | 12,036 | 8,923,168 | 0.507 s |
| Large | 100,000 / 10,000 | 120,036 | 96,467,328 | 6.320 s |

Preparation time and additional storage are real costs, excluded from query
latency just as Git preparation is. The artifact represents one explicitly
selected membership view; new publications require a new export. It does not
resolve live refs. See [frozen graph snapshots](graph-snapshots.md) for the public
API, CLI, integrity checks, cache bounds, and replacement semantics.

### Default profile: 10,000-commit trunk

| Operation | Scion p50 / p95, µs | Baseline p50, µs | Git p50, µs | Baseline / Scion | Git / Scion |
|---|---:|---:|---:|---:|---:|
| Prepared durable commit | 1,044.256 / 1,400.253 | 2,028.201 | 2,744.893 | 1.942× | 2.629× |
| Positive ancestry | 773.479 / 1,033.110 | 1,628.750 | 3,637.858 | 2.106× | 4.703× |
| Negative ancestry | 799.307 / 952.289 | 1,979.270 | 3,510.663 | 2.476× | 4.392× |
| Ancestor merge base | 750.233 / 910.215 | 1,625.065 | 3,087.524 | 2.166× | 4.115× |
| Divergent merge base | 761.780 / 1,604.163 | 2,073.742 | 1,793.980 | 2.722× | 2.355× |
| Fresh file read, 1 KiB | 1,246.042 / 1,503.361 | 2,329.858 | 1,416.765 | 1.870× | 1.137× |
| Fresh one-file text diff | 1,263.498 / 1,710.664 | 3,275.277 | 1,758.136 | 2.592× | 1.391× |
| Resident spread file read | 17.286 / 29.785 | 38.158 | 37.276 | 2.207× | 2.156× |

Every functional median beats Git, and all four graph-query rows improve by at
least 2× over the earlier Scion build. Six of eight functional rows reach 2×.
Fresh file reads (1.870×) and prepared commits (1.942×) remain below that threshold
in this run. The non-snapshot comparison below preserves their earlier results;
these subsequent observations are retained rather than replaced with a preferred
run.

| Operation | Scion p50 / p95, µs | Baseline p50, µs | Git p50, µs | Baseline / Scion | Git / Scion |
|---|---:|---:|---:|---:|---:|
| Process startup control | 658.685 / 804.475 | 994.562 | 998.398 | 1.510× | 1.516× |
| Repository-open control | 830.257 / 1,305.641 | 1,322.446 | 1,082.646 | 1.593× | 1.304× |
| Resident no-op control | 19.650 / 145.290 | 20.241 | 20.891 | 1.030× | 1.063× |

### Large profile: 100,000-commit trunk

| Operation | Scion p50 / p95, µs | Baseline p50, µs | Git p50, µs | Baseline / Scion | Git / Scion |
|---|---:|---:|---:|---:|---:|
| Prepared durable commit | 967.706 / 1,263.390 | 2,200.750 | 2,913.522 | 2.274× | 3.011× |
| Positive ancestry | 970.429 / 1,186.954 | 2,200.608 | 35,124.071 | 2.268× | 36.194× |
| Negative ancestry | 970.199 / 1,277.921 | 2,808.472 | 38,889.853 | 2.895× | 40.084× |
| Ancestor merge base | 898.230 / 1,043.789 | 2,053.747 | 32,357.663 | 2.286× | 36.024× |
| Divergent merge base | 862.837 / 1,020.775 | 2,363.603 | 8,296.653 | 2.739× | 9.616× |
| Fresh file read, 1 KiB | 1,189.358 / 1,562.148 | 2,318.851 | 1,445.924 | 1.950× | 1.216× |
| Fresh one-file text diff | 1,359.774 / 1,832.835 | 3,477.107 | 1,930.002 | 2.557× | 1.419× |
| Resident spread file read | 16.815 / 36.285 | 37.397 | 39.469 | 2.224× | 2.347× |

Every functional median beats Git. Graph queries improve 2.268–2.895× over the
earlier Scion and 9.616–40.084× over Git. Seven of eight functional rows reach 2×
over the baseline; fresh file reads reach 1.950×. The no-op control favors Git in
this run, and none of the three controls reaches 2× improvement.

| Operation | Scion p50 / p95, µs | Baseline p50, µs | Git p50, µs | Baseline / Scion | Git / Scion |
|---|---:|---:|---:|---:|---:|
| Process startup control | 634.734 / 761.204 | 957.680 | 954.505 | 1.509× | 1.504× |
| Repository-open control | 822.967 / 1,018.051 | 1,289.098 | 1,068.757 | 1.566× | 1.299× |
| Resident no-op control | 19.670 / 96.465 | 14.632 | 14.622 | 0.744× | 0.743× |

### Raw evidence

| Profile | Complete JSON | Every sample | Full summary and tails |
|---|---|---|---|
| Default | [JSON](benchmarks/git-comparison-2026-10-25-snapshot-default.json) | [CSV](benchmarks/git-comparison-2026-10-25-snapshot-default.csv) | [Summary](benchmarks/git-comparison-2026-10-25-snapshot-default-summary.csv) |
| Large | [JSON](benchmarks/git-comparison-2026-10-25-snapshot-large.json) | [CSV](benchmarks/git-comparison-2026-10-25-snapshot-large.csv) | [Summary](benchmarks/git-comparison-2026-10-25-snapshot-large-summary.csv) |

```sh
mkdir -p /tmp/scion-benchmarks
PATH=/path/to/git-2.56.0/bin:$PATH CARGO_INCREMENTAL=0 \
  python3 scripts/compare_git.py --static-crt --graph-snapshot --keep-workdir \
  --work-root /tmp/scion-benchmarks \
  --baseline-driver /path/to/preserved/comparison_driver \
  --baseline-report docs/benchmarks/git-comparison-2026-10-25-git-2.56-full.json \
  --output target/git-comparison/snapshot-default
# Add --commits 100000 --branch-commits 10000 for the large profile.
```

A failed large-history attempt with externally copied live database files and
exhausted workspace storage was discarded. Both successful runs above use
isolated temporary fixture directories. Their raw reports contain the complete
measurement evidence; no failed-run timings contribute to these tables.

## Optimized three-way comparison: 25 October 2026

This complete default-profile run uses clean published source
[`3b3110c`](https://github.com/wieslawsoltes/scion/commit/3b3110cf4f9cbc286a64289917707342da8d11e2),
the preserved earlier Scion executable, and Git 2.56.0. Each implementation runs
31 samples after three warmups on the same logical workload: 100,000 file paths,
a 10,000-commit trunk, and 1,000 commits on each divergent arm. The two Scion
stores are constructed independently; all their changeset IDs and query outputs
must agree. All six execution orders rotate through the sample schedule.

The candidate is a verified static-CRT GNU/Linux x86_64 release build. The
baseline is the exact earlier dynamic executable, SHA-256
`ff6a44c736b135d4ec920f4d497a92fa3cac4a2936ff284a6c4abeb1ef6aa6c6`,
verified against its unchanged clean-source report. Startup improvements from
linkage are therefore included in fresh-process results, not presented as purely
algorithmic gains. The JSON records compiler, host, source files, both executable
identities, build command, flags, linkage, and original baseline provenance.

| Operation | Scion p50 / p95, µs | Baseline p50, µs | Git p50, µs | Baseline / Scion | Git / Scion |
|---|---:|---:|---:|---:|---:|
| Prepared durable commit | 1,021.698 / 1,136.911 | 2,053.031 | 2,812.003 | 2.009× | 2.752× |
| Positive ancestry | 1,162.399 / 1,433.547 | 1,507.950 | 3,282.029 | 1.297× | 2.823× |
| Negative ancestry | 1,116.921 / 1,460.057 | 1,757.715 | 3,170.311 | 1.574× | 2.838× |
| Ancestor merge base | 1,061.838 / 1,238.744 | 1,491.815 | 2,914.877 | 1.405× | 2.745× |
| Divergent merge base | 1,200.617 / 1,497.303 | 1,785.717 | 1,561.130 | 1.487× | 1.300× |
| Fresh file read, 1 KiB | 1,039.635 / 1,171.624 | 2,190.586 | 1,287.227 | 2.107× | 1.238× |
| Fresh one-file text diff | 1,141.017 / 1,551.145 | 3,118.914 | 1,611.074 | 2.733× | 1.412× |
| Resident spread file read | 17.116 / 35.143 | 38.208 | 47.241 | 2.232× | 2.760× |

Ratios above one favor the current Scion build. All eight functional medians
beat Git. Four reach the 2× improvement target against the earlier Scion build;
the four graph-query rows remain below it. The durable-commit result is 2.009×,
close to the threshold and subject to ordinary shared-host timing variation.
It retains FULL-synchronous publication and the explicit reference CAS.

### Controls

| Operation | Scion p50 / p95, µs | Baseline p50, µs | Git p50, µs | Baseline / Scion | Git / Scion |
|---|---:|---:|---:|---:|---:|
| Process startup control | 535.656 / 676.448 | 867.596 | 837.911 | 1.620× | 1.564× |
| Repository-open control | 942.859 / 1,717.755 | 1,405.415 | 1,124.342 | 1.491× | 1.192× |
| Resident no-op control | 14.351 / 33.971 | 14.502 | 14.311 | 1.011× | 0.997× |

These controls are measured and retained, never subtracted. The resident no-op
is effectively equal and is slightly faster for Git in this run. None of the
three controls improves by 2×. Fresh file/diff queries use the public consistent
SQLite snapshot API; resident reads retain live visibility. This run does not use
a frozen graph artifact.

### Raw evidence and reproduction

- [JSON report with every sample and provenance](benchmarks/git-comparison-2026-10-25-optimized-full.json)
- [Raw CSV](benchmarks/git-comparison-2026-10-25-optimized-full.csv)
- [Summary CSV, including all p95s and controls](benchmarks/git-comparison-2026-10-25-optimized-full-summary.csv)

```sh
PATH=/path/to/git-2.56.0/bin:$PATH CARGO_INCREMENTAL=0 \
  python3 scripts/compare_git.py --static-crt --keep-workdir \
  --baseline-driver /path/to/preserved/comparison_driver \
  --baseline-report docs/benchmarks/git-comparison-2026-10-25-git-2.56-full.json \
  --output target/git-comparison/optimized-full
```

Use the preserved executable matching the baseline report; compiling a current
worker and relabeling it as the baseline is rejected by the hash check. These
results describe one machine and fixture. Historical results below retain their
original source, setup, and limitations.

## Run it

From the repository root, with Rust, Python 3 and Git on `PATH`:

```sh
CARGO_INCREMENTAL=0 python3 scripts/compare_git.py --quick
CARGO_INCREMENTAL=0 python3 scripts/compare_git.py \
  --output target/git-comparison/full
```

The script builds the release example automatically. On supported GNU/Linux
x64/ARM64 hosts, `--static-crt` builds and verifies the static C runtime variant
used by Linux release bundles; the [release guide](releases.md#reproducing-gnulinux-static-crt-builds)
documents requirements, flags, paths, and runtime limitations. Reports identify
the target, Rust flags, and observed linkage. `--offline` asks Cargo to
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
`--work-root PATH` selects the filesystem containing both repositories. By
default, fixtures use the operating system's temporary directory. Use a local
filesystem with ample free space, outside directories managed by live file
synchronization or backup tools that rewrite files in place. SQLite's database,
WAL, and shared-memory files must remain under SQLite's coordination while the
benchmark runs; copying those files independently is not a consistent backup.
For example, `--work-root /tmp/scion-benchmarks` keeps live fixture databases
separate from a synchronized source checkout. Create that directory first.
`--keep-workdir` retains the fixture, identifiers and worker logs for investigation.
The temporary fixture is removed otherwise. Output consists of a JSON report,
a CSV containing every sample and a separate summary CSV.

To repeat every read/graph query on a retained fixture, select another Git binary
on `PATH` and use the original JSON report:

```sh
PATH=/path/to/git-prefix/bin:$PATH CARGO_INCREMENTAL=0 \
  python3 scripts/compare_git.py \
  --reuse-report target/git-comparison/full.json \
  --samples 31 --warmup 3 --output target/git-comparison/query-repeat
```

The original run must use `--keep-workdir`. Reuse validates the stored fixture
marker, dimensions, identifiers and expected file content. It reapplies the
benchmark Git configuration, repacks objects and writes Git's commit-graph using
the selected executable before measuring all fresh-process queries, process-startup
controls and resident file reads. The Scion store opens read-only. The retained
fixture is never deleted, and the output may not overwrite the original report or
fixture marker. Only `--samples` and `--warmup` can override the saved dimensions.

### Comparing an earlier Scion build

`--baseline-driver PATH --baseline-report PATH` adds the earlier Scion executable
to the same run. Its SHA-256 must match the report from its own clean checkout;
copy the executable outside the candidate's Cargo output directory before
building the candidate. Baseline provenance remains separate from candidate
provenance. To make a new baseline, run the original harness from a clean checkout
of the chosen baseline revision and retain that executable and JSON report.

```sh
python3 scripts/compare_git.py --static-crt --keep-workdir \
  --baseline-driver /path/to/preserved/comparison_driver \
  --baseline-report /path/to/baseline/results.json \
  --output target/git-comparison/three-way
```

The candidate and Git keep alternating order, while the baseline rotates through
all three positions. Full runs create independent Scion stores, send identical
requests, and require identical changeset IDs after every edit and history chain.
This also measures changes in physical storage layout without giving the old
binary the new layout. Query replay preserves those separate stores and validates
their original baseline provenance. A replay from an original two-way report
explicitly uses its one retained Scion store for both binaries. Missing or
mismatched independent baseline fixtures are errors.

These reports explicitly declare `mode: queries_only`; they do not contain new
prepared-commit or resident writer no-op timings. They record the source report's
SHA-256, original Git/source/worker identities and the newly measured source,
Scion worker, Git executable, helper path, configuration and samples. Git's helpers
must be installed under the selected build's prefix: the harness deliberately
removes inherited `GIT_*` variables, including `GIT_EXEC_PATH`.

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
The current worker calls `create_commit_with_blobs`: staged blobs, prepared trees,
changeset, derived metadata, membership, publication log, and the optional
reference CAS share one FULL-synchronous transaction when objects fit the local
inline tier. Previously uploaded blobs retain their independent durability
contract. Larger objects still use synced loose files before membership is
published. Earlier recorded workers used separate blob/ref acknowledgements;
their source and executable hashes identify that implementation.

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
HTTP server. Git likewise operates on local repository storage. The current driver
opens each fresh query through `LocalStore::open_read_snapshot`, which pins the
committed database/WAL view at the first database query and retains it for the
single operation. Resident readers use `open_read_only` and keep live visibility
between requests. Neither mode initializes or implicitly migrates the schema. The fixture
already contains the tree and graph indexes required by these queries. Arbitrary
lazy index creation still requires a writable store.

Before these measurements, the harness closes both writers, repacks Git's objects
and runs `git commit-graph write --reachable --changed-paths`. Git therefore has
its native persisted graph acceleration available, while Scion uses the graph
records it derives during commit creation.
[Git commit-graph documentation](https://git-scm.com/docs/git-commit-graph).

With `--graph-snapshot`, only the four candidate ancestry/merge-base rows use an
explicit prebuilt `GraphSnapshot`. Export duration, file size, metadata, and
SHA-256 are recorded separately, and the file hash is rechecked after measurement.
The preserved baseline and all other rows retain their existing APIs.

The measured operations are:

| Operation | Scion API | Git command / equivalent |
| --- | --- | --- |
| Positive ancestry | `is_ancestor(root, trunk)` | `merge-base --is-ancestor` |
| Negative ancestry | `is_ancestor(side, trunk)` | `merge-base --is-ancestor` |
| Ancestor merge base | `merge_bases(root, trunk)` | `merge-base --all` |
| Divergent merge bases | `merge_bases(left, right)` | `merge-base --all` |
| One-file text diff | `diff(previous, tip)` | `diff --no-ext-diff --no-textconv --no-renames --unified=3` |
| File content read | `read_file(tip, path)` | `show tip:path` |
| Repository-open control | Open `LocalStore` read-only, then no-op | `rev-parse --is-bare-repository` |
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
expected bytes. Current repository caches retain verified roots, trees, point
lookups and small blob contents within one shared byte/entry budget. The earliest
historical baseline predates this additional row; three-way runs record it for
both preserved and candidate workers.

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
It builds the supported static Linux worker and uploads raw JSON and CSV
artifacts. Separate all-feature CI gates test static Linux x64 and ARM64 builds.
CI fails for incorrect results or harness
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

### After durable object batching, 25 October 2026

A diagnostic syscall trace of twelve prepared commits found 48 immutable-file
flushes, 101 directory flushes, 26 WAL flushes and one database flush, including
startup/shutdown. Those instrumented timings are not benchmark samples. The count
identified repeated file/directory publication barriers as a concrete write cost.

The local backend now keeps encoded objects up to 256 KiB in SQLite. The engine
prepares changed directory trees and the changeset before calling
`publish_commit_with_objects`; LocalStore can commit those objects, derived
indexes, membership and log in one `synchronous=FULL` transaction. Independently
acknowledged blobs and reference CAS still have their own durability boundaries.
Large objects remain synced loose files. Buffers are bounded: 32 MiB of staged raw
trees in the engine and 16 MiB of encoded objects in the backend; overflow is
durably written before membership becomes visible. Other storage implementations
inherit a safe object-first fallback through their existing object routing.

This adds a local SQLite table without changing canonical objects, content IDs or
network envelopes. New code reads all old loose objects. Backups must include
SQLite and any live WAL together with the object directory; older binaries do not
understand newly inlined objects. Tests cover transaction rollback, legacy schema
opening, mixed inline/loose reads, concurrent idempotent publication, sharded
object placement and recovery after a process exits without running SQLite's
close-time cleanup. This tests process-exit recovery, not physical power loss.

The unchanged full profile again produced exactly the same fixture identifiers
and contents as both prior runs. Source revision `eb9e5fd` plus working-tree and
executable hashes are recorded. Local compilation and service tests were paused
through the measurement window; the shared-host limitations still apply.

Raw data: [JSON report](benchmarks/git-comparison-2026-10-25-batched.json),
[all samples](benchmarks/git-comparison-2026-10-25-batched.csv), and
[summary CSV](benchmarks/git-comparison-2026-10-25-batched-summary.csv).

| Operation | Scope | Scion median | Git median | Within-run comparison |
| --- | --- | ---: | ---: | --- |
| Prepared durable commit | Resident | 2.483 ms | 3.150 ms | Scion 1.27× faster |
| Positive ancestry | Fresh process | 2.244 ms | 4.848 ms | Scion 2.16× faster |
| Negative ancestry | Fresh process | 2.943 ms | 5.142 ms | Scion 1.75× faster |
| Ancestor merge base | Fresh process | 2.362 ms | 4.815 ms | Scion 2.04× faster |
| Divergent merge base | Fresh process | 2.900 ms | 2.645 ms | Git 1.10× faster |
| One-file text diff | Fresh process | 5.143 ms | 2.537 ms | Git 2.03× faster |
| File read, 1 KiB | Fresh process | 2.885 ms | 1.754 ms | Git 1.65× faster |
| File reads across warmed paths | Resident | 0.0729 ms | 0.0793 ms | Scion 1.09× faster |

Prepared-commit median improved from 6.433 ms to 2.483 ms, about 2.59× within
Scion, and was 1.27× faster than Git's median in this run. Its p95 was 5.798 ms
versus Git's 4.546 ms, so that median win does not extend to the tail. The fresh
diff median rose from 3.659 ms to 5.143 ms and remained slower than Git; fresh file
reads and divergent merge bases also remained losses. These results support the
write-batching change, while leaving query/open and tree-processing costs as
measured optimization targets. Every earlier run and every sample is retained.


### Streaming trees and read-only opening, 25 October 2026

Both final measurement profiles were built from clean public commit
[`f06cc594bb1603dc837a9379f646e1fd1c06827b`](https://github.com/wieslawsoltes/scion/commit/f06cc594bb1603dc837a9379f646e1fd1c06827b).
Their recorded core-source hashes and release executable hashes are identical;
`source.dirty` and `build.source_changed_during_build` are false in both reports.
The default profile reproduces the exact fixture contents and commit identifiers
from all earlier full runs. The larger-history profile preserves the same files,
prepared edits and initial commit identifiers while extending the graph workload.

The measured source adds a streaming decoder for canonical tree objects and
borrowed path validation, avoiding intermediate enum buffering and per-name string
clones. Unknown or noncanonical JSON shapes fall back to the unchanged derived
Serde decoder. Differential tests and the canonical fuzz target compare accepted
inputs and normalized values with that original decoder. The engine and client
use the same public `Tree::from_json` method; canonical bytes and IDs are unchanged.

Read queries now use the explicit read-only LocalStore opener and cached SQL
statements. Publication also verifies existing immutable object envelopes before
accepting a duplicate. The bundled database is SQLite 3.51.3, which includes the
[upstream WAL-reset corruption fix](https://sqlite.org/releaselog/3_51_3.html);
the durability settings remain WAL with `synchronous=FULL`. Local builds and service tests were paused for both measurement
windows. The host was still shared: AMD EPYC 9V74, nine visible logical CPUs,
approximately 9.7 GiB RAM, Linux x86_64 with an overlay filesystem, Rust 1.99.1 and
Git 2.51.1. These are observed workload results, not an isolated causal experiment
for an individual optimization.

#### Default history: 100,000 files and 10,000 trunk commits

This uses the unchanged default dimensions: 1,000 commits per divergent arm,
31 samples and three warmups. Both p50 and p95 are shown in milliseconds, including
controls and the resident read row. Ratios use unrounded p50 values.

Raw data: [JSON report](benchmarks/git-comparison-2026-10-25-final.json),
[all samples](benchmarks/git-comparison-2026-10-25-final.csv), and
[summary CSV](benchmarks/git-comparison-2026-10-25-final-summary.csv).

| Operation and scope | Scion p50 / p95 (ms) | Git p50 / p95 (ms) | Ratio of medians |
| --- | ---: | ---: | --- |
| Prepared durable commit · resident | 2.101 / 2.944 | 3.054 / 3.756 | Scion 1.45× faster |
| Positive ancestry · fresh process | 1.792 / 4.037 | 3.663 / 8.501 | Scion 2.04× faster |
| Negative ancestry · fresh process | 2.067 / 2.968 | 3.541 / 4.328 | Scion 1.71× faster |
| Ancestor merge base · fresh process | 1.700 / 2.224 | 3.279 / 4.354 | Scion 1.93× faster |
| Divergent merge base · fresh process | 1.939 / 2.167 | 1.784 / 1.928 | Git 1.09× faster |
| One-file text diff · fresh process | 3.251 / 3.729 | 1.844 / 2.126 | Git 1.76× faster |
| File read, 1 KiB · fresh process | 2.372 / 2.618 | 1.496 / 1.695 | Git 1.59× faster |
| File reads across warmed paths · resident | 0.042 / 0.083 | 0.043 / 0.058 | Scion 1.03× faster |
| Repository-open control · fresh process | 1.429 / 1.924 | 1.258 / 1.743 | Git 1.14× faster |
| Process-startup control · fresh process | 0.959 / 1.056 | 1.005 / 1.175 | Scion 1.05× faster |
| No-op control · resident | 0.015 / 0.030 | 0.020 / 0.068 | Scion 1.34× faster |

Prepared durable commits are 1.45× faster than Git at the median in this run, and
their p95 is also lower. Positive/negative ancestry and ancestor merge-base queries
are 1.71–2.04× faster. Git remains faster for fresh file reads, text diffs and the
1,000-commit divergent merge-base workload. The resident read medians differ by
about 3%, with a worse Scion p95, so that row should be treated as near parity.

Relative to the preceding Scion batching run, the fresh diff median falls from
5.143 to 3.251 ms and the fresh file-read median from 2.885 to 2.372 ms. Git's
corresponding timings also change between runs; these differences must not be
attributed entirely to the decoder or read-only opener.

#### Larger history: 100,000 files and 100,000 trunk commits

This is an additional scaling profile, not a replacement for the default result.
Only the trunk length and divergent-arm lengths change; each arm has 10,000
commits. It retains all measured rows, 31 samples, three warmups and the same
storage, durability and Git commit-graph configuration.

```sh
CARGO_INCREMENTAL=0 python3 scripts/compare_git.py \
  --commits 100000 --branch-commits 10000 \
  --output target/git-comparison/large-history
```

Raw data: [JSON report](benchmarks/git-comparison-2026-10-25-large-history.json),
[all samples](benchmarks/git-comparison-2026-10-25-large-history.csv), and
[summary CSV](benchmarks/git-comparison-2026-10-25-large-history-summary.csv).

| Operation and scope | Scion p50 / p95 (ms) | Git p50 / p95 (ms) | Ratio of medians |
| --- | ---: | ---: | --- |
| Prepared durable commit · resident | 1.883 / 2.510 | 2.838 / 3.990 | Scion 1.51× faster |
| Positive ancestry · fresh process | 2.986 / 4.875 | 49.570 / 84.075 | Scion 16.60× faster |
| Negative ancestry · fresh process | 3.325 / 5.355 | 48.414 / 62.689 | Scion 14.56× faster |
| Ancestor merge base · fresh process | 2.555 / 3.713 | 37.958 / 52.857 | Scion 14.86× faster |
| Divergent merge base · fresh process | 2.881 / 4.231 | 9.727 / 12.531 | Scion 3.38× faster |
| One-file text diff · fresh process | 3.586 / 4.176 | 2.060 / 2.510 | Git 1.74× faster |
| File read, 1 KiB · fresh process | 2.595 / 3.208 | 1.769 / 2.117 | Git 1.47× faster |
| File reads across warmed paths · resident | 0.045 / 0.097 | 0.042 / 0.087 | Git 1.07× faster |
| Repository-open control · fresh process | 1.815 / 2.572 | 1.469 / 3.325 | Git 1.24× faster |
| Process-startup control · fresh process | 1.056 / 1.259 | 1.119 / 1.416 | Scion 1.06× faster |
| No-op control · resident | 0.015 / 0.039 | 0.015 / 0.026 | Scion 1.02× faster |

At this graph size, positive ancestry is 16.60× faster than Git, negative ancestry
14.56× faster, ancestor merge-base lookup 14.86× faster and the divergent
merge-base query 3.38× faster at the median. Their p95 values also favor Scion. This
is the measured advantage of Scion's persisted graph skips for these long linear
histories, compared with Git's native commit-graph acceleration on the same DAG.
The functional DAG oracle and counted-I/O regression tests remain separate from
these timing observations.

Prepared commits are measured **before** constructing the long history in both
profiles. Their 1.51× median advantage in this second run therefore confirms another
measurement of the same prepared-edit workload; it does not establish that commit
latency improves as history grows. Fresh reads and diffs remain Git wins, and Git
also leads the resident read row in this run. All samples and tails are retained.

The evidence supports substantially faster graph queries on the larger fixture
and faster durable prepared commits in both final runs. It does not establish a
universal speed advantage, network/server throughput, cold-cache performance,
large binary-file behavior or production fleet scale. Fresh-process file and tree
work remains an area for further improvement.


### Git 2.56.0 follow-up, 25 October 2026

The earlier runs used the host's installed Git 2.51.1. To check version sensitivity,
the full default profile and all larger-history queries were repeated against
upstream [Git v2.56.0](https://github.com/git/git/tree/v2.56.0). Both were
recorded from clean public Scion commit
[`78f3b0d21e2539b6fb6197e08749860ab5db1385`](https://github.com/wieslawsoltes/scion/commit/78f3b0d21e2539b6fb6197e08749860ab5db1385).
The Scion release executable's SHA-256 is identical to both preceding full runs:
`ff6a44c736b135d4ec920f4d497a92fa3cac4a2936ff284a6c4abeb1ef6aa6c6`.
The source and executable hashes match between the two new reports as well, and
both reports record a clean tree with no source change during the build.

#### Git build and reproduction

Git was built from official tag `v2.56.0` (tag object
`2478544319491bbd2a42cc57a83e3d2f4730a280`), source commit
[`a018953688f1b10bddf91bff8747068f5f4746a4`](https://github.com/git/git/commit/a018953688f1b10bddf91bff8747068f5f4746a4).
It used the Makefile's default C optimization (`-O2`) and default enabled Rust
build with Rust 1.99.1. Optional Tcl/Tk, localization, Perl, Python, curl, Expat and
OpenSSL integrations were disabled. These optional integrations are not used by
any measured local command. The binaries and helpers were installed under
`/tmp/scion-git-current`; the reports record the actual binary path, SHA-256 and
compiled helper path rather than relying on a version label alone.

Equivalent build commands, with a C/C++ toolchain, Rust, Make and zlib development
files available, are:

```sh
git clone --depth 1 --branch v2.56.0 https://github.com/git/git.git /tmp/scion-git-source
cd /tmp/scion-git-source
make -j4 prefix=/tmp/scion-git-current \
  NO_TCLTK=YesPlease NO_GETTEXT=YesPlease NO_PERL=YesPlease NO_PYTHON=YesPlease \
  NO_CURL=YesPlease NO_EXPAT=YesPlease NO_OPENSSL=YesPlease
make install prefix=/tmp/scion-git-current \
  NO_TCLTK=YesPlease NO_GETTEXT=YesPlease NO_PERL=YesPlease NO_PYTHON=YesPlease \
  NO_CURL=YesPlease NO_EXPAT=YesPlease NO_OPENSSL=YesPlease
```

The measured Git binary's SHA-256 is
`652ab642ac26b3c5441dc5a73950bcfe252ff8c411f76ab2ba0e4a3cc4fd16be`.
`git --version --build-options` reports Rust enabled, zlib 1.3, `SHA1_DC`,
`SHA256_BLK`, the `files` reference format and default SHA-1 objects. The benchmark
continues to use Git SHA-1 and Scion BLAKE3, as in all earlier runs. From a clean
Scion checkout, the measured scopes can be reproduced with:

```sh
PATH=/tmp/scion-git-current/bin:$PATH CARGO_INCREMENTAL=0 \
  python3 scripts/compare_git.py --keep-workdir \
  --output target/git-comparison/git256-full

# First generate and retain the larger fixture if it is not already available.
PATH=/tmp/scion-git-current/bin:$PATH CARGO_INCREMENTAL=0 \
  python3 scripts/compare_git.py --commits 100000 --branch-commits 10000 \
  --keep-workdir --output target/git-comparison/large-fixture

PATH=/tmp/scion-git-current/bin:$PATH CARGO_INCREMENTAL=0 \
  python3 scripts/compare_git.py \
  --reuse-report target/git-comparison/large-fixture.json --samples 31 --warmup 3 \
  --output target/git-comparison/git256-large-queries
```

The recorded larger-history follow-up reused the already verified Git 2.51.1
fixture. Its JSON records that original report's exact SHA-256 and unchanged
fixture identifiers. Git 2.56.0 repacking and commit-graph preparation ran before
all new query timings. Recreating the fixture with the commands above generates
the same logical workload but does not reproduce the earlier run's physical cache
state or exact timings.

#### Full default profile against Git 2.56.0

This is a new full run with 100,000 files, 10,000 trunk commits, 1,000 commits per
arm, 31 samples and three warmups. Its generated fixture identifiers match the
preceding default runs exactly. All 682 individual samples are retained.

Raw data: [JSON report](benchmarks/git-comparison-2026-10-25-git-2.56-full.json),
[all samples](benchmarks/git-comparison-2026-10-25-git-2.56-full.csv), and
[summary CSV](benchmarks/git-comparison-2026-10-25-git-2.56-full-summary.csv).

| Operation and scope | Scion p50 / p95 (ms) | Git 2.56.0 p50 / p95 (ms) | Ratio of medians |
| --- | ---: | ---: | --- |
| Prepared durable commit · resident | 2.279 / 3.114 | 2.879 / 3.833 | Scion 1.26× faster |
| Positive ancestry · fresh process | 2.084 / 2.982 | 4.145 / 5.852 | Scion 1.99× faster |
| Negative ancestry · fresh process | 2.461 / 3.075 | 4.036 / 6.662 | Scion 1.64× faster |
| Ancestor merge base · fresh process | 1.846 / 2.650 | 3.329 / 4.496 | Scion 1.80× faster |
| Divergent merge base · fresh process | 2.396 / 3.192 | 1.969 / 4.160 | Git 1.22× faster |
| One-file text diff · fresh process | 3.626 / 4.558 | 1.856 / 2.098 | Git 1.95× faster |
| File read, 1 KiB · fresh process | 2.533 / 3.452 | 1.574 / 2.515 | Git 1.61× faster |
| File reads across warmed paths · resident | 0.039 / 0.050 | 0.042 / 0.075 | Scion 1.08× faster |
| Repository-open control · fresh process | 1.621 / 2.742 | 1.180 / 1.529 | Git 1.37× faster |
| Process-startup control · fresh process | 0.989 / 1.444 | 1.030 / 1.291 | Scion 1.04× faster |
| No-op control · resident | 0.021 / 0.053 | 0.015 / 0.032 | Git 1.40× faster |

The prepared durable commit median is 1.26× faster than Git 2.56.0, with lower
p95 as well. The measured ancestry and ancestor merge-base cases are 1.64–1.99×
faster. Git remains faster at the median for the short divergent-base query,
fresh file reads and fresh text diffs. The resident read median favors Scion by
about 8%; this modest difference is kept separate from the larger graph results.

#### Larger-history queries against Git 2.56.0

This run retains the 100,000-file, 100,000-trunk-commit, 10,000-commits-per-arm
fixture and measures **every query/control row**, with 31 samples and three
warmups. The report explicitly declares `mode: queries_only`; its 558 samples
contain no new prepared-commit or resident writer no-op measurements. No earlier
Git 2.51.1 commit timing is presented as a Git 2.56.0 result.

Raw data: [JSON report](benchmarks/git-comparison-2026-10-25-git-2.56-large-queries.json),
[all samples](benchmarks/git-comparison-2026-10-25-git-2.56-large-queries.csv), and
[summary CSV](benchmarks/git-comparison-2026-10-25-git-2.56-large-queries-summary.csv).

| Operation and scope | Scion p50 / p95 (ms) | Git 2.56.0 p50 / p95 (ms) | Ratio of medians |
| --- | ---: | ---: | --- |
| Positive ancestry · fresh process | 2.342 / 2.829 | 43.704 / 57.218 | Scion 18.66× faster |
| Negative ancestry · fresh process | 3.017 / 3.479 | 42.148 / 51.351 | Scion 13.97× faster |
| Ancestor merge base · fresh process | 2.307 / 2.741 | 30.971 / 36.698 | Scion 13.42× faster |
| Divergent merge base · fresh process | 2.258 / 2.811 | 6.081 / 7.174 | Scion 2.69× faster |
| One-file text diff · fresh process | 3.326 / 4.024 | 1.562 / 1.704 | Git 2.13× faster |
| File read, 1 KiB · fresh process | 2.313 / 2.642 | 1.296 / 1.614 | Git 1.78× faster |
| File reads across warmed paths · resident | 0.051 / 0.075 | 0.054 / 0.066 | Scion 1.06× faster |
| Repository-open control · fresh process | 1.405 / 1.896 | 1.094 / 1.178 | Git 1.28× faster |
| Process-startup control · fresh process | 0.898 / 1.025 | 0.843 / 1.058 | Git 1.06× faster |

Positive ancestry is 18.66× faster than Git 2.56.0 at the median, negative ancestry
13.97× faster, ancestor merge-base lookup 13.42× faster and the divergent
merge-base query 2.69× faster. The corresponding p95 values also favor Scion.
The version follow-up therefore retains the large graph-query advantage on this
fixture. It also retains the losses: Git is 1.78× faster for fresh file reads and
2.13× faster for fresh text diffs. Resident reads are close at the median, and
Scion has the worse p95 in that row.

These current-version results support the same scoped conclusion as the earlier
runs: strong graph-query performance on the long linear histories and a smaller
durable prepared-commit advantage, with material fresh-read and diff work still
to improve. All versions, workloads, controls, raw samples and tail latencies
remain published; they do not establish universal superiority over Git.
