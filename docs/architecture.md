# Architecture

Scion separates immutable source-control data, mutable repository metadata, derived indexes, and working-copy state. The public Rust interfaces are usable independently of the HTTP services and command-line client.

## Crate boundaries

| Layer | Crates | Depends on |
|---|---|---|
| Data model | `scion-types` | Serialization, hashing, shared errors |
| Persistence | `scion-store` | Model; local, memory, or optional PostgreSQL storage |
| Graph | `scion-graph` | Model and a graph-access trait |
| Repository semantics | `scion-engine` | Model, storage, graph, text diff/merge |
| Network contract | `scion-protocol` | Model and capability encoding |
| Authority | `scion-server` | Engine, protocol, access policy, HTTP runtime |
| Client and materialization | `scion-client` | Protocol, object codec, working-copy filesystem |
| Cache services | `scion-cache` | Protocol, bounded client cache, HTTP runtime |
| Migration | `scion-git` | Engine and Git subprocess protocols |
| Commands | `scion-cli` | Public library APIs |

An application can embed `Repository` over any `Storage` implementation. It does not need a Tokio runtime, HTTP server, or checkout to create commits or query history.

## Immutable identity

`ObjectId` is a 32-byte BLAKE3 digest displayed as 64 hexadecimal characters. Identity hashes a version marker, an object-kind domain, and canonical uncompressed bytes. Different kinds with identical payloads produce different identifiers. Compression is outside the identity boundary.

Structured objects use deterministic JSON: struct fields serialize in their declared order and maps use sorted keys. This is this project's format contract, not a claim that arbitrary JSON canonicalizers produce the same bytes. Changes to field order, optional-field omission, map ordering, or normalization require format review.

An encoded object includes a magic/version marker, kind, uncompressed size, expected digest, and a Zstandard frame. Decoding checks the declared domain, size ceiling, resulting byte count, and recomputed digest. The same encoded bytes are used on disk, in snapshots, and on the object transport. See [the byte layout](protocol.md#binary-object-envelope).

## Changesets and publication

A `Changeset` contains ordered parent IDs, author, optional committer, timestamp, message, ordered path changes, and optional metadata. Each file change is a put or delete relative to the first parent. A put carries the blob ID, size, mode, and optional copy provenance. File content is never embedded in the changeset.

Commit creation validates parents and referenced content, updates persistent trees, builds the graph record, persists immutable objects and derived pointers, and finally publishes repository membership with a deduplicated log entry. A crash before publication may leave unreachable immutable objects. It must not expose a partially prepared commit as a repository member.

The change-log sequence orders published records for consumers. It is not a changeset ID, branch position, or substitute for DAG ancestry. Log reads use an exclusive lower bound: `after=N` returns records following sequence `N`.

References occupy a separate mutable namespace. `update_ref(name, expected, target)` compares and updates atomically; an absent expected value means the ref must not exist. Creating a changeset does not move a ref. A rejected ref update leaves the immutable commit available for inspection, rebasing, merging, or a later update.

## Persistent trees

Each directory is an immutable `Tree` containing only its immediate file and subdirectory entries. Subdirectories refer to other tree IDs. The root tree is derived from the first parent's root plus the changeset's delta.

Changes are grouped by directory prefix. Only affected directory nodes are read and rewritten; unchanged subtree IDs are reused. A one-file change does not reconstruct a flat manifest of every repository file. Rewriting a directory still processes that directory's entry map, so very wide directories are a separate performance dimension from tree depth.

File/directory replacement is explicit. A live file cannot coexist with live descendants. Deleting `a` and adding `a/b` changes a file into a directory. A conflicting put at `a` alongside a put below it is rejected.

Tree differences compare IDs before descending. Equal subtree IDs terminate the traversal. Content bytes are loaded only when a caller requests a text patch or a merge needs them. Binary changes still have exact before/after file entries even when no text patch is emitted.

## Commit graph

`GraphAccess` supplies immutable `GraphNode` values. Nodes record parents, generation information, and first-parent binary-lifting links. First-parent jumps use the skip index. General ancestry examines every relevant DAG parent, including merge parents, and uses graph metadata to prune work.

Merge-base queries return all maximal common ancestors. The merge engine accepts multiple bases when their trees are identical. When criss-cross history has different base trees, it returns a structured virtual-base conflict rather than choosing an arbitrary base.

Graph queries have explicit traversal limits. These protect the service from unbounded requests; they do not imply that all arbitrary DAG queries are logarithmic.

## Derived data

The `DerivedData` trait defines a stable name, version, and deterministic derivation from a changeset and its ordered parent values. The generic evaluator walks parent dependencies iteratively, persists content-addressed results, and stores a versioned mapping per changeset. Applications choose eager preparation or lazy evaluation. Concurrent callers for the same repository/index/revision share a bounded in-flight computation; traversal and admission limits bound cold work.

Racing derivations can calculate the same result; immutable insertion requires identical bytes for a repeated key. An index implementation that emits different results for the same version and inputs violates the contract. Application-specific indexes remain server-side and do not need a client protocol change.

The built-in history index uses a persistent path trie whose leaves point to linked history records, including tombstones. A path-history query follows only that path's records after the index is prepared. Directory history combines descendant changes and retains deleted-directory history. Cached blame preserves unchanged-line attribution and can inherit attribution from explicit copy sources. Exact-content copy detection uses a persistent hash trie. Path search works over repository paths and selected prefixes.

Cold lazy derivation can traverse historical dependencies. Its cost is distinct from a warm indexed query and should be measured separately.

## Mutation semantics

Merges compare base, ours, and theirs. Non-overlapping text edits can merge automatically. Concurrent additions, delete/modify collisions, incompatible modes, and unresolved binary edits return structured conflicts. No merge changeset is published when conflicts remain.

Rebase replays a first-parent delta onto a selected revision, preserving author/message metadata and recording its source revision. Rebasing a merge changeset intentionally flattens that delta. The engine also provides a first-parent stack rebase API with an old-to-new mapping and a conflict result at the stopping point.

Revert creates a new changeset that applies the inverse delta onto a specified revision. For merge commits, `mainline` selects a zero-based parent. Bisection returns a candidate from the good/bad interval; executing builds, remembering outcomes, and deciding whether a test is reliable belong to the calling automation. Candidate balancing is exact for intervals up to 512 eligible commits and samples candidate scores for larger intervals.

## Storage backends

`Storage` requires idempotent immutable objects and records, atomic compare-and-swap, durable membership/log publication, and bounded scans. The supplied backends are:

| Backend | Intended use |
|---|---|
| `MemoryStore` | Deterministic tests and embedded ephemeral repositories |
| `LocalStore` | Compressed object files plus SQLite metadata for a single local authority |
| `PostgresStore` | Shared durable metadata and encoded objects with pooled connections and TLS support |
| `ShardedStore` | One metadata authority and independently selected object backends, partitioned by object hash |

The repository engine does not coordinate changes through a global branch mutex. Durable storage still coordinates transactions: SQLite serializes writers, PostgreSQL ref updates use atomic SQL operations, and log visibility uses a committed frontier. See [operations](operations.md#change-log-ordering) for that distinction.

## Thin clients and working copies

`Client` uses independent control and object transports. It asks for exact immutable trees, recurses only into selected subtrees, and then fetches selected blobs. A private local cache stores verified compressed objects within a byte budget. Shared node and regional caches use the same object RPC.

Working-copy state records revision, repository, endpoint configuration, sparse paths, tracked file entries, and whether each file has been materialized. Missing materialized files are deletions. Missing lazy files are still tracked and unchanged. The distinction is essential to safe commits from sparse workspaces.

The writable client supports explicit hydration, change monitoring, dirty-file checks, and repository-relative path validation. On Linux, the optional FUSE feature exposes a read-only revision as a filesystem that obtains file contents on read. These are separate workflows: explicit lazy hydration supports editable working copies; FUSE supplies transparent read access to an immutable revision.

Checkout uses a staged transaction with a durable recovery journal and backup files. Installation does not overwrite an unexpected destination; local modifications are checked again after downloads. Reopening an interrupted transaction rolls it back, including an interrupted rollback, while preserving files modified outside the transaction. Windows and macOS materialization rejects case-equivalent and Unicode-equivalent path collisions. Change monitoring releases the working-copy lock between status reads.

## Authorization and cache reuse

The authority checks bearer identity and repository/path policy for control operations. For object reads it validates a revision/path witness and issues a short-lived MAC-protected ticket. Tickets include object identity, repository, revision, path, kind, and expiry. The object endpoint does not expose a bare unauthenticated hash lookup.

Shared caches validate authorization before serving a hit and only accept the expected upstream tier. They coalesce concurrent misses for one object and bound upstream concurrency. Cached bytes do not become authority: integrity checking and capability validation remain required independently.

LRU bookkeeping takes logarithmic time in the number of cached objects. Disk operations and decompression run outside the global cache metadata mutex and outside async runtime worker threads. A circuit breaker limits repeated upstream failures; an open circuit never redirects traffic to a different tier. Periodic snapshots can warm replacement nodes without making cached objects authoritative.

## Embedding example

The following uses only public library APIs and does not create a working copy:

```rust
use scion_engine::Repository;
use scion_store::{MemoryStore, SharedStore};
use scion_types::{Changeset, FileChange, FileEntry, FileKind, RepoPath};
use std::{collections::BTreeMap, sync::Arc};

fn create_repository() -> scion_types::Result<()> {
    let storage: SharedStore = Arc::new(MemoryStore::new());
    let repository = Repository::new("demo", storage)?;
    let bytes = b"Hello, Scion!\n";
    let blob = repository.put_blob(bytes)?;
    let commit = repository.create_commit(Changeset {
        parents: vec![],
        author: "Ada <ada@example.test>".into(),
        committer: None,
        timestamp_ms: 0,
        message: "Initialize repository".into(),
        changes: BTreeMap::from([(
            RepoPath::new("hello.txt")?,
            FileChange::put(FileEntry {
                id: blob,
                size: bytes.len() as u64,
                kind: FileKind::Regular,
            }),
        )]),
        metadata: BTreeMap::new(),
    })?;
    let id = commit.id()?;
    assert!(repository.update_ref("main", None, Some(id))?);
    assert_eq!(repository.read_file(id, &RepoPath::new("hello.txt")?)?, bytes);
    Ok(())
}
```
