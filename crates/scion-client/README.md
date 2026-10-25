# scion-client

Reusable Rust client, verified object cache, sparse working copies, and durable virtual worktrees for [Scion](https://github.com/wieslawsoltes/scion).

The client sends source-control operations to the authority. Immutable object bytes can travel through a regional cache or a node daemon; a signed capability from the authority is required before a cached object is returned. Content hashes verify identity, while capabilities authorize access.

## Components

| API | Responsibility |
| --- | --- |
| `Client` | Typed asynchronous RPC, capability acquisition, bounded object transfer, authenticated local cache access, sparse manifest traversal and pagination |
| `ObjectCache` | Verified compressed objects, byte budgets, memory/disk eviction, validated snapshot import/export |
| `WorkingCopy` | Locked ordinary checkout, lazy hydration, sparse selection, status, commit, journaled checkout/recovery and filesystem notifications |
| `WorkingCopy::status_report` | Status plus content hashing and cache-hit counters, using a disposable persisted stat cache |
| `overlay::Overlay` | Durable writable virtual namespace and file operations, usable and testable without a kernel mount |
| `fuse::VirtualFs` | Linux read-only mount that downloads file contents on access |
| `fuse::WritableFs` | Linux writable mount backed by `Overlay` |

## Ordinary working copies

```rust,no_run
use scion_client::{Client, WorkingCopy};
use scion_types::{RepoPath, Result};

async fn checkout() -> Result<()> {
    let client = Client::new(
        "http://127.0.0.1:7420", None, "example", None, None,
    )?;
    let revision = client.resolve("main").await?;
    let mut work = WorkingCopy::clone(
        &client, revision, "work", vec![RepoPath::new("src")?], true,
    ).await?;
    work.hydrate(&client, &RepoPath::new("src/main.rs")?).await?;
    let report = work.status_report()?;
    println!("{} changes; {} bytes hashed", report.changes.len(), report.stats.bytes_hashed);
    Ok(())
}
```

The metadata format distinguishes virtual files from materialized files. An absent virtual file remains unchanged. Physical file/directory replacements are explicit edits, and checkout protects untracked files and refuses symlink ancestors. Checkout uses a durable journal so an interrupted update can recover its previous state without overwriting unrelated subsequent edits.

On Unix, a status-cache hit requires matching device, inode, change time, modification time, mode, link count and length. A verified observation must be outside a conservative two-second timestamp race window. An observation made too close to a write must be rehashed after that window; merely waiting never makes the earlier observation trustworthy. Systems without a reliable change-time/identity fingerprint verify content instead. Cache corruption causes a content scan, and the source-control state format remains compatible.

`StatusStats` exposes `files_examined`, `files_hashed`, `bytes_hashed`, `cache_hits`, and `untracked_entries`. A settled, unchanged Unix worktree can report zero bytes hashed. The scan still checks filesystem metadata and discovers untracked files; the counters do not claim that these operations are free. `invalidate_status_cache` and `invalidate_status_paths` let callers discard affected fingerprints after notifications or a full rescan. `scn status --refresh` forces content verification when a filesystem's remote attribute caching requires it.

## Durable writable overlays

An overlay stores an immutable manifest, a transactional namespace, and local inode data. Opening a virtual file fetches and verifies its immutable blob. Truncating a virtual file to zero does not fetch the discarded contents. Unchanged virtual files do not need downloads for status, rename, or commit.

```rust,no_run
use scion_client::{Client, overlay::{Overlay, OverlayConfig, OpenFileOptions, ROOT_INODE}};
use scion_protocol::ObjectWitness;
use scion_types::{Error, Result};

async fn write_new_file(client: &Client) -> Result<()> {
    let revision = client.resolve("main").await?;
    let files = client.manifest(revision, &[]).await?;
    let overlay = Overlay::initialize(
        "overlay", OverlayConfig::new(client, revision, vec![], 1000, 1000), files,
    )?;
    let file = overlay.create(ROOT_INODE, "new.txt", 0o644, 1000, 1000)?;
    // New local files do not need a provider. Supply a real BlobSource when
    // opening immutable base files; the FUSE adapter supplies the online client.
    let source = |_witness: ObjectWitness| -> Result<Vec<u8>> {
        Err(Error::NotFound("no remote source configured".into()))
    };
    let mut handle = overlay.open_file(file.inode, OpenFileOptions {
        read: true, write: true, ..Default::default()
    }, &source)?;
    overlay.write(&mut handle, 0, b"hello\n")?;
    overlay.fsync(&handle, false)?;
    overlay.commit(client, "Ada".into(), "Add new file".into(), Some("main".into())).await?;
    Ok(())
}
```

The reusable core implements create, reads, writes, append, holes, truncate, directories, symlinks, attribute changes, rename with replacement/no-replacement/exchange, unlink, and explicit synchronization. Open handles retain inode identity after rename, replacement, and unlink. The FUSE adapter also retains kernel lookup references, including `O_PATH` descriptors, until `forget`. Unlinked data is reclaimed after the final reference closes; `Overlay::collect_orphans` performs offline recovery cleanup while protecting direct API handles and ignoring inactive stale mount records. New destinations must remain in the sparse selection. Empty directories and ownership/permission bits beyond the executable distinction are local filesystem state; the Scion history model versions file entries.

Namespace operations use SQLite WAL with full synchronization. The clean-to-dirty transition is recorded before modifying data. Repeated writes to an already-dirty inode avoid another metadata transaction; `fsync` persists data and metadata. Explicit commit holds a process-shared operation lease while uploading changed bytes, creating the server changeset, and advancing the local baseline. Existing handles continue to work after that baseline change. Reference updates use a separate compare-and-swap; a failed reference update does not erase the created commit.

The clean-cache byte counter is maintained transactionally, so a cache within budget does not require scanning every cached inode on each open. Dirty and open files are never evicted. Overlay database version one migrates automatically to version two, which adds cache accounting; the configuration format remains version one. The backing directory is private implementation storage: change source files through the API or mounted namespace, not by editing its internal files.

## Linux mounts

Enable the `fuse` feature. No `libfuse` development library is needed at build time. Runtime mounting requires Linux, `/dev/fuse`, and permission to mount; unprivileged mounting normally uses `fusermount3`.

`fuse::mount` provides a read-only mount. `fuse::mount_writable` uses an existing overlay and a clean-cache byte budget. Both block until unmounted and should be invoked on a blocking thread when the caller uses an async runtime. A writable overlay and its mountpoint must be separate directories, neither containing the other.

Before a mounted overlay commits, Linux `syncfs` flushes kernel writeback, including dirty shared mappings. The client first validates the active mount guard, recorded source and mount ID, kernel mount information, and the opened descriptor's mount ID. Stale inactive records are ignored. Applications modifying files concurrently with the writeback barrier remain a normal snapshot-consistency boundary; callers should quiesce such writers when they require an exact application-level checkpoint.

The portable core has focused correctness and operation-count tests. `scripts/smoke_fuse.py` and the dedicated Linux FUSE workflow additionally exercise the real kernel mount, dirty writable mappings, two server commits, and clean unmount/reopen. The smoke gate fails when its required FUSE capability is absent.

See the workspace [protocol](https://github.com/wieslawsoltes/scion/blob/main/docs/protocol.md), [security](https://github.com/wieslawsoltes/scion/blob/main/SECURITY.md), and [operations](https://github.com/wieslawsoltes/scion/blob/main/docs/operations.md) documentation for deployment contracts.

Licensed under MIT OR Apache-2.0.
