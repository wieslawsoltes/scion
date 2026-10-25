//! Linux adapter for the independently testable durable overlay.
use crate::{
    Client,
    overlay::{
        BlobSource, NodeAttr, NodeKind, OpenFileOptions, Overlay, OverlayHandle, RenameMode,
        SetAttributes,
    },
};
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use scion_protocol::ObjectWitness;
use scion_types::{Error, Result};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::File,
    io::ErrorKind,
    os::unix::ffi::OsStrExt,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
const TTL: Duration = Duration::ZERO;
const MAX_HANDLES: usize = 1024;
const MAX_DIRECTORY_BYTES: usize = 64 * 1024 * 1024;
struct RemoteSource {
    client: Client,
    runtime: tokio::runtime::Runtime,
}
impl BlobSource for RemoteSource {
    fn fetch(&self, witness: ObjectWitness) -> Result<Vec<u8>> {
        self.runtime.block_on(self.client.fetch_object(witness))
    }
}
struct DirectoryHandle {
    inode: u64,
    entries: Vec<(u64, FileType, String)>,
    bytes: usize,
}
/// Writable FUSE implementation. Mutable namespace and contents are delegated to
/// `Overlay`; the adapter holds no independent source-control state.
pub struct WritableFs {
    overlay: Overlay,
    source: RemoteSource,
    handles: BTreeMap<u64, OverlayHandle>,
    directories: BTreeMap<u64, DirectoryHandle>,
    directory_bytes: usize,
    next_handle: u64,
    clean_cache_bytes: u64,
    _mount_guard: File,
    mount_identity: Option<(std::path::PathBuf, String)>,
}
impl WritableFs {
    pub fn new(client: Client, overlay: Overlay, clean_cache_bytes: u64) -> Result<Self> {
        let config = overlay.config()?;
        if config.server != client.server() || config.repo != client.repo() {
            return Err(Error::Invalid(
                "mount client does not match overlay repository".into(),
            ));
        }
        let guard = overlay.mount_guard()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        Ok(Self {
            overlay,
            source: RemoteSource { client, runtime },
            handles: BTreeMap::new(),
            directories: BTreeMap::new(),
            directory_bytes: 0,
            next_handle: 1,
            clean_cache_bytes,
            _mount_guard: guard,
            mount_identity: None,
        })
    }
    fn handle_id(&mut self) -> Result<u64> {
        if self.handles.len() + self.directories.len() >= MAX_HANDLES {
            return Err(Error::Io(std::io::Error::from_raw_os_error(libc::EMFILE)));
        }
        let id = self.next_handle;
        self.next_handle = self
            .next_handle
            .checked_add(1)
            .ok_or_else(|| Error::Limit("file handle IDs exhausted".into()))?;
        Ok(id)
    }
    fn open_inode(&mut self, inode: u64, flags: i32) -> Result<u64> {
        let id = self.handle_id()?;
        let handle = self
            .overlay
            .open_file(inode, open_options(flags), &self.source)?;
        self.handles.insert(id, handle);
        // Dirty/open files are protected. The byte budget bounds reusable clean
        // copies, while application-owned open and dirty data may exceed it.
        self.overlay.prune_clean_cache(self.clean_cache_bytes)?;
        Ok(id)
    }
}
fn open_options(flags: i32) -> OpenFileOptions {
    let mode = flags & libc::O_ACCMODE;
    OpenFileOptions {
        read: mode != libc::O_WRONLY,
        write: mode != libc::O_RDONLY,
        append: flags & libc::O_APPEND != 0,
        truncate: flags & libc::O_TRUNC != 0,
        sync: flags & (libc::O_SYNC | libc::O_DSYNC) != 0,
    }
}
fn name(value: &OsStr) -> Result<&str> {
    value
        .to_str()
        .ok_or_else(|| Error::Invalid("Scion paths must be UTF-8".into()))
}
fn kind(kind: NodeKind) -> FileType {
    match kind {
        NodeKind::Directory => FileType::Directory,
        NodeKind::Symlink => FileType::Symlink,
        _ => FileType::RegularFile,
    }
}
fn attr(value: NodeAttr) -> FileAttr {
    FileAttr {
        ino: value.inode,
        size: value.size,
        blocks: value.size.div_ceil(512),
        atime: UNIX_EPOCH + Duration::from_nanos(value.atime_ns),
        mtime: UNIX_EPOCH + Duration::from_nanos(value.mtime_ns),
        ctime: UNIX_EPOCH + Duration::from_nanos(value.ctime_ns),
        crtime: UNIX_EPOCH,
        kind: kind(value.kind),
        perm: (value.mode & 0o7777) as u16,
        nlink: value.nlink,
        uid: value.uid,
        gid: value.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}
fn errno(error: Error) -> i32 {
    match error {
        Error::Io(e) => e.raw_os_error().unwrap_or(match e.kind() {
            ErrorKind::NotFound => libc::ENOENT,
            ErrorKind::AlreadyExists => libc::EEXIST,
            ErrorKind::PermissionDenied => libc::EACCES,
            ErrorKind::NotADirectory => libc::ENOTDIR,
            ErrorKind::IsADirectory => libc::EISDIR,
            ErrorKind::DirectoryNotEmpty => libc::ENOTEMPTY,
            ErrorKind::InvalidInput => libc::EINVAL,
            ErrorKind::Unsupported => libc::EOPNOTSUPP,
            _ => libc::EIO,
        }),
        Error::NotFound(_) => libc::ENOENT,
        Error::Permission(_) => libc::EACCES,
        Error::Invalid(_) => libc::EINVAL,
        Error::Conflict(_) => libc::EBUSY,
        Error::Limit(_) => libc::EFBIG,
        _ => libc::EIO,
    }
}
fn reply_empty(reply: ReplyEmpty, result: Result<()>) {
    match result {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(errno(e)),
    }
}
fn time_ns(time: TimeOrNow) -> u64 {
    let time = match time {
        TimeOrNow::SpecificTime(time) => time,
        TimeOrNow::Now => SystemTime::now(),
    };
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as u64
}
impl Filesystem for WritableFs {
    fn init(
        &mut self,
        _request: &Request<'_>,
        _config: &mut fuser::KernelConfig,
    ) -> std::result::Result<(), i32> {
        if let Some((path, source)) = &self.mount_identity {
            crate::mount_session::register(self.overlay.root(), path, source).map_err(errno)?;
        }
        Ok(())
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, entry: &OsStr, reply: ReplyEntry) {
        match name(entry).and_then(|n| self.overlay.lookup(parent, n)) {
            Ok(node) => reply.entry(&TTL, &attr(node), 0),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.overlay.getattr(ino) {
            Ok(node) => reply.attr(&TTL, &attr(node)),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if flags.is_some_and(|f| f != 0) {
            reply.error(libc::EOPNOTSUPP);
            return;
        }
        match self.overlay.setattr(
            ino,
            SetAttributes {
                mode,
                uid,
                gid,
                size,
                atime_ns: atime.map(time_ns),
                mtime_ns: mtime.map(time_ns),
            },
            &self.source,
        ) {
            Ok(node) => reply.attr(&TTL, &attr(node)),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        match self.overlay.readlink(ino, &self.source) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn mknod(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        entry: &OsStr,
        mode: u32,
        umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        if mode & libc::S_IFMT != libc::S_IFREG {
            reply.error(libc::EOPNOTSUPP);
            return;
        }
        match name(entry).and_then(|n| {
            self.overlay
                .create(parent, n, mode & !umask, req.uid(), req.gid())
        }) {
            Ok(node) => reply.entry(&TTL, &attr(node), 0),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn mkdir(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        entry: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        match name(entry).and_then(|n| {
            self.overlay
                .mkdir(parent, n, mode & !umask, req.uid(), req.gid())
        }) {
            Ok(node) => reply.entry(&TTL, &attr(node), 0),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn unlink(&mut self, _req: &Request<'_>, parent: u64, entry: &OsStr, reply: ReplyEmpty) {
        reply_empty(
            reply,
            name(entry).and_then(|n| self.overlay.unlink(parent, n)),
        );
    }
    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, entry: &OsStr, reply: ReplyEmpty) {
        reply_empty(
            reply,
            name(entry).and_then(|n| self.overlay.rmdir(parent, n)),
        );
    }
    fn symlink(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        entry: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        match name(entry).and_then(|n| {
            self.overlay.symlink(
                parent,
                n,
                target.as_os_str().as_bytes(),
                req.uid(),
                req.gid(),
            )
        }) {
            Ok(node) => reply.entry(&TTL, &attr(node), 0),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        entry: &OsStr,
        new_parent: u64,
        new_entry: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        let mode = match flags {
            0 => RenameMode::Replace,
            libc::RENAME_NOREPLACE => RenameMode::NoReplace,
            libc::RENAME_EXCHANGE => RenameMode::Exchange,
            _ => {
                reply.error(libc::EOPNOTSUPP);
                return;
            }
        };
        reply_empty(
            reply,
            name(entry).and_then(|old| {
                name(new_entry)
                    .and_then(|new| self.overlay.rename(parent, old, new_parent, new, mode))
            }),
        );
    }
    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        match self.open_inode(ino, flags) {
            Ok(handle) => reply.opened(handle, 0),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn create(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        entry: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        if self.handles.len() + self.directories.len() >= MAX_HANDLES {
            reply.error(libc::EMFILE);
            return;
        }
        let node = match name(entry).and_then(|n| {
            self.overlay
                .create(parent, n, mode & !umask, req.uid(), req.gid())
        }) {
            Ok(node) => node,
            Err(e) => {
                reply.error(errno(e));
                return;
            }
        };
        match self.open_inode(node.inode, flags) {
            Ok(handle) => reply.created(&TTL, &attr(node), 0, handle, 0),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _owner: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        let Some(handle) = self.handles.get_mut(&fh).filter(|h| h.inode == ino) else {
            reply.error(libc::EBADF);
            return;
        };
        match self.overlay.read(handle, offset as u64, size as usize) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        let Some(handle) = self.handles.get_mut(&fh).filter(|h| h.inode == ino) else {
            reply.error(libc::EBADF);
            return;
        };
        match self.overlay.write(handle, offset as u64, data) {
            Ok(size) => reply.written(size as u32),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn flush(&mut self, _req: &Request<'_>, ino: u64, fh: u64, _owner: u64, reply: ReplyEmpty) {
        let Some(handle) = self.handles.get(&fh).filter(|h| h.inode == ino) else {
            reply.error(libc::EBADF);
            return;
        };
        reply_empty(reply, self.overlay.fsync(handle, false));
    }
    fn fsync(&mut self, _req: &Request<'_>, ino: u64, fh: u64, data_only: bool, reply: ReplyEmpty) {
        let Some(handle) = self.handles.get(&fh).filter(|h| h.inode == ino) else {
            reply.error(libc::EBADF);
            return;
        };
        reply_empty(reply, self.overlay.fsync(handle, data_only));
    }
    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        _owner: Option<u64>,
        flush: bool,
        reply: ReplyEmpty,
    ) {
        let Some(handle) = self.handles.remove(&fh).filter(|h| h.inode == ino) else {
            reply.error(libc::EBADF);
            return;
        };
        let result = if flush {
            self.overlay.fsync(&handle, false)
        } else {
            Ok(())
        };
        drop(handle);
        reply_empty(
            reply,
            result.and_then(|()| {
                self.overlay
                    .prune_clean_cache(self.clean_cache_bytes)
                    .map(|_| ())
            }),
        );
    }
    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let result = (|| -> Result<u64> {
            let id = self.handle_id()?;
            let listing = self.overlay.readdir(ino)?;
            let mut entries = vec![
                (ino, FileType::Directory, ".".into()),
                (self.overlay.parent(ino)?, FileType::Directory, "..".into()),
            ];
            let mut bytes = 64;
            for entry in listing {
                bytes += 32 + entry.name.len();
                entries.push((entry.attr.inode, kind(entry.attr.kind), entry.name));
            }
            if self.directory_bytes.saturating_add(bytes) > MAX_DIRECTORY_BYTES {
                return Err(Error::Limit(
                    "open directory snapshots exceed 64 MiB".into(),
                ));
            }
            self.directory_bytes += bytes;
            self.directories.insert(
                id,
                DirectoryHandle {
                    inode: ino,
                    entries,
                    bytes,
                },
            );
            Ok(id)
        })();
        match result {
            Ok(id) => reply.opened(id, 0),
            Err(e) => reply.error(errno(e)),
        }
    }
    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        let Some(handle) = self.directories.get(&fh).filter(|h| h.inode == ino) else {
            reply.error(libc::EBADF);
            return;
        };
        for (index, (id, kind, name)) in handle.entries.iter().enumerate().skip(offset as usize) {
            if reply.add(*id, (index + 1) as i64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }
    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        if let Some(handle) = self.directories.remove(&fh).filter(|h| h.inode == ino) {
            self.directory_bytes -= handle.bytes;
            reply.ok();
        } else {
            reply.error(libc::EBADF);
        }
    }
    fn fsyncdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply_empty(reply, self.overlay.fsync_directory(ino));
    }
    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        match (
            fs2::total_space(self.overlay.root()),
            fs2::free_space(self.overlay.root()),
            fs2::available_space(self.overlay.root()),
        ) {
            (Ok(total), Ok(free), Ok(available)) => reply.statfs(
                total / 4096,
                free / 4096,
                available / 4096,
                0,
                0,
                4096,
                255,
                4096,
            ),
            _ => reply.error(libc::EIO),
        }
    }
    fn access(&mut self, _req: &Request<'_>, _ino: u64, _mask: i32, reply: ReplyEmpty) {
        // mount_writable always uses DefaultPermissions, including supplementary
        // groups and root's execute-bit rule in the kernel.
        reply.ok();
    }
}
/// Blocks until unmounted. The overlay directory must be outside the mountpoint;
/// this prevents the backing store recursively depending on its own FUSE mount.
pub fn mount_writable(
    client: Client,
    overlay: Overlay,
    mountpoint: &Path,
    clean_cache_bytes: u64,
) -> Result<()> {
    let mountpoint = std::fs::canonicalize(mountpoint)?;
    if !mountpoint.is_dir() {
        return Err(Error::Invalid("mountpoint must be a directory".into()));
    }
    if std::fs::read_dir(&mountpoint)?.next().is_some() {
        return Err(Error::Conflict("mountpoint must be empty".into()));
    }
    if overlay.root().starts_with(&mountpoint) || mountpoint.starts_with(overlay.root()) {
        return Err(Error::Invalid(
            "overlay storage and mountpoint must not contain one another".into(),
        ));
    }
    let revision = overlay.config()?.revision;
    let source = format!(
        "scion-overlay:{}:{}:{}",
        revision.short(),
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let mut filesystem = WritableFs::new(client, overlay, clean_cache_bytes)?;
    filesystem.mount_identity = Some((mountpoint.clone(), source.clone()));
    fuser::mount2(
        filesystem,
        mountpoint,
        &[
            MountOption::RW,
            MountOption::FSName(source),
            MountOption::Subtype("scion-overlay".into()),
            MountOption::DefaultPermissions,
            MountOption::NoDev,
            MountOption::NoSuid,
        ],
    )?;
    Ok(())
}
