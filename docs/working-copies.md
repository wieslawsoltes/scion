# Working copies and writable overlays

Scion offers three ways to work with files. They share the server-side repository
model and verified object transport, but keep different local state.

| Mode | Materialization | Editing | Platforms |
|---|---|---|---|
| Ordinary checkout | Selected files downloaded at clone/checkout | Normal filesystem tools; `scn commit` | Linux, macOS, Windows |
| Lazy checkout | Selected manifest first; explicit `scn hydrate` | Normal tools after hydration; `scn commit` | Linux, macOS, Windows |
| Writable overlay | Manifest first; base bytes fetched on access | Reusable overlay API or Linux FUSE; `scn overlay commit` | Portable API; Linux kernel mount |

Read-only FUSE mounts remain available for immutable source views. Native Windows
ProjFS and macOS File Provider adapters are not included.

## Ordinary sparse and lazy checkouts

```sh
scn clone main work --sparse src --sparse tests
scn clone main lazy-work --sparse src --lazy
cd lazy-work
scn hydrate src/parser.rs
scn status --stats
scn commit -m "Update parser" --author "Ada <ada@example.test>"
```

Sparse matching uses path components: `src` includes its descendants, but not
`src-old`. An unmaterialized tracked file remains unchanged when it is absent.
Deleting a previously materialized file is an actual deletion. Commits record
selected working-copy changes without requiring a staging area or push.

A commit optionally updates a ref with `--ref NAME`, using the previous working
revision as its compare-and-swap expectation. If the ref moved concurrently, the
new commit remains durable and the working copy advances to it. Inspect the
reported ID and reconcile the histories before retrying a ref update.

### Status caching

`scn status --stats` reports `files_examined`, `files_hashed`, `bytes_hashed`,
`cache_hits`, and `untracked_entries`. On Unix, cached content hashes require
matching device, inode, change time, modification time, mode, link count, and
length. An observation inside the conservative two-second timestamp race window
is rehashed; waiting later does not retroactively validate that observation.
Platforms without a sufficiently reliable identity/change-time fingerprint
verify content instead.

The cache is disposable. Invalid cache contents trigger verification, and file
notifications invalidate affected paths. A settled unchanged Unix checkout can
hash zero content bytes while still scanning metadata and discovering untracked
files. `scn status --refresh` forces content verification, including on filesystems
whose remote attribute caching makes local stat observations insufficient.
`scn watch` releases the working-copy lock while waiting so other commands can run.

### Checkout recovery

Checkout stages downloads, rechecks local edits, and records a durable recovery
journal before installation. An interrupted checkout is rolled back when the
working copy next opens. Files changed outside the transaction are preserved and
reported as recovery conflicts. Keep the journal and backup files with the
working copy until recovery completes. Case/Unicode-equivalent collisions are
rejected before materialization on Windows and macOS.

## Writable Linux mount

Build the optional client feature and create a mountpoint:

```sh
cargo build --release -p scion-cli --features fuse --locked
mkdir -p /tmp/scion-work
scn mount main /tmp/scion-work --writable --overlay ./work-overlay \
  --sparse src --uid "$(id -u)" --gid "$(id -g)"
```

The mount command runs until unmounted. In another terminal:

```sh
# Edit source files under /tmp/scion-work with normal filesystem tools.
scn overlay status ./work-overlay
scn overlay diff ./work-overlay
scn overlay commit ./work-overlay -m "Update source" \
  --author "Ada <ada@example.test>" --ref main
scn overlay info ./work-overlay
fusermount3 -u /tmp/scion-work
```

The backing directory and mountpoint must be separate and neither may contain the
other. The backing directory holds SQLite metadata, local inode bytes, lifetime
pins, and verified cache data. Treat it as private storage; edit source files
through the mount or overlay API. Runtime mounting needs Linux, `/dev/fuse`, and
mount permissions. Building the feature does not require libfuse development
headers.

An existing overlay retains its current revision and sparse selection. Remount it
using the exact revision from `scn overlay info`; a changed `main` ref may no longer
match that baseline. A second active mount of the same overlay is rejected so
independent kernels cannot cache conflicting mutable state.

### Filesystem behavior

The overlay implements file creation, reads/writes, append, sparse holes,
truncation, directory creation/removal, symlinks, attributes, rename, unlink, and
explicit synchronization. Rename preserves copy provenance. Unchanged virtual
files can be renamed or committed without downloading their contents; truncating
a virtual file to zero avoids fetching bytes that will be discarded. New paths
must remain inside the sparse selection.

Open handles keep their inode after rename, replacement, or unlink. The FUSE
adapter also retains kernel lookup references, including `O_PATH` descriptors,
until `forget`. Separate lifetime-pin files preserve this behavior on Windows
without placing mandatory locks on writable content handles. Empty directories,
user/group ownership, and permission bits beyond the executable distinction are
local filesystem state; Scion history versions supported file entries.

### Commit and durability

Namespace changes use FULL-synchronous SQLite WAL transactions. A clean-to-dirty
transition is recorded before bytes are modified. Further writes to an already
dirty inode avoid repeated metadata synchronization. `fsync` persists the file
and associated metadata.

A mounted commit first validates the active mount identity and uses Linux
`syncfs` to flush pending kernel writes, including shared writable mappings.
It then acquires the cross-process overlay write lease, verifies and synchronizes
dirty data, uploads changed contents, creates the server changeset, and advances
the local baseline. Reverted dirty files are synchronized before their dirty
ledger is cleared, even when they contribute no final delta. Existing handles
remain usable and can dirty the inode again after the commit.

The writeback barrier does not turn concurrent application writes into an atomic
multi-file transaction. Quiesce writers when an application-consistent checkpoint
is required. The caller must share the active mount namespace for a mounted
commit; stale inactive mount records are ignored. A failed optional ref CAS does
not erase the server changeset or revert the new overlay baseline.

SQLite/file synchronization and restart tests establish the exercised durability
contract. They are not evidence of every filesystem's power-loss behavior. An
interruption between a successful server commit and local baseline persistence
can leave a durable server changeset without an updated overlay baseline; inspect
server history and the overlay before resubmitting uncertain work.

### Cache limits and cleanup

Clean hydrated files can be evicted and downloaded again. Dirty files and open
inodes are retained even when they exceed the requested clean-cache budget.
Byte accounting is maintained transactionally instead of scanning all hydrated
inodes on each open.

```sh
scn --cache-bytes 536870912 overlay prune ./work-overlay
# After unmounting, reclaim unlinked data and interrupted temporary files:
scn overlay gc ./work-overlay
```

Mounted orphan cleanup waits for the final handle and kernel lookup reference.
Offline cleanup protects handles opened through the portable API and rejects an
active mount. Writable mount startup and clean unmount also perform recovery cleanup.
Overlay database version one upgrades to version two for clean-cache accounting;
the configuration format remains version one. Preserve the complete backing
directory when backing up local, uncommitted work.

## Reusable overlay API

`scion_client::overlay::Overlay` is independent of FUSE. Its public methods provide
the same durable namespace, file-handle, status, delta, commit, cache, and cleanup
operations. `BlobSource` supplies verified immutable base content; applications
can implement it with their transport or use the network client integration.
The [client crate README](../crates/scion-client/README.md) includes a complete
Rust example that writes and commits a new file without a kernel mount.

The [Linux FUSE workflow](../.github/workflows/fuse.yml) and
[`scripts/smoke_fuse.py`](../scripts/smoke_fuse.py) exercise a real mount with
ordinary I/O, mmap writeback, retained inode identities, two authority commits,
cleanup, and reopen. Portable client tests run on all three supported desktop
operating systems. See [verification evidence](testing.md#recorded-integration-evidence).
