# scion-store

Reusable immutable storage and transactional metadata for [Scion](https://github.com/wieslawsoltes/scion).

- `Storage`: object I/O, immutable records, compare-and-swap metadata and atomic changeset-log publication.
- `MemoryStore`: deterministic concurrent test and embedded backend.
- `LocalStore`: small compressed objects and metadata in SQLite WAL, with larger objects in durable sharded files.
- `PostgresStore` with the `postgres` feature: pooled shared metadata and compressed objects with Rustls TLS.
- `ShardedStore`: independent metadata authority and hash-distributed object backends.
- `encode_object` / `decode_object`: the same versioned zstd envelope on disk and on the network, with size, domain and hash verification.

```rust
use scion_store::{MemoryStore, Storage};
use scion_types::ObjectKind;
let store = MemoryStore::new();
let id = store.put_object(ObjectKind::Blob, b"hello")?;
assert_eq!(store.get_object(ObjectKind::Blob, id)?, b"hello");
assert!(store.compare_and_swap("example/refs", "main", None, Some(b"first"))?);
assert!(!store.compare_and_swap("example/refs", "main", None, Some(b"stale"))?);
# Ok::<(), scion_types::Error>(())
```

Objects are limited to 256 MiB uncompressed. Changeset publication atomically adds repository membership and a deduplicated log entry; references remain separate. Durability relies on the selected backend's transactional contract. The local backend serializes metadata writers and is not a distributed database. PostgreSQL log readers briefly establish a committed sequence frontier; see the operating guide for the coordination semantics.

## Durable publication batches

`PreparedObject::new(kind, bytes)` validates size and computes a content ID.
Its immutable `encode()` method reuses that computed ID when producing the
version 1 envelope. Private fields and read-only byte access prevent changing
the content or supplying an unchecked ID. Stored-object reads and duplicate
publication still verify the entire decoded content hash.
`Storage::publish_commit_with_objects` accepts these immutable objects alongside
the existing `ImmutableRecord` batch. Its portable default writes objects through
the backend's own `put_object` method before publishing metadata and membership;
existing third-party implementations need no new method. `ShardedStore` therefore
keeps content in object shards and publishes metadata through its authority.

`LocalStore` stores encoded objects of at most 256 KiB in SQLite. A usual commit's
changed trees, changeset, graph/tree indexes, membership and log share one
`synchronous=FULL` WAL transaction. A standalone `put_object` still acknowledges
durable content independently. Standalone ref compare-and-swap is a separate
atomic SQL statement and durability boundary.
Encoded batches use at most 16 MiB of buffering; overflow chunks are durably
written before the final publication. Larger objects keep their synced loose-file
representation. Failed publication can leave unreachable objects, but it cannot
publish a changeset whose supplied content is not durable.

All existing loose objects remain readable, and canonical content, IDs and the
versioned network envelope are unchanged. The local schema adds `inline_objects`
on open. Back up SQLite (including any live WAL) together with the object directory;
copying only loose files no longer captures every immutable object. Older Scion
binaries that do not understand the inline table cannot read newly inlined objects.

Duplicate writes verify the existing envelope and content hash before they are
acknowledged. A corrupt duplicate in the final publication transaction rolls back
that transaction's objects, records, membership, and log. Alternate valid
compression of the same content remains accepted.

### Prepared content with an optional reference update

`Storage::publish_commit_with_objects_and_cas` accepts the same prepared objects
and immutable metadata, plus an optional `RecordCas`. `PublicationResult` returns
the deduplicated log sequence and `cas_applied: Option<bool>`. A stale expected
reference value returns `Some(false)` while retaining the published immutable
commit; the caller can retry a separate ref update. The bundled CAS namespace
must be exactly `repo/{repo}/refs`, and its key must satisfy the existing reference
name contract. It cannot target older membership/graph/derived records, another
repository's refs, or an immutable entry included in the same publication batch.
The standalone low-level `compare_and_swap` retains its generic namespace API.

`LocalStore` includes the optional CAS in the final FULL-synchronous transaction,
so typical small-object commits can durably publish uploaded blobs, changed trees,
changeset, indexes, membership/log and a successful reference update with one
barrier. SQL errors roll back that final transaction. The portable default first
publishes the complete durable commit and then applies CAS; an error between
these operations can leave the commit published. Every backend preserves the
rule that a visible reference never precedes its durable commit. Independently
acknowledged `put_object` and `publish_commit_with_objects` calls keep their
existing durability and reference-independence contracts.

## Codec reuse

Objects up to 1 MiB use thread-local Zstd compression/decompression contexts,
avoiding fresh streaming buffers and context allocation per object. There is no
global codec lock. Larger objects use incremental streaming so an untrusted large
size header cannot trigger an eager full-size allocation, and large compressor
workspaces are not retained by threads. Content hashes, envelope version, size
limits and integrity validation are unchanged. Old streaming frames, concatenated
frames and valid skippable frames remain readable; emitted frames remain readable
by the previous streaming decoder. Compression bytes can differ while content
identities remain identical.

Trees, changesets and derived metadata use Zstd level 1 to reduce publication
CPU cost; file blobs retain level 3. The two small-object encoder contexts are
separate, so interleaved metadata and blob writes keep their configured policy.
Large objects use the same per-kind levels through the streaming path. Compression
ratio depends on content: level 1 can produce larger or smaller metadata frames,
while object identities, size limits and decoding checks remain unchanged.
Previously written level 3 metadata and new level 1 frames interoperate through
the same version 1 envelope and standard Zstd decoder.

Related metadata from 64 KiB through 256 KiB can reuse independently compressed
8 KiB Zstd frames. The first eligible object uses ordinary whole-frame encoding.
Only a whole-frame encoded/raw ratio between 1/8 and 7/8 retains an admission
candidate; very repetitive and incompressible data keep the ordinary path. A
later eligible object must match at least 75% of its bytes at fixed chunk
boundaries before framing is used. Each cache hit requires exact raw-byte equality
after its BLAKE3 lookup key matches. Unrelated metadata returns to whole-frame
encoding. Small metadata, file blobs, and streamed objects preserve their existing
paths and do not replace the admission candidate.

This cache has a shared 1 MiB accounted retention budget per encoding thread for
the previous raw candidate, cached raw/compressed buffer capacities, the actual
index allocation, and conservative allocation/control allowances. A lazy index
holds at most 64 frames; decode-only threads, blob writes, and small metadata do
not allocate it. This budget is separate from the reusable codec contexts and
transient input/output buffers. Cache pressure only causes recompression. The
first admitted encode constructs frames and can cost more than whole-frame
encoding; subsequent related edits reuse them. Extra frame boundaries can also
increase encoded size and decoding cost.

Framing uses ordinary concatenated Zstd frames inside the existing version 1
envelope. Encoded bytes may depend on recent encodes on the same thread; raw
content and IDs remain stable. All complete reads and duplicate publications
still verify the full content hash. The cache never supplies object reads,
membership, or durability acknowledgments, and publication transactions are
unchanged.

## Metadata layout and explicit upgrades

New local stores use an ordinary SQLite rowid table for `records`, with a narrow
unique primary-key index on `(namespace,key)`. Large graph/index values live in
the row payload rather than the comparison key. This prevents a metadata key
search from reading unrelated wide values through B-tree overflow pages. The
`inline_objects` table is unchanged.

Existing stores retain their original layout when opened, and every read/write
API supports both layouts. `LocalStore::migrate_records_to_rowid()` explicitly
upgrades the standard legacy `WITHOUT ROWID` records table. It returns `true` for
a completed migration and `false` when already upgraded. The copy and schema swap
are one FULL-synchronous transaction; interruption or SQL failure rolls back the
upgrade. Content IDs, all record bytes, objects, log sequences and reference values
are preserved. Prepared statements are refreshed after the swap.

Back up the complete store and provide free space for a copied metadata table and
its WAL before upgrading. The copy temporarily serializes other metadata writers;
existing WAL snapshots continue reading their old view. The migration rejects
custom record constraints, collations, indexes, triggers and detected view or
foreign-key dependencies rather than discarding them. Dependency detection is
conservative. Read-only handles and read snapshots reject migration.

The migration does not run VACUUM: freed pages remain available to later writes.
Plan any optional file compaction separately as an operational maintenance task.
There is no implicit expensive migration during ordinary open or query startup.

## Bounded small-object probes

`Storage::try_get_small_object(kind, id, max_decoded_bytes)` lets query engines
inspect a small changeset delta without materializing or decompressing a large
initial import. `Some(bytes)` always contains the complete, size-checked and
hash-verified object. `None` means that the optimization is unsupported or the
representation exceeds a bound; it does not certify existence or payload validity.
Call `get_object` when the ordinary full result is required.

The default implementation performs no I/O and returns `None`, preserving custom
backends. LocalStore and MemoryStore inspect the envelope's kind, ID, declared
size and global size limits before returning an oversized result. Missing objects
and malformed inspected data return errors. ShardedStore delegates to the selected
object shard. A bounded encoded payload is selected in SQLite; loose objects are
read through a fixed header and capped file read. Database engines can still read
index/overflow pages while finding a record, but an oversized value is not copied
into the decoder. Legacy loose objects remain supported.

The encoded-input bound uses Zstd's worst-case single-frame compression bound for
the requested decoded limit. A legal representation with concatenated frames or
very large skippable frames can therefore return `None` despite fitting the decoded
limit. Normal
`get_object` behavior and all existing envelope/hash checks remain unchanged.

## Local I/O options

`LocalStore::open` and `open_read_only` both use `LocalStoreOptions::default()`:
ordinary SQLite reads (`mmap_size: 0`) and SQLite's normal last-connection
checkpoint/cleanup policy (`checkpoint_on_close: true`). Fresh-process profiling
found that mapping setup, page faults and teardown outweighed its reduced copying
for the measured repository queries, so mapping is an explicit option for
long-lived deployments. FULL synchronization of acknowledged writes and ordinary
WAL auto-checkpointing remain enabled in every configuration.

Use `open_with_options` or `open_read_only_with_options` to select a policy:

```rust
use scion_store::{LocalStore, LocalStoreOptions};
let store = LocalStore::open_with_options("scion-data", LocalStoreOptions {
    mmap_size: 256 * 1024 * 1024, // Optional bounded mapping for database reads.
    checkpoint_on_close: false,  // Retain WAL/coordination files on final close.
})?;
# Ok::<(), scion_types::Error>(())
```

Mapping configuration is deferred until the first database operation, and errors
are returned before that operation proceeds. SQLite may reduce or disable mapping
when the platform or VFS cannot provide it. Mapped readers still observe concurrent
WAL commits and use normal SQLite locking; this is not immutable or exclusive mode.
As described in [SQLite's mmap guide](https://www.sqlite.org/mmap.html), an underlying
mapped I/O failure can cause a process signal instead of a returned I/O error, and
Windows mappings can delay truncation during VACUUM. Leave `mmap_size: 0` to use
the ordinary read/error path.

With `checkpoint_on_close: false`, WAL files can remain after every connection
closes. Back up the complete store with a consistent SQLite backup or quiesced
filesystem snapshot, including any live WAL and the loose-object directory; a
database-file-only copy can omit acknowledged commits. The default
`checkpoint_on_close: true` preserves final checkpoint and cleanup when a writable
connection is the last open connection. No write durability setting is weakened by
either option.

## Read-only access

`LocalStore::open_read_only(path)` opens an existing store without creating its
directories, migrating its schema, or changing journal settings. Object, record,
log, and prefix reads reuse prepared statements. `get_record_pair` can read a
membership guard and derived record in one query; the local implementation uses
one live-WAL snapshot. Object reads prepare their inline query directly; current
stores need no separate table-existence query, including on the first read.
Only a preparation failure can trigger a catalog check for legacy absence. An
existing incompatible schema, a table disappearing after it was observed, or a
query/integrity error is returned rather than hidden by loose-file fallback.
Absence is not cached, so live legacy readers still observe later writer upgrades.
Both full and bounded object reads retain their complete envelope/hash checks.
All mutation methods reject writes, including idempotent writes and loose-object
insertion. Readers observe concurrent commits through the live WAL; this does not
use SQLite's immutable
mode. SQLite can maintain WAL reader coordination sidecars, so the API is not a
promise that no filesystem coordination files are touched.

### Consistent read snapshots

`LocalStore::open_read_snapshot(path)` and `open_read_snapshot_with_options`
provide a consistent analysis view using an ordinary deferred SQLite read
transaction. The first database query pins the latest committed WAL state; later
queries on that handle retain it until the handle is dropped. Commits completed
after constructing the handle but before its first query are included. A new
snapshot observes later commits. Regular `open_read_only` continues to provide
live visibility between queries.

WAL writers can publish while a read snapshot is active. Long-lived snapshots can
delay checkpoint progress and increase WAL retention, so scope a snapshot to one
logical query/analysis operation. Legacy non-WAL databases retain SQLite's normal
read-lock behavior, which can block writers. No immutable or exclusive mode is
used, and every mutation remains rejected. Metadata and inline-object reads share
the snapshot; immutable loose objects are read and verified by content ID.

High-level repository queries can lazily derive missing indexes. Such queries
still need a writable backend when their required derived data is absent; opening
the underlying store read-only does not disable that derivation contract.

Existing file-only schemas remain readable without migration. If a writer later
upgrades the store, an open reader detects and reads the new inline object table.
The workspace bundles SQLite 3.51.3, including the upstream
[concurrent WAL-reset corruption fix](https://sqlite.org/releaselog/3_51_3.html),
and tests the linked version to prevent a vulnerable dependency regression.

Licensed under MIT OR Apache-2.0.
