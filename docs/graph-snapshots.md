# Frozen graph snapshots

A graph snapshot is a self-contained file for ancestry, merge-base, and
first-parent queries over a fixed set of published commits. Opening it requires
the expected repository and source identity. It can answer these queries without
opening SQLite or retaining the whole graph in memory.

This is an explicit analysis view. Commits published later are absent, references
are not resolved, and missing IDs return `NotFound`. Ordinary `Repository`
operations keep their existing storage semantics. Selecting a snapshot never
silently changes a live query into a historical one.

## Command-line use

Create a snapshot from a local store, placing the output outside that store:

```sh
mkdir -p snapshots
scn --repo demo graph-snapshot create --data ./scion-data snapshots/demo.graph
scn --json graph-snapshot info snapshots/demo.graph
```

Creation prints the repository, source identity and publication sequence,
included node count, format version, index capacity, and file size. Use complete
64-character changeset IDs for queries:

```sh
scn --repo demo graph-snapshot ancestor snapshots/demo.graph ANCESTOR DESCENDANT
scn --repo demo graph-snapshot merge-base snapshots/demo.graph LEFT RIGHT
scn --repo demo graph-snapshot nth-parent snapshots/demo.graph REVISION 100
```

The query commands accept `--identity ID` to pin the source identity reported by
`info`. Without it, the command selects the identity in the chosen file and still
requires the repository to match. The CLI runs locally; it does not initialize
an HTTP client or consult working-copy references. `--max-commits N` bounds export
size, and zero permits only an empty repository.

## Verified preparation

`scion_engine::export_local_graph_snapshot` opens a consistent read-only
`LocalStore` snapshot. It reads bounded publication-log pages, requires positive
strictly increasing sequence numbers, and verifies repository membership and the
complete changeset content hash for every included ID. Sequence gaps are valid
because the local log allocates sequences across repositories.

The exporter hashes the repository and ordered sequence/ID pairs into a source
identity. It passes verified IDs and parent lists to the graph writer, which
rebuilds every graph index and requires parents to precede their children. It
does not copy or trust existing derived graph records. A corrupt changeset,
missing membership, broken parent closure, or exceeded bound aborts preparation.

The publication log is collected within the configured limit; each entry uses
40 bytes before vector allocation overhead. Changesets are decoded one at a time
under the existing object-size limit. The writer stores its index on disk and
retains a bounded graph cache, rather than collecting every decoded node in RAM.

## Reusable graph API

`scion-graph` exposes `GraphSnapshot`, `GraphSnapshotMetadata`, `SnapshotSource`,
and `GraphSnapshotOptions`. A snapshot implements `GraphAccess`, so the same
ancestry, merge-base, and first-parent algorithms work over either storage form.

```rust,no_run
use scion_graph::{GraphSnapshot, is_ancestor};
use scion_types::{ObjectId, Result};

fn query(path: &str, repository: &str, source: ObjectId,
         ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
    let snapshot = GraphSnapshot::open(path, repository, source)?;
    is_ancestor(&snapshot, ancestor, descendant)
}
```

Applications with another verified data source can call `GraphSnapshot::write`
with an exact count and a topologically ordered stream of IDs and parents.
They are responsible for verifying source membership and changeset bytes. The
writer proves parent closure, rejects duplicate IDs and parents, and derives the
graph records. The source identity is a consistency label, not an authorization
credential or a cryptographic signature.

## File format and resource bounds

Version 1 uses a fixed 512-byte header, an open-addressed index of 80-byte slots,
and bounded binary graph records. Identical first-parent and linear-jump arrays
share one encoded array. Readers use positional file reads; they do not map the
file or load its whole index.

Header, occupied and empty index slots, and graph records have domain-separated
checksums. Slot checksums bind their positions and source context, so a zeroed
slot fails validation rather than silently changing a membership result. Bounds,
reserved fields, version, identity, file length, and graph invariants are checked
before using decoded data. Corrupt data is reported instead of falling back to a
different source.

The format limits a snapshot to ten million nodes, 64 parents per node, and 256
hash probes per lookup. Cache retention defaults to 8 MiB and 4,096 nodes; callers
can disable it or choose a byte limit up to 64 MiB. Retained nodes have already
been verified. In-place mutation is unsupported; use a fresh reader or disable
retention when checking each accessed record again. `inspect` validates only the
fixed header and length, not every node in the file.

Checksums detect accidental damage; they do not authenticate an untrusted
producer. Consumers must trust the snapshot's source, just as they must trust
their selected repository metadata backend.

## Publication and lifecycle

The writer builds a temporary file beside the destination, synchronizes its
contents, and atomically replaces the destination. Unix also synchronizes the
parent directory. Existing readers retain their original file view after
replacement. Validation, stream, and write failures before replacement leave an
existing artifact intact. A directory-sync failure after replacement can expose
the complete new artifact while leaving crash durability uncertain. Windows has
no portable directory-sync equivalent in Rust's standard library.

The local exporter requires an existing destination parent outside the source
store and rejects unsafe source/output aliases. Keep those paths stable during
export. Snapshots are rebuildable artifacts; a new export is how a caller selects
later publications. There is no automatic invalidation, live-reference cache, or
transparent fallback to SQLite.

## Performance comparisons

`scripts/compare_git.py --graph-snapshot` prepares this public artifact before
timing candidate graph queries, alongside Git's existing pack and commit-graph
preparation. It reports export wall time, file bytes, source identity, and file
SHA-256 separately. Each measured query still includes fresh process startup,
opening and validating the snapshot, its full computation and response, and
shutdown. The artifact hash is checked again after measurement.

File reads, diffs, resident workers, durable commits, and the preserved baseline
continue using their existing APIs. All graph results must match the same logical
fixture and the preserved baseline. Preparation cost and extra storage must be
considered when deciding whether this explicit snapshot suits a workload.
