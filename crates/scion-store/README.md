# scion-store

Reusable immutable storage and transactional metadata for [Scion](https://github.com/wieslawsoltes/scion).

- `Storage`: object I/O, immutable records, compare-and-swap metadata and atomic changeset-log publication.
- `MemoryStore`: deterministic concurrent test and embedded backend.
- `LocalStore`: sharded compressed object files, atomic replacement and SQLite WAL metadata.
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

Licensed under MIT OR Apache-2.0.
