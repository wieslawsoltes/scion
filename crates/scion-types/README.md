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

`Tree::canonical_bytes()` emits the same bytes as `canonical(&tree)` while writing
fixed JSON fields and hexadecimal IDs directly. Arbitrary UTF-8 name escaping and
integer formatting retain Serde's behavior. `Tree::from_json()` and
`Tree::entry_from_json()` provide compatible complete-tree and validated point
reads. Encoding and complete decoding keep tree-name validation separate through
`Tree::validate()`; point lookup validates all names and sibling payloads before
returning an entry. These paths preserve the established version 1 content IDs.

See the repository documentation for the version 1 format. This format is independently specified and does not claim compatibility with Anthropic's private protocol. Licensed under MIT OR Apache-2.0.
