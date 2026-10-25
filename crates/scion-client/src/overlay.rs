//! Durable writable virtual worktrees, independent of the Linux FUSE adapter.
//!
//! The immutable manifest stays in SQLite; file bytes are fetched only on first
//! access. Namespace transactions use SQLite WAL with FULL synchronization. Every
//! content mutation records its dirty inode before changing bytes, and `fsync`
//! synchronizes both data and metadata. A process-shared operation lock makes an
//! explicit commit a stable snapshot while a mount is active. Open handles retain
//! inode identity across rename and unlink. The storage directory is private to
//! this API: edit files through the overlay, never its internal `data` directory.
use crate::{Client, StatusEntry, StatusKind};
use fs2::FileExt;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use scion_protocol::{ObjectWitness, RpcRequest, now_ms};
use scion_types::{
    Changeset, CopySource, Error, FileChange, FileEntry, FileKind, ObjectId, ObjectKind, RepoPath,
    Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const ROOT_INODE: u64 = 1;
const MAX_READ: usize = 16 * 1024 * 1024;

/// Supplies verified immutable bytes. The overlay also verifies the returned
/// length and hash, so custom transports cannot corrupt an immutable base file.
pub trait BlobSource {
    fn fetch(&self, witness: ObjectWitness) -> Result<Vec<u8>>;
}
impl<F: Fn(ObjectWitness) -> Result<Vec<u8>>> BlobSource for F {
    fn fetch(&self, witness: ObjectWitness) -> Result<Vec<u8>> {
        self(witness)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayConfig {
    pub version: u32,
    pub server: String,
    pub objects: String,
    pub repo: String,
    pub revision: ObjectId,
    pub sparse: Vec<RepoPath>,
    pub uid: u32,
    pub gid: u32,
}
impl OverlayConfig {
    pub fn new(
        client: &Client,
        revision: ObjectId,
        sparse: Vec<RepoPath>,
        uid: u32,
        gid: u32,
    ) -> Self {
        Self {
            version: 1,
            server: client.server().into(),
            objects: client.objects().into(),
            repo: client.repo().into(),
            revision,
            sparse,
            uid,
            gid,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Directory,
    File,
    Symlink,
    Submodule,
}
impl NodeKind {
    fn code(self) -> i64 {
        match self {
            Self::Directory => 0,
            Self::File => 1,
            Self::Symlink => 2,
            Self::Submodule => 3,
        }
    }
    fn decode(value: i64) -> Result<Self> {
        match value {
            0 => Ok(Self::Directory),
            1 => Ok(Self::File),
            2 => Ok(Self::Symlink),
            3 => Ok(Self::Submodule),
            _ => Err(Error::Corrupt("unknown overlay node kind".into())),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeAttr {
    pub inode: u64,
    pub kind: NodeKind,
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub atime_ns: u64,
    pub mtime_ns: u64,
    pub ctime_ns: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub attr: NodeAttr,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayStatus {
    pub revision: ObjectId,
    pub changes: Vec<StatusEntry>,
    pub files_hashed: u64,
    pub bytes_hashed: u64,
}
/// Space recovered from unlinked inodes and interrupted unpublished data files.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ReclaimStats {
    pub inodes: u64,
    pub bytes: u64,
    pub interrupted_files: u64,
}
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenFileOptions {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub truncate: bool,
    pub sync: bool,
}
/// Stable open inode. Unlinking its name does not invalidate this handle.
pub struct OverlayHandle {
    pub inode: u64,
    file: File,
    read: bool,
    write: bool,
    append: bool,
    sync: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameMode {
    Replace,
    NoReplace,
    Exchange,
}
#[derive(Debug, Clone, Default)]
pub struct SetAttributes {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime_ns: Option<u64>,
    pub mtime_ns: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Origin {
    commit: ObjectId,
    path: RepoPath,
    entry: FileEntry,
}
#[derive(Clone)]
struct Node {
    attr: NodeAttr,
    origin: Option<Origin>,
    local: bool,
    dirty: bool,
}
/// A process-safe overlay. Instances may be opened by a mount and CLI at once.
pub struct Overlay {
    root: PathBuf,
    db: Mutex<Connection>,
}
impl Overlay {
    pub fn initialize(
        path: impl AsRef<Path>,
        config: OverlayConfig,
        files: BTreeMap<RepoPath, FileEntry>,
    ) -> Result<Self> {
        if config.version != 1 {
            return Err(Error::Invalid("unsupported overlay version".into()));
        }
        fs::create_dir_all(path.as_ref())?;
        if fs::read_dir(path.as_ref())?.next().is_some() {
            return Err(io_error(
                ErrorKind::AlreadyExists,
                "overlay destination must be empty",
            ));
        }
        let root = fs::canonicalize(path)?;
        fs::create_dir(root.join("data"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        let mut conn = connect(&root, true)?;
        conn.execute_batch("CREATE TABLE config (id INTEGER PRIMARY KEY CHECK(id=1), value TEXT NOT NULL);
            CREATE TABLE nodes (inode INTEGER PRIMARY KEY AUTOINCREMENT, kind INTEGER NOT NULL,
              mode INTEGER NOT NULL, uid INTEGER NOT NULL, gid INTEGER NOT NULL,
              atime INTEGER NOT NULL, mtime INTEGER NOT NULL, ctime INTEGER NOT NULL,
              origin TEXT, local INTEGER NOT NULL DEFAULT 0, dirty INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE entries (path TEXT PRIMARY KEY, parent INTEGER NOT NULL, name TEXT NOT NULL,
              inode INTEGER NOT NULL REFERENCES nodes(inode), UNIQUE(parent,name));
            CREATE INDEX entries_inode ON entries(inode);
            CREATE INDEX entries_parent ON entries(parent,name);
            CREATE TABLE base (path TEXT PRIMARY KEY, entry TEXT NOT NULL);
            CREATE TABLE edits (path TEXT PRIMARY KEY, inode INTEGER);
            CREATE INDEX nodes_cache ON nodes(local,dirty,atime,inode);
            CREATE TABLE usage (id INTEGER PRIMARY KEY CHECK(id=1),clean_bytes INTEGER NOT NULL);
            INSERT INTO usage VALUES(1,0);
            PRAGMA user_version=2;") .map_err(sql_error)?;
        let tx = conn.transaction().map_err(sql_error)?;
        tx.execute(
            "INSERT INTO config VALUES(1,?1)",
            [serde_json::to_string(&config)?],
        )
        .map_err(sql_error)?;
        insert_node(
            &tx,
            NodeKind::Directory,
            0o755,
            config.uid,
            config.gid,
            None,
        )?;
        tx.execute("INSERT INTO entries VALUES('',1,'',1)", [])
            .map_err(sql_error)?;
        let mut dirs = BTreeMap::from([(RepoPath::root(), ROOT_INODE)]);
        for (path, entry) in files {
            if path.is_root() {
                return Err(Error::Invalid("overlay manifest contains root file".into()));
            }
            let mut parent = ROOT_INODE;
            let mut prefix = RepoPath::root();
            let components: Vec<_> = path.components().collect();
            for name in &components[..components.len() - 1] {
                prefix = prefix.join(name)?;
                parent = if let Some(inode) = dirs.get(&prefix) {
                    *inode
                } else {
                    let id = insert_node(
                        &tx,
                        NodeKind::Directory,
                        0o755,
                        config.uid,
                        config.gid,
                        None,
                    )?;
                    tx.execute(
                        "INSERT INTO entries VALUES(?1,?2,?3,?4)",
                        params![prefix.as_str(), parent, *name, id],
                    )
                    .map_err(sql_error)?;
                    dirs.insert(prefix.clone(), id);
                    id
                };
            }
            let kind = match entry.kind {
                FileKind::Symlink => NodeKind::Symlink,
                FileKind::Submodule => NodeKind::Submodule,
                _ => NodeKind::File,
            };
            let mode = match entry.kind {
                FileKind::Executable => 0o755,
                FileKind::Symlink => 0o777,
                _ => 0o644,
            };
            let origin = Origin {
                commit: config.revision,
                path: path.clone(),
                entry: entry.clone(),
            };
            let id = insert_node(&tx, kind, mode, config.uid, config.gid, Some(&origin))?;
            tx.execute(
                "INSERT INTO entries VALUES(?1,?2,?3,?4)",
                params![path.as_str(), parent, components.last(), id],
            )
            .map_err(sql_error)?;
            tx.execute(
                "INSERT INTO base VALUES(?1,?2)",
                params![path.as_str(), serde_json::to_string(&entry)?],
            )
            .map_err(sql_error)?;
        }
        tx.commit().map_err(sql_error)?;
        sync_dir(&root)?;
        Ok(Self {
            root,
            db: Mutex::new(conn),
        })
    }
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let root = fs::canonicalize(path)?;
        for name in ["overlay.db", "data"] {
            if fs::symlink_metadata(root.join(name))?
                .file_type()
                .is_symlink()
            {
                return Err(Error::Invalid(
                    "overlay metadata must not be a symlink".into(),
                ));
            }
        }
        let conn = connect(&root, false)?;
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(sql_error)?;
        if version != 1 && version != 2 {
            return Err(Error::Invalid(
                "unsupported overlay database version".into(),
            ));
        }
        let result = Self {
            root,
            db: Mutex::new(conn),
        };
        if version == 1 {
            let _lease = result.lease(true)?;
            let mut conn = result.db.lock();
            // Another process may have migrated while this one waited for the
            // operation lease. Version two adds only disposable cache accounting.
            let version: u32 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .map_err(sql_error)?;
            if version == 1 {
                let tx = conn.transaction().map_err(sql_error)?;
                tx.execute_batch("CREATE INDEX IF NOT EXISTS nodes_cache ON nodes(local,dirty,atime,inode);
                    CREATE TABLE IF NOT EXISTS usage (id INTEGER PRIMARY KEY CHECK(id=1),clean_bytes INTEGER NOT NULL);
                    INSERT OR REPLACE INTO usage SELECT 1,COALESCE(sum(json_extract(origin,'$.entry.size')),0) FROM nodes WHERE local=1 AND dirty=0 AND origin IS NOT NULL;
                    PRAGMA user_version=2;").map_err(sql_error)?;
                tx.commit().map_err(sql_error)?;
            }
        }
        result.config()?;
        Ok(result)
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn config(&self) -> Result<OverlayConfig> {
        let _lease = self.lease(false)?;
        read_config(&self.db.lock())
    }
    /// Prevent two kernels from independently caching mutable state for one overlay.
    /// CLI operations use the operation lock and do not need this session guard.
    pub fn mount_guard(&self) -> Result<File> {
        let lock = lock_file(&self.root.join("mount.lock"))?;
        lock.try_lock_exclusive()
            .map_err(|_| Error::Conflict("overlay is already mounted".into()))?;
        Ok(lock)
    }
    fn lease(&self, write: bool) -> Result<File> {
        let lock = lock_file(&self.root.join("operation.lock"))?;
        if write {
            FileExt::lock_exclusive(&lock)?;
        } else {
            FileExt::lock_shared(&lock)?;
        }
        Ok(lock)
    }
    pub fn getattr(&self, inode: u64) -> Result<NodeAttr> {
        let _lease = self.lease(false)?;
        self.attr_locked(&self.db.lock(), inode)
    }
    fn attr_locked(&self, conn: &Connection, inode: u64) -> Result<NodeAttr> {
        let mut node = get_node(conn, inode)?;
        if node.local {
            let meta = fs::symlink_metadata(self.data_path(inode))?;
            if !meta.is_file() {
                return Err(Error::Corrupt("overlay data is not a regular file".into()));
            }
            node.attr.size = meta.len();
            node.attr.atime_ns = meta
                .accessed()
                .ok()
                .map(system_time_ns)
                .unwrap_or(node.attr.atime_ns);
            node.attr.mtime_ns = meta
                .modified()
                .ok()
                .map(system_time_ns)
                .unwrap_or(node.attr.mtime_ns);
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let ctime = (i128::from(meta.ctime()) * 1_000_000_000
                    + i128::from(meta.ctime_nsec()))
                .clamp(0, i128::from(i64::MAX)) as u64;
                node.attr.ctime_ns = node.attr.ctime_ns.max(ctime);
            }
        }
        let count: u32 = conn
            .query_row(
                "SELECT count(*) FROM entries WHERE inode=?1",
                [inode],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        node.attr.nlink = if node.attr.kind == NodeKind::Directory && count > 0 {
            2 + conn.query_row("SELECT count(*) FROM entries e JOIN nodes n ON n.inode=e.inode WHERE e.parent=?1 AND e.inode<>?1 AND n.kind=0", [inode], |r|r.get::<_,u32>(0)).map_err(sql_error)?
        } else {
            count
        };
        Ok(node.attr)
    }
    pub fn lookup(&self, parent: u64, name: &str) -> Result<NodeAttr> {
        validate_name(name)?;
        let _lease = self.lease(false)?;
        let conn = self.db.lock();
        require_dir(&conn, parent)?;
        self.attr_locked(&conn, lookup_inode(&conn, parent, name)?)
    }
    pub fn resolve(&self, path: &RepoPath) -> Result<NodeAttr> {
        let _lease = self.lease(false)?;
        let conn = self.db.lock();
        let id = conn
            .query_row(
                "SELECT inode FROM entries WHERE path=?1",
                [path.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_error)?
            .ok_or_else(|| io_error(ErrorKind::NotFound, "overlay path does not exist"))?;
        self.attr_locked(&conn, id)
    }
    pub fn parent(&self, inode: u64) -> Result<u64> {
        let _lease = self.lease(false)?;
        self.db
            .lock()
            .query_row("SELECT parent FROM entries WHERE inode=?1", [inode], |r| {
                r.get(0)
            })
            .optional()
            .map_err(sql_error)?
            .ok_or_else(|| io_error(ErrorKind::NotFound, "overlay inode has no name"))
    }
    pub fn readdir(&self, inode: u64) -> Result<Vec<DirectoryEntry>> {
        let _lease = self.lease(false)?;
        let conn = self.db.lock();
        require_dir(&conn, inode)?;
        let mut statement = conn
            .prepare("SELECT name,inode FROM entries WHERE parent=?1 AND inode<>?1 ORDER BY name")
            .map_err(sql_error)?;
        let rows = statement
            .query_map([inode], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?))
            })
            .map_err(sql_error)?;
        let mut entries = Vec::new();
        for row in rows {
            let (name, id) = row.map_err(sql_error)?;
            entries.push(DirectoryEntry {
                name,
                attr: self.attr_locked(&conn, id)?,
            });
        }
        Ok(entries)
    }
    pub fn mkdir(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<NodeAttr> {
        self.create_node(parent, name, NodeKind::Directory, mode, uid, gid, None)
    }
    pub fn create(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<NodeAttr> {
        self.create_node(parent, name, NodeKind::File, mode, uid, gid, Some(&[]))
    }
    pub fn symlink(
        &self,
        parent: u64,
        name: &str,
        target: &[u8],
        uid: u32,
        gid: u32,
    ) -> Result<NodeAttr> {
        if target.contains(&0) || target.len() > 4096 {
            return Err(Error::Invalid("invalid symlink target".into()));
        }
        self.create_node(
            parent,
            name,
            NodeKind::Symlink,
            0o777,
            uid,
            gid,
            Some(target),
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn create_node(
        &self,
        parent: u64,
        name: &str,
        kind: NodeKind,
        mode: u32,
        uid: u32,
        gid: u32,
        bytes: Option<&[u8]>,
    ) -> Result<NodeAttr> {
        validate_name(name)?;
        let _lease = self.lease(true)?;
        let mut conn = self.db.lock();
        require_dir(&conn, parent)?;
        if find_inode(&conn, parent, name)?.is_some() {
            return Err(io_error(ErrorKind::AlreadyExists, "overlay entry exists"));
        }
        let path = inode_path(&conn, parent)?.join(name)?;
        ensure_selected(&read_config(&conn)?, &path, kind == NodeKind::Directory)?;
        let tx = conn.transaction().map_err(sql_error)?;
        let inode = insert_node(&tx, kind, mode & 0o7777, uid, gid, None)?;
        if let Some(bytes) = bytes {
            self.persist_data(inode, bytes)?;
            tx.execute("UPDATE nodes SET local=1,dirty=1 WHERE inode=?1", [inode])
                .map_err(sql_error)?;
            record_edit(&tx, &path, Some(inode))?;
        }
        tx.execute(
            "INSERT INTO entries VALUES(?1,?2,?3,?4)",
            params![path.as_str(), parent, name, inode],
        )
        .map_err(sql_error)?;
        touch(&tx, parent)?;
        tx.commit().map_err(sql_error)?;
        self.attr_locked(&conn, inode)
    }
    pub fn unlink(&self, parent: u64, name: &str) -> Result<()> {
        self.remove(parent, name, false)
    }
    pub fn rmdir(&self, parent: u64, name: &str) -> Result<()> {
        self.remove(parent, name, true)
    }
    fn remove(&self, parent: u64, name: &str, directory: bool) -> Result<()> {
        validate_name(name)?;
        let _lease = self.lease(true)?;
        let mut conn = self.db.lock();
        let inode = lookup_inode(&conn, parent, name)?;
        let node = get_node(&conn, inode)?;
        if directory {
            require_empty_dir(&conn, inode)?;
        } else if node.attr.kind == NodeKind::Directory {
            return Err(io_error(
                ErrorKind::IsADirectory,
                "unlink target is directory",
            ));
        }
        let path = inode_path(&conn, inode)?;
        let tx = conn.transaction().map_err(sql_error)?;
        tx.execute("DELETE FROM entries WHERE path=?1", [path.as_str()])
            .map_err(sql_error)?;
        if !directory {
            record_edit(&tx, &path, None)?;
        }
        touch(&tx, parent)?;
        touch(&tx, inode)?;
        tx.commit().map_err(sql_error)?;
        Ok(())
    }
    pub fn rename(
        &self,
        parent: u64,
        name: &str,
        new_parent: u64,
        new_name: &str,
        mode: RenameMode,
    ) -> Result<()> {
        validate_name(name)?;
        validate_name(new_name)?;
        let _lease = self.lease(true)?;
        let mut conn = self.db.lock();
        require_dir(&conn, parent)?;
        require_dir(&conn, new_parent)?;
        let source = lookup_inode(&conn, parent, name)?;
        let destination = find_inode(&conn, new_parent, new_name)?;
        if parent == new_parent && name == new_name {
            return Ok(());
        }
        if mode == RenameMode::NoReplace && destination.is_some() {
            return Err(io_error(
                ErrorKind::AlreadyExists,
                "rename destination exists",
            ));
        }
        if mode == RenameMode::Exchange && destination.is_none() {
            return Err(io_error(ErrorKind::NotFound, "exchange destination absent"));
        }
        let old = inode_path(&conn, source)?;
        let new = inode_path(&conn, new_parent)?.join(new_name)?;
        if old.contains(&new) || (mode == RenameMode::Exchange && new.contains(&old)) {
            return Err(Error::Invalid(
                "cannot move directory into its descendant".into(),
            ));
        }
        if let Some(dest) = destination
            && mode != RenameMode::Exchange
        {
            let a = get_node(&conn, source)?.attr.kind;
            let b = get_node(&conn, dest)?.attr.kind;
            if a == NodeKind::Directory && b != NodeKind::Directory {
                return Err(io_error(
                    ErrorKind::NotADirectory,
                    "directory rename destination is file",
                ));
            }
            if a != NodeKind::Directory && b == NodeKind::Directory {
                return Err(io_error(
                    ErrorKind::IsADirectory,
                    "file rename destination is directory",
                ));
            }
            if b == NodeKind::Directory {
                require_empty_dir(&conn, dest)?;
            }
        }
        let moving = subtree(&conn, &old)?;
        let replacing = if destination.is_some() {
            subtree(&conn, &new)?
        } else {
            Vec::new()
        };
        let config = read_config(&conn)?;
        for entry in &moving {
            let suffix = &entry.path.as_str()[old.as_str().len()..];
            let path = RepoPath::new(format!("{}{suffix}", new.as_str()))?;
            ensure_selected(&config, &path, entry.kind == NodeKind::Directory)?;
        }
        if mode == RenameMode::Exchange {
            for entry in &replacing {
                let suffix = &entry.path.as_str()[new.as_str().len()..];
                let path = RepoPath::new(format!("{}{suffix}", old.as_str()))?;
                ensure_selected(&config, &path, entry.kind == NodeKind::Directory)?;
            }
        }
        let tx = conn.transaction().map_err(sql_error)?;
        // Remove names in one transaction before inserting either subtree. This
        // permits atomic exchange without temporary user-visible path names.
        for entry in moving.iter().chain(replacing.iter()) {
            tx.execute("DELETE FROM entries WHERE path=?1", [entry.path.as_str()])
                .map_err(sql_error)?;
            if entry.kind != NodeKind::Directory {
                record_edit(&tx, &entry.path, None)?;
            }
        }
        move_entries(&tx, &moving, &old, &new, new_parent, new_name)?;
        if mode == RenameMode::Exchange {
            move_entries(&tx, &replacing, &new, &old, parent, name)?;
        }
        touch(&tx, parent)?;
        touch(&tx, new_parent)?;
        touch(&tx, source)?;
        if let Some(dest) = destination {
            touch(&tx, dest)?;
        }
        tx.commit().map_err(sql_error)?;
        Ok(())
    }
    pub fn open_file(
        &self,
        inode: u64,
        options: OpenFileOptions,
        source: &impl BlobSource,
    ) -> Result<OverlayHandle> {
        if !options.read && !options.write {
            return Err(Error::Invalid("open requires read or write access".into()));
        }
        if options.truncate && !options.write {
            return Err(io_error(
                ErrorKind::PermissionDenied,
                "truncate requires write access",
            ));
        }
        let _lease = self.lease(true)?;
        let mut conn = self.db.lock();
        let node = get_node(&conn, inode)?;
        if node.attr.kind == NodeKind::Directory {
            return Err(io_error(
                ErrorKind::IsADirectory,
                "cannot open directory as file",
            ));
        }
        if node.attr.kind == NodeKind::Symlink {
            return Err(Error::Invalid("use readlink for symbolic links".into()));
        }
        if options.truncate {
            self.truncate_locked(&mut conn, inode, 0, source)?;
        } else {
            self.ensure_local(&conn, &node, source)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(options.write)
            .open(self.data_path(inode))?;
        FileExt::lock_shared(&file)?; // clean-cache eviction must not detach a live inode
        Ok(OverlayHandle {
            inode,
            file,
            read: options.read,
            write: options.write,
            append: options.append,
            sync: options.sync,
        })
    }
    pub fn read(&self, handle: &mut OverlayHandle, offset: u64, length: usize) -> Result<Vec<u8>> {
        if !handle.read {
            return Err(io_error(
                ErrorKind::PermissionDenied,
                "handle is not readable",
            ));
        }
        if length > MAX_READ {
            return Err(Error::Limit("overlay read exceeds 16 MiB".into()));
        }
        let _lease = self.lease(false)?;
        handle.file.seek(SeekFrom::Start(offset))?;
        let available = handle
            .file
            .metadata()?
            .len()
            .saturating_sub(offset)
            .min(length as u64);
        let mut bytes = Vec::with_capacity(available as usize);
        (&mut handle.file).take(available).read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    pub fn write(&self, handle: &mut OverlayHandle, offset: u64, bytes: &[u8]) -> Result<usize> {
        if !handle.write {
            return Err(io_error(
                ErrorKind::PermissionDenied,
                "handle is not writable",
            ));
        }
        let _lease = self.lease(true)?;
        let offset = if handle.append {
            handle.file.metadata()?.len()
        } else {
            offset
        };
        if offset
            .checked_add(bytes.len() as u64)
            .is_none_or(|end| end > scion_store::MAX_OBJECT_SIZE as u64)
        {
            return Err(Error::Limit(
                "overlay file exceeds object size limit".into(),
            ));
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        mark_content_dirty(&mut self.db.lock(), handle.inode)?;
        handle.file.seek(SeekFrom::Start(offset))?;
        handle.file.write_all(bytes)?;
        if handle.sync {
            handle.file.sync_all()?;
        }
        Ok(bytes.len())
    }
    pub fn truncate(&self, inode: u64, size: u64, source: &impl BlobSource) -> Result<()> {
        let _lease = self.lease(true)?;
        self.truncate_locked(&mut self.db.lock(), inode, size, source)
    }
    fn truncate_locked(
        &self,
        conn: &mut Connection,
        inode: u64,
        size: u64,
        source: &impl BlobSource,
    ) -> Result<()> {
        if size > scion_store::MAX_OBJECT_SIZE as u64 {
            return Err(Error::Limit(
                "overlay file exceeds object size limit".into(),
            ));
        }
        let node = get_node(conn, inode)?;
        if node.attr.kind == NodeKind::Directory {
            return Err(io_error(
                ErrorKind::IsADirectory,
                "cannot truncate directory",
            ));
        }
        if node.attr.kind == NodeKind::Symlink {
            return Err(Error::Invalid("cannot truncate symbolic link".into()));
        }
        if !node.local && size == 0 {
            // Truncating a virtual file discards its base bytes without fetching.
            self.persist_data(inode, &[])?;
            let tx = conn.transaction().map_err(sql_error)?;
            dirty_inode(&tx, inode)?;
            tx.execute("UPDATE nodes SET local=1 WHERE inode=?1", [inode])
                .map_err(sql_error)?;
            tx.execute("UPDATE nodes SET mode=mode & 1023 WHERE inode=?1", [inode])
                .map_err(sql_error)?;
            tx.commit().map_err(sql_error)?;
        } else {
            self.ensure_local(conn, &node, source)?;
            mark_content_dirty(conn, inode)?;
            OpenOptions::new()
                .write(true)
                .open(self.data_path(inode))?
                .set_len(size)?;
        }
        Ok(())
    }
    pub fn readlink(&self, inode: u64, source: &impl BlobSource) -> Result<Vec<u8>> {
        let _lease = self.lease(true)?;
        let conn = self.db.lock();
        let node = get_node(&conn, inode)?;
        if node.attr.kind != NodeKind::Symlink {
            return Err(Error::Invalid("inode is not a symlink".into()));
        }
        self.ensure_local(&conn, &node, source)?;
        let bytes = fs::read(self.data_path(inode))?;
        if bytes.len() > 4096 || bytes.contains(&0) {
            return Err(Error::Invalid("unsupported symlink target".into()));
        }
        Ok(bytes)
    }
    pub fn setattr(
        &self,
        inode: u64,
        attributes: SetAttributes,
        source: &impl BlobSource,
    ) -> Result<NodeAttr> {
        let _lease = self.lease(true)?;
        let mut conn = self.db.lock();
        get_node(&conn, inode)?;
        if let Some(size) = attributes.size {
            self.truncate_locked(&mut conn, inode, size, source)?;
        }
        if (attributes.atime_ns.is_some() || attributes.mtime_ns.is_some())
            && get_node(&conn, inode)?.local
        {
            let mut times = fs::FileTimes::new();
            if let Some(time) = attributes.atime_ns {
                times = times
                    .set_accessed(UNIX_EPOCH + Duration::from_nanos(time.min(i64::MAX as u64)));
            }
            if let Some(time) = attributes.mtime_ns {
                times = times
                    .set_modified(UNIX_EPOCH + Duration::from_nanos(time.min(i64::MAX as u64)));
            }
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.data_path(inode))?
                .set_times(times)?;
        }
        let tx = conn.transaction().map_err(sql_error)?;
        if let Some(mode) = attributes.mode {
            tx.execute(
                "UPDATE nodes SET mode=?2 WHERE inode=?1",
                params![inode, mode & 0o7777],
            )
            .map_err(sql_error)?;
            dirty_inode(&tx, inode)?;
        }
        if let Some(uid) = attributes.uid {
            tx.execute(
                "UPDATE nodes SET uid=?2 WHERE inode=?1",
                params![inode, uid],
            )
            .map_err(sql_error)?;
        }
        if let Some(gid) = attributes.gid {
            tx.execute(
                "UPDATE nodes SET gid=?2 WHERE inode=?1",
                params![inode, gid],
            )
            .map_err(sql_error)?;
        }
        if let Some(atime) = attributes.atime_ns {
            tx.execute(
                "UPDATE nodes SET atime=?2 WHERE inode=?1",
                params![inode, atime.min(i64::MAX as u64)],
            )
            .map_err(sql_error)?;
        }
        if let Some(mtime) = attributes.mtime_ns {
            tx.execute(
                "UPDATE nodes SET mtime=?2 WHERE inode=?1",
                params![inode, mtime.min(i64::MAX as u64)],
            )
            .map_err(sql_error)?;
        }
        tx.execute(
            "UPDATE nodes SET ctime=?2 WHERE inode=?1",
            params![inode, nanos()],
        )
        .map_err(sql_error)?;
        tx.commit().map_err(sql_error)?;
        self.attr_locked(&conn, inode)
    }
    /// Flush all writes to this inode and all namespace/dirty metadata. `flush`
    /// may call this too; close is not required for an explicit durable checkpoint.
    pub fn fsync(&self, handle: &OverlayHandle, data_only: bool) -> Result<()> {
        let _lease = self.lease(true)?;
        if data_only {
            handle.file.sync_data()?;
        } else {
            handle.file.sync_all()?;
        }
        self.db
            .lock()
            .execute_batch("PRAGMA wal_checkpoint(FULL)")
            .map_err(sql_error)?;
        sync_dir(&self.root.join("data"))?;
        sync_dir(&self.root)
    }
    pub fn fsync_directory(&self, inode: u64) -> Result<()> {
        let _lease = self.lease(true)?;
        let conn = self.db.lock();
        require_dir(&conn, inode)?;
        conn.execute_batch("PRAGMA wal_checkpoint(FULL)")
            .map_err(sql_error)?;
        sync_dir(&self.root)
    }
    pub fn status(&self) -> Result<OverlayStatus> {
        let _lease = self.lease(false)?;
        let conn = self.db.lock();
        let (_, report) = self.changes_locked(&conn)?;
        Ok(report)
    }
    /// Produce the exact delta without contacting the authority. Commit identity,
    /// provenance validation and reference mutation remain server operations.
    pub fn changes(&self) -> Result<BTreeMap<RepoPath, FileChange>> {
        let _lease = self.lease(false)?;
        Ok(self.changes_locked(&self.db.lock())?.0)
    }
    fn changes_locked(
        &self,
        conn: &Connection,
    ) -> Result<(BTreeMap<RepoPath, FileChange>, OverlayStatus)> {
        let config = read_config(conn)?;
        let mut report = OverlayStatus {
            revision: config.revision,
            changes: Vec::new(),
            files_hashed: 0,
            bytes_hashed: 0,
        };
        let mut changes = BTreeMap::new();
        let mut statement=conn.prepare("SELECT e.path,e.inode,b.entry FROM edits e LEFT JOIN base b ON b.path=e.path ORDER BY e.path").map_err(sql_error)?;
        let rows = statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<u64>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(sql_error)?;
        for row in rows {
            let (path, inode, baseline) = row.map_err(sql_error)?;
            let path = RepoPath::new(path)?;
            let baseline: Option<FileEntry> =
                baseline.map(|v| serde_json::from_str(&v)).transpose()?;
            if let Some(inode) = inode {
                let node = get_node(conn, inode)?;
                if node.attr.kind == NodeKind::Directory {
                    continue;
                }
                let entry = self.node_entry(&node, &mut report)?;
                if baseline.as_ref() == Some(&entry) {
                    continue;
                }
                let copy_from =
                    node.origin
                        .as_ref()
                        .filter(|o| o.path != path)
                        .map(|o| CopySource {
                            changeset: o.commit,
                            path: o.path.clone(),
                        });
                changes.insert(path.clone(), FileChange::Put { entry, copy_from });
                report.changes.push(StatusEntry {
                    path,
                    status: if baseline.is_some() {
                        StatusKind::Modified
                    } else {
                        StatusKind::Added
                    },
                });
            } else if baseline.is_some() {
                changes.insert(path.clone(), FileChange::Delete);
                report.changes.push(StatusEntry {
                    path,
                    status: StatusKind::Deleted,
                });
            }
        }
        Ok((changes, report))
    }
    fn node_entry(&self, node: &Node, report: &mut OverlayStatus) -> Result<FileEntry> {
        let kind = match node.attr.kind {
            NodeKind::File => {
                if node.attr.mode & 0o111 != 0 {
                    FileKind::Executable
                } else {
                    FileKind::Regular
                }
            }
            NodeKind::Symlink => FileKind::Symlink,
            NodeKind::Submodule => FileKind::Submodule,
            NodeKind::Directory => {
                return Err(Error::Invalid("directory has no file entry".into()));
            }
        };
        let (id, size) = if node.local && node.dirty {
            let file = File::open(self.data_path(node.attr.inode))?;
            if file.metadata()?.len() > scion_store::MAX_OBJECT_SIZE as u64 {
                return Err(Error::Limit("overlay file exceeds object limit".into()));
            }
            let (id, size) = ObjectId::hash_reader(
                ObjectKind::Blob,
                file.take(scion_store::MAX_OBJECT_SIZE as u64 + 1),
            )?;
            if size > scion_store::MAX_OBJECT_SIZE as u64 {
                return Err(Error::Limit("overlay file exceeds object limit".into()));
            }
            report.files_hashed += 1;
            report.bytes_hashed += size;
            (id, size)
        } else if let Some(origin) = &node.origin {
            (origin.entry.id, origin.entry.size)
        } else {
            return Err(Error::Corrupt(
                "overlay node has neither local data nor immutable origin".into(),
            ));
        };
        Ok(FileEntry { id, size, kind })
    }
    /// Commit under a cross-process write lease: mounted writes wait, so the
    /// created delta and local baseline describe one exact snapshot. A failed ref
    /// CAS leaves the successful commit in the overlay and reports its identity.
    pub async fn commit(
        &self,
        client: &Client,
        author: String,
        message: String,
        reference: Option<String>,
    ) -> Result<ObjectId> {
        #[cfg(target_os = "linux")]
        crate::mount_session::flush_active(&self.root)?;
        let _lease = self.lease(true)?;
        let (config, changes) = {
            let conn = self.db.lock();
            (read_config(&conn)?, self.changes_locked(&conn)?.0)
        };
        if config.server != client.server() || config.repo != client.repo() {
            return Err(Error::Invalid(
                "client does not match overlay repository".into(),
            ));
        }
        if changes.is_empty() {
            return Err(Error::Invalid("overlay has no changes".into()));
        }
        // No-op/reverted edits are omitted from `changes` but their dirty
        // ledger is also reset by this commit. Persist their final restored
        // bytes before any clean baseline can become durable.
        self.sync_dirty_data(&self.db.lock())?;
        for (path, change) in &changes {
            if let FileChange::Put { entry, .. } = change {
                let node = {
                    let conn = self.db.lock();
                    let inode = conn
                        .query_row(
                            "SELECT inode FROM edits WHERE path=?1",
                            [path.as_str()],
                            |r| r.get(0),
                        )
                        .map_err(sql_error)?;
                    get_node(&conn, inode)?
                };
                if node.local && node.dirty {
                    let file = File::open(self.data_path(node.attr.inode))?;
                    let mut bytes = Vec::with_capacity(entry.size as usize);
                    file.take(scion_store::MAX_OBJECT_SIZE as u64 + 1)
                        .read_to_end(&mut bytes)?;
                    if bytes.len() as u64 != entry.size
                        || ObjectId::hash(ObjectKind::Blob, &bytes) != entry.id
                    {
                        return Err(Error::Conflict(format!(
                            "overlay data changed outside its API at {path}"
                        )));
                    }
                    if client.put_blob(&bytes).await? != entry.id {
                        return Err(Error::Corrupt(
                            "authority returned wrong blob identity".into(),
                        ));
                    }
                }
            }
        }
        let commit = Changeset {
            parents: vec![config.revision],
            author,
            committer: None,
            timestamp_ms: now_ms(),
            message,
            changes: changes.clone(),
            metadata: BTreeMap::new(),
        };
        let id = client.create_commit(commit).await?;
        {
            let mut conn = self.db.lock();
            let tx = conn.transaction().map_err(sql_error)?;
            for (path, change) in &changes {
                match change {
                    FileChange::Delete => {
                        tx.execute("DELETE FROM base WHERE path=?1", [path.as_str()])
                            .map_err(sql_error)?;
                    }
                    FileChange::Put { entry, .. } => {
                        tx.execute("INSERT INTO base VALUES(?1,?2) ON CONFLICT(path) DO UPDATE SET entry=excluded.entry",params![path.as_str(),serde_json::to_string(entry)?]).map_err(sql_error)?;
                        let origin = Origin {
                            commit: id,
                            path: path.clone(),
                            entry: entry.clone(),
                        };
                        tx.execute("UPDATE nodes SET origin=?2 WHERE inode=(SELECT inode FROM entries WHERE path=?1)",params![path.as_str(),serde_json::to_string(&origin)?]).map_err(sql_error)?;
                    }
                }
            }
            let mut next = config.clone();
            next.revision = id;
            tx.execute(
                "UPDATE config SET value=?1 WHERE id=1",
                [serde_json::to_string(&next)?],
            )
            .map_err(sql_error)?;
            clear_dirty_edits(&tx, id, &self.root)?;
            tx.commit().map_err(sql_error)?;
        }
        if let Some(name) = reference {
            let updated: bool = client
                .rpc(RpcRequest::SetRef {
                    name,
                    expected: Some(config.revision),
                    target: Some(id),
                })
                .await?;
            if !updated {
                return Err(Error::Conflict(format!(
                    "created {id}; reference moved concurrently, overlay now points at the new commit"
                )));
            }
        }
        Ok(id)
    }
    fn sync_dirty_data(&self, conn: &Connection) -> Result<usize> {
        let ids = {
            let mut statement = conn.prepare("SELECT inode FROM nodes WHERE local=1 AND dirty=1 AND inode IN (SELECT inode FROM edits WHERE inode IS NOT NULL)").map_err(sql_error)?;
            statement
                .query_map([], |r| r.get::<_, u64>(0))
                .map_err(sql_error)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(sql_error)?
        };
        for inode in &ids {
            // Windows FlushFileBuffers requires write access even when the
            // contents themselves are unchanged by this synchronization.
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.data_path(*inode))?
                .sync_all()?;
        }
        Ok(ids.len())
    }
    /// Evict only clean hydrated base data, oldest access first. Dirty work and
    /// open inodes are never removed; they may exceed the requested cache budget.
    pub fn prune_clean_cache(&self, max_bytes: u64) -> Result<u64> {
        let _lease = self.lease(true)?;
        let mut conn = self.db.lock();
        let mut total: u64 = conn
            .query_row("SELECT clean_bytes FROM usage WHERE id=1", [], |r| r.get(0))
            .map_err(sql_error)?;
        if total <= max_bytes {
            return Ok(0);
        }
        let candidates = {
            let mut statement = conn.prepare("SELECT inode,json_extract(origin,'$.entry.size') FROM nodes WHERE local=1 AND dirty=0 AND origin IS NOT NULL ORDER BY atime,inode").map_err(sql_error)?;
            statement
                .query_map([], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, u64>(1)?)))
                .map_err(sql_error)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(sql_error)?
        };
        let mut removed = 0;
        for (id, size) in candidates {
            if total <= max_bytes {
                break;
            }
            let path = self.data_path(id);
            let file = File::open(&path)?;
            if file.try_lock_exclusive().is_err() {
                continue;
            }
            // The shared data-file locks protect open inode identity. Clearing
            // the cache marker before removing bytes makes crash recovery safe.
            let tx = conn.transaction().map_err(sql_error)?;
            tx.execute("UPDATE nodes SET local=0 WHERE inode=?1", [id])
                .map_err(sql_error)?;
            adjust_clean_bytes(&tx, -(size as i64))?;
            tx.commit().map_err(sql_error)?;
            fs::remove_file(path)?;
            total = total.saturating_sub(size);
            removed += size;
        }
        sync_dir(&self.root.join("data"))?;
        Ok(removed)
    }
    /// Reclaim unreachable local data while no kernel mount is active. Live
    /// `OverlayHandle`s keep a shared data-file lock, so open-unlinked files are
    /// retained. Inactive stale mount records do not block recovery.
    pub fn collect_orphans(&self) -> Result<ReclaimStats> {
        let _mount_guard = self.mount_guard()?;
        let _lease = self.lease(true)?;
        let mut conn = self.db.lock();
        self.collect_orphans_locked(&mut conn)
    }
    fn collect_orphans_locked(&self, conn: &mut Connection) -> Result<ReclaimStats> {
        let ids = {
            let mut statement = conn.prepare("SELECT inode FROM nodes n WHERE NOT EXISTS (SELECT 1 FROM entries e WHERE e.inode=n.inode)").map_err(sql_error)?;
            statement
                .query_map([], |r| r.get::<_, u64>(0))
                .map_err(sql_error)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(sql_error)?
        };
        let mut stats = ReclaimStats::default();
        for id in ids {
            let recovered = self.collect_inode_locked(conn, id)?;
            stats.inodes += recovered.inodes;
            stats.bytes += recovered.bytes;
        }
        // A crash after deleting metadata but before removing data can leave a
        // numeric file with no node. An interrupted tempfile write has a known
        // private prefix. Never follow links or remove unrelated named files.
        for item in fs::read_dir(self.root.join("data"))? {
            let item = item?;
            let name = item.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let unpublished = if let Ok(id) = name.parse::<u64>() {
                !conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM nodes WHERE inode=?1)",
                        [id],
                        |r| r.get::<_, bool>(0),
                    )
                    .map_err(sql_error)?
            } else {
                name.starts_with(".scion-data-") || name.starts_with(".tmp")
            };
            if !unpublished {
                continue;
            }
            let metadata = item.file_type()?;
            if !metadata.is_file() {
                continue;
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(item.path())?;
            if !try_exclusive(&file)? {
                continue;
            }
            let length = file.metadata()?.len();
            drop(file);
            fs::remove_file(item.path())?;
            stats.bytes += length;
            stats.interrupted_files += 1;
        }
        if stats.inodes > 0 || stats.interrupted_files > 0 {
            sync_dir(&self.root.join("data"))?;
        }
        Ok(stats)
    }
    /// The adapter calls this only after the kernel releases lookup references
    /// and after all its file/directory handles close. The data lock also covers
    /// direct users of the reusable API in this or another process.
    #[cfg(all(feature = "fuse", target_os = "linux"))]
    pub(crate) fn collect_unlinked_inode(&self, inode: u64) -> Result<ReclaimStats> {
        let _lease = self.lease(true)?;
        let mut conn = self.db.lock();
        let stats = self.collect_inode_locked(&mut conn, inode)?;
        if stats.inodes > 0 {
            sync_dir(&self.root.join("data"))?;
        }
        Ok(stats)
    }
    fn collect_inode_locked(&self, conn: &mut Connection, inode: u64) -> Result<ReclaimStats> {
        let named: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM entries WHERE inode=?1)",
                [inode],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM nodes WHERE inode=?1)",
                [inode],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        if named || !exists {
            return Ok(ReclaimStats::default());
        }
        let node = get_node(conn, inode)?;
        let path = self.data_path(inode);
        let data = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                let file = OpenOptions::new().read(true).write(true).open(&path)?;
                if !try_exclusive(&file)? {
                    return Ok(ReclaimStats::default());
                }
                Some(file)
            }
            Ok(_) => {
                return Err(Error::Corrupt(
                    "unlinked overlay data is not a regular file".into(),
                ));
            }
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let bytes = data
            .as_ref()
            .map(|f| f.metadata().map(|m| m.len()))
            .transpose()?
            .unwrap_or_default();
        let tx = conn.transaction().map_err(sql_error)?;
        if node.local
            && !node.dirty
            && let Some(origin) = node.origin
        {
            adjust_clean_bytes(&tx, -(origin.entry.size as i64))?;
        }
        tx.execute("DELETE FROM nodes WHERE inode=?1", [inode])
            .map_err(sql_error)?;
        tx.commit().map_err(sql_error)?;
        let had_data = data.is_some();
        // Other API opens cannot race while the operation lease is held. Close
        // our own lock before unlink for Windows delete-sharing compatibility.
        drop(data);
        if had_data {
            fs::remove_file(path)?;
        }
        Ok(ReclaimStats {
            inodes: 1,
            bytes,
            interrupted_files: 0,
        })
    }
    fn data_path(&self, inode: u64) -> PathBuf {
        self.root.join("data").join(inode.to_string())
    }
    fn ensure_local(&self, conn: &Connection, node: &Node, source: &impl BlobSource) -> Result<()> {
        if node.local {
            if !fs::symlink_metadata(self.data_path(node.attr.inode))?.is_file() {
                return Err(Error::Corrupt("invalid local overlay data".into()));
            }
            return Ok(());
        }
        let origin = node
            .origin
            .as_ref()
            .ok_or_else(|| Error::Corrupt("virtual file has no origin".into()))?;
        let bytes = source.fetch(ObjectWitness {
            commit: origin.commit,
            path: origin.path.clone(),
            kind: ObjectKind::Blob,
            id: origin.entry.id,
        })?;
        if bytes.len() as u64 != origin.entry.size
            || ObjectId::hash(ObjectKind::Blob, &bytes) != origin.entry.id
        {
            return Err(Error::Corrupt(
                "overlay source returned invalid blob".into(),
            ));
        }
        self.persist_data(node.attr.inode, &bytes)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.data_path(node.attr.inode))?;
        file.set_times(
            fs::FileTimes::new()
                .set_accessed(UNIX_EPOCH + Duration::from_nanos(node.attr.atime_ns))
                .set_modified(UNIX_EPOCH + Duration::from_nanos(node.attr.mtime_ns)),
        )?;
        file.sync_all()?;
        let tx = conn.unchecked_transaction().map_err(sql_error)?;
        tx.execute(
            "UPDATE nodes SET local=1,atime=?2 WHERE inode=?1",
            params![node.attr.inode, nanos()],
        )
        .map_err(sql_error)?;
        if !node.dirty {
            adjust_clean_bytes(&tx, origin.entry.size as i64)?;
        }
        tx.commit().map_err(sql_error)?;
        Ok(())
    }
    fn persist_data(&self, inode: u64, bytes: &[u8]) -> Result<()> {
        let directory = self.root.join("data");
        let path = self.data_path(inode);
        let mut temp = tempfile::Builder::new()
            .prefix(".scion-data-")
            .tempfile_in(&directory)?;
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        // Called only for unpublished new data or an unmaterialized inode under
        // the exclusive operation lease. A leftover file is an unreferenced
        // interrupted hydration, so replacing it cannot detach a live handle.
        temp.persist(path).map_err(|e| Error::Io(e.error))?;
        sync_dir(&directory)
    }
}

#[derive(Clone)]
struct NamedNode {
    path: RepoPath,
    parent: u64,
    name: String,
    inode: u64,
    kind: NodeKind,
}
fn subtree(conn: &Connection, prefix: &RepoPath) -> Result<Vec<NamedNode>> {
    // Range query avoids LIKE wildcard interpretation and uses the primary index.
    let start = format!("{}/", prefix.as_str());
    let end = format!("{}0", prefix.as_str());
    let mut statement=conn.prepare("SELECT e.path,e.parent,e.name,e.inode,n.kind FROM entries e JOIN nodes n ON n.inode=e.inode WHERE e.path=?1 OR (e.path>=?2 AND e.path<?3) ORDER BY e.path").map_err(sql_error)?;
    let rows = statement
        .query_map(params![prefix.as_str(), start, end], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, u64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })
        .map_err(sql_error)?;
    let mut result = Vec::new();
    for row in rows {
        let (p, parent, name, inode, k) = row.map_err(sql_error)?;
        result.push(NamedNode {
            path: RepoPath::new(p)?,
            parent,
            name,
            inode,
            kind: NodeKind::decode(k)?,
        });
    }
    Ok(result)
}
fn move_entries(
    tx: &Transaction<'_>,
    entries: &[NamedNode],
    from: &RepoPath,
    to: &RepoPath,
    new_parent: u64,
    new_name: &str,
) -> Result<()> {
    for entry in entries {
        let suffix = &entry.path.as_str()[from.as_str().len()..];
        let path = RepoPath::new(format!("{}{suffix}", to.as_str()))?;
        let root = entry.path == *from;
        tx.execute(
            "INSERT INTO entries VALUES(?1,?2,?3,?4)",
            params![
                path.as_str(),
                if root { new_parent } else { entry.parent },
                if root { new_name } else { &entry.name },
                entry.inode
            ],
        )
        .map_err(sql_error)?;
        if entry.kind != NodeKind::Directory {
            record_edit(tx, &path, Some(entry.inode))?;
        }
    }
    Ok(())
}
fn connect(root: &Path, create: bool) -> Result<Connection> {
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        | if create {
            rusqlite::OpenFlags::SQLITE_OPEN_CREATE
        } else {
            rusqlite::OpenFlags::empty()
        };
    let conn = Connection::open_with_flags(root.join("overlay.db"), flags).map_err(sql_error)?;
    conn.busy_timeout(Duration::from_secs(30))
        .map_err(sql_error)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")
        .map_err(sql_error)?;
    Ok(conn)
}
fn read_config(conn: &Connection) -> Result<OverlayConfig> {
    let text: String = conn
        .query_row("SELECT value FROM config WHERE id=1", [], |r| r.get(0))
        .map_err(sql_error)?;
    let config: OverlayConfig = serde_json::from_str(&text)?;
    if config.version != 1 {
        return Err(Error::Invalid("unsupported overlay configuration".into()));
    }
    Ok(config)
}
fn get_node(conn: &Connection, inode: u64) -> Result<Node> {
    let row=conn.query_row("SELECT kind,mode,uid,gid,atime,mtime,ctime,origin,local,dirty FROM nodes WHERE inode=?1",[inode],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,u32>(1)?,r.get::<_,u32>(2)?,r.get::<_,u32>(3)?,r.get::<_,u64>(4)?,r.get::<_,u64>(5)?,r.get::<_,u64>(6)?,r.get::<_,Option<String>>(7)?,r.get::<_,bool>(8)?,r.get::<_,bool>(9)?))).optional().map_err(sql_error)?
        .ok_or_else(||io_error(ErrorKind::NotFound,"overlay inode does not exist"))?;
    let (kind, mode, uid, gid, atime_ns, mtime_ns, ctime_ns, origin, local, dirty) = row;
    let origin: Option<Origin> = origin.map(|s| serde_json::from_str(&s)).transpose()?;
    let size = origin.as_ref().map_or(0, |o| o.entry.size);
    Ok(Node {
        attr: NodeAttr {
            inode,
            kind: NodeKind::decode(kind)?,
            size,
            mode,
            uid,
            gid,
            nlink: 1,
            atime_ns,
            mtime_ns,
            ctime_ns,
        },
        origin,
        local,
        dirty,
    })
}
fn insert_node(
    tx: &Transaction<'_>,
    kind: NodeKind,
    mode: u32,
    uid: u32,
    gid: u32,
    origin: Option<&Origin>,
) -> Result<u64> {
    let time = nanos();
    tx.execute("INSERT INTO nodes(kind,mode,uid,gid,atime,mtime,ctime,origin) VALUES(?1,?2,?3,?4,?5,?5,?5,?6)",params![kind.code(),mode,uid,gid,time,origin.map(serde_json::to_string).transpose()?]).map_err(sql_error)?;
    u64::try_from(tx.last_insert_rowid())
        .map_err(|_| Error::Corrupt("invalid overlay inode".into()))
}
fn lookup_inode(conn: &Connection, parent: u64, name: &str) -> Result<u64> {
    find_inode(conn, parent, name)?
        .ok_or_else(|| io_error(ErrorKind::NotFound, "overlay entry does not exist"))
}
fn find_inode(conn: &Connection, parent: u64, name: &str) -> Result<Option<u64>> {
    conn.query_row(
        "SELECT inode FROM entries WHERE parent=?1 AND name=?2",
        params![parent, name],
        |r| r.get(0),
    )
    .optional()
    .map_err(sql_error)
}
fn inode_path(conn: &Connection, inode: u64) -> Result<RepoPath> {
    let path: String = conn
        .query_row("SELECT path FROM entries WHERE inode=?1", [inode], |r| {
            r.get(0)
        })
        .optional()
        .map_err(sql_error)?
        .ok_or_else(|| io_error(ErrorKind::NotFound, "overlay inode is unlinked"))?;
    RepoPath::new(path)
}
fn require_dir(conn: &Connection, inode: u64) -> Result<()> {
    if get_node(conn, inode)?.attr.kind != NodeKind::Directory {
        return Err(io_error(
            ErrorKind::NotADirectory,
            "overlay inode is not a directory",
        ));
    }
    Ok(())
}
fn require_empty_dir(conn: &Connection, inode: u64) -> Result<()> {
    require_dir(conn, inode)?;
    let children: u64 = conn
        .query_row(
            "SELECT count(*) FROM entries WHERE parent=?1 AND inode<>?1",
            [inode],
            |r| r.get(0),
        )
        .map_err(sql_error)?;
    if children != 0 {
        return Err(io_error(
            ErrorKind::DirectoryNotEmpty,
            "overlay directory is not empty",
        ));
    }
    Ok(())
}
fn record_edit(tx: &Transaction<'_>, path: &RepoPath, inode: Option<u64>) -> Result<()> {
    tx.execute(
        "INSERT INTO edits VALUES(?1,?2) ON CONFLICT(path) DO UPDATE SET inode=excluded.inode",
        params![path.as_str(), inode],
    )
    .map_err(sql_error)?;
    Ok(())
}
// Only the transition to dirty needs durable metadata before data mutation.
// Subsequent writes update the actual data inode's times and size, which getattr
// reads directly. The exclusive operation lease serializes this test with commit.
fn mark_content_dirty(conn: &mut Connection, inode: u64) -> Result<()> {
    let node = get_node(conn, inode)?;
    if node.dirty && node.attr.mode & 0o6000 == 0 {
        return Ok(());
    }
    let tx = conn.transaction().map_err(sql_error)?;
    dirty_inode(&tx, inode)?;
    tx.execute("UPDATE nodes SET mode=mode & 1023 WHERE inode=?1", [inode])
        .map_err(sql_error)?;
    tx.commit().map_err(sql_error)?;
    Ok(())
}
fn clear_dirty_edits(tx: &Transaction<'_>, revision: ObjectId, root: &Path) -> Result<()> {
    // The write fast path reads times from local data instead of rewriting WAL
    // on every write. Preserve them in the clean baseline before that data can
    // be evicted and later hydrated again.
    let dirty_ids = {
        let mut statement = tx.prepare("SELECT inode FROM nodes WHERE local=1 AND dirty=1 AND inode IN (SELECT inode FROM edits WHERE inode IS NOT NULL)").map_err(sql_error)?;
        statement
            .query_map([], |r| r.get::<_, u64>(0))
            .map_err(sql_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(sql_error)?
    };
    for inode in dirty_ids {
        let node = get_node(tx, inode)?;
        let meta = fs::symlink_metadata(root.join("data").join(inode.to_string()))?;
        let atime = meta
            .accessed()
            .ok()
            .map(system_time_ns)
            .unwrap_or(node.attr.atime_ns);
        let mtime = meta
            .modified()
            .ok()
            .map(system_time_ns)
            .unwrap_or(node.attr.mtime_ns);
        let ctime = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                (i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()))
                    .clamp(0, i128::from(i64::MAX)) as u64
            }
            #[cfg(not(unix))]
            {
                node.attr.ctime_ns
            }
        };
        tx.execute(
            "UPDATE nodes SET atime=?2,mtime=?3,ctime=max(ctime,?4) WHERE inode=?1",
            params![inode, atime, mtime, ctime],
        )
        .map_err(sql_error)?;
    }

    // A replacement inode can be byte-identical to its baseline and therefore
    // omitted from the changeset. It still needs an immutable origin before its
    // dirty marker is cleared, so later rename/eviction/reopen remains valid.
    let origins = {
        let mut statement = tx.prepare("SELECT e.path,e.inode,b.entry FROM edits e JOIN base b ON b.path=e.path WHERE e.inode IS NOT NULL").map_err(sql_error)?;
        statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, u64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(sql_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(sql_error)?
    };
    for (path, inode, entry) in origins {
        let origin = Origin {
            commit: revision,
            path: RepoPath::new(path)?,
            entry: serde_json::from_str(&entry)?,
        };
        tx.execute(
            "UPDATE nodes SET origin=?2 WHERE inode=?1",
            params![inode, serde_json::to_string(&origin)?],
        )
        .map_err(sql_error)?;
    }
    // Include reverted/no-op edits omitted from the actual changeset. Otherwise
    // a later write through an existing handle could skip recreating its edit row.
    let newly_clean: i64 = tx.query_row("SELECT COALESCE(sum(json_extract(origin,'$.entry.size')),0) FROM nodes WHERE local=1 AND dirty=1 AND origin IS NOT NULL AND inode IN (SELECT inode FROM edits WHERE inode IS NOT NULL)", [], |r| r.get(0)).map_err(sql_error)?;
    adjust_clean_bytes(tx, newly_clean)?;
    tx.execute(
        "UPDATE nodes SET dirty=0 WHERE inode IN (SELECT inode FROM edits WHERE inode IS NOT NULL)",
        [],
    )
    .map_err(sql_error)?;
    tx.execute("DELETE FROM edits", []).map_err(sql_error)?;
    Ok(())
}
fn adjust_clean_bytes(tx: &Transaction<'_>, delta: i64) -> Result<()> {
    tx.execute(
        "UPDATE usage SET clean_bytes=max(0,clean_bytes+?1) WHERE id=1",
        [delta],
    )
    .map_err(sql_error)?;
    Ok(())
}
fn dirty_inode(tx: &Transaction<'_>, inode: u64) -> Result<()> {
    let node = get_node(tx, inode)?;
    if node.local
        && !node.dirty
        && let Some(origin) = &node.origin
    {
        adjust_clean_bytes(tx, -(origin.entry.size as i64))?;
    }
    tx.execute(
        "UPDATE nodes SET dirty=1,ctime=?2 WHERE inode=?1",
        params![inode, nanos()],
    )
    .map_err(sql_error)?;
    if node.attr.kind != NodeKind::Directory {
        let mut statement = tx
            .prepare("SELECT path FROM entries WHERE inode=?1")
            .map_err(sql_error)?;
        let paths = statement
            .query_map([inode], |r| r.get::<_, String>(0))
            .map_err(sql_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        for path in paths {
            record_edit(tx, &RepoPath::new(path)?, Some(inode))?;
        }
    }
    Ok(())
}
fn touch(tx: &Transaction<'_>, inode: u64) -> Result<()> {
    tx.execute(
        "UPDATE nodes SET mtime=?2,ctime=?2 WHERE inode=?1",
        params![inode, nanos()],
    )
    .map_err(sql_error)?;
    Ok(())
}
fn ensure_selected(config: &OverlayConfig, path: &RepoPath, directory: bool) -> Result<()> {
    if !config.sparse.is_empty()
        && !config
            .sparse
            .iter()
            .any(|prefix| prefix.contains(path) || (directory && path.contains(prefix)))
    {
        return Err(Error::Permission(format!(
            "{path} is outside this overlay's sparse selection"
        )));
    }
    Ok(())
}
fn validate_name(name: &str) -> Result<()> {
    let path = RepoPath::new(name)?;
    if path.is_root() || path.components().count() != 1 || name == ".scion" || name == ".git" {
        return Err(Error::Invalid("invalid overlay entry name".into()));
    }
    Ok(())
}
fn try_exclusive(file: &File) -> Result<bool> {
    match file.try_lock_exclusive() {
        Ok(()) => Ok(true),
        Err(error)
            if error.kind() == ErrorKind::WouldBlock
                || error.raw_os_error()
                    == Some(fs2::lock_contended_error().raw_os_error().unwrap_or(-1)) =>
        {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}
fn lock_file(path: &Path) -> Result<File> {
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Error::Invalid("overlay lock must not be a symlink".into()));
    }
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?)
}
fn system_time_ns(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as u64
}
fn nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as u64
}
fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn io_error(kind: ErrorKind, message: &str) -> Error {
    Error::Io(std::io::Error::new(kind, message))
}
fn sql_error(error: rusqlite::Error) -> Error {
    Error::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    fn fixture() -> (
        tempfile::TempDir,
        Overlay,
        impl BlobSource,
        Arc<AtomicUsize>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"hello";
        let entry = FileEntry {
            id: ObjectId::hash(ObjectKind::Blob, bytes),
            size: 5,
            kind: FileKind::Regular,
        };
        let config = OverlayConfig {
            version: 1,
            server: "http://localhost:7420".into(),
            objects: "http://localhost:7420".into(),
            repo: "test".into(),
            revision: ObjectId::hash(ObjectKind::Changeset, b"base"),
            sparse: vec![],
            uid: 1000,
            gid: 1000,
        };
        let files = BTreeMap::from([
            (RepoPath::new("src/a").unwrap(), entry.clone()),
            (RepoPath::new("src/b").unwrap(), entry.clone()),
            (RepoPath::new("untouched").unwrap(), entry),
        ]);
        let overlay = Overlay::initialize(dir.path(), config, files).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let source = move |_witness: ObjectWitness| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(bytes.to_vec())
        };
        (dir, overlay, source, count)
    }
    fn rw() -> OpenFileOptions {
        OpenFileOptions {
            read: true,
            write: true,
            ..Default::default()
        }
    }
    #[test]
    fn lazy_reads_and_truncation_do_not_download_untouched_files() {
        let (_dir, overlay, source, calls) = fixture();
        assert!(overlay.status().unwrap().changes.is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let a = overlay.resolve(&RepoPath::new("src/a").unwrap()).unwrap();
        let mut h = overlay.open_file(a.inode, rw(), &source).unwrap();
        assert_eq!(overlay.read(&mut h, 1, 3).unwrap(), b"ell");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let b = overlay.resolve(&RepoPath::new("src/b").unwrap()).unwrap();
        overlay.truncate(b.inode, 0, &source).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let delta = overlay.changes().unwrap();
        assert_eq!(delta.len(), 1);
        assert!(
            matches!(delta[&RepoPath::new("src/b").unwrap()],FileChange::Put{ref entry,..} if entry.size==0)
        );
        assert_eq!(overlay.status().unwrap().files_hashed, 1);
        assert_eq!(overlay.prune_clean_cache(0).unwrap(), 0); // live a is pinned
        drop(h);
        assert_eq!(overlay.prune_clean_cache(0).unwrap(), 5);
        let mut h = overlay.open_file(a.inode, rw(), &source).unwrap();
        assert_eq!(overlay.read(&mut h, 0, 10).unwrap(), b"hello");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
    #[test]
    fn writes_holes_append_truncate_fsync_and_reopen_are_durable() {
        let (dir, overlay, source, _) = fixture();
        let file = overlay.create(1, "new", 0o644, 1, 2).unwrap();
        let mut handle = overlay.open_file(file.inode, rw(), &source).unwrap();
        overlay.write(&mut handle, 3, b"abc").unwrap();
        assert_eq!(overlay.read(&mut handle, 0, 10).unwrap(), b"\0\0\0abc");
        overlay.truncate(file.inode, 4, &source).unwrap();
        let mut app = overlay
            .open_file(
                file.inode,
                OpenFileOptions {
                    append: true,
                    ..rw()
                },
                &source,
            )
            .unwrap();
        overlay.write(&mut app, 0, b"z").unwrap();
        assert_eq!(overlay.read(&mut handle, 0, 10).unwrap(), b"\0\0\0az");
        overlay.fsync(&app, false).unwrap();
        drop(handle);
        drop(app);
        drop(overlay);
        let overlay = Overlay::open(dir.path()).unwrap();
        let mut handle = overlay.open_file(file.inode, rw(), &source).unwrap();
        assert_eq!(overlay.read(&mut handle, 0, 10).unwrap(), b"\0\0\0az");
        let changes = overlay.changes().unwrap();
        assert!(
            matches!(changes[&RepoPath::new("new").unwrap()],FileChange::Put{ref entry,..} if entry.size==5 && entry.id==ObjectId::hash(ObjectKind::Blob,b"\0\0\0az"))
        );
    }
    #[test]
    fn rename_overwrite_and_unlink_preserve_open_inode_identity() {
        let (_dir, overlay, source, _) = fixture();
        let src = overlay.resolve(&RepoPath::new("src").unwrap()).unwrap();
        let a = overlay.lookup(src.inode, "a").unwrap();
        let b = overlay.lookup(src.inode, "b").unwrap();
        let mut ha = overlay.open_file(a.inode, rw(), &source).unwrap();
        let mut hb = overlay.open_file(b.inode, rw(), &source).unwrap();
        assert!(
            overlay
                .rename(src.inode, "a", src.inode, "b", RenameMode::NoReplace)
                .is_err()
        );
        overlay
            .rename(src.inode, "a", src.inode, "b", RenameMode::Replace)
            .unwrap();
        assert_eq!(overlay.lookup(src.inode, "b").unwrap().inode, a.inode);
        assert_eq!(overlay.getattr(b.inode).unwrap().nlink, 0);
        overlay.write(&mut hb, 0, b"orphan").unwrap();
        assert_eq!(overlay.read(&mut ha, 0, 20).unwrap(), b"hello");
        overlay.unlink(src.inode, "b").unwrap();
        overlay.write(&mut ha, 0, b"open!").unwrap();
        assert_eq!(overlay.read(&mut ha, 0, 20).unwrap(), b"open!");
        assert_eq!(overlay.getattr(a.inode).unwrap().nlink, 0);
        assert_eq!(
            overlay.changes().unwrap(),
            BTreeMap::from([
                (RepoPath::new("src/a").unwrap(), FileChange::Delete),
                (RepoPath::new("src/b").unwrap(), FileChange::Delete)
            ])
        );
    }
    #[test]
    fn directory_rename_exchange_and_removal_are_transactional() {
        let (dir, overlay, _source, calls) = fixture();
        let src = overlay.resolve(&RepoPath::new("src").unwrap()).unwrap();
        assert!(overlay.rmdir(1, "src").is_err());
        assert!(
            overlay
                .rename(1, "src", src.inode, "loop", RenameMode::Replace)
                .is_err()
        );
        let out = overlay.mkdir(1, "out", 0o755, 1, 1).unwrap();
        overlay
            .rename(1, "src", 1, "out", RenameMode::Exchange)
            .unwrap();
        assert_eq!(overlay.lookup(1, "src").unwrap().inode, out.inode);
        assert_eq!(overlay.lookup(1, "out").unwrap().inode, src.inode);
        assert_eq!(
            overlay
                .resolve(&RepoPath::new("out/a").unwrap())
                .unwrap()
                .kind,
            NodeKind::File
        );
        overlay.rmdir(1, "src").unwrap();
        overlay.fsync_directory(1).unwrap();
        let delta = overlay.changes().unwrap();
        assert_eq!(delta.len(), 4);
        assert!(
            matches!(delta[&RepoPath::new("out/a").unwrap()],FileChange::Put{copy_from:Some(ref origin),..} if origin.path.as_str()=="src/a")
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        drop(overlay);
        let overlay = Overlay::open(dir.path()).unwrap();
        assert_eq!(overlay.changes().unwrap(), delta);
    }
    #[test]
    fn permissions_symlinks_size_limits_and_invalid_sources_are_checked() {
        let (_dir, overlay, source, calls) = fixture();
        let a = overlay.resolve(&RepoPath::new("src/a").unwrap()).unwrap();
        overlay
            .setattr(
                a.inode,
                SetAttributes {
                    mode: Some(0o755),
                    ..Default::default()
                },
                &source,
            )
            .unwrap();
        let delta = overlay.changes().unwrap();
        assert!(
            matches!(delta[&RepoPath::new("src/a").unwrap()],FileChange::Put{ref entry,..} if entry.kind==FileKind::Executable)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let link = overlay.symlink(1, "link", b"src/a", 1000, 1000).unwrap();
        assert_eq!(overlay.readlink(link.inode, &source).unwrap(), b"src/a");
        assert!(overlay.open_file(link.inode, rw(), &source).is_err());
        assert!(
            overlay
                .truncate(a.inode, scion_store::MAX_OBJECT_SIZE as u64 + 1, &source)
                .is_err()
        );
        let corrupt = |_witness: ObjectWitness| Ok(b"wrong".to_vec());
        assert!(matches!(
            overlay.open_file(a.inode, rw(), &corrupt),
            Err(Error::Corrupt(_))
        ));
        assert!(overlay.mkdir(1, "../escape", 0o755, 1, 1).is_err());
    }
    #[test]
    fn two_instances_observe_namespace_and_dirty_changes_without_reopening() {
        let (dir, a, source, _) = fixture();
        let b = Overlay::open(dir.path()).unwrap();
        let guard = a.mount_guard().unwrap();
        assert!(b.mount_guard().is_err());
        let file = b.create(1, "shared", 0o644, 1, 1).unwrap();
        assert_eq!(a.lookup(1, "shared").unwrap().inode, file.inode);
        let mut handle = a.open_file(file.inode, rw(), &source).unwrap();
        a.write(&mut handle, 0, b"shared bytes").unwrap();
        assert_eq!(b.status().unwrap().changes.len(), 1);
        assert_eq!(b.status().unwrap().bytes_hashed, 12);
        b.rename(1, "shared", 1, "renamed", RenameMode::Replace)
            .unwrap();
        assert_eq!(a.read(&mut handle, 0, 20).unwrap(), b"shared bytes");
        assert!(a.lookup(1, "shared").is_err());
        drop(guard);
        assert!(b.mount_guard().is_ok());
    }
    #[test]
    fn sparse_overlay_rejects_unseen_destinations_before_any_mutation() {
        let (_base_dir, base, source, calls) = fixture();
        let mut config = base.config().unwrap();
        config.sparse = vec![RepoPath::new("src").unwrap()];
        let entry = FileEntry {
            id: ObjectId::hash(ObjectKind::Blob, b"hello"),
            size: 5,
            kind: FileKind::Regular,
        };
        let directory = tempfile::tempdir().unwrap();
        let overlay = Overlay::initialize(
            directory.path(),
            config,
            BTreeMap::from([(RepoPath::new("src/a").unwrap(), entry)]),
        )
        .unwrap();
        assert!(matches!(
            overlay.create(1, "unseen", 0o644, 1, 1),
            Err(Error::Permission(_))
        ));
        assert!(matches!(
            overlay.mkdir(1, "unseen", 0o755, 1, 1),
            Err(Error::Permission(_))
        ));
        let src = overlay.lookup(1, "src").unwrap();
        assert!(matches!(
            overlay.rename(src.inode, "a", 1, "unseen", RenameMode::Replace),
            Err(Error::Permission(_))
        ));
        assert!(matches!(
            overlay.rename(1, "src", 1, "unseen", RenameMode::Replace),
            Err(Error::Permission(_))
        ));
        assert!(overlay.status().unwrap().changes.is_empty());
        assert!(overlay.lookup(src.inode, "a").is_ok());
        let new = overlay.create(src.inode, "new", 0o644, 1, 1).unwrap();
        let mut handle = overlay.open_file(new.inode, rw(), &source).unwrap();
        overlay.write(&mut handle, 0, b"allowed").unwrap();
        assert_eq!(overlay.status().unwrap().changes.len(), 1);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn repeated_writes_sync_dirty_ledger_once_and_reverted_edits_can_dirty_again() {
        let (_dir, overlay, source, _) = fixture();
        let node = overlay.resolve(&RepoPath::new("src/a").unwrap()).unwrap();
        let mut handle = overlay.open_file(node.inode, rw(), &source).unwrap();
        overlay.write(&mut handle, 0, b"world").unwrap();
        let changes_after_transition = overlay.db.lock().total_changes();
        for _ in 0..100 {
            overlay.write(&mut handle, 0, b"world").unwrap();
        }
        assert_eq!(overlay.db.lock().total_changes(), changes_after_transition);
        assert_eq!(overlay.getattr(node.inode).unwrap().size, 5);
        overlay.write(&mut handle, 0, b"hello").unwrap();
        assert!(overlay.changes().unwrap().is_empty());
        // This is the baseline transition also used when another file commits.
        {
            let mut conn = overlay.db.lock();
            let tx = conn.transaction().unwrap();
            clear_dirty_edits(&tx, read_config(&tx).unwrap().revision, overlay.root()).unwrap();
            tx.commit().unwrap();
        }
        overlay.write(&mut handle, 0, b"again").unwrap();
        assert!(overlay.db.lock().total_changes() > changes_after_transition);
        assert_eq!(overlay.status().unwrap().changes.len(), 1);
        assert_eq!(overlay.read(&mut handle, 0, 5).unwrap(), b"again");
        overlay.fsync(&handle, false).unwrap();
    }

    #[test]
    fn clean_cache_accounting_is_constant_work_and_migrates_v1() {
        let (dir, overlay, source, _) = fixture();
        let a = overlay.resolve(&RepoPath::new("src/a").unwrap()).unwrap();
        let mut handle = overlay.open_file(a.inode, rw(), &source).unwrap();
        let clean_bytes = || {
            overlay
                .db
                .lock()
                .query_row("SELECT clean_bytes FROM usage", [], |r| r.get::<_, u64>(0))
                .unwrap()
        };
        assert_eq!(clean_bytes(), 5);
        let changes = overlay.db.lock().total_changes();
        for _ in 0..100 {
            assert_eq!(overlay.prune_clean_cache(100).unwrap(), 0);
        }
        assert_eq!(overlay.db.lock().total_changes(), changes);
        overlay.write(&mut handle, 0, b"world").unwrap();
        assert_eq!(clean_bytes(), 0);
        overlay.write(&mut handle, 0, b"hello").unwrap();
        {
            let mut conn = overlay.db.lock();
            let tx = conn.transaction().unwrap();
            clear_dirty_edits(&tx, read_config(&tx).unwrap().revision, overlay.root()).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(clean_bytes(), 5);
        drop(handle);
        // Simulate an original v1 overlay without the optimization table.
        overlay
            .db
            .lock()
            .execute_batch("DROP TABLE usage; DROP INDEX nodes_cache; PRAGMA user_version=1;")
            .unwrap();
        drop(overlay);
        let overlay = Overlay::open(dir.path()).unwrap();
        assert_eq!(overlay.prune_clean_cache(0).unwrap(), 5);
        assert!(overlay.status().unwrap().changes.is_empty());
    }

    #[test]
    fn identical_replacement_inode_keeps_an_origin_after_noop_baseline_reset() {
        let (_dir, overlay, source, _) = fixture();
        let src = overlay.lookup(1, "src").unwrap();
        overlay.unlink(src.inode, "a").unwrap();
        let node = overlay.create(src.inode, "a", 0o644, 1, 1).unwrap();
        let mut handle = overlay.open_file(node.inode, rw(), &source).unwrap();
        overlay.write(&mut handle, 0, b"hello").unwrap();
        assert!(overlay.changes().unwrap().is_empty());
        {
            let mut conn = overlay.db.lock();
            let tx = conn.transaction().unwrap();
            clear_dirty_edits(&tx, read_config(&tx).unwrap().revision, overlay.root()).unwrap();
            tx.commit().unwrap();
        }
        drop(handle);
        assert_eq!(overlay.prune_clean_cache(0).unwrap(), 5);
        overlay
            .rename(src.inode, "a", src.inode, "renamed", RenameMode::Replace)
            .unwrap();
        assert_eq!(overlay.changes().unwrap().len(), 2);
        let mut handle = overlay.open_file(node.inode, rw(), &source).unwrap();
        assert_eq!(overlay.read(&mut handle, 0, 5).unwrap(), b"hello");
    }
    #[test]
    fn commit_barrier_syncs_reverted_dirty_files_omitted_from_the_delta() {
        let (_dir, overlay, source, _) = fixture();
        let a = overlay.resolve(&RepoPath::new("src/a").unwrap()).unwrap();
        let b = overlay.resolve(&RepoPath::new("src/b").unwrap()).unwrap();
        let mut a_handle = overlay.open_file(a.inode, rw(), &source).unwrap();
        let mut b_handle = overlay.open_file(b.inode, rw(), &source).unwrap();
        overlay.write(&mut a_handle, 0, b"world").unwrap();
        overlay.write(&mut a_handle, 0, b"hello").unwrap();
        overlay.write(&mut b_handle, 0, b"world").unwrap();
        assert_eq!(overlay.changes().unwrap().len(), 1);
        let _lease = overlay.lease(true).unwrap();
        assert_eq!(overlay.sync_dirty_data(&overlay.db.lock()).unwrap(), 2);
    }
    #[test]
    fn orphan_collection_preserves_open_unlinked_handles_then_reclaims_bytes() {
        let (_dir, overlay, source, _) = fixture();
        let node = overlay.create(1, "temporary", 0o644, 1, 1).unwrap();
        let mut handle = overlay.open_file(node.inode, rw(), &source).unwrap();
        overlay.write(&mut handle, 0, b"temporary bytes").unwrap();
        overlay.unlink(1, "temporary").unwrap();
        assert_eq!(overlay.collect_orphans().unwrap().inodes, 0);
        overlay.write(&mut handle, 0, b"still open").unwrap();
        assert_eq!(overlay.read(&mut handle, 0, 10).unwrap(), b"still open");
        drop(handle);
        let stats = overlay.collect_orphans().unwrap();
        assert_eq!(stats.inodes, 1);
        assert_eq!(stats.bytes, 15);
        assert!(!overlay.data_path(node.inode).exists());
        assert!(overlay.getattr(node.inode).is_err());
        assert!(overlay.status().unwrap().changes.is_empty());
    }
    #[test]
    fn recovery_collection_cleans_interrupted_files_and_ignores_stale_mount_record() {
        let (dir, overlay, source, _) = fixture();
        let src = overlay.lookup(1, "src").unwrap();
        let a = overlay.lookup(src.inode, "a").unwrap();
        let handle = overlay.open_file(a.inode, rw(), &source).unwrap();
        drop(handle);
        overlay.unlink(src.inode, "a").unwrap();
        fs::write(
            overlay.root().join("mounted.json"),
            b"stale invalid session",
        )
        .unwrap();
        fs::write(overlay.root().join("data/99999999"), b"unpublished").unwrap();
        fs::write(
            overlay.root().join("data/.scion-data-interrupted"),
            b"temporary",
        )
        .unwrap();
        let active = overlay.mount_guard().unwrap();
        assert!(matches!(overlay.collect_orphans(), Err(Error::Conflict(_))));
        drop(active);
        drop(overlay);
        let overlay = Overlay::open(dir.path()).unwrap();
        let stats = overlay.collect_orphans().unwrap();
        assert_eq!(stats.inodes, 1);
        assert_eq!(stats.interrupted_files, 2);
        assert_eq!(stats.bytes, 5 + 11 + 9);
        assert_eq!(overlay.prune_clean_cache(0).unwrap(), 0);
        assert_eq!(overlay.status().unwrap().changes.len(), 1);
        assert!(overlay.collect_orphans().unwrap().inodes == 0);
    }
    #[test]
    fn repeated_atomic_saves_do_not_retain_replaced_local_data() {
        let (_dir, overlay, source, _) = fixture();
        let src = overlay.lookup(1, "src").unwrap();
        for _ in 0..20 {
            let node = overlay.create(src.inode, "save-tmp", 0o644, 1, 1).unwrap();
            let mut handle = overlay.open_file(node.inode, rw(), &source).unwrap();
            overlay.write(&mut handle, 0, b"replacement").unwrap();
            drop(handle);
            overlay
                .rename(src.inode, "save-tmp", src.inode, "a", RenameMode::Replace)
                .unwrap();
            overlay.collect_orphans().unwrap();
        }
        let data_files = fs::read_dir(overlay.root().join("data")).unwrap().count();
        assert_eq!(data_files, 1);
        assert_eq!(overlay.changes().unwrap().len(), 1);
    }
    #[test]
    fn clean_cache_eviction_preserves_restored_file_mtime() {
        let (_dir, overlay, source, _) = fixture();
        let a = overlay.resolve(&RepoPath::new("src/a").unwrap()).unwrap();
        let mut handle = overlay.open_file(a.inode, rw(), &source).unwrap();
        overlay.write(&mut handle, 0, b"world").unwrap();
        overlay.write(&mut handle, 0, b"hello").unwrap();
        let modified = overlay.getattr(a.inode).unwrap().mtime_ns;
        overlay.fsync(&handle, false).unwrap();
        {
            let mut conn = overlay.db.lock();
            let tx = conn.transaction().unwrap();
            clear_dirty_edits(&tx, read_config(&tx).unwrap().revision, overlay.root()).unwrap();
            tx.commit().unwrap();
        }
        drop(handle);
        assert_eq!(overlay.prune_clean_cache(0).unwrap(), 5);
        assert_eq!(overlay.getattr(a.inode).unwrap().mtime_ns, modified);
        let handle = overlay.open_file(a.inode, rw(), &source).unwrap();
        assert_eq!(overlay.getattr(a.inode).unwrap().mtime_ns, modified);
        drop(handle);
    }
}
