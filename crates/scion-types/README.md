# scion-types

The canonical data model for the independent [Scion](https://github.com/wieslawsoltes/scion) source-control system.

This crate defines domain-separated BLAKE3 object IDs, validated repository-relative paths, ordered first-parent changesets, persistent directory-tree objects, file modes, copy provenance and structured SCM results. It is independent of databases, HTTP, async runtimes and working copies.

```rust
use scion_types::{ObjectId, ObjectKind, RepoPath};
let id = ObjectId::hash(ObjectKind::Blob, b"hello\n");
let path = RepoPath::new("src/main.rs")?;
assert_eq!(path.as_str(), "src/main.rs");
assert_eq!(id.to_string().len(), 64);
# Ok::<(), scion_types::Error>(())
```

Canonical encoding uses fixed-order Rust structures and ordered maps serialized to JSON. Object identity is calculated from uncompressed bytes, independent of transport compression. Path validation runs during deserialization, not only during construction. The portable UTF-8 path format excludes traversal, metadata directories, Windows drive syntax and device names.

See the repository documentation for the version 1 format. This format is independently specified and does not claim compatibility with Anthropic's private protocol. Licensed under MIT OR Apache-2.0.
