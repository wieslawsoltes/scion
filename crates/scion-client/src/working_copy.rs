//! Sparse working-copy state, safe hydration, dirty checks and filesystem events.
use crate::Client;
use fs2::FileExt;
use futures::{StreamExt, TryStreamExt, stream};
use scion_protocol::{ObjectWitness, RpcRequest, now_ms};
use scion_types::{
    Changeset, Error, FileChange, FileEntry, FileKind, ObjectId, ObjectKind, RepoPath, Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedFile {
    pub entry: FileEntry,
    pub materialized: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkingCopyState {
    pub version: u32,
    pub server: String,
    pub objects: String,
    pub repo: String,
    pub revision: ObjectId,
    pub sparse: Vec<RepoPath>,
    pub files: BTreeMap<RepoPath, TrackedFile>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusKind {
    Added,
    Modified,
    Deleted,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusEntry {
    pub path: RepoPath,
    pub status: StatusKind,
}

/// An exclusively locked working copy. Credentials are never persisted in state.
pub struct WorkingCopy {
    root: PathBuf,
    state: WorkingCopyState,
    _lock: File,
}
impl WorkingCopy {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let root = find_root(path.as_ref())?;
        let metadata = root.join(".scion");
        if fs::symlink_metadata(&metadata)?.file_type().is_symlink() {
            return Err(Error::Invalid(
                "working-copy metadata must not be a symlink".into(),
            ));
        }
        let lock = lock(&metadata)?;
        recover_checkout(&root)?;
        let state: WorkingCopyState =
            serde_json::from_slice(&fs::read(metadata.join("state.json"))?)?;
        if state.version != 1 {
            return Err(Error::Invalid("unsupported working-copy format".into()));
        }
        validate_platform_paths(state.files.keys(), cfg!(any(windows, target_os = "macos")))?;
        Ok(Self {
            root,
            state,
            _lock: lock,
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn state(&self) -> &WorkingCopyState {
        &self.state
    }
    pub async fn clone(
        client: &Client,
        revision: ObjectId,
        destination: impl AsRef<Path>,
        sparse: Vec<RepoPath>,
        lazy: bool,
    ) -> Result<Self> {
        let destination = destination.as_ref();
        fs::create_dir_all(destination)?;
        if fs::read_dir(destination)?.next().is_some() {
            return Err(Error::Conflict("clone destination must be empty".into()));
        }
        let root = fs::canonicalize(destination)?;
        let metadata = root.join(".scion");
        fs::create_dir(&metadata)?;
        let lock = lock(&metadata)?;
        let files: BTreeMap<RepoPath, TrackedFile> = client
            .manifest(revision, &sparse)
            .await?
            .into_iter()
            .map(|(p, e)| {
                (
                    p,
                    TrackedFile {
                        entry: e,
                        materialized: false,
                    },
                )
            })
            .collect();
        validate_platform_paths(files.keys(), cfg!(any(windows, target_os = "macos")))?;
        let state = WorkingCopyState {
            version: 1,
            server: client.server().into(),
            objects: client.objects().into(),
            repo: client.repo().into(),
            revision,
            sparse,
            files,
        };
        let mut wc = Self {
            root,
            state,
            _lock: lock,
        };
        wc.save()?;
        if !lazy {
            wc.hydrate(client, &RepoPath::root()).await?;
        }
        Ok(wc)
    }
    pub fn save(&self) -> Result<()> {
        write_json(&self.root.join(".scion/state.json"), &self.state)
    }
    /// Absent unmaterialized entries are intentionally ignored. This invariant
    /// prevents a commit from deleting every file in a lazy checkout.
    pub fn status(&self) -> Result<Vec<StatusEntry>> {
        let mut changes = BTreeMap::new();
        for (path, tracked) in &self.state.files {
            // A virtual file that is merely absent is unchanged. A physical
            // directory at that file, or a regular-file ancestor, is an explicit
            // type replacement and needs deletion of the old virtual entries.
            let (current, replaced) = match locate_tracked(&self.root, path)? {
                TrackedLocation::Path(disk) => match fs::symlink_metadata(&disk) {
                    Ok(metadata) if metadata.is_dir() => (None, true),
                    Ok(_) => (disk_entry(&disk, Some(tracked.entry.kind))?, false),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, false),
                    Err(error) => return Err(error.into()),
                },
                TrackedLocation::Missing => (None, false),
                TrackedLocation::Replaced => (None, true),
            };
            match current {
                Some((entry, _)) if entry != tracked.entry => {
                    changes.insert(path.clone(), StatusKind::Modified);
                }
                None if tracked.materialized || replaced => {
                    changes.insert(path.clone(), StatusKind::Deleted);
                }
                _ => {}
            }
        }
        let mut walk = ignore::WalkBuilder::new(&self.root);
        walk.hidden(false)
            .require_git(false)
            .add_custom_ignore_filename(".scionignore")
            .filter_entry(|e| e.file_name() != ".scion" && e.file_name() != ".git");
        for entry in walk.build() {
            let entry = entry.map_err(|e| Error::Io(std::io::Error::other(e)))?;
            if entry.path() == self.root || entry.file_type().is_some_and(|t| t.is_dir()) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .map_err(|e| Error::Invalid(e.to_string()))?;
            let path = portable_path(relative)?;
            if !self.selected(&path) || self.state.files.contains_key(&path) {
                continue;
            }
            safe_path(&self.root, &path, false)?;
            changes.insert(path, StatusKind::Added);
        }
        Ok(changes
            .into_iter()
            .map(|(path, status)| StatusEntry { path, status })
            .collect())
    }
    fn selected(&self, path: &RepoPath) -> bool {
        self.state.sparse.is_empty() || self.state.sparse.iter().any(|p| p.contains(path))
    }
    pub async fn hydrate(&mut self, client: &Client, prefix: &RepoPath) -> Result<usize> {
        self.ensure_client(client)?;
        let requested: Vec<_> = self
            .state
            .files
            .iter()
            .filter(|(p, t)| prefix.contains(p) && !t.materialized)
            .map(|(p, t)| (p.clone(), t.entry.clone()))
            .collect();
        let revision = self.state.revision;
        let mut count = 0;
        // Batches bound memory, persist progress, and overlap independent network reads.
        for batch in requested.chunks(32) {
            let fetched: Vec<_> = stream::iter(batch.iter().cloned())
                .map(|(path, entry)| async move {
                    let bytes = client
                        .fetch_object(ObjectWitness {
                            commit: revision,
                            path: path.clone(),
                            kind: ObjectKind::Blob,
                            id: entry.id,
                        })
                        .await?;
                    Ok::<_, Error>((path, entry, bytes))
                })
                .buffer_unordered(8)
                .try_collect()
                .await?;
            for (path, entry, bytes) in fetched {
                let disk = safe_path(&self.root, &path, true)?;
                if let Some((actual, _)) = disk_entry(&disk, Some(entry.kind))? {
                    if actual != entry {
                        return Err(Error::Conflict(format!("hydration would overwrite {path}")));
                    }
                } else {
                    write_file(&disk, &entry, &bytes, &self.root)?;
                }
                if let Some(tracked) = self.state.files.get_mut(&path) {
                    tracked.materialized = true;
                }
                count += 1;
            }
            self.save()?;
        }
        Ok(count)
    }
    pub async fn checkout(
        &mut self,
        client: &Client,
        revision: ObjectId,
        force: bool,
    ) -> Result<()> {
        self.ensure_client(client)?;
        let status = self.status()?;
        if !force && status.iter().any(|s| s.status != StatusKind::Added) {
            return Err(Error::Conflict(
                "working copy has local modifications; commit them or use --force".into(),
            ));
        }
        let target = client.manifest(revision, &self.state.sparse).await?;
        validate_platform_paths(target.keys(), cfg!(any(windows, target_os = "macos")))?;
        for path in target.keys() {
            self.preflight_target(path, &target)?;
        }
        // Fetch all required contents before mutating disk, in bounded concurrent batches.
        // Staging gives failed network transfers an entirely untouched working copy.
        let staging = tempfile::tempdir_in(self.root.join(".scion"))?;
        let requested: Vec<_> = target
            .iter()
            .filter(|(p, e)| {
                self.state
                    .files
                    .get(*p)
                    .is_none_or(|t| t.entry != **e || !t.materialized)
                    || force
            })
            .map(|(p, e)| (p.clone(), e.clone()))
            .collect();
        for batch in requested.chunks(32) {
            let fetched: Vec<_> = stream::iter(batch.iter().cloned())
                .map(|(path, entry)| async move {
                    let bytes = client
                        .fetch_object(ObjectWitness {
                            commit: revision,
                            path: path.clone(),
                            kind: ObjectKind::Blob,
                            id: entry.id,
                        })
                        .await?;
                    Ok::<_, Error>((path, entry, bytes))
                })
                .buffer_unordered(8)
                .try_collect()
                .await?;
            for (path, entry, bytes) in fetched {
                let disk = safe_path(staging.path(), &path, true)?;
                write_file(&disk, &entry, &bytes, staging.path())?;
            }
        }
        if !force
            && self
                .status()?
                .iter()
                .any(|change| change.status != StatusKind::Added)
        {
            return Err(Error::Conflict(
                "working copy changed while the checkout was downloading; no files were replaced"
                    .into(),
            ));
        }
        for path in target.keys() {
            self.preflight_target(path, &target)?;
        }
        let requested_paths: BTreeSet<_> = requested.iter().map(|(path, _)| path).collect();
        let mut removals: Vec<_> = self
            .state
            .files
            .keys()
            .filter(|p| !target.contains_key(*p) || requested_paths.contains(*p))
            .cloned()
            .collect();
        removals.sort_by_key(|p| std::cmp::Reverse(p.as_str().len()));
        let previous = self.state.clone();
        let mut journal = CheckoutJournal {
            previous: previous.clone(),
            written: requested.iter().cloned().collect(),
            removals,
            backed_up: BTreeSet::new(),
            phase: CheckoutPhase::Backup,
        };
        let backup = self.root.join(".scion/checkout-backup");
        if backup.exists() {
            fs::remove_dir_all(&backup)?;
        }
        fs::create_dir(&backup)?;
        write_json(&self.root.join(".scion/checkout.json"), &journal)?;
        let result = (|| -> Result<()> {
            // Durable backup precedes destructive mutation. Recovery can replay
            // this phase after interruption without losing local tracked edits.
            for path in &journal.removals {
                let Some(disk) = tracked_path(&self.root, path)? else {
                    continue;
                };
                match fs::symlink_metadata(&disk) {
                    Ok(metadata) if metadata.is_dir() => {
                        fs::remove_dir(&disk)?;
                    }
                    Ok(_) => {
                        let saved = safe_path(&backup, path, true)?;
                        fs::rename(&disk, &saved)?;
                        sync_directory(saved.parent().unwrap_or(&backup))?;
                        sync_directory(disk.parent().unwrap_or(&self.root))?;
                        journal.backed_up.insert(path.clone());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                remove_empty_parents(disk.parent(), &self.root);
            }
            journal.phase = CheckoutPhase::Apply;
            write_json(&self.root.join(".scion/checkout.json"), &journal)?;
            for (path, entry) in &requested {
                let staged = safe_path(staging.path(), path, false)?;
                let disk = safe_path(&self.root, path, true)?;
                let (_, bytes) = disk_entry(&staged, Some(entry.kind))?
                    .ok_or_else(|| Error::Corrupt("staged checkout file disappeared".into()))?;
                write_file(&disk, entry, &bytes, &self.root)?;
            }
            self.state.revision = revision;
            self.state.files = target
                .into_iter()
                .map(|(p, e)| {
                    (
                        p,
                        TrackedFile {
                            entry: e,
                            materialized: true,
                        },
                    )
                })
                .collect();
            self.save()?;
            fs::remove_file(self.root.join(".scion/checkout.json"))?;
            sync_directory(&self.root.join(".scion"))?;
            Ok(())
        })();
        if let Err(error) = result {
            self.state = previous;
            recover_checkout(&self.root)?;
            return Err(error);
        }
        let _ = fs::remove_dir_all(backup);
        Ok(())
    }

    fn preflight_target(
        &self,
        path: &RepoPath,
        target: &BTreeMap<RepoPath, FileEntry>,
    ) -> Result<()> {
        let parts: Vec<_> = path.components().collect();
        let mut prefix = RepoPath::root();
        let mut disk = self.root.clone();
        for (index, part) in parts.iter().enumerate() {
            prefix = prefix.join(part)?;
            disk.push(part);
            let metadata = match fs::symlink_metadata(&disk) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            if index + 1 < parts.len() {
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    if self.state.files.contains_key(&prefix) && !target.contains_key(&prefix) {
                        return Ok(());
                    }
                    return Err(Error::Conflict(format!(
                        "checkout would cross untracked ancestor {prefix}"
                    )));
                }
            } else if metadata.is_dir() {
                let mut pending = vec![disk.clone()];
                while let Some(directory) = pending.pop() {
                    for child in fs::read_dir(directory)? {
                        let child = child?;
                        if child.file_type()?.is_dir() {
                            pending.push(child.path());
                        } else {
                            let relative =
                                portable_path(child.path().strip_prefix(&self.root).map_err(
                                    |_| Error::Invalid("checkout path escaped root".into()),
                                )?)?;
                            if !self.state.files.contains_key(&relative) {
                                return Err(Error::Conflict(format!(
                                    "checkout would overwrite untracked {relative}"
                                )));
                            }
                        }
                    }
                }
            } else if !self.state.files.contains_key(path) {
                return Err(Error::Conflict(format!(
                    "checkout would overwrite untracked {path}"
                )));
            }
        }
        Ok(())
    }
    pub async fn commit(
        &mut self,
        client: &Client,
        author: String,
        message: String,
        update_ref: Option<String>,
    ) -> Result<ObjectId> {
        self.ensure_client(client)?;
        let status = self.status()?;
        if status.is_empty() {
            return Err(Error::Invalid("working copy has no changes".into()));
        }
        let mut changes = BTreeMap::new();
        for change in &status {
            if change.status == StatusKind::Deleted {
                changes.insert(change.path.clone(), FileChange::Delete);
                continue;
            }
            let disk = safe_path(&self.root, &change.path, false)?;
            let kind = self.state.files.get(&change.path).map(|t| t.entry.kind);
            let (mut entry, bytes) = disk_entry(&disk, kind)?.ok_or_else(|| {
                Error::Conflict(format!("file disappeared during commit: {}", change.path))
            })?;
            entry.id = client.put_blob(&bytes).await?;
            changes.insert(change.path.clone(), FileChange::put(entry));
        }
        let parent = self.state.revision;
        let commit = Changeset {
            parents: vec![parent],
            author,
            committer: None,
            timestamp_ms: now_ms(),
            message,
            changes: changes.clone(),
            metadata: BTreeMap::new(),
        };
        let id = client.create_commit(commit).await?;
        for (path, change) in changes {
            match change {
                FileChange::Put { entry, .. } => {
                    self.state.files.insert(
                        path,
                        TrackedFile {
                            entry,
                            materialized: true,
                        },
                    );
                }
                FileChange::Delete => {
                    self.state.files.remove(&path);
                }
            }
        }
        self.state.revision = id;
        self.save()?;
        if let Some(name) = update_ref {
            let changed: bool = client
                .rpc(RpcRequest::SetRef {
                    name,
                    expected: Some(parent),
                    target: Some(id),
                })
                .await?;
            if !changed {
                return Err(Error::Conflict(format!(
                    "commit {id} is durable and checked out, but the reference changed concurrently"
                )));
            }
        }
        Ok(id)
    }
    fn ensure_client(&self, client: &Client) -> Result<()> {
        if client.repo() != self.state.repo || client.server() != self.state.server {
            return Err(Error::Invalid(
                "client does not match this working copy's repository and authority".into(),
            ));
        }
        Ok(())
    }
    /// Return a watcher and an event receiver. Keep the watcher alive while receiving.
    pub fn watch(
        &self,
    ) -> Result<(
        notify::RecommendedWatcher,
        std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    )> {
        use notify::Watcher;
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })
        .map_err(|e| Error::Storage(e.to_string()))?;
        watcher
            .watch(&self.root, notify::RecursiveMode::Recursive)
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok((watcher, rx))
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum CheckoutPhase {
    Backup,
    Apply,
    Restore,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckoutJournal {
    previous: WorkingCopyState,
    written: BTreeMap<RepoPath, FileEntry>,
    removals: Vec<RepoPath>,
    #[serde(default)]
    backed_up: BTreeSet<RepoPath>,
    phase: CheckoutPhase,
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| Error::Invalid("metadata file lacks parent".into()))?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| Error::Io(e.error))?;
    sync_directory(directory)
}
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}
/// Roll back an interrupted checkout while holding the working-copy lock.
/// Externally modified target files are never deleted during recovery.
fn recover_checkout(root: &Path) -> Result<()> {
    let marker = root.join(".scion/checkout.json");
    let bytes = match fs::read(&marker) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let mut journal: CheckoutJournal = serde_json::from_slice(&bytes)?;
    let backup = root.join(".scion/checkout-backup");
    if matches!(journal.phase, CheckoutPhase::Apply | CheckoutPhase::Restore) {
        let mut written: Vec<_> = journal
            .written
            .iter()
            .map(|(p, e)| (p.clone(), e.clone()))
            .collect();
        written.sort_by_key(|(p, _)| std::cmp::Reverse(p.as_str().len()));
        // Validate before entering rollback. A restart during rollback recognizes
        // already restored originals by the atomically consumed backup entry.
        for (path, expected) in &written {
            let restored = matches!(journal.phase, CheckoutPhase::Restore)
                && journal.backed_up.contains(path)
                && fs::symlink_metadata(safe_path(&backup, path, false)?).is_err();
            if restored {
                continue;
            }
            if let Some(disk) = tracked_path(root, path)? {
                if fs::symlink_metadata(&disk).is_ok_and(|m| m.is_dir()) {
                    if matches!(journal.phase, CheckoutPhase::Restore) {
                        continue;
                    }
                    return Err(Error::Conflict(format!(
                        "checkout recovery found an unexpected directory at {path}; preserve it before retrying"
                    )));
                }
                if let Some((actual, _)) = disk_entry(&disk, Some(expected.kind))?
                    && actual != *expected
                {
                    return Err(Error::Conflict(format!(
                        "checkout recovery preserved externally modified {path}; move it aside and retry"
                    )));
                }
            }
        }
        journal.phase = CheckoutPhase::Restore;
        write_json(&marker, &journal)?;
        for (path, _) in written {
            if journal.backed_up.contains(&path)
                && fs::symlink_metadata(safe_path(&backup, &path, false)?).is_err()
            {
                continue;
            }
            if let Some(disk) = tracked_path(root, &path)? {
                if fs::symlink_metadata(&disk).is_ok_and(|m| m.is_dir()) {
                    continue;
                }
                match fs::remove_file(&disk) {
                    Ok(()) => remove_empty_parents(disk.parent(), root),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
    for path in &journal.removals {
        let saved = safe_path(&backup, path, false)?;
        if fs::symlink_metadata(&saved).is_err() {
            continue;
        }
        let disk = safe_path(root, path, true)?;
        match fs::symlink_metadata(&disk) {
            Ok(m) if m.is_dir() => fs::remove_dir(&disk)?,
            Ok(_) => {
                return Err(Error::Conflict(format!(
                    "checkout recovery will not overwrite {path}; move it aside and retry"
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        fs::rename(&saved, &disk)?;
        sync_directory(disk.parent().unwrap_or(root))?;
        sync_directory(saved.parent().unwrap_or(&backup))?;
    }
    write_json(&root.join(".scion/state.json"), &journal.previous)?;
    fs::remove_file(marker)?;
    sync_directory(&root.join(".scion"))?;
    let _ = fs::remove_dir_all(backup);
    Ok(())
}
fn validate_platform_paths<'a>(
    paths: impl Iterator<Item = &'a RepoPath>,
    case_insensitive: bool,
) -> Result<()> {
    if !case_insensitive {
        return Ok(());
    }
    use unicode_normalization::UnicodeNormalization;
    let mut spelling = BTreeMap::new();
    for path in paths {
        let mut prefix = RepoPath::root();
        for component in path.components() {
            prefix = prefix.join(component)?;
            let normalized: String = prefix.as_str().nfd().flat_map(char::to_lowercase).collect();
            if let Some(previous) = spelling.insert(normalized, prefix.clone())
                && previous != prefix
            {
                return Err(Error::Conflict(format!(
                    "checkout paths {previous} and {prefix} collide on this filesystem"
                )));
            }
        }
    }
    Ok(())
}
fn lock(metadata: &Path) -> Result<File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(metadata.join("lock"))?;
    file.try_lock_exclusive()
        .map_err(|_| Error::Conflict("working copy is already locked".into()))?;
    Ok(file)
}
fn find_root(path: &Path) -> Result<PathBuf> {
    let mut p = fs::canonicalize(path)?;
    if p.is_file() {
        p.pop();
    }
    loop {
        if p.join(".scion/state.json").is_file() {
            return Ok(p);
        }
        if !p.pop() {
            return Err(Error::NotFound("not inside a Scion working copy".into()));
        }
    }
}
fn portable_path(path: &Path) -> Result<RepoPath> {
    let mut parts = Vec::new();
    for c in path.components() {
        if let Component::Normal(p) = c {
            parts.push(
                p.to_str()
                    .ok_or_else(|| Error::Invalid("non-UTF-8 paths are unsupported".into()))?,
            );
        } else {
            return Err(Error::Invalid("invalid working-copy path".into()));
        }
    }
    RepoPath::new(parts.join("/"))
}
/// Refuse symlink ancestors; a tracked final symlink is treated as content.
fn safe_path(root: &Path, path: &RepoPath, create_parents: bool) -> Result<PathBuf> {
    let parts: Vec<_> = path.components().collect();
    let mut result = root.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        result.push(part);
        if i + 1 < parts.len() {
            match fs::symlink_metadata(&result) {
                Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
                    return Err(Error::Conflict(format!(
                        "unsafe non-directory ancestor of {path}"
                    )));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if create_parents {
                        fs::create_dir(&result)?;
                        sync_directory(result.parent().unwrap_or(root))?;
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(result)
}
// A regular-file ancestor represents a legitimate directory-to-file edit;
// a symlink ancestor must never be followed while examining tracked descendants.
enum TrackedLocation {
    Path(PathBuf),
    Missing,
    Replaced,
}
fn tracked_path(root: &Path, path: &RepoPath) -> Result<Option<PathBuf>> {
    Ok(match locate_tracked(root, path)? {
        TrackedLocation::Path(path) => Some(path),
        TrackedLocation::Missing | TrackedLocation::Replaced => None,
    })
}
fn locate_tracked(root: &Path, path: &RepoPath) -> Result<TrackedLocation> {
    let parts: Vec<_> = path.components().collect();
    let mut result = root.to_path_buf();
    for (index, part) in parts.iter().enumerate() {
        result.push(part);
        if index + 1 < parts.len() {
            match fs::symlink_metadata(&result) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(Error::Conflict(format!(
                        "unsafe symlink ancestor of {path}"
                    )));
                }
                Ok(m) if !m.is_dir() => return Ok(TrackedLocation::Replaced),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(TrackedLocation::Missing);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(TrackedLocation::Path(result))
}
fn disk_entry(path: &Path, expected: Option<FileKind>) -> Result<Option<(FileEntry, Vec<u8>)>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let (kind, bytes) = if metadata.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        (
            FileKind::Symlink,
            target
                .to_str()
                .ok_or_else(|| Error::Invalid("non-UTF-8 symlink".into()))?
                .as_bytes()
                .to_vec(),
        )
    } else if metadata.is_file() {
        if metadata.len() > scion_store::MAX_OBJECT_SIZE as u64 {
            return Err(Error::Limit(
                "working-copy file exceeds object size limit".into(),
            ));
        }
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = expected == Some(FileKind::Executable);
        let kind = if expected == Some(FileKind::Submodule) {
            FileKind::Submodule
        } else if executable {
            FileKind::Executable
        } else {
            FileKind::Regular
        };
        let file = File::open(path)?;
        let mut bytes = Vec::new();
        file.take(scion_store::MAX_OBJECT_SIZE as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > scion_store::MAX_OBJECT_SIZE {
            return Err(Error::Limit(
                "working-copy file grew beyond the object limit".into(),
            ));
        }
        (kind, bytes)
    } else {
        return Err(Error::Conflict(format!(
            "expected a file at {}",
            path.display()
        )));
    };
    let entry = FileEntry {
        id: ObjectId::hash(ObjectKind::Blob, &bytes),
        size: bytes.len() as u64,
        kind,
    };
    Ok(Some((entry, bytes)))
}
fn write_file(path: &Path, entry: &FileEntry, bytes: &[u8], root: &Path) -> Result<()> {
    if ObjectId::hash(ObjectKind::Blob, bytes) != entry.id || bytes.len() as u64 != entry.size {
        return Err(Error::Corrupt(
            "working-copy object does not match entry".into(),
        ));
    }
    if entry.kind == FileKind::Symlink {
        let target = std::str::from_utf8(bytes)
            .map_err(|_| Error::Invalid("symlink target must be UTF-8".into()))?;
        let target_path = Path::new(target);
        if target_path.is_absolute() {
            return Err(Error::Invalid(
                "absolute symlinks cannot be materialized".into(),
            ));
        }
        let base = path.parent().unwrap_or(root);
        let mut depth = base
            .strip_prefix(root)
            .map_err(|_| Error::Invalid("symlink outside working copy".into()))?
            .components()
            .count();
        for component in target_path.components() {
            match component {
                Component::ParentDir => {
                    if depth == 0 {
                        return Err(Error::Invalid("symlink escapes working copy".into()));
                    }
                    depth -= 1;
                }
                Component::Normal(_) => depth += 1,
                Component::CurDir => {}
                _ => return Err(Error::Invalid("unsafe symlink target".into())),
            }
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, path)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(target, path)?;
        #[cfg(not(any(unix, windows)))]
        return Err(Error::Invalid(
            "symlinks unsupported on this platform".into(),
        ));
    } else {
        let parent = path
            .parent()
            .ok_or_else(|| Error::Invalid("file lacks parent".into()))?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.write_all(bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file().set_permissions(fs::Permissions::from_mode(
                if entry.kind == FileKind::Executable {
                    0o755
                } else {
                    0o644
                },
            ))?;
        }
        tmp.as_file().sync_all()?;
        tmp.persist_noclobber(path).map_err(|e| {
            if e.error.kind() == std::io::ErrorKind::AlreadyExists {
                Error::Conflict(format!(
                    "materialization will not overwrite {}",
                    path.display()
                ))
            } else {
                Error::Io(e.error)
            }
        })?;
    }
    sync_directory(path.parent().unwrap_or(root))?;
    Ok(())
}
fn remove_empty_parents(mut current: Option<&Path>, root: &Path) {
    while let Some(path) = current {
        if path == root {
            break;
        }
        if fs::remove_dir(path).is_err() {
            break;
        }
        current = path.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(lazy: bool) -> (tempfile::TempDir, WorkingCopy) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        fs::create_dir(root.join(".scion")).unwrap();
        let id = ObjectId::hash(ObjectKind::Blob, b"hello");
        let files = BTreeMap::from([(
            RepoPath::new("src/main.rs").unwrap(),
            TrackedFile {
                entry: FileEntry {
                    id,
                    size: 5,
                    kind: FileKind::Regular,
                },
                materialized: !lazy,
            },
        )]);
        let state = WorkingCopyState {
            version: 1,
            server: "http://localhost:7420".into(),
            objects: "http://localhost:7420".into(),
            repo: "test".into(),
            revision: id,
            sparse: vec![],
            files,
        };
        let lock = lock(&root.join(".scion")).unwrap();
        (
            temp,
            WorkingCopy {
                root,
                state,
                _lock: lock,
            },
        )
    }
    #[test]
    fn lazy_absence_is_not_a_deletion() {
        let (_temp, wc) = fixture(true);
        assert!(wc.status().unwrap().is_empty());
    }
    #[test]
    fn materialized_absence_is_a_deletion() {
        let (_temp, wc) = fixture(false);
        assert_eq!(wc.status().unwrap()[0].status, StatusKind::Deleted);
    }
    #[test]
    fn state_roundtrip_and_exclusive_lock() {
        let (temp, wc) = fixture(true);
        wc.save().unwrap();
        assert!(WorkingCopy::open(temp.path()).is_err());
        drop(wc);
        assert!(WorkingCopy::open(temp.path()).is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn symlink_ancestor_cannot_escape() {
        let (temp, wc) = fixture(true);
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), temp.path().join("src")).unwrap();
        assert!(wc.status().is_err());
        assert!(safe_path(temp.path(), &RepoPath::new("src/leak").unwrap(), true).is_err());
    }
    #[test]
    fn path_prefix_selection_uses_components() {
        let (_temp, mut wc) = fixture(true);
        wc.state.sparse = vec![RepoPath::new("src").unwrap()];
        assert!(!wc.selected(&RepoPath::new("src-other/x").unwrap()));
    }
    #[test]
    fn directory_to_file_change_tracks_deletions_and_addition() {
        let (temp, wc) = fixture(false);
        fs::write(temp.path().join("src"), b"replacement").unwrap();
        let changes = wc.status().unwrap();
        assert!(
            changes
                .iter()
                .any(|c| c.path.as_str() == "src/main.rs" && c.status == StatusKind::Deleted)
        );
        assert!(
            changes
                .iter()
                .any(|c| c.path.as_str() == "src" && c.status == StatusKind::Added)
        );
    }
    #[test]
    fn file_to_directory_change_tracks_deletions_and_addition() {
        let (temp, mut wc) = fixture(false);
        let entry = wc
            .state
            .files
            .remove(&RepoPath::new("src/main.rs").unwrap())
            .unwrap();
        wc.state.files.insert(RepoPath::new("src").unwrap(), entry);
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/new"), b"new").unwrap();
        let changes = wc.status().unwrap();
        assert!(
            changes
                .iter()
                .any(|c| c.path.as_str() == "src" && c.status == StatusKind::Deleted)
        );
        assert!(
            changes
                .iter()
                .any(|c| c.path.as_str() == "src/new" && c.status == StatusKind::Added)
        );
    }

    #[test]
    fn interrupted_checkout_restores_original_bytes_and_metadata() {
        let (temp, wc) = fixture(false);
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/main.rs"), b"hello").unwrap();
        wc.save().unwrap();
        let path = RepoPath::new("src/main.rs").unwrap();
        let next = FileEntry {
            id: ObjectId::hash(ObjectKind::Blob, b"world"),
            size: 5,
            kind: FileKind::Regular,
        };
        let backup = temp.path().join(".scion/checkout-backup");
        fs::create_dir_all(backup.join("src")).unwrap();
        fs::rename(temp.path().join("src/main.rs"), backup.join("src/main.rs")).unwrap();
        let journal = CheckoutJournal {
            previous: wc.state.clone(),
            written: BTreeMap::from([(path.clone(), next)]),
            removals: vec![path.clone()],
            backed_up: BTreeSet::from([path]),
            phase: CheckoutPhase::Apply,
        };
        write_json(&temp.path().join(".scion/checkout.json"), &journal).unwrap();
        fs::write(temp.path().join("src/main.rs"), b"world").unwrap();
        drop(wc);
        let recovered = WorkingCopy::open(temp.path()).unwrap();
        assert!(recovered.status().unwrap().is_empty());
        assert_eq!(fs::read(temp.path().join("src/main.rs")).unwrap(), b"hello");
        assert!(!temp.path().join(".scion/checkout.json").exists());
    }
    #[test]
    fn recovery_preserves_edits_made_after_interruption() {
        let (temp, wc) = fixture(false);
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/main.rs"), b"user edit").unwrap();
        wc.save().unwrap();
        let path = RepoPath::new("src/main.rs").unwrap();
        let next = FileEntry {
            id: ObjectId::hash(ObjectKind::Blob, b"world"),
            size: 5,
            kind: FileKind::Regular,
        };
        let journal = CheckoutJournal {
            previous: wc.state.clone(),
            written: BTreeMap::from([(path.clone(), next)]),
            removals: vec![path.clone()],
            backed_up: BTreeSet::from([path]),
            phase: CheckoutPhase::Apply,
        };
        write_json(&temp.path().join(".scion/checkout.json"), &journal).unwrap();
        drop(wc);
        assert!(matches!(
            WorkingCopy::open(temp.path()),
            Err(Error::Conflict(_))
        ));
        assert_eq!(
            fs::read(temp.path().join("src/main.rs")).unwrap(),
            b"user edit"
        );
    }

    #[test]
    fn platform_collision_check_covers_directories_and_unicode() {
        let paths = [
            RepoPath::new("Foo/a").unwrap(),
            RepoPath::new("foo/b").unwrap(),
        ];
        assert!(validate_platform_paths(paths.iter(), true).is_err());
        assert!(validate_platform_paths(paths.iter(), false).is_ok());
        let paths = [
            RepoPath::new("café").unwrap(),
            RepoPath::new("cafe\u{301}").unwrap(),
        ];
        assert!(validate_platform_paths(paths.iter(), true).is_err());
    }

    #[test]
    fn interrupted_rollback_is_idempotent_after_original_restore() {
        let (temp, wc) = fixture(false);
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/main.rs"), b"hello").unwrap();
        wc.save().unwrap();
        let path = RepoPath::new("src/main.rs").unwrap();
        let next = FileEntry {
            id: ObjectId::hash(ObjectKind::Blob, b"world"),
            size: 5,
            kind: FileKind::Regular,
        };
        let journal = CheckoutJournal {
            previous: wc.state.clone(),
            written: BTreeMap::from([(path.clone(), next)]),
            removals: vec![path.clone()],
            backed_up: BTreeSet::from([path]),
            phase: CheckoutPhase::Restore,
        };
        fs::create_dir(temp.path().join(".scion/checkout-backup")).unwrap();
        write_json(&temp.path().join(".scion/checkout.json"), &journal).unwrap();
        drop(wc);
        let recovered = WorkingCopy::open(temp.path()).unwrap();
        assert!(recovered.status().unwrap().is_empty());
        assert_eq!(fs::read(temp.path().join("src/main.rs")).unwrap(), b"hello");
    }
    #[test]
    fn materialization_atomically_refuses_existing_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        fs::write(&path, b"local work").unwrap();
        let entry = FileEntry {
            id: ObjectId::hash(ObjectKind::Blob, b"server"),
            size: 6,
            kind: FileKind::Regular,
        };
        assert!(matches!(
            write_file(&path, &entry, b"server", directory.path()),
            Err(Error::Conflict(_))
        ));
        assert_eq!(fs::read(path).unwrap(), b"local work");
    }
    #[test]
    fn virtual_file_replaced_by_directory_records_delete_and_new_child() {
        let (temp, mut wc) = fixture(true);
        let entry = wc
            .state
            .files
            .remove(&RepoPath::new("src/main.rs").unwrap())
            .unwrap();
        wc.state
            .files
            .insert(RepoPath::new("src").unwrap(), entry.clone());
        wc.state
            .files
            .insert(RepoPath::new("untouched.txt").unwrap(), entry);
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/new"), b"new").unwrap();
        let changes = wc.status().unwrap();
        assert_eq!(
            changes,
            vec![
                StatusEntry {
                    path: RepoPath::new("src").unwrap(),
                    status: StatusKind::Deleted
                },
                StatusEntry {
                    path: RepoPath::new("src/new").unwrap(),
                    status: StatusKind::Added
                },
            ]
        );
    }
    #[test]
    fn virtual_descendants_replaced_by_ancestor_file_record_each_deletion() {
        let (temp, mut wc) = fixture(true);
        let entry = wc.state.files[&RepoPath::new("src/main.rs").unwrap()].clone();
        wc.state
            .files
            .insert(RepoPath::new("src/nested/other.rs").unwrap(), entry.clone());
        wc.state
            .files
            .insert(RepoPath::new("untouched.txt").unwrap(), entry);
        fs::write(temp.path().join("src"), b"replacement").unwrap();
        let changes = wc.status().unwrap();
        assert_eq!(
            changes,
            vec![
                StatusEntry {
                    path: RepoPath::new("src").unwrap(),
                    status: StatusKind::Added
                },
                StatusEntry {
                    path: RepoPath::new("src/main.rs").unwrap(),
                    status: StatusKind::Deleted
                },
                StatusEntry {
                    path: RepoPath::new("src/nested/other.rs").unwrap(),
                    status: StatusKind::Deleted
                },
            ]
        );
    }
}
