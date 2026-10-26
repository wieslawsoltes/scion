# Metadata compression performance investigation

## Production result, 26 October 2026

The production implementation at clean published source
[`bd393c7`](https://github.com/wieslawsoltes/scion/commit/bd393c7cd064d03d231d6b0af8a05afb181f13c0)
completed the fixed evaluation. Across the five full default-profile runs, all
eight functional operations beat Git 2.56.0 and improve at least 2× over the
preserved Scion baseline when comparing **medians of run p50 values**. Prepared
durable commits improve **2.2165× over the baseline and 3.3610× over Git**:
1,022.379 µs for current Scion, 2,266.107 µs for baseline Scion, and 3,436.183 µs
for Git. The candidate's median of run p95 values is 1,322.540 µs; this is a
separate statistic, not a percentile of pooled samples.

These comparisons measure the complete current production revision, including
the previously merged publication, query, and caching improvements. They do not
isolate the frame cache's incremental contribution. The earlier prototypes and
unfavorable observations below remain part of the investigation record.

### Implemented policy and correctness

The reusable [`scion-store` codec](../crates/scion-store/README.md#codec-reuse)
contains the private [metadata frame cache](../crates/scion-store/src/codec/metadata_cache.rs).
Trees, changesets, and derived metadata from 64 KiB through 256 KiB are eligible
for reuse of standard independent 8 KiB Zstandard level-one frames. The first
eligible input uses whole-frame encoding. Its encoded/raw ratio must lie in
[1/8, 7/8] to retain an admission candidate. A later input must match at least 75%
of its bytes at fixed chunk boundaries, including the final partial chunk,
before framing is attempted. Unrelated input returns to whole-frame encoding;
a framed result outside the ratio range falls back to whole-frame encoding too.

One-shot BLAKE3 keys identify possible frame matches. Every hit also requires
complete raw-byte equality, so a key collision cannot authorize reuse of the
wrong content. Only completed successful compression can replace the admission
base. Successfully completed valid frames may remain after an encoding error,
while the failed candidate never becomes the comparison base.

A lazy vector index holds at most 64 frames. Its **1 MiB accounted retention
budget per encoding thread** includes the previous raw candidate's capacity,
raw and encoded frame capacities, the actual index allocation, 512 bytes per
frame, and a 128-byte control allowance. Eviction and clock rollover are bounded.
This is a retention budget, not a cap on process RSS, temporary input/output
buffers, or Zstandard's separate reusable context. Decode-only threads, blobs,
and small metadata do not allocate the frame index.

Canonical bytes and domain-separated object IDs stay unchanged. Ordinary
concatenated Zstandard frames fit the existing version-one envelope, and both
current and legacy streaming decoders verify them. Small metadata, level-three
file blobs, and the large streaming path preserve their existing policies.
Stored-object reads and duplicate publications still verify complete content
hashes; the cache supplies no object reads, membership, or durability
acknowledgments. FULL-synchronous publication is unchanged. Compressed bytes may
vary with prior encodes on a thread while the decoded content and ID remain
identical.

The [cache regressions](../crates/scion-store/src/codec/metadata_cache_tests.rs)
and [envelope/backend regressions](../crates/scion-store/src/codec/metadata_integration_tests.rs)
cover admission and tails, exact matches and collisions, allocation and entry
bounds, eviction and rollover, encoding failures, unchanged ineligible paths,
legacy decoding, alternate valid representations, and corrupt-duplicate
transaction rollback. The production vector index differs from the diagnostic
prototype's tree index.

### Complete fixed evaluation and its limits

The [declared evaluation plan](benchmarks/metadata-2026-10-26-evaluation-plan.json)
was written before the first replacement measurement. A temporary environment
reset had removed the unfinished earlier batch's raw evidence after three
reported completions and the start of trial four. Those partial timings are
unavailable and are **excluded**, not reconstructed or combined with the new
batch. The new plan fixes five full default runs, five alternating MemoryStore
trials, and a new full large-history fixture followed by its query-only replay.
All commands ran sequentially with prebuilt binaries, source/artifact identity
guards, and no concurrent builds, tests, or source edits. The
[stage events](benchmarks/metadata-2026-10-26-evaluation-events.jsonl),
[completed result](benchmarks/metadata-2026-10-26-evaluation-result.json), and
[evidence index](benchmarks/metadata-2026-10-26-evidence-index.json) retain the
commands, outputs, hashes, and publication paths. JSON, JSONL, logs, and diagnostic
source are copied verbatim; the index separately records CSV line-ending
normalization and both original and published hashes.

The five default runs use 100,000 paths, 10,000 trunk commits, 1,000 commits on
each branch, 31 measured samples, and three warmups. Candidate Scion, preserved
Scion, and Git rotate through all six execution orders with independent stores.
The following ratios divide each implementation's median of the five run p50
values. The final column retains the range of the five independently calculated
baseline/current ratios.

| Functional operation | Baseline / current Scion | Git / current Scion | Per-run baseline / current range |
|---|---:|---:|---:|
| Prepared durable commit | 2.2165× | 3.3610× | 2.1905–2.4268× |
| Positive ancestry | 2.3192× | 4.7698× | 2.2371–2.4370× |
| Negative ancestry | 2.6625× | 4.6541× | 2.6625–2.7910× |
| Ancestor merge base | 2.4032× | 4.3575× | 2.2587–2.4803× |
| Divergent merge base | 2.8013× | 2.5279× | 2.6135–2.8565× |
| Fresh-process file read | 2.0155× | 1.1197× | 1.8626–2.2715× |
| Resident spread file reads | 2.2120× | 2.4161× | 1.8150–2.2428× |
| Small diff | 2.5217× | 1.2306× | 2.4551–2.7975× |

Source: [strict five-run summary](benchmarks/git-comparison-2026-10-26-metadata-summary.json)
and full raw runs
[1](benchmarks/git-comparison-2026-10-26-metadata-run-1.json),
[2](benchmarks/git-comparison-2026-10-26-metadata-run-2.json),
[3](benchmarks/git-comparison-2026-10-26-metadata-run-3.json),
[4](benchmarks/git-comparison-2026-10-26-metadata-run-4.json), and
[5](benchmarks/git-comparison-2026-10-26-metadata-run-5.json).

The aggregate meets the functional target, but individual runs can miss it.
Fresh-process file reads improve only 1.8626× in run three, and resident spread
reads improve 1.8150× in run five. All functional run medians still beat Git.
No-op, process startup, and repository open are retained controls; their
aggregate baseline/current ratios are **1.0171×, 1.4883×, and 1.7194×**. No-op
is slightly slower than the baseline in run two (0.9969×). A blanket claim that
every benchmark or every run improved 2× would therefore be incorrect.

The old large-history fixture was also lost. The declared plan builds a new
fixture with 100,000 paths, 100,000 trunk commits, 10,000 commits per branch,
and 120,036 total commits, then replays queries against that new report:

| Additional workload | Observed baseline / current improvement | Observed Git / current improvement |
|---|---:|---:|
| [Large full comparison: eight functional operations](benchmarks/git-comparison-2026-10-26-metadata-large-full.json) | 2.1105–3.0698× | 1.1807–49.5177× |
| [Large query-only replay: seven functional operations](benchmarks/git-comparison-2026-10-26-metadata-large-queries.json) | 2.0888–3.3129× | 1.1897–52.5419× |
| [Original MemoryStore workload: all eleven cases](benchmarks/scaling-2026-10-26-metadata.json) | 2.2326–34.2457× | Not measured against Git |

Large-history rows are individual 31-sample reports, not five-run aggregates.
The full report's prepared-commit p50 is 1,048.819 µs, a 2.1401× baseline and
3.3498× Git improvement. Its no-op control is slower than both Git and baseline
(0.9573× and 0.9685× respectively). The query replay does not remeasure commits
or no-op. Process-startup and repository-open controls remain below 2× in both
large reports. MemoryStore ratios divide medians of five trial p50 values from
the unchanged original harness; initial commits have one observation per trial,
and individual operation samples are unavailable for pooling.

Only graph queries use the explicit frozen graph snapshot; other operations use
the ordinary repository APIs. Preparing the large snapshot takes 6.027 seconds
in the full run and 6.063 seconds in the replay, with 96,467,328 bytes in each
artifact. Git's repack and commit-graph preparation are recorded too. These setup
costs are outside query timing and matter when deciding whether a frozen
snapshot fits a workload. See [the complete comparison](git-comparison.md) for
all p50/p95 observations, controls, setup costs, and benchmark scope.

The [restored Scion baseline](benchmarks/durable-baseline-2026-10-26-recovery.json)
is again byte-identical to the original executable with SHA-256
`ff6a44c736b135d4ec920f4d497a92fa3cac4a2936ff284a6c4abeb1ef6aa6c6`.
The [new Git build record](benchmarks/git-2.56-2026-10-26-build.json) uses the
official Git 2.56.0 release archive, Rust 1.99.1, and the same build options.
Its binary SHA-256 is
`6b0fe38de2fe4331c56251819920c58820f8dfffd9e342b9a854bfffbbd67935`,
which differs from the earlier clone-based Git binary. Each new measurement
records this new identity; old reports remain unchanged.

### Codec tradeoffs remain explicit

The adaptive **prototype**, measured separately below, adds 3.43% to encoded
bundle bytes and about 20 µs to full decode/verification work on its extracted
corpus. Its first admitted encode also pays frame-construction cost:
304.888 µs versus the whole-frame control's 220.471 µs in that admission
observation. These are codec-only observations from the prototype, not new
production end-to-end storage-size or decoding guarantees. The production
comparison measures complete operations with their actual SQLite and durability
costs, and the all-operation improvements above include prior optimizations.
Extra frame boundaries and admission costs remain relevant for one-off or
unrelated metadata; bounded exact reuse is intended for successive similar
edits. The following historical sections retain the alternatives and failures
that informed the policy.

## Target and evaluation contract

The complete durable comparison at clean source
[`991fdc9`](https://github.com/wieslawsoltes/scion/commit/991fdc9c36190104019840110c69a689edc9e5a1)
beats Git in every functional median. Prepared commits improve 1.833× over the
preserved Scion baseline and remain below the requested 2× target in that run.
Its candidate p50 is 1,066.705 µs and baseline p50 is 1,954.782 µs, leaving an
89.314 µs reduction to reach 2× against that observation.

Any further optimization must preserve complete canonical bytes and object IDs,
version-1 envelope compatibility, bounded retained memory, duplicate verification,
FULL-synchronous publication, and the original benchmark workloads. Changes belong
in reusable production crates. Diagnostic examples are not product dependencies.

After a candidate passes correctness checks, evaluate one fixed batch of **five
complete default-profile comparisons**, retaining every result. Each run keeps
31 samples, three warmups, independent candidate/baseline stores, all six execution
order permutations, Git 2.56.0, and source/build/hash checks. Report the median of
run p50 values and the range of individual run results; do not pool reconstructed
samples or stop when a threshold happens to pass. Keep startup/open/no-op controls
and all earlier observations visible. Re-run the unchanged five-trial MemoryStore
workload and the large-history query replay to check affected paths.

## Summarize the fixed batch

After all five complete reports exist, use the strict read-only summarizer:

```sh
python3 scripts/summarize_git_runs.py \
  target/run-1.json target/run-2.json target/run-3.json \
  target/run-4.json target/run-5.json \
  --output target/five-run-summary.json
```

It requires exactly five reports by default, verifies all 33 complete sample
groups per report, recomputes stored percentiles and ratios, and rejects mixed
source/build/Git/baseline identities or comparison settings. Duplicate reports,
including reserialized copies, and output overwrites are rejected. The summary
retains every run and all controls, records input and summarizer hashes, and
labels medians of run p50 and run p95 values separately. Ratios divide median
run p50 values; individual operation latencies are not pooled or reconstructed.

## Rejected simple parameter changes

A codec-only probe uses the actual 1,000-entry root, 100-entry edited directory,
small changeset, and unchanged level-three blob copied from a retained generated
fixture. It measures 31 rotating forward/reverse trials of 32 calls per object
and fully decodes and hash-checks every representation. The following numbers
are **medians of trial means**, not durable-commit latency percentiles.

| Metadata setting | Encode time | Encoded bytes | Size versus current level 1 | Decode and full verification |
|---|---:|---:|---:|---:|
| Level 1 | 212.145 µs | 39,528 | 1.000× | 81.124 µs |
| Level 1, target length 4 | 88.083 µs | 77,348 | 1.957× | 50.898 µs |
| Level 1, target length 8 | 71.561 µs | 82,632 | 2.090× | 51.781 µs |
| Level 1, target length 16 | 58.068 µs | 93,719 | 2.371× | 51.288 µs |
| Level −1 | 145.859 µs | 74,874 | 1.894× | 53.822 µs |
| Level −2 | 116.566 µs | 76,294 | 1.930× | 54.345 µs |

The faster settings save codec CPU but nearly double or more than double metadata
bytes. Additional SQLite/WAL work could consume the saving. They are not adopted
as production defaults on this evidence. The ordinary blob representation stays
byte-identical at level three, and every tested representation passes complete
content verification.

Evidence: [raw measurements](benchmarks/codec-parameters-2026-10-25.json),
[summary and source references](benchmarks/codec-parameters-2026-10-25-summary.json),
[build provenance](benchmarks/codec-parameters-2026-10-25-build.json), and
[copied-corpus provenance](benchmarks/codec-parameters-2026-10-25-corpus.json).
The [probe source](../scripts/diagnostics/codec_parameters.rs) and
[corpus extractor](../scripts/diagnostics/extract_codec_corpus.py) retain the
experiment's implementation. Their hashes are recorded in the evidence; the
source was a separate untracked example over clean base source 991fdc9, not the
published production executable.

## Bounded frame-reuse candidate

The next candidate tests independent standard Zstandard frames for moderately
sized metadata, with exact raw-byte checks and bounded reuse of unchanged chunks.
It must keep the complete Scion envelope and full-object hash verification,
leave blobs and the streaming path unchanged, and retain only successfully
encoded frames. Inputs outside the measured eligibility range keep the current
whole-frame encoding.

The diagnostic compares 4/8/16 KiB chunks and 32/64 KiB lower thresholds, with a
256 KiB upper input limit and at most 1 MiB of retained chunk storage. Accounting
includes raw and encoded vector capacities plus index overhead; an entry limit
bounds small-object bookkeeping. A complete eligible input must fit in the
cache under conservative capacity accounting, to avoid a sequential scan that
continually evicts its own reusable chunks.

Measure cold misses, successive actual edits, encoded size, complete decode/hash
cost, cache hits, and cross-object pressure before selecting a production
configuration. Compatibility alone does not establish an end-to-end improvement.

## First frame-reuse result: retain as an unfavorable observation

The first frame prototype runs eleven fixed trials over 34 actual sequential
edit bundles, with rotating configuration order. It measures 31 edits after
three warmups, nine cold-cache encodes, nine checked decodes, and mixed-pressure
controls per trial. Codec contexts are primed equally. Times below are medians
of per-trial p50 values. Encoding excludes verification, but every output is
subsequently checked by the complete production decoder and the legacy streaming
decoder. This is a codec experiment; it measures no SQLite writes or durability.

| Setting | Successive-edit encoding | CPU saved | Encoded bundle | Cold-cache encoding | Checked decoding |
|---|---:|---:|---:|---:|---:|
| Current level 1 | 205.249 µs | — | 39,655 B | 204.608 µs | 79.490 µs |
| 4 KiB chunks, 32 KiB minimum | 137.708 µs | 67.541 µs | 41,922 B | 380.452 µs | 133.681 µs |
| 8 KiB chunks, 32 KiB minimum | 120.081 µs | 85.168 µs | 41,015 B | 318.119 µs | 100.001 µs |
| 16 KiB chunks, 32 KiB minimum | 112.880 µs | 92.369 µs | 40,858 B | 278.849 µs | 90.366 µs |

The 64 KiB minimum variants select the same objects in this corpus; their full
results remain in the report. Although 16 KiB chunks save 92.369 µs of encoding,
they add 3.0% to encoded size, 36.3% to cold encoding, and 13.7% to checked
decoding. Repetitive, random, and mixed-pressure controls also expose substantial
penalties. This unconditional framing policy is not adopted.

The next diagnostic tests whole-frame encoding for cold or unrelated inputs,
exact-byte reuse before admission, one-shot chunk lookup hashes, and a compression
ratio gate to avoid already-cheap repetitive or incompressible inputs. The
previous raw comparison buffer must share the cache budget. The first three
admission observations must be retained explicitly; the initial prototype did
not retain its three warmup observations.

Evidence: [all first-prototype samples](benchmarks/metadata-frames-2026-10-25-first.json),
[summary and controls](benchmarks/metadata-frames-2026-10-25-first-summary.json),
[build provenance](benchmarks/metadata-frames-2026-10-25-first-build.json),
[corpus provenance](benchmarks/metadata-frames-2026-10-25-corpus.json),
[exact prototype source](../scripts/diagnostics/metadata_frames_first.rs), and
[extraction script](../scripts/diagnostics/extract_metadata_edits.py).
Its retention metric charges actual vector capacities plus an estimated 256 bytes
per entry; production accounting needs a more conservative explicit index bound.
Unchanged large/streaming controls vary with context and allocator history and
must not be interpreted as implementation improvements.

## Adaptive frame-reuse diagnostic

The second prototype keeps whole-frame encoding until a later eligible object
matches at least 75% of its bytes at fixed chunk boundaries. Only a previous
whole-frame compressed/raw ratio between 1/8 and 7/8 qualifies for retention.
This avoids copying already-cheap repetitive or incompressible inputs. Small
objects, blobs, and inputs above the eligibility limit do not displace the
comparison base. A framed result outside the ratio range falls back to ordinary
encoding and resets admission.

Chunk keys use one-shot BLAKE3, followed by exact raw-byte equality on every hit.
The previous raw object shares the frame cache's 1 MiB retention budget. The
prototype charges actual vector capacities, 512 bytes per frame, a 4 KiB index
reserve, and 128 bytes for previous-buffer bookkeeping. All eleven fixed trials
and all 5,852 observations are retained; no trial was selected or discarded.

| Setting | Successive-edit encoding | CPU saved | Encoded bundle | Cold encoding | Checked decoding |
|---|---:|---:|---:|---:|---:|
| Current level 1 | 208.293 µs | — | 39,655 B | 211.868 µs | 79.479 µs |
| 4 KiB chunks, 64 KiB minimum | 93.090 µs | 115.203 µs | 41,922 B | 207.962 µs | 133.070 µs |
| 8 KiB chunks, 64 KiB minimum | 83.606 µs | 124.687 µs | 41,015 B | 209.585 µs | 99.519 µs |
| 16 KiB chunks, 64 KiB minimum | 93.050 µs | 115.243 µs | 40,858 B | 213.781 µs | 90.166 µs |

The 8 KiB / 64 KiB candidate saves 124.687 µs of encoding in this corpus, with
3.43% more encoded bytes and about 20 µs more complete decode/verification work.
Its cold difference is within observed timing variation. The random, repetitive,
and mixed-pressure controls now remain whole-frame, retain zero cache bytes, and
have approximately baseline repeated-encoding cost. Unchanged large-object and
streaming controls remain observations of allocator/context variation.

Admission has a real first-use cost:

| Configuration | First whole-frame encoding | First frame construction | Next cached encoding |
|---|---:|---:|---:|
| Current level 1 | 205.159 µs | 220.471 µs | 206.691 µs |
| Adaptive 8 KiB / 64 KiB | 205.809 µs | 304.888 µs | 86.280 µs |

The second eligible object pays for frame construction. Subsequent similar edits
can reuse exact chunks. A single previous comparison object can lose admission
when unrelated wide directories alternate. Peak accounted retention in this
prototype was 864,718 bytes, including 103,013 bytes of previous raw content.

This supports a production trial of 8 KiB frames for 64–256 KiB metadata. It does
not establish a durable-write improvement: SQLite page writes, WAL size,
synchronization, and engine behavior remain part of the fixed full comparisons.
The full raw results and both current/legacy decoder checks remain available:
[measurements](benchmarks/metadata-frames-2026-10-25-adaptive.json),
[summary and controls](benchmarks/metadata-frames-2026-10-25-adaptive-summary.json),
[build provenance](benchmarks/metadata-frames-2026-10-25-adaptive-build.json), and
[exact diagnostic source](../scripts/diagnostics/metadata_frames_adaptive.rs).

### Preserved baseline identity

The historical durable baseline was rebuilt from its exact clean source and
compiler after its local executable was lost. All 33 recorded source hashes
match, and the rebuilt executable is byte-identical to the original SHA-256
`ff6a44c736b135d4ec920f4d497a92fa3cac4a2936ff284a6c4abeb1ef6aa6c6`.
The [recovery build record](benchmarks/durable-baseline-2026-10-25-recovery.json)
records the actual command, environment, source and binary checks. The historical
baseline report and its timing observations are unchanged.
