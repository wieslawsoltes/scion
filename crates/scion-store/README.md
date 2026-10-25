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
durable content independently. Ref compare-and-swap is a separate transaction.
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

## Read-only access

`LocalStore::open_read_only(path)` opens an existing store without creating its
directories, migrating its schema, or changing journal settings. Object, record,
log, and prefix reads reuse prepared statements. All mutation methods reject
writes, including idempotent writes and loose-object insertion. Readers observe
concurrent commits through the live WAL; this does not use SQLite's immutable
mode. SQLite can maintain WAL reader coordination sidecars, so the API is not a
promise that no filesystem coordination files are touched.

Existing file-only schemas remain readable without migration. If a writer later
upgrades the store, an open reader detects and reads the new inline object table.
The workspace bundles SQLite 3.51.3, including the upstream
[concurrent WAL-reset corruption fix](https://sqlite.org/releaselog/3_51_3.html),
and tests the linked version to prevent a vulnerable dependency regression.

Licensed under MIT OR Apache-2.0.
