# Command reference

`scn --help` and `scn COMMAND --help` describe the executable built from the current revision. Normal structured output is pretty-printed JSON; `--json` emits compact JSON suitable for scripts. `cat` writes raw bytes by default and a base64 `data` field in JSON mode.

## Connection and output options

| Option | Environment | Default or behavior |
|---|---|---|
| `--server URL` | `SCION_SERVER` | Working-copy setting, otherwise `http://127.0.0.1:7420` |
| `--objects URL` | `SCION_OBJECTS` | Working-copy setting, otherwise authority URL |
| `--repo NAME` | `SCION_REPO` | Working-copy setting, otherwise `default` |
| `--token TOKEN` | `SCION_TOKEN` | Bearer credential; prefer environment/credential-manager injection |
| `--cache-dir PATH` | None | Working-copy `.scion/cache` when applicable |
| `--cache-bytes N` | None | 1 GiB object-byte budget |
| `--json` | None | Compact machine-readable output |

Global options can appear before or after subcommands. Explicit options/environment values override saved working-copy connection settings. Tokens are not saved in working-copy metadata. On Unix, `--objects unix:///absolute/path/scion.sock` selects a node daemon's Unix socket; the control server still uses HTTP(S).

## Initialize and clone

```sh
scn init --author "Ada <ada@example.test>" --message "Initialize" --ref main
scn clone main work
scn clone main selected --sparse src --sparse tests
scn clone main lazy --sparse src --lazy
```

`init` creates an empty root changeset and conditionally creates the named ref. If the ref exists, the CAS fails and the newly created root remains immutable. Ref creation requires administrator permission in an authenticated repository.

`clone` accepts an exact changeset ID or a ref name. Sparse paths use directory components: `src` selects `src` and its descendants, not `src-old`. A lazy checkout records the selected manifest but initially leaves file contents unmaterialized.

## Edit, inspect, and commit

```sh
cd work
scn status
scn status --stats
scn status --refresh
scn commit -m "Implement parser" --author "Ada <ada@example.test>"
scn commit -m "Fix parser" --author "Ada <ada@example.test>" --ref main
```

There is no staging area. A commit records current changes in the selected working-copy paths, uploads changed content, and creates a server-side changeset. It does not require a push. The optional `--ref` attempts a separate CAS from the prior working-copy revision to the new commit.

If that CAS loses a race, the commit is still durable and the working copy records its new revision. The error identifies the created commit. Read the updated ref, decide how to reconcile the histories, and update the ref with the expected old value. Do not retry as an unconditional ref overwrite.

`status` distinguishes added, modified, and deleted materialized files. A tracked lazy file that has never been hydrated is not a deletion. `--stats` includes examined/hashed file counts, bytes hashed, verified cache hits, and untracked entries. `--refresh` discards cached fingerprints before scanning. See [status caching](working-copies.md#status-caching) for timestamp and platform behavior.

```sh
scn hydrate src
scn hydrate
scn checkout main
scn checkout CHANGESET_ID --force
scn watch
```

`hydrate` downloads a selected prefix, or all selected paths when the prefix is omitted. `checkout` rejects dirty-file replacement unless `--force` was requested. Untracked-file and unsafe-path checks remain relevant even for a forced checkout. On Windows and macOS, checkout rejects case-equivalent and Unicode-equivalent path collisions before changing files.

Checkout stages downloads before installing files, rechecks local modifications, and records a recovery journal. Opening a working copy after an interrupted checkout automatically restores its previous state. If a file changed outside the transaction, recovery preserves it and reports an actionable conflict instead of replacing it. Keep the working-copy metadata and recovery backups together until recovery completes.

`watch` reports working-copy changes using filesystem notifications. It releases the working-copy lock while waiting for events, so other Scion commands can operate on the same checkout. It waits and retries if a commit temporarily holds that lock.

Working-copy operations accept `--path DIRECTORY` where shown by their help output; `status` and `watch` accept a positional directory. This is useful for automation that manages several workspaces without changing the process working directory.

## Query without a checkout

```sh
scn info
scn refs
scn cat main src/lib.rs
scn log main --limit 50
scn history main src/lib.rs --limit 50
scn history main src --limit 50
scn blame main src/lib.rs
scn diff BASE TARGET
scn search main parser --prefix src --limit 100
scn copies main
scn ancestor ANCESTOR DESCENDANT
scn merge-base LEFT RIGHT
```

Revisions can be full IDs or ref names. Prefix IDs are display conveniences and are not accepted as identity. `log` uses DAG history; path history follows the first-parent history index. `search` matches paths. `copies` reports exact-content or explicit copy provenance available to the engine.

Directory history includes descendant changes and remains available after the directory is deleted. The authenticated principal needs read access to the whole selected subtree.

Text patches are omitted for binary data and files beyond the patch-size limit; before/after entries remain available. Blame requires UTF-8 text without NUL bytes. Complete logs and graph metadata require full repository read permission; path-scoped APIs enforce directory policy.

## Frozen local graph queries

```sh
mkdir -p snapshots
scn --repo demo graph-snapshot create --data ./scion-data snapshots/demo.graph
scn --json graph-snapshot info snapshots/demo.graph
scn --repo demo graph-snapshot ancestor snapshots/demo.graph ANCESTOR DESCENDANT
scn --repo demo graph-snapshot merge-base snapshots/demo.graph LEFT RIGHT
scn --repo demo graph-snapshot nth-parent snapshots/demo.graph REVISION 100
```

Creation reads one consistent local publication view, verifies changesets, and
rebuilds the graph. The output must be outside the source store. `--max-commits N`
bounds export size. Queries require an explicit repository and full changeset
IDs; `--identity ID` additionally pins the source identity printed by `info`.
These commands run locally without a server, working-copy context, or async
runtime. Later publications require another export. Missing IDs fail explicitly.
See [frozen graph snapshots](graph-snapshots.md) for examples and lifecycle rules.

## Reference updates

```sh
scn ref main
scn ref release NEW_CHANGESET
scn ref main NEW_CHANGESET --expected OLD_CHANGESET
scn ref release --delete --expected OLD_CHANGESET
```

An update without `--expected` means create only if absent. It is not a force update. Existing refs require an expected old ID. A ref deletion is also conditional. The server requires repository administrator permission for ref mutations.

## Merge, rebase, revert, and bisect

```sh
scn merge LEFT RIGHT --author "Ada <ada@example.test>" -m "Merge feature"
scn rebase CHANGESET ONTO
scn rebase TIP ONTO --base BASE
scn revert CHANGESET ONTO --mainline 0 --author "Ada <ada@example.test>" -m "Revert regression"
scn bisect GOOD BAD
scn bisect GOOD BAD --skip UNTESTABLE_CHANGESET
```

These operations run on the authority and do not modify a working copy or move a ref. For a merge, single-commit rebase, or revert, inspect the returned `changeset` and `conflicts`. A successful result has an ID and no conflicts. A conflict result has a null ID and explicit conflict records; callers must not treat a successful HTTP exchange alone as a successful merge.

A merge with multiple maximal bases recursively synthesizes a deterministic virtual base. Ambiguity that survives synthesis is returned as an explicit conflict; synthetic bases do not create visible changesets or move refs.

The CLI rebase command replays one first-parent delta by default. With `--base BASE`, it replays the first-parent range `(BASE, TIP]` and returns `rewritten` (the old-to-new changeset mapping), `tip` (the last successfully replayed commit), and `conflicts`. It stops at the first conflict; completed rewrites remain available. The base must be on that first-parent chain. Rebase of a merge changeset flattens its first-parent delta. Revert uses a **zero-based** mainline parent; `0` means the first parent. Bisection returns a candidate; your automation executes tests and records good/bad outcomes.

After a successful operation, update a ref with the resulting ID and its expected old value, or check out the new ID directly.

## Raw typed RPC

Every public control request can also be invoked using its JSON representation:

```sh
scn --json rpc '{"method":"list_refs"}'
scn --json rpc '{"method":"change_log","after":0,"limit":100}'
scn --json rpc - < request.json
```

This command deserializes a typed `RpcRequest`; it is useful for automation and new integrations. See [protocol methods](protocol.md#control-methods) for the schema. Supply actual 64-character IDs for ID fields rather than a ref name unless the field explicitly accepts revisions as strings.

For large file lists, use `list_files_page` with `commit`, `prefix`, `after`, and `limit`. Start with a null `after`, consume the `entries` map, and use the returned `next` visible path as the next exclusive cursor until `next` is null. This keeps each reply bounded and avoids leaking restricted filenames through pagination.

## Git migration

The Git bridge operates directly on a trusted local storage backend. It does not use HTTP credentials and is an administrative migration tool.

```sh
scn --repo demo import-git /path/to/git/repository --data ./scion-data
scn --repo demo import-git /path/to/git/repository --data ./scion-data --rev main
scn --repo demo import-git /path/to/git/repository --data ./scion-data --no-refs
```

By default the importer walks all reachable refs in parent-before-child order. It uses `git cat-file --batch` and first-parent NUL-delimited diffs, without checking out each revision. Durable mappings make repeated runs skip previously ingested commits. Imported references retain their canonical Git names, including `refs/heads/main` and `refs/tags/v1`. Branches also receive shortened aliases such as `main`, unless that alias would start with `refs/`. Tags use only their canonical names, so a tag `v1` cannot collide with a branch `tags/v1`. Raw annotated tag objects are retained by Git object ID with a mutable name pointer, allowing a later import to follow force-updated tags.

The bridge preserves supported file contents, modes, ordered parents, author/committer metadata, and raw Git headers for auditing. Scion computes independent changeset IDs. Paths and commit metadata must be UTF-8 and pass Scion's repository-path rules. Submodule entries retain their referenced Git object ID; the importer does not recursively clone submodules.

Export a reachable DAG to a Git repository:

```sh
scn --repo demo export-git main ./exported.git --data ./scion-data --ref refs/heads/main
git -C ./exported.git log refs/heads/main
```

Export uses Git's `fast-import`. It preserves history structure, file modes, content, author/committer timezones, and distinct committer timestamps. SHA-256-origin or SHA-256 submodule history selects a SHA-256 Git destination; an existing destination with an incompatible object format is rejected before import begins. Export does not promise original Git commit IDs or signature validity because Scion and Git metadata identities differ. Use a dedicated destination repository and inspect the result before introducing it into another workflow.

## Cache snapshots

```sh
scn --cache-bytes 536870912 cache export ./node-cache ./cache.snapshot
scn --cache-bytes 536870912 cache import ./new-cache ./cache.snapshot
scn --cache-bytes 536870912 cache stats ./new-cache
```

Use the intended byte budget when opening an existing cache. Objects beyond the budget can be evicted. A cache directory belongs to one process at a time; stop its daemon before using standalone cache commands on that directory. Snapshot import verifies framing, object sizes, and hashes. Earlier verified frames remain usable after an interrupted import; rerunning import is idempotent.

## Linux filesystem mount

Build the optional FUSE feature:

```sh
cargo build -p scion-cli --release --features fuse --locked
mkdir -p /tmp/scion-view
scn --cache-dir ./mount-cache mount main /tmp/scion-view \
  --sparse src --uid "$(id -u)" --gid "$(id -g)"
```

Without `--writable`, the mount is read-only and remains attached to the selected immutable revision. File contents are fetched when opened and served through local handles; directory metadata comes from the sparse manifest. The host needs `/dev/fuse` and permission to mount a FUSE filesystem. Unmount using the host's FUSE tooling, for example `fusermount3 -u /tmp/scion-view`.

Add `--writable --overlay DIRECTORY` to use a durable editable namespace:

```sh
mkdir -p /tmp/scion-work
scn mount main /tmp/scion-work --writable --overlay ./work-overlay \
  --sparse src --uid "$(id -u)" --gid "$(id -g)"
# Use another terminal for the following commands while the mount runs.
scn overlay status ./work-overlay
scn overlay diff ./work-overlay
scn overlay commit ./work-overlay -m "Update source" --author "Ada" --ref main
```

Keep the backing directory outside the mountpoint, with neither directory containing the other. Commit flushes the validated mount's kernel writeback before taking the overlay write lease. Quiesce concurrent application writers for an application-consistent checkpoint. Remount an existing overlay at the revision reported by `scn overlay info`, which may differ from a moving branch ref.

## Overlay management

```sh
scn overlay init main ./work-overlay --sparse src
scn overlay info ./work-overlay
scn overlay status ./work-overlay
scn overlay diff ./work-overlay
scn --cache-bytes 536870912 overlay prune ./work-overlay
scn overlay gc ./work-overlay
```

`init` creates a sparse virtual manifest without downloading file contents. `diff` reports the precise first-parent delta including copy/rename provenance. `prune` evicts only clean hydrated contents and retains dirty or open inodes. `gc` requires an unmounted overlay and reclaims unlinked data and interrupted temporary files while protecting direct API handles. A failed optional reference CAS leaves the successful server commit recorded as the overlay's new revision.

The portable overlay API is reusable on Linux, macOS, and Windows; transparent kernel mounting is Linux-only. Native Windows ProjFS and macOS File Provider integrations are not included. See [working copies](working-copies.md) for file operations, durability, lifetime handling, recovery, and API examples.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Command completed; inspect merge-like result objects for semantic conflicts |
| 1 | Other command, storage, input, or transport failure |
| 2 | Conflict error, including a stale reference CAS |
| 3 | Permission error |
| 4 | Requested object, revision, or path not found |

Errors are written to standard error; with `--json`, the error text is wrapped in a JSON object. Binary file contents from non-JSON `cat` are written only to standard output.
