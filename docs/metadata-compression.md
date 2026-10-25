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
