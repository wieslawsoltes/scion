//! Explicit immutable, repository-scoped graph snapshots. No live-store lookup.
use crate::{GraphAccess, GraphNode, build_node};
use scion_types::{Error, ObjectId, Result, validate_name};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io,
    path::Path,
    sync::Mutex,
};

const MAGIC: &[u8; 8] = b"SCNGRPH1";
const VERSION: u32 = 1;
const HEADER_BYTES: usize = 512;
const HEADER_HASH: usize = HEADER_BYTES - 32;
const SLOT_BYTES: usize = 80;
const SLOT_HASH: usize = SLOT_BYTES - 32;
const RECORD_HEADER: usize = 56;
const MIN_RECORD: usize = RECORD_HEADER + 32;
const MAX_RECORD: usize = MIN_RECORD + 3 * 64 * 32;
const MAX_PROBES: u64 = 256;
const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 4096;

/// Hard work/file-size bound; the builder does not retain this many nodes in RAM.
pub const MAX_SNAPSHOT_NODES: u64 = 10_000_000;

/// Caller-defined identity of the verified source cut. This is a consistency
/// label, not a secret, signature, or authorization credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSource {
    pub identity: ObjectId,
    /// Optional source high-water mark. The graph format does not interpret it.
    pub sequence: Option<u64>,
}

/// Fixed-header metadata; inspecting it does not scan or authenticate all nodes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphSnapshotMetadata {
    pub repository: String,
    pub source: SnapshotSource,
    pub node_count: u64,
    pub index_slots: u64,
    pub file_bytes: u64,
    pub format_version: u32,
}

/// Bounded retention shared by lookups on one reader, or used during building.
#[derive(Clone, Copy, Debug)]
pub struct GraphSnapshotOptions {
    /// Zero disables caching; at most 64 MiB and 4,096 decoded nodes are retained.
    /// Accounting includes vector allocation capacities and index overhead.
    pub cache_capacity_bytes: usize,
}
impl Default for GraphSnapshotOptions {
    fn default() -> Self {
        Self {
            cache_capacity_bytes: 8 * 1024 * 1024,
        }
    }
}

/// A frozen graph membership view, backed by bounded positional file reads.
///
/// Missing IDs return [`Error::NotFound`], even if a live repository later
/// publishes them. Callers explicitly select this artifact and its expected
/// repository/source identity; this type never opens a database or falls back
/// to a different membership view. All ordinary graph algorithms accept it.
///
/// Checksums detect damaged headers, occupied/empty index slots, and records on
/// access. They are not signatures: the caller must trust the artifact's source.
/// Open readers continue using their original file after atomic replacement.
/// In-place modification is unsupported; a cached node is previously verified
/// immutable data. Use a zero-byte cache when checking every accessed record.
pub struct GraphSnapshot {
    file: File,
    layout: Layout,
    cache: Mutex<NodeCache>,
    cache_capacity: usize,
}

impl GraphSnapshot {
    /// Build and atomically replace an explicit frozen graph artifact.
    ///
    /// The caller verifies membership and source changeset bytes. This writer
    /// independently proves closed topological order (all parents precede a
    /// node), rejects duplicate IDs/parents and more than 64 parents, and derives
    /// every graph record instead of accepting precomputed indexes. The stream
    /// must contain exactly `declared_count` items. Errors from the stream are
    /// propagated. Building retains only a bounded cache and one small record;
    /// the hash index lives in the temporary file, not a whole-graph map.
    ///
    /// The destination directory must exist. All errors before atomic rename
    /// leave the old destination intact. File data is synced before publication;
    /// Unix also syncs the parent directory afterward. A directory-sync error
    /// after rename may already expose the complete new file, with uncertain
    /// crash durability. Windows has no portable directory-fsync equivalent;
    /// file contents are synced and replacement uses the native atomic rename.
    pub fn write(
        path: impl AsRef<Path>,
        repository: &str,
        source: SnapshotSource,
        declared_count: u64,
        commits: impl IntoIterator<Item = Result<(ObjectId, Vec<ObjectId>)>>,
    ) -> Result<GraphSnapshotMetadata> {
        Self::write_with_options(
            path,
            repository,
            source,
            declared_count,
            commits,
            GraphSnapshotOptions::default(),
        )
    }

    /// Build with an explicit memory budget for reads of preceding nodes.
    pub fn write_with_options(
        path: impl AsRef<Path>,
        repository: &str,
        source: SnapshotSource,
        declared_count: u64,
        commits: impl IntoIterator<Item = Result<(ObjectId, Vec<ObjectId>)>>,
        options: GraphSnapshotOptions,
    ) -> Result<GraphSnapshotMetadata> {
        validate_repository(repository)?;
        validate_options(options)?;
        let path = path.as_ref();
        if path.file_name().is_none() {
            return Err(Error::Invalid(
                "snapshot destination must name a file".into(),
            ));
        }
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut layout = Layout::new(repository, source, declared_count)?;
        let temporary = tempfile::Builder::new()
            .prefix(".scion-graph-")
            .tempfile_in(parent)?;
        let file = temporary.as_file();
        write_all_at(file, &[0; HEADER_BYTES], 0)?;
        // Even empty slots have position/source-bound hashes. Sparse zero-filled
        // holes must never silently turn a damaged occupied slot into NotFound.
        let mut buffer = Vec::with_capacity(SLOT_BYTES * 512);
        let mut offset = HEADER_BYTES as u64;
        for slot in 0..layout.metadata.index_slots {
            buffer.extend_from_slice(&encode_slot(&layout, slot, None));
            if buffer.len() >= SLOT_BYTES * 512 {
                write_all_at(file, &buffer, offset)?;
                offset += buffer.len() as u64;
                buffer.clear();
            }
        }
        write_all_at(file, &buffer, offset)?;
        let mut builder = Builder {
            file,
            layout: &layout,
            end: layout.data_start,
            cache: Mutex::new(NodeCache::default()),
            cache_capacity: options.cache_capacity_bytes,
        };
        let mut count = 0;
        for commit in commits {
            let (id, parents) = commit?;
            if count == declared_count {
                return Err(Error::Invalid(
                    "snapshot stream exceeds declared count".into(),
                ));
            }
            if parents.len() > 64 {
                return Err(Error::Invalid("snapshot node exceeds 64 parents".into()));
            }
            let (slot, existing) = find_slot(file, &layout, id, builder.end)?;
            if existing.is_some() {
                return Err(Error::Invalid(format!("duplicate snapshot changeset {id}")));
            }
            let node = build_node(&builder, &parents)?;
            let encoded = encode_record(&layout, id, &node, builder.end)?;
            let location = Location {
                id,
                offset: builder.end,
                length: encoded.len() as u32,
            };
            write_all_at(file, &encoded, builder.end)?;
            write_all_at(
                file,
                &encode_slot(&layout, slot, Some(location)),
                slot_offset(slot)?,
            )?;
            builder.end = builder
                .end
                .checked_add(encoded.len() as u64)
                .ok_or_else(|| Error::Limit("snapshot length overflow".into()))?;
            insert_cached(&builder.cache, builder.cache_capacity, id, node)?;
            count += 1;
        }
        if count != declared_count {
            return Err(Error::Invalid(
                "snapshot stream ended before declared count".into(),
            ));
        }
        let end = builder.end;
        drop(builder);
        layout.metadata.file_bytes = end;
        write_all_at(file, &layout.encode_header(), 0)?;
        file.sync_all()?;
        // persist replaces an existing file atomically on Unix and Windows.
        // The temporary file is on the same filesystem as its destination.
        temporary
            .persist(path)
            .map_err(|error| Error::Io(error.error))?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(layout.metadata)
    }

    /// Verify the fixed header and file length, without loading the index.
    pub fn inspect(path: impl AsRef<Path>) -> Result<GraphSnapshotMetadata> {
        let file = File::open(path)?;
        Ok(Layout::read(&file)?.metadata)
    }

    /// Open the chosen frozen membership view and check both expected labels.
    pub fn open(
        path: impl AsRef<Path>,
        expected_repository: &str,
        expected_identity: ObjectId,
    ) -> Result<Self> {
        Self::open_with_options(
            path,
            expected_repository,
            expected_identity,
            GraphSnapshotOptions::default(),
        )
    }

    /// Open with a bounded decoded-node cache; zero forces verification on reads.
    pub fn open_with_options(
        path: impl AsRef<Path>,
        expected_repository: &str,
        expected_identity: ObjectId,
        options: GraphSnapshotOptions,
    ) -> Result<Self> {
        validate_repository(expected_repository)?;
        validate_options(options)?;
        // std opens Windows files with read/write/delete sharing, allowing
        // replacement while this handle continues reading the old file.
        let file = File::open(path)?;
        let layout = Layout::read(&file)?;
        if layout.metadata.repository != expected_repository
            || layout.metadata.source.identity != expected_identity
        {
            return Err(Error::Invalid(
                "snapshot repository or source identity mismatch".into(),
            ));
        }
        Ok(Self {
            file,
            layout,
            cache: Mutex::new(NodeCache::default()),
            cache_capacity: options.cache_capacity_bytes,
        })
    }

    /// Metadata verified from this reader's own immutable file handle.
    pub fn metadata(&self) -> &GraphSnapshotMetadata {
        &self.layout.metadata
    }
}

impl GraphAccess for GraphSnapshot {
    fn node(&self, id: ObjectId) -> Result<GraphNode> {
        read_cached(
            &self.file,
            &self.layout,
            self.layout.metadata.file_bytes,
            id,
            &self.cache,
            self.cache_capacity,
        )
    }
}

struct Builder<'a> {
    file: &'a File,
    layout: &'a Layout,
    end: u64,
    cache: Mutex<NodeCache>,
    cache_capacity: usize,
}
impl GraphAccess for Builder<'_> {
    fn node(&self, id: ObjectId) -> Result<GraphNode> {
        read_cached(
            self.file,
            self.layout,
            self.end,
            id,
            &self.cache,
            self.cache_capacity,
        )
        .map_err(|error| match error {
            Error::NotFound(_) => {
                Error::Invalid(format!("snapshot parent {id} must precede its child"))
            }
            other => other,
        })
    }
}

struct Layout {
    metadata: GraphSnapshotMetadata,
    data_start: u64,
    context: [u8; 32],
}
impl Layout {
    fn new(repository: &str, source: SnapshotSource, count: u64) -> Result<Self> {
        let slots = index_slots(count)?;
        let data_start = slot_offset(slots)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"scion-graph-snapshot/context/v1\0");
        hasher.update(&(repository.len() as u64).to_le_bytes());
        hasher.update(repository.as_bytes());
        hasher.update(&source.identity.0);
        hasher.update(&[u8::from(source.sequence.is_some())]);
        hasher.update(&source.sequence.unwrap_or(0).to_le_bytes());
        hasher.update(&count.to_le_bytes());
        hasher.update(&slots.to_le_bytes());
        Ok(Self {
            metadata: GraphSnapshotMetadata {
                repository: repository.to_owned(),
                source,
                node_count: count,
                index_slots: slots,
                file_bytes: data_start,
                format_version: VERSION,
            },
            data_start,
            context: *hasher.finalize().as_bytes(),
        })
    }

    fn encode_header(&self) -> [u8; HEADER_BYTES] {
        let mut bytes = [0; HEADER_BYTES];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..12].copy_from_slice(&VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&(HEADER_BYTES as u32).to_le_bytes());
        bytes[16..24].copy_from_slice(&self.metadata.node_count.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.metadata.index_slots.to_le_bytes());
        bytes[32..40].copy_from_slice(&(HEADER_BYTES as u64).to_le_bytes());
        bytes[40..48].copy_from_slice(&self.data_start.to_le_bytes());
        bytes[48..56].copy_from_slice(&self.metadata.file_bytes.to_le_bytes());
        bytes[56..88].copy_from_slice(&self.metadata.source.identity.0);
        bytes[88..90].copy_from_slice(&(self.metadata.repository.len() as u16).to_le_bytes());
        bytes[90..92]
            .copy_from_slice(&u16::from(self.metadata.source.sequence.is_some()).to_le_bytes());
        bytes[92..96].copy_from_slice(&(SLOT_BYTES as u32).to_le_bytes());
        bytes[96..104].copy_from_slice(&self.metadata.source.sequence.unwrap_or(0).to_le_bytes());
        bytes[128..128 + self.metadata.repository.len()]
            .copy_from_slice(self.metadata.repository.as_bytes());
        let hash = checksum(b"header", &[0; 32], 0, &bytes[..HEADER_HASH]);
        bytes[HEADER_HASH..].copy_from_slice(&hash);
        bytes
    }

    fn read(file: &File) -> Result<Self> {
        let mut bytes = [0; HEADER_BYTES];
        read_exact_at(file, &mut bytes, 0)?;
        if bytes[HEADER_HASH..] != checksum(b"header", &[0; 32], 0, &bytes[..HEADER_HASH])
            || &bytes[..8] != MAGIC
            || u32_at(&bytes, 8) != VERSION
            || u32_at(&bytes, 12) != HEADER_BYTES as u32
            || u64_at(&bytes, 32) != HEADER_BYTES as u64
            || u32_at(&bytes, 92) != SLOT_BYTES as u32
            || bytes[104..128].iter().any(|byte| *byte != 0)
        {
            return Err(corrupt("invalid snapshot header"));
        }
        let name_length = u16::from_le_bytes([bytes[88], bytes[89]]) as usize;
        let sequence_flag = u16::from_le_bytes([bytes[90], bytes[91]]);
        if name_length == 0
            || name_length > 240
            || sequence_flag > 1
            || bytes[128 + name_length..HEADER_HASH]
                .iter()
                .any(|byte| *byte != 0)
            || (sequence_flag == 0 && u64_at(&bytes, 96) != 0)
        {
            return Err(corrupt("invalid snapshot header fields"));
        }
        let repository = std::str::from_utf8(&bytes[128..128 + name_length])
            .map_err(|_| corrupt("invalid snapshot repository encoding"))?;
        validate_repository(repository).map_err(|_| corrupt("invalid snapshot repository"))?;
        let source = SnapshotSource {
            identity: ObjectId(bytes[56..88].try_into().expect("fixed header field")),
            sequence: (sequence_flag == 1).then(|| u64_at(&bytes, 96)),
        };
        let mut layout = Self::new(repository, source, u64_at(&bytes, 16))?;
        let length = u64_at(&bytes, 48);
        let minimum = layout
            .data_start
            .checked_add(layout.metadata.node_count * MIN_RECORD as u64);
        let maximum = layout
            .data_start
            .checked_add(layout.metadata.node_count * MAX_RECORD as u64);
        if u64_at(&bytes, 24) != layout.metadata.index_slots
            || u64_at(&bytes, 40) != layout.data_start
            || minimum.is_none_or(|minimum| length < minimum)
            || maximum.is_none_or(|maximum| length > maximum)
            || file.metadata()?.len() != length
        {
            return Err(corrupt("invalid snapshot layout or file length"));
        }
        layout.metadata.file_bytes = length;
        Ok(layout)
    }
}

#[derive(Clone, Copy)]
struct Location {
    id: ObjectId,
    offset: u64,
    length: u32,
}

fn encode_slot(layout: &Layout, slot: u64, location: Option<Location>) -> [u8; SLOT_BYTES] {
    let mut bytes = [0; SLOT_BYTES];
    if let Some(location) = location {
        bytes[..32].copy_from_slice(&location.id.0);
        bytes[32..40].copy_from_slice(&location.offset.to_le_bytes());
        bytes[40..44].copy_from_slice(&location.length.to_le_bytes());
    }
    let hash = checksum(b"slot", &layout.context, slot, &bytes[..SLOT_HASH]);
    bytes[SLOT_HASH..].copy_from_slice(&hash);
    bytes
}

fn find_slot(
    file: &File,
    layout: &Layout,
    id: ObjectId,
    data_end: u64,
) -> Result<(u64, Option<Location>)> {
    let digest = checksum(b"bucket", &layout.context, 0, &id.0);
    let first = u64::from_le_bytes(digest[..8].try_into().expect("fixed hash prefix"));
    let mask = layout.metadata.index_slots - 1;
    for distance in 0..layout.metadata.index_slots.min(MAX_PROBES) {
        let slot = first.wrapping_add(distance) & mask;
        let mut bytes = [0; SLOT_BYTES];
        read_exact_at(file, &mut bytes, slot_offset(slot)?)?;
        if bytes[SLOT_HASH..] != checksum(b"slot", &layout.context, slot, &bytes[..SLOT_HASH])
            || bytes[44..48] != [0; 4]
        {
            return Err(corrupt("snapshot index slot checksum or reserved fields"));
        }
        let offset = u64_at(&bytes, 32);
        if offset == 0 {
            if bytes[..SLOT_HASH].iter().any(|byte| *byte != 0) {
                return Err(corrupt("invalid empty snapshot index slot"));
            }
            return Ok((slot, None));
        }
        let length = u32_at(&bytes, 40) as usize;
        if !(MIN_RECORD..=MAX_RECORD).contains(&length)
            || !(length - MIN_RECORD).is_multiple_of(32)
            || offset < layout.data_start
            || offset
                .checked_add(length as u64)
                .is_none_or(|end| end > data_end)
        {
            return Err(corrupt("snapshot record range outside its data section"));
        }
        if bytes[..32] == id.0 {
            return Ok((
                slot,
                Some(Location {
                    id,
                    offset,
                    length: length as u32,
                }),
            ));
        }
    }
    Err(Error::Limit(
        "snapshot hash-index probe limit exceeded".into(),
    ))
}

fn encode_record(layout: &Layout, id: ObjectId, node: &GraphNode, offset: u64) -> Result<Vec<u8>> {
    validate_node(id, node, layout.metadata.node_count)?;
    let shared = !node.linear_jumps.is_empty() && node.linear_jumps == node.jumps;
    let mut bytes = Vec::with_capacity(
        MIN_RECORD
            + 32 * (node.parents.len()
                + node.jumps.len()
                + if shared { 0 } else { node.linear_jumps.len() }),
    );
    bytes.extend_from_slice(&id.0);
    bytes.extend_from_slice(&node.generation.to_le_bytes());
    bytes.extend_from_slice(&node.first_parent_depth.to_le_bytes());
    bytes.extend_from_slice(&[
        node.parents.len() as u8,
        node.jumps.len() as u8,
        node.linear_jumps.len() as u8,
        u8::from(shared),
    ]);
    bytes.extend_from_slice(&[0; 4]);
    for id in node.parents.iter().chain(&node.jumps).chain(if shared {
        [].as_slice()
    } else {
        &node.linear_jumps
    }) {
        bytes.extend_from_slice(&id.0);
    }
    let hash = checksum(b"record", &layout.context, offset, &bytes);
    bytes.extend_from_slice(&hash);
    Ok(bytes)
}

fn read_record(file: &File, layout: &Layout, location: Location) -> Result<GraphNode> {
    // find_slot validated the entire range and bounded length before allocation.
    let mut bytes = vec![0; location.length as usize];
    read_exact_at(file, &mut bytes, location.offset)?;
    let hash_start = bytes.len() - 32;
    if bytes[hash_start..]
        != checksum(
            b"record",
            &layout.context,
            location.offset,
            &bytes[..hash_start],
        )
        || bytes[..32] != location.id.0
        || bytes[52..56] != [0; 4]
    {
        return Err(corrupt("snapshot record checksum, ID or reserved fields"));
    }
    let (parents, jumps, linear, shared) = (
        bytes[48] as usize,
        bytes[49] as usize,
        bytes[50] as usize,
        bytes[51],
    );
    if parents > 64
        || jumps > 64
        || linear > 64
        || shared > 1
        || (shared == 1 && (linear == 0 || linear != jumps))
        || hash_start
            != RECORD_HEADER + 32 * (parents + jumps + if shared == 1 { 0 } else { linear })
    {
        return Err(corrupt("invalid snapshot record lengths or flags"));
    }
    let mut ids = bytes[RECORD_HEADER..hash_start]
        .as_chunks::<32>()
        .0
        .iter()
        .map(|id| ObjectId(*id));
    let parents = ids.by_ref().take(parents).collect::<Vec<_>>();
    let jumps = ids.by_ref().take(jumps).collect::<Vec<_>>();
    let linear_jumps = if shared == 1 {
        jumps.clone()
    } else {
        ids.collect()
    };
    let node = GraphNode {
        parents,
        generation: u64_at(&bytes, 32),
        first_parent_depth: u64_at(&bytes, 40),
        jumps,
        linear_jumps,
    };
    validate_node(location.id, &node, layout.metadata.node_count)?;
    Ok(node)
}

fn validate_node(id: ObjectId, node: &GraphNode, count: u64) -> Result<()> {
    let expected_jumps = (64 - node.first_parent_depth.leading_zeros()) as usize;
    if node.parents.len() > 64
        || node.jumps.len() != expected_jumps
        || node.linear_jumps.len() > node.jumps.len()
        || node.generation >= count
        || node.first_parent_depth > node.generation
        || node.parents.contains(&id)
        || node.jumps.contains(&id)
        || node.parents.iter().collect::<BTreeSet<_>>().len() != node.parents.len()
        || node.jumps.iter().collect::<BTreeSet<_>>().len() != node.jumps.len()
        || !node.jumps.starts_with(&node.linear_jumps)
        || (node.parents.is_empty() && (node.generation != 0 || node.first_parent_depth != 0))
        || (!node.parents.is_empty()
            && (node.generation == 0
                || node.first_parent_depth == 0
                || node.jumps.first() != node.parents.first()))
        || (node.parents.len() == 1 && node.linear_jumps.first() != node.parents.first())
        || (node.parents.len() != 1 && !node.linear_jumps.is_empty())
    {
        return Err(corrupt("invalid snapshot graph invariants"));
    }
    Ok(())
}

#[derive(Default)]
struct NodeCache {
    values: BTreeMap<ObjectId, (GraphNode, usize, u64)>,
    order: BTreeSet<(u64, ObjectId)>,
    bytes: usize,
    clock: u64,
}
impl NodeCache {
    fn tick(&mut self) -> u64 {
        if self.clock == u64::MAX {
            self.values.clear();
            self.order.clear();
            self.bytes = 0;
            self.clock = 0;
        }
        self.clock += 1;
        self.clock
    }
    fn get(&mut self, id: ObjectId) -> Option<GraphNode> {
        let stamp = self.tick();
        let (node, _, old) = self.values.get_mut(&id)?;
        self.order.remove(&(*old, id));
        *old = stamp;
        self.order.insert((stamp, id));
        Some(node.clone())
    }
    fn insert(&mut self, id: ObjectId, node: GraphNode, capacity: usize) {
        let bytes = 512
            + 32 * (node.parents.capacity() + node.jumps.capacity() + node.linear_jumps.capacity());
        if bytes > capacity || self.values.contains_key(&id) {
            return;
        }
        while self.bytes + bytes > capacity || self.values.len() >= MAX_CACHE_ENTRIES {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            if let Some((_, removed, _)) = self.values.remove(&oldest) {
                self.bytes -= removed;
            }
        }
        let stamp = self.tick();
        self.bytes += bytes;
        self.order.insert((stamp, id));
        self.values.insert(id, (node, bytes, stamp));
    }
}

fn read_cached(
    file: &File,
    layout: &Layout,
    end: u64,
    id: ObjectId,
    cache: &Mutex<NodeCache>,
    capacity: usize,
) -> Result<GraphNode> {
    if capacity > 0
        && let Some(node) = cache
            .lock()
            .map_err(|_| corrupt("snapshot cache lock poisoned"))?
            .get(id)
    {
        return Ok(node);
    }
    let (_, location) = find_slot(file, layout, id, end)?;
    let location = location.ok_or_else(|| {
        Error::NotFound(format!(
            "changeset {id} in graph snapshot {}",
            layout.metadata.repository
        ))
    })?;
    let node = read_record(file, layout, location)?;
    if capacity > 0 {
        insert_cached(cache, capacity, id, node.clone())?;
    }
    Ok(node)
}

fn insert_cached(
    cache: &Mutex<NodeCache>,
    capacity: usize,
    id: ObjectId,
    node: GraphNode,
) -> Result<()> {
    if capacity > 0 {
        cache
            .lock()
            .map_err(|_| corrupt("snapshot cache lock poisoned"))?
            .insert(id, node, capacity);
    }
    Ok(())
}

fn validate_repository(repository: &str) -> Result<()> {
    validate_name(repository)?;
    if repository.contains('/') {
        return Err(Error::Invalid(
            "snapshot repository cannot contain slashes".into(),
        ));
    }
    Ok(())
}
fn validate_options(options: GraphSnapshotOptions) -> Result<()> {
    if options.cache_capacity_bytes > MAX_CACHE_BYTES {
        return Err(Error::Limit("snapshot cache exceeds 64 MiB".into()));
    }
    Ok(())
}
fn index_slots(count: u64) -> Result<u64> {
    if count > MAX_SNAPSHOT_NODES {
        return Err(Error::Limit("snapshot exceeds ten million nodes".into()));
    }
    count
        .checked_mul(2)
        .and_then(|count| count.max(1).checked_next_power_of_two())
        .ok_or_else(|| Error::Limit("snapshot index size overflow".into()))
}
fn slot_offset(slot: u64) -> Result<u64> {
    slot.checked_mul(SLOT_BYTES as u64)
        .and_then(|offset| offset.checked_add(HEADER_BYTES as u64))
        .ok_or_else(|| Error::Limit("snapshot index offset overflow".into()))
}
fn checksum(domain: &[u8], context: &[u8; 32], position: u64, bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"scion-graph-snapshot/checksum/v1\0");
    hasher.update(domain);
    hasher.update(&[0]);
    hasher.update(context);
    hasher.update(&position.to_le_bytes());
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("validated fixed field"),
    )
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated fixed field"),
    )
}
fn corrupt(message: &str) -> Error {
    Error::Corrupt(message.into())
}

fn read_exact_at(file: &File, mut bytes: &mut [u8], mut offset: u64) -> Result<()> {
    while !bytes.is_empty() {
        #[cfg(unix)]
        let result = std::os::unix::fs::FileExt::read_at(file, bytes, offset);
        #[cfg(windows)]
        let result = std::os::windows::fs::FileExt::seek_read(file, bytes, offset);
        #[cfg(not(any(unix, windows)))]
        let result = {
            let _ = (file, offset);
            Err(io::Error::from(io::ErrorKind::Unsupported))
        };
        match result {
            Ok(0) => return Err(corrupt("truncated graph snapshot")),
            Ok(count) => {
                offset += count as u64;
                bytes = &mut bytes[count..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
fn write_all_at(file: &File, mut bytes: &[u8], mut offset: u64) -> Result<()> {
    while !bytes.is_empty() {
        #[cfg(unix)]
        let result = std::os::unix::fs::FileExt::write_at(file, bytes, offset);
        #[cfg(windows)]
        let result = std::os::windows::fs::FileExt::seek_write(file, bytes, offset);
        #[cfg(not(any(unix, windows)))]
        let result = {
            let _ = (file, offset);
            Err(io::Error::from(io::ErrorKind::Unsupported))
        };
        match result {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
            Ok(count) => {
                offset += count as u64;
                bytes = &bytes[count..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;
