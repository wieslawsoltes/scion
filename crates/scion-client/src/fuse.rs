//! Linux read-only on-demand filesystem. Directory metadata comes from the sparse
//! manifest; opening a file fetches its verified blob. Reads use local file handles
//! and kernel caching, avoiding whole-object decompression for each read request.
use crate::Client;
pub use crate::writable_fuse::{WritableFs, mount_writable};
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, Request,
};
use scion_protocol::ObjectWitness;
use scion_types::{Error, FileEntry, FileKind, ObjectId, ObjectKind, RepoPath, Result};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    time::{Duration, UNIX_EPOCH},
};

const TTL: Duration = Duration::from_secs(60);
const MAX_OPEN_BYTES: u64 = 1024 * 1024 * 1024;
#[derive(Clone)]
struct Node {
    path: RepoPath,
    parent: u64,
    entry: Option<FileEntry>,
    children: BTreeMap<String, u64>,
}
struct Handle {
    file: File,
    size: u64,
}
/// Immutable virtual filesystem. All writes are rejected by the read-only mount.
pub struct VirtualFs {
    client: Client,
    revision: ObjectId,
    nodes: BTreeMap<u64, Node>,
    runtime: tokio::runtime::Runtime,
    handles: BTreeMap<u64, Handle>,
    next_handle: u64,
    open_bytes: u64,
    uid: u32,
    gid: u32,
}
impl VirtualFs {
    pub fn new(
        client: Client,
        revision: ObjectId,
        files: BTreeMap<RepoPath, FileEntry>,
        uid: u32,
        gid: u32,
    ) -> Result<Self> {
        let mut nodes = BTreeMap::from([(
            1,
            Node {
                path: RepoPath::root(),
                parent: 1,
                entry: None,
                children: BTreeMap::new(),
            },
        )]);
        let mut paths = BTreeMap::from([(RepoPath::root(), 1u64)]);
        let mut next = 2;
        for (path, entry) in files {
            let components: Vec<_> = path.components().collect();
            let mut parent = 1;
            let mut prefix = RepoPath::root();
            for (i, component) in components.iter().enumerate() {
                prefix = prefix.join(component)?;
                let id = if let Some(id) = paths.get(&prefix) {
                    *id
                } else {
                    let id = next;
                    next += 1;
                    paths.insert(prefix.clone(), id);
                    nodes.insert(
                        id,
                        Node {
                            path: prefix.clone(),
                            parent,
                            entry: None,
                            children: BTreeMap::new(),
                        },
                    );
                    nodes
                        .get_mut(&parent)
                        .ok_or_else(|| Error::Corrupt("virtual tree has no parent".into()))?
                        .children
                        .insert((*component).into(), id);
                    id
                };
                if i + 1 == components.len() {
                    nodes
                        .get_mut(&id)
                        .ok_or_else(|| Error::Corrupt("virtual tree has no file node".into()))?
                        .entry = Some(entry.clone());
                }
                parent = id;
            }
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        Ok(Self {
            client,
            revision,
            nodes,
            runtime,
            handles: BTreeMap::new(),
            next_handle: 1,
            open_bytes: 0,
            uid,
            gid,
        })
    }
    fn kind(node: &Node) -> FileType {
        match node.entry.as_ref().map(|e| e.kind) {
            None => FileType::Directory,
            Some(FileKind::Symlink) => FileType::Symlink,
            _ => FileType::RegularFile,
        }
    }
    fn attr(&self, ino: u64, node: &Node) -> FileAttr {
        let size = node.entry.as_ref().map_or(0, |e| e.size);
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: Self::kind(node),
            perm: match node.entry.as_ref().map(|e| e.kind) {
                None | Some(FileKind::Executable) => 0o555,
                Some(FileKind::Symlink) => 0o777,
                _ => 0o444,
            },
            nlink: if node.entry.is_none() {
                2 + node
                    .children
                    .values()
                    .filter(|id| self.nodes.get(id).is_some_and(|n| n.entry.is_none()))
                    .count() as u32
            } else {
                1
            },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
    fn data(&self, node: &Node) -> Result<Vec<u8>> {
        let entry = node
            .entry
            .as_ref()
            .ok_or_else(|| Error::Invalid("directory has no data".into()))?;
        self.runtime
            .block_on(self.client.fetch_object(ObjectWitness {
                commit: self.revision,
                path: node.path.clone(),
                kind: ObjectKind::Blob,
                id: entry.id,
            }))
    }
}
impl Filesystem for VirtualFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(parent) = self.nodes.get(&parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(id) = parent.children.get(name) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(node) = self.nodes.get(id) else {
            reply.error(libc::EIO);
            return;
        };
        reply.entry(&TTL, &self.attr(*id, node), 0);
    }
    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.nodes.get(&ino) {
            Some(node) => reply.attr(&TTL, &self.attr(ino, node)),
            None => reply.error(libc::ENOENT),
        }
    }
    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        let Some(node) = self.nodes.get(&ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        if Self::kind(node) != FileType::Symlink {
            reply.error(libc::EINVAL);
            return;
        }
        match self.data(node) {
            Ok(bytes) => reply.data(&bytes),
            Err(_) => reply.error(libc::EIO),
        }
    }
    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        if flags & libc::O_ACCMODE != libc::O_RDONLY {
            reply.error(libc::EROFS);
            return;
        }
        let Some(node) = self.nodes.get(&ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(entry) = &node.entry else {
            reply.error(libc::EISDIR);
            return;
        };
        if self.handles.len() >= 256 || self.open_bytes.saturating_add(entry.size) > MAX_OPEN_BYTES
        {
            reply.error(libc::EMFILE);
            return;
        }
        let bytes = match self.data(node) {
            Ok(bytes) => bytes,
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };
        let mut file = match tempfile::tempfile() {
            Ok(file) => file,
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };
        if file.write_all(&bytes).is_err() {
            reply.error(libc::EIO);
            return;
        }
        let size = bytes.len() as u64;
        let handle = self.next_handle;
        self.next_handle += 1;
        self.open_bytes += size;
        self.handles.insert(handle, Handle { file, size });
        reply.opened(handle, 0);
    }
    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        let Some(handle) = self.handles.get_mut(&fh) else {
            reply.error(libc::EBADF);
            return;
        };
        let length = (size as u64).min(handle.size.saturating_sub(offset as u64)) as usize;
        let mut data = vec![0; length];
        if handle.file.seek(SeekFrom::Start(offset as u64)).is_err()
            || handle.file.read_exact(&mut data).is_err()
        {
            reply.error(libc::EIO);
        } else {
            reply.data(&data);
        }
    }
    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Some(handle) = self.handles.remove(&fh) {
            self.open_bytes -= handle.size;
        }
        reply.ok();
    }
    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        let Some(node) = self.nodes.get(&ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        if node.entry.is_some() {
            reply.error(libc::ENOTDIR);
            return;
        }
        let mut entries = vec![
            (ino, FileType::Directory, ".".to_owned()),
            (node.parent, FileType::Directory, "..".to_owned()),
        ];
        entries.extend(node.children.iter().filter_map(|(name, id)| {
            self.nodes
                .get(id)
                .map(|node| (*id, Self::kind(node), name.clone()))
        }));
        for (index, (id, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(id, (index + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}
/// Blocks until unmounted. Requires `/dev/fuse` and a permitted FUSE mount on Linux.
/// Invoke from a blocking thread when called within an async runtime.
pub fn mount(
    client: Client,
    revision: ObjectId,
    files: BTreeMap<RepoPath, FileEntry>,
    mountpoint: &Path,
    uid: u32,
    gid: u32,
) -> Result<()> {
    if !mountpoint.is_dir() {
        return Err(Error::Invalid(
            "mountpoint must be an existing directory".into(),
        ));
    }
    if std::fs::read_dir(mountpoint)?.next().is_some() {
        return Err(Error::Conflict("mountpoint must be empty".into()));
    }
    let fs = VirtualFs::new(client, revision, files, uid, gid)?;
    fuser::mount2(
        fs,
        mountpoint,
        &[
            MountOption::RO,
            MountOption::FSName(format!("scion:{}", revision.short())),
            MountOption::DefaultPermissions,
            MountOption::NoDev,
            MountOption::NoSuid,
        ],
    )?;
    Ok(())
}
