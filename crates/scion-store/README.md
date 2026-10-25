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

Licensed under MIT OR Apache-2.0.
