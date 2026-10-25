# Metadata compression performance investigation

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
