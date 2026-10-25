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
| SCN-05 | Durable change log | Idempotent publication and repository membership; committed sequence frontiers for PostgreSQL and Spanner | Local restart/process-exit tests, PostgreSQL conformance, official Spanner emulator |
| SCN-06 | Generic recursive indexes | Versioned `DerivedData`, iterative parent evaluation, eager/lazy policy | Engine derived-data test and public trait |
| SCN-07 | Graph skip indexes | Generations, binary lifting, paired linear spans, all-parent ancestry, and maximal merge bases | Graph tests, exhaustive small-DAG oracle, and bounded graph-read regressions |
| SCN-08 | History and blame | Persistent path trie/history chains; line attribution with copy provenance | Engine history/blame implementation and `blame_tracks_unchanged_lines_and_explicit_copies` |
| SCN-09 | Copy and path indexes | Explicit/exact-content copy lookup; prefix-scoped path search | Engine copy hash trie and path-search tests |
| SCN-10 | Checkout-free operations | Typed RPCs for commits, diff, ancestry, history, blame, merge, rebase, revert, and bisection | `RpcRequest`; authority integration suite; CLI `rpc` |
| SCN-11 | Sparse working copies | Component-aware sparse manifest traversal and selected content materialization | `Client::manifest`; working-copy selection tests |
| SCN-12 | On-demand files | Explicit lazy hydration, durable writable overlay, Linux read-only/writable FUSE | Portable overlay tests; real kernel mount, mmap, orphan lifetime, commit, and remount workflow |
| SCN-13 | Immutable object negotiation | Exact tree/blob requests with bounded payloads and integrity checks | Object codec tests and `envelope` fuzz target |
| SCN-14 | Shared cache tiers | Same object endpoint, byte-bounded disk/memory cache, miss coalescing, validated upstream topology | Cache tests, service integration, container topology CI |
| SCN-15 | Snapshot bootstrapping | Portable verified cache snapshots, idempotent imports, startup restoration, periodic/shutdown export | Snapshot round-trip and corruption tests |
| SCN-16 | Distributed persistence | SQLite/PostgreSQL/Spanner metadata, object sharding, S3/GCS/Azure adapters | Backend conformance, provider SDK contracts, real MinIO, and official Spanner emulator tests |
| SCN-17 | Repository/directory IAM | Static bearer or configured OIDC identities, atomic policy reload, directory rules, path witnesses, signed tickets | Signed JWT/JWKS fixtures, revocation, directory-grant, and authority tests |
| SCN-18 | Git migration | Streaming topological ingestion, first-parent deltas, durable OID mapping, supported mode/content preservation | `scion-git` migration/restart/export tests |
| SCN-19 | Operational controls | Request admission, rate limiting, payload bounds, health/readiness/metrics, graceful shutdown | Authority configuration and integration tests |
| SCN-20 | Verification | Deterministic repository/backend scenarios, corruption tests, fuzz targets, cross-platform builds | Workspace tests, Fuzz workflow, CI workflow |
| SCN-21 | Multiple merge bases | Deterministic recursive synthesis over maximal common bases; explicit unresolved conflicts | Symmetry, idempotence, nested/criss-cross and structural-conflict tests |

## Independent design choices

| Choice | This implementation |
|---|---|
| Hashing | BLAKE3 with kind and format domain separation |
| Structured encoding | Deterministic JSON of versioned Rust model types |
| Compression | Zstandard; same encoded object bytes for storage and transfer |
| Local metadata | FULL-synchronous SQLite WAL with small inline objects and larger durable object files |
| Shared metadata | PostgreSQL or Spanner, with object shards or independently configured cloud objects |
| Control transport | Versioned HTTP/JSON RPC |
| Object authorization | Short-lived keyed-BLAKE3 capabilities over repository/revision/path/object scope |
| Working-copy portability | Validated UTF-8 repository-relative paths |
| Text merge | Three-way merge and recursive virtual-base synthesis with explicit unresolved-conflict results |
| Transparent virtualization | Linux read-only/writable FUSE over a portable durable overlay, behind a Cargo feature |

These choices are documented in [architecture](architecture.md), [protocol](protocol.md), and [operations](operations.md). They are not inferred upstream implementation details.

## Deliberate boundaries

### Durability and scale

The engine avoids a repository-wide mutation mutex and creates commits without moving refs. Durable backends still coordinate writes: SQLite serializes writers; PostgreSQL uses transactions and a log frontier gate; Spanner publication updates a per-repository counter row. Strictly lock-free distributed persistence is therefore not a verified property of this implementation.

PostgreSQL, Spanner, hash-partitioned objects, and S3/GCS/Azure adapters provide concrete shared-backend choices. Automatic shard rebalancing, cross-region failover orchestration, fleet snapshot distribution, and production load characterization remain deployment responsibilities. Emulator and SDK contract tests establish exercised behavior; they do not reproduce a production cloud region.

No component packs the complete repository into Git packfiles. There is no repack requirement. Authoritative retention, backup scheduling, and capacity management are still operator responsibilities.

### Virtualization and filesystem behavior

Ordinary sparse/lazy workspaces explicitly hydrate selected files. Linux FUSE also provides transparent writable access through the durable overlay. Its integration workflow mounts the real kernel filesystem and verifies mmap writeback, commits, inode lifetime, orphan cleanup, and remount. Windows and macOS run the portable client/overlay tests; native ProjFS and File Provider integrations are not implemented. Mounting requires a capable Linux host with `/dev/fuse`. Concurrent application writers must be quiesced for an application-consistent checkpoint, and portable source history does not version empty directories or every host filesystem attribute.

### Merge and query semantics

Multiple merge bases are sorted and combined through a deterministic recursive virtual-base synthesis. It memoizes common-base and tree work, and preserves structured conflicts when virtual ancestors cannot be reconciled. The work is bounded by recursion depth, head count, subproblem count, and graph traversal limits. Binary changes are versioned exactly; conflicting binary edits require resolution. Large text operations have resource bounds.

The CLI rebase operation replays one delta by default and supports a first-parent interval with `--base`; the reusable engine exposes both forms. The bisection API chooses a candidate and leaves test execution to its caller. Search is path-based. A dedicated content-search index and review application are separate products.

### Identity and access management

Configured OIDC/JWKS federation, exact issuer/subject bindings, static credentials, repository/directory policies, and atomic policy reload are implemented. Interactive login, enterprise account provisioning, provider application registration, policy distribution across authorities, and a review approval UI are separate integrations. Shared capability keys trust cache operators; deployments needing distinct trust boundaries must introduce an appropriate key and authorization topology.

Already issued capabilities expire after five minutes. A policy change does not erase previously downloaded source content. Root-tree and full-history queries require full repository read access; path-scoped views use filtered APIs rather than exposing protected directory names.

### Git interoperability

Migration and export are implemented with documented portable-path and UTF-8 metadata restrictions. Scion is not a Git wire-protocol server, and its changeset IDs are not Git commit hashes. Preserving file contents and supported history structure does not imply byte-identical reconstruction of signed Git commits or annotated tags on export.

### Adjacent Anthropic systems

Antique, code review semantics, organization-specific landing policies, Claude-based operational agents, and internal CI/revert automation are mentioned alongside Scion in the source article. Their private implementations are not published and are not reproduced here. Scion exposes primitives that independent integrations can use.

## Updating coverage

When a capability changes, update its implementation contract and test reference in the matrix. For performance claims, provide the benchmark workload, machine, compiler, backend, and raw results. Avoid converting source-code presence, a green unit suite, or another organization's reported results into an unsupported claim of full production equivalence.
