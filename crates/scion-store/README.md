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
one live-WAL snapshot. The optional inline-object table is probed lazily on first
object access, so metadata-only clients avoid that schema query. All mutation methods reject
writes, including idempotent writes and loose-object insertion. Readers observe
concurrent commits through the live WAL; this does not use SQLite's immutable
mode. SQLite can maintain WAL reader coordination sidecars, so the API is not a
promise that no filesystem coordination files are touched.

High-level repository queries can lazily derive missing indexes. Such queries
still need a writable backend when their required derived data is absent; opening
the underlying store read-only does not disable that derivation contract.

Existing file-only schemas remain readable without migration. If a writer later
upgrades the store, an open reader detects and reads the new inline object table.
The workspace bundles SQLite 3.51.3, including the upstream
[concurrent WAL-reset corruption fix](https://sqlite.org/releaselog/3_51_3.html),
and tests the linked version to prevent a vulnerable dependency regression.

Licensed under MIT OR Apache-2.0.
