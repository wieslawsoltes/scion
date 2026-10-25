//! Explicit graph exports from a consistent local publication-log snapshot.
use crate::Repository;
use scion_graph::{GraphSnapshot, GraphSnapshotMetadata, MAX_SNAPSHOT_NODES, SnapshotSource};
use scion_store::{LocalStore, Storage};
use scion_types::{Error, LogEntry, ObjectId, Result};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

const LOG_PAGE_SIZE: usize = 4096;

/// Bounds on the publication log collected before exporting a frozen graph.
/// The graph writer independently limits record sizes, parent counts and its
/// cache. Every changeset retains the storage backend's object-size limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphSnapshotExportLimits {
    /// Maximum commits in the frozen repository. Zero permits an empty graph;
    /// values above [`MAX_SNAPSHOT_NODES`] are rejected before opening storage.
    pub max_commits: u64,
}

impl Default for GraphSnapshotExportLimits {
    fn default() -> Self {
        Self {
            max_commits: MAX_SNAPSHOT_NODES,
        }
    }
}

/// Export all publications visible in one read-only local SQLite snapshot.
///
/// The first publication-log query fixes the database view, including committed
/// WAL data. Later publications remain outside this artifact. Each included ID
/// must still have repository membership and a fully hash-verified, valid
/// changeset. The graph writer derives indexes from those changesets' parents,
/// requiring each parent to have been included earlier; existing derived graph
/// records are never trusted or copied.
///
/// The source identity hashes a versioned domain, the repository name, and the
/// ordered publication sequences and object IDs. Sequence gaps are valid because
/// the local log allocates them across repositories. The metadata also records
/// the highest included sequence, or zero for an empty repository.
///
/// A bounded vector retains the publication log (40 bytes per entry, excluding
/// allocation overhead). Changeset contents are verified one at a time. Export
/// does not add transparent caches or change live [`Repository`] queries.
///
/// The destination's parent must exist and resolve outside the source store.
/// Symlink destinations and aliases of source files are rejected. The graph
/// writer publishes by atomic replacement: failures before replacement leave an
/// existing artifact intact; a later directory-sync error may expose a complete
/// new artifact. Keep source/output directory paths stable during this operation.
pub fn export_local_graph_snapshot(
    data_path: impl AsRef<Path>,
    repository_name: &str,
    destination: impl AsRef<Path>,
    limits: GraphSnapshotExportLimits,
) -> Result<GraphSnapshotMetadata> {
    if limits.max_commits > MAX_SNAPSHOT_NODES {
        return Err(Error::Invalid(format!(
            "graph export limit exceeds {MAX_SNAPSHOT_NODES} commits"
        )));
    }
    let source = fs::canonicalize(data_path)?;
    let destination = protected_destination(&source, destination.as_ref())?;
    let store = Arc::new(LocalStore::open_read_snapshot(&source)?);
    let repository = Repository::new(repository_name, store.clone())?;
    let entries = collect_log(
        |after, limit| store.read_log(repository.name(), after, limit),
        limits.max_commits,
    )?;
    let identity = source_identity(repository.name(), &entries);
    let high_water = entries.last().map_or(0, |entry| entry.sequence);
    let declared_count = entries.len() as u64;
    GraphSnapshot::write(
        destination,
        repository.name(),
        SnapshotSource {
            identity,
            sequence: Some(high_water),
        },
        declared_count,
        entries.into_iter().map(|entry| {
            let changeset = repository.get_changeset(entry.id)?;
            Ok((entry.id, changeset.parents))
        }),
    )
}

fn collect_log(
    mut read: impl FnMut(u64, usize) -> Result<Vec<LogEntry>>,
    limit: u64,
) -> Result<Vec<LogEntry>> {
    let mut entries = Vec::new();
    let mut after = 0;
    loop {
        let page = read(after, LOG_PAGE_SIZE)?;
        if page.is_empty() {
            return Ok(entries);
        }
        if page.len() > LOG_PAGE_SIZE {
            return Err(Error::Corrupt(
                "publication-log page exceeds its limit".into(),
            ));
        }
        if entries.len() as u64 + page.len() as u64 > limit {
            return Err(Error::Limit(format!(
                "graph export exceeds {limit} commits"
            )));
        }
        entries
            .try_reserve(page.len())
            .map_err(|_| Error::Limit("unable to allocate the bounded publication log".into()))?;
        for entry in page {
            if entry.sequence <= after {
                return Err(Error::Corrupt(
                    "publication-log sequences must be positive and strictly increasing".into(),
                ));
            }
            after = entry.sequence;
            entries.push(entry);
        }
    }
}

fn source_identity(repository: &str, entries: &[LogEntry]) -> ObjectId {
    let mut hasher = blake3::Hasher::new_derive_key("scion graph snapshot publication log v1");
    hasher.update(&(repository.len() as u64).to_le_bytes());
    hasher.update(repository.as_bytes());
    hasher.update(&(entries.len() as u64).to_le_bytes());
    for entry in entries {
        hasher.update(&entry.sequence.to_le_bytes());
        hasher.update(&entry.id.0);
    }
    ObjectId(*hasher.finalize().as_bytes())
}

fn protected_destination(source: &Path, destination: &Path) -> Result<PathBuf> {
    let name = destination
        .file_name()
        .ok_or_else(|| Error::Invalid("graph snapshot destination needs a file name".into()))?;
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // Check the supplied route as well as its final canonical location: a
    // symlink below objects/ must not turn an internal output into an external
    // pathname that appears safe after resolution.
    for ancestor in parent
        .ancestors()
        .filter(|path| !path.as_os_str().is_empty())
    {
        if fs::canonicalize(ancestor)?.starts_with(source) {
            return Err(Error::Invalid(
                "graph snapshot output path must not traverse the source store".into(),
            ));
        }
    }
    let parent = fs::canonicalize(parent)?;
    if parent.starts_with(source) {
        return Err(Error::Invalid(
            "graph snapshot output must be outside the source store".into(),
        ));
    }
    let destination = parent.join(name);
    let metadata = match fs::symlink_metadata(&destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(destination),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::Invalid(
            "graph snapshot output must be a regular file, not a symlink".into(),
        ));
    }
    // Check portable file identities, including source paths that resolve
    // through symlinks. A link-count shortcut would miss those aliases. This
    // scan reads directory entries/metadata, never the source files' contents.
    let output = same_file::Handle::from_path(&destination)?;
    let mut pending = vec![source.to_path_buf()];
    let mut visited = BTreeSet::new();
    while let Some(directory) = pending.pop() {
        let directory = fs::canonicalize(directory)?;
        if !visited.insert(directory.clone()) {
            continue;
        }
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                // WAL coordination files can disappear when another handle
                // closes. A removed pathname cannot alias the output anymore.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                match same_file::Handle::from_path(path) {
                    Ok(handle) if handle == output => {
                        return Err(Error::Invalid(
                            "graph snapshot output aliases a source-store file".into(),
                        ));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_types::ObjectKind;

    fn entry(sequence: u64) -> LogEntry {
        LogEntry {
            sequence,
            id: ObjectId::hash(ObjectKind::Changeset, &sequence.to_le_bytes()),
        }
    }

    #[test]
    fn publication_pages_are_bounded_and_gaps_are_preserved() {
        let expected: Vec<_> = (1..=LOG_PAGE_SIZE as u64 + 2)
            .map(|sequence| entry(sequence * 3))
            .collect();
        let mut calls = 0;
        let collected = collect_log(
            |after, limit| {
                calls += 1;
                assert!(limit <= 10_000);
                Ok(expected
                    .iter()
                    .filter(|entry| entry.sequence > after)
                    .take(limit)
                    .cloned()
                    .collect())
            },
            MAX_SNAPSHOT_NODES,
        )
        .unwrap();
        assert_eq!(collected, expected);
        assert_eq!(calls, 3);
        for sequences in [vec![0], vec![1, 1], vec![3, 2]] {
            let mut page = sequences.into_iter().map(entry).collect();
            assert!(matches!(
                collect_log(|_, _| Ok(std::mem::take(&mut page)), 10),
                Err(Error::Corrupt(_))
            ));
        }
        assert!(matches!(
            collect_log(|_, _| Ok(vec![entry(1)]), 0),
            Err(Error::Limit(_))
        ));
        assert!(collect_log(|_, _| Ok(Vec::new()), 0).unwrap().is_empty());
    }

    #[test]
    fn identity_binds_repository_order_sequences_and_ids() {
        let original = vec![entry(1), entry(3)];
        let identity = source_identity("alpha", &original);
        assert_eq!(identity, source_identity("alpha", &original));
        assert_ne!(identity, source_identity("beta", &original));
        assert_ne!(identity, source_identity("alpha", &original[..1]));
        assert_ne!(identity, source_identity("alpha", &[entry(3), entry(1)]));
        let mut changed = original.clone();
        changed[1].sequence = 4;
        assert_ne!(identity, source_identity("alpha", &changed));
        changed = original;
        changed[1].id = entry(2).id;
        assert_ne!(identity, source_identity("alpha", &changed));
    }
}
