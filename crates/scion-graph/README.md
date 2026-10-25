# scion-graph

Storage-independent directed acyclic commit graph algorithms. `GraphAccess`
supplies immutable `GraphNode` records; `build_node` constructs generation and
binary-lifting indexes incrementally. Query functions preserve every merge parent.

The crate provides first-parent jumps, complete ancestry, all maximal merge bases,
topological history, and balanced bisection candidates. Inputs are content-addressed
Scion identifiers, but no database, network, runtime, or working copy is required.

## Explicit frozen graph snapshots

`GraphSnapshot` is a separately selected, immutable graph artifact implementing
`GraphAccess`. Open it with the expected repository name and source identity, then
use the same ancestry, merge-base, first-parent, history, and bisection functions.
An ID missing from the artifact returns `Error::NotFound`, including when that ID
was published to a live repository after the snapshot. The reader does not open
SQLite, discover a live store, or silently switch membership views.

```rust
use scion_graph::{GraphSnapshot, GraphSnapshotOptions, is_ancestor};

# fn query(path: &std::path::Path, repository: &str,
#          expected_identity: scion_types::ObjectId,
#          ancestor: scion_types::ObjectId, descendant: scion_types::ObjectId)
#          -> scion_types::Result<bool> {
let snapshot = GraphSnapshot::open_with_options(
    path,
    repository,
    expected_identity,
    GraphSnapshotOptions { cache_capacity_bytes: 0 },
)?;
is_ancestor(&snapshot, ancestor, descendant)
# }
```

`inspect(path)` validates the fixed header and file length and returns serializable
`GraphSnapshotMetadata`: repository, source identity and optional source sequence,
node count, index slot count, file size, and format version. Inspection does not
scan every record. Source identity is a caller-defined consistency label, not an
authentication credential or signature. Callers remain responsible for selecting
a trusted artifact and enforcing access permissions.

### Building an artifact

`GraphSnapshot::write(path, repository, source, declared_count, commits)` accepts a
stream of `Result<(ObjectId, Vec<ObjectId>)>`. `SnapshotSource` contains an opaque
`identity` and an optional `sequence`; the latter can identify a source high-water
mark but has no graph-specific meaning.

The caller verifies each changeset's source bytes and repository membership before
yielding its ID and ordered parents. The writer independently requires every
parent to appear earlier, rejects duplicate changeset IDs and duplicate parents,
and rebuilds all generation and jump metadata using the crate's normal derivation
algorithm. It never accepts caller-supplied `GraphNode` indexes. The declared
count must exactly match the stream, and source errors abort publication.

The hash index is built on disk. Retention is bounded independently of the number
of commits: the default decoded-node cache is 8 MiB with at most 4,096 entries,
including conservative allocation/index accounting. `write_with_options` and
`open_with_options` accept a cache capacity from zero through 64 MiB. Zero disables
retention; retained values are positive, previously validated immutable records.
Artifacts support at most 10,000,000 commits, 64 parents per commit, and 64 levels
per jump array. Excessive hash collisions stop after a bounded probe budget.

### Integrity, publication, and compatibility

Version 1 uses a fixed 512-byte header, a fixed-width open-addressed hash index
with a load factor no greater than one half, and compact graph records. Identical
first-parent and merge-safe jump arrays share their encoded bytes. Existing
SQLite graph JSON, changeset IDs, and network representations are unchanged.

Header fields, including repository and source labels, are checksummed. Every
80-byte index slot has a checksum bound to its position and source context,
including empty slots: zeroing an occupied slot or an index page cannot silently
turn an existing member into a negative lookup. Each record binds its checksum to
its ID, contents, and file offset. Readers validate offsets, lengths, reserved
fields, count limits, and structural graph invariants before allocating bounded
record buffers. Checksums detect damaged data on access; they do not establish
authenticity against an actor able to rewrite the artifact and recompute hashes.

Safe positional file APIs work on Unix and Windows without memory mapping or
unsafe code in this crate. Queries do not load the whole index. The writer uses a
temporary file in the destination directory, syncs completed file contents, and
atomically replaces the requested path. Existing open readers keep the previous
file and its frozen membership. Never modify a published artifact in place.

Validation, input, and write errors before replacement leave an existing artifact
untouched. Unix additionally syncs the containing directory after replacement. A
failure of that final directory sync can occur after the new complete artifact is
visible, so its crash durability is then uncertain. Windows syncs file contents
and uses native atomic replacement; Rust's portable file API has no equivalent
directory-fsync guarantee there. The destination directory must already exist.
