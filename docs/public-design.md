# Public design, implementation coverage, and boundaries

## Source and scope

The primary reference is Anthropic's [Redesigning source control at Anthropic](https://www.anthropic.com/research/redesigning-source-control), published October 13, 2026 and updated October 15. It describes an online, asymmetric architecture: immutable first-parent changesets, server-side queries and mutations, recursively derived indexes, independent CAS refs, sparse/on-demand clients, tiered object caches, Git ingestion, and repository/directory access control.

Anthropic links [Mononoke](https://github.com/facebook/sapling/blob/main/eden/mononoke/README.md) and this [Bonsai schema permalink](https://github.com/facebook/sapling/blob/79071c2a3ea20c36ed71332608535e5801101bba/eden/mononoke/mononoke_types/serialization/bonsai.thrift) as prior art. Those sources inform architectural understanding; this repository does not vendor their implementation.

The article does not publish Scion's source code, object format, API schema, merge algorithm, IAM language, or production deployment configuration. This project supplies its own explicit contracts for those choices. Its hashes, encodings, protocol, and database schema should not be represented as compatible with Anthropic's private service.

## Conformance matrix

“Implemented” below means a concrete implementation exists in this repository, with the cited verification surface. It does not mean the code has been validated at Anthropic's production scale. The GitHub workflow results establish which automated gates passed for a particular commit.

| ID | Public design theme | Implementation contract | Evidence and verification |
|---|---|---|---|
| SCN-01 | Online asymmetric client | Source-control semantics live in `Repository` and the authority; `Client` handles transport and files | `scion-engine`, `scion-server`, `scion-client`; service integration tests |
| SCN-02 | Immutable changed-file commits | Ordered parents, first-parent put/delete deltas, domain-separated IDs, independently stored blobs | `scion-types`; canonical/type tests; `immutable_commit_is_independent_of_refs_and_idempotent` |
| SCN-03 | Incremental commits | Persistent directory updates rewrite affected paths and reuse unrelated subtree IDs | Engine tree implementation; `one_file_commit_does_not_read_or_rewrite_unrelated_subtrees` |
| SCN-04 | Independent refs | Atomic expected-value updates and deletes; commit creation never moves a ref | Storage CAS tests and `concurrent_commits_do_not_move_refs_and_cas_has_one_winner` |
| SCN-05 | Durable change log | Idempotent publication and repository membership; committed sequence frontier for PostgreSQL | Local restart tests, PostgreSQL conformance job |
| SCN-06 | Generic recursive indexes | Versioned `DerivedData`, iterative parent evaluation, eager/lazy policy | Engine derived-data test and public trait |
| SCN-07 | Graph skip indexes | Generations and first-parent binary lifting; all-parent ancestry and maximal merge bases | `binary_lifting_and_second_parent_reachability`; `criss_cross_returns_all_maximal_bases` |
| SCN-08 | History and blame | Persistent path trie/history chains; line attribution with copy provenance | Engine history/blame implementation and `blame_tracks_unchanged_lines_and_explicit_copies` |
| SCN-09 | Copy and path indexes | Explicit/exact-content copy lookup; prefix-scoped path search | Engine copy hash trie and path-search tests |
| SCN-10 | Checkout-free operations | Typed RPCs for commits, diff, ancestry, history, blame, merge, rebase, revert, and bisection | `RpcRequest`; authority integration suite; CLI `rpc` |
| SCN-11 | Sparse working copies | Component-aware sparse manifest traversal and selected content materialization | `Client::manifest`; working-copy selection tests |
| SCN-12 | On-demand files | Explicit lazy hydration plus optional Linux read-only FUSE mount | Lazy-file tests; FUSE implementation; real mounts require a capable Linux host |
| SCN-13 | Immutable object negotiation | Exact tree/blob requests with bounded payloads and integrity checks | Object codec tests and `envelope` fuzz target |
| SCN-14 | Shared cache tiers | Same object endpoint, byte-bounded disk/memory cache, miss coalescing, validated upstream topology | Cache tests, service integration, container topology CI |
| SCN-15 | Snapshot bootstrapping | Portable verified cache snapshots, idempotent imports, daemon startup snapshot option | Snapshot round-trip and corruption tests |
| SCN-16 | Distributed persistence | Pluggable storage, pooled PostgreSQL, independent hash-partitioned object backends | `Storage`, `PostgresStore`, `ShardedStore`; backend conformance tests |
| SCN-17 | Repository/directory IAM | Hashed bearer credentials, component-aware rules, path witnesses, signed object tickets | Policy/ticket tests and authority integration tests |
| SCN-18 | Git migration | Streaming topological ingestion, first-parent deltas, durable OID mapping, supported mode/content preservation | `scion-git` migration/restart/export tests |
| SCN-19 | Operational controls | Request admission, rate limiting, payload bounds, health/readiness/metrics, graceful shutdown | Authority configuration and integration tests |
| SCN-20 | Verification | Deterministic repository/backend scenarios, corruption tests, fuzz targets, cross-platform builds | Workspace tests, Fuzz workflow, CI workflow |

## Independent design choices

| Choice | This implementation |
|---|---|
| Hashing | BLAKE3 with kind and format domain separation |
| Structured encoding | Deterministic JSON of versioned Rust model types |
| Compression | Zstandard; same encoded object bytes for storage and transfer |
| Local metadata | SQLite with durable immutable file storage |
| Shared metadata | Optional PostgreSQL backend and object shards |
| Control transport | Versioned HTTP/JSON RPC |
| Object authorization | Short-lived keyed-BLAKE3 capabilities over repository/revision/path/object scope |
| Working-copy portability | Validated UTF-8 repository-relative paths |
| Text merge | Three-way merge with explicit unresolved-conflict results |
| Transparent virtualization | Linux read-only FUSE, behind a Cargo feature |

These choices are documented in [architecture](architecture.md), [protocol](protocol.md), and [operations](operations.md). They are not inferred upstream implementation details.

## Deliberate boundaries

### Durability and scale

The engine avoids a repository-wide mutation mutex and creates commits without moving refs. Durable backends still coordinate writes: SQLite serializes writers; PostgreSQL uses transactions and a log frontier gate. Strictly lock-free distributed persistence is therefore not a verified property of this implementation.

PostgreSQL and hash-partitioned objects provide a shared backend topology. Google Spanner, cloud-object-store-specific adapters, automatic shard rebalancing, cross-region failover orchestration, and production load characterization are not included. A storage trait makes alternate backends possible; it does not constitute an implemented adapter.

No component packs the complete repository into Git packfiles. There is no repack requirement. Authoritative retention, backup scheduling, and capacity management are still operator responsibilities.

### Virtualization and filesystem behavior

Writable sparse/lazy workspaces explicitly hydrate selected files. Linux FUSE provides transparent read-only access to an immutable revision. A writable virtual filesystem, Windows ProjFS, and macOS File Provider integrations are not implemented. FUSE compilation and unit checks do not demonstrate that a restricted container can mount `/dev/fuse`.

### Merge and query semantics

Multiple merge bases with different trees produce an explicit virtual-base conflict. The engine does not silently choose one base or claim a recursive virtual-base synthesis algorithm. Binary changes are versioned exactly; conflicting binary edits require resolution. Large text operations have resource bounds.

The CLI rebase operation replays one delta; the reusable engine also provides stack replay. The bisection API chooses a candidate and leaves test execution to its caller. Search is path-based. A dedicated content-search index and review application are separate products.

### Identity and access management

Repository policies and directory rules are implemented. Enterprise SSO provisioning, external identity federation, policy hot reload, and a review approval UI are not part of this release. Shared capability keys trust cache operators; deployments needing distinct trust boundaries must introduce an appropriate key and authorization topology.

Already issued capabilities expire after a bounded interval. A policy change does not erase previously downloaded source content. Root-tree and full-history queries require full repository read access; path-scoped views use filtered APIs rather than exposing protected directory names.

### Git interoperability

Migration and export are implemented with documented portable-path and UTF-8 metadata restrictions. Scion is not a Git wire-protocol server, and its changeset IDs are not Git commit hashes. Preserving file contents and supported history structure does not imply byte-identical reconstruction of signed Git commits or annotated tags on export.

### Adjacent Anthropic systems

Antique, code review semantics, organization-specific landing policies, Claude-based operational agents, and internal CI/revert automation are mentioned alongside Scion in the source article. Their private implementations are not published and are not reproduced here. Scion exposes primitives that independent integrations can use.

## Updating coverage

When a capability changes, update its implementation contract and test reference in the matrix. For performance claims, provide the benchmark workload, machine, compiler, backend, and raw results. Avoid converting source-code presence, a green unit suite, or another organization's reported results into an unsupported claim of full production equivalence.
