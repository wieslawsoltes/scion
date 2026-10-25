use super::*;
use crate::{is_ancestor, merge_bases, nth_first_parent};
use scion_types::ObjectKind;
use std::fs;

fn id(number: u64) -> ObjectId {
    ObjectId::hash(ObjectKind::Changeset, &number.to_le_bytes())
}
fn source(number: u64) -> SnapshotSource {
    SnapshotSource {
        identity: id(number),
        sequence: Some(number),
    }
}
fn write(path: &Path, commits: &[(ObjectId, Vec<ObjectId>)]) -> GraphSnapshotMetadata {
    GraphSnapshot::write(
        path,
        "repo",
        source(9000),
        commits.len() as u64,
        commits.iter().cloned().map(Ok),
    )
    .unwrap()
}
fn open(path: &Path) -> GraphSnapshot {
    GraphSnapshot::open_with_options(
        path,
        "repo",
        source(9000).identity,
        GraphSnapshotOptions {
            cache_capacity_bytes: 0,
        },
    )
    .unwrap()
}
fn reachable(parents: &BTreeMap<ObjectId, Vec<ObjectId>>, start: ObjectId) -> BTreeSet<ObjectId> {
    let mut seen = BTreeSet::new();
    let mut pending = vec![start];
    while let Some(id) = pending.pop() {
        if seen.insert(id) {
            pending.extend(&parents[&id]);
        }
    }
    seen
}

#[test]
fn arbitrary_dags_match_independent_reachability_and_merge_base_oracles() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph");
    // Include unrelated roots and a criss-cross pair before deterministic DAGs.
    let mut commits = vec![
        (id(0), vec![]),
        (id(1), vec![]),
        (id(2), vec![id(0)]),
        (id(3), vec![id(0)]),
        (id(4), vec![id(2), id(3)]),
        (id(5), vec![id(3), id(2)]),
        (id(6), vec![id(4), id(1)]),
    ];
    let mut random = 0x1234_5678_u64;
    for number in 7..64 {
        let mut parents = Vec::new();
        for _ in 0..1 + (number % 3) {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let parent = id(random % number);
            if !parents.contains(&parent) {
                parents.push(parent);
            }
        }
        commits.push((id(number), parents));
    }
    let metadata = write(&path, &commits);
    assert_eq!(metadata, GraphSnapshot::inspect(&path).unwrap());
    let graph = GraphSnapshot::open(&path, "repo", source(9000).identity).unwrap();
    let parents = commits.iter().cloned().collect::<BTreeMap<_, _>>();
    let ancestors = commits
        .iter()
        .map(|(id, _)| (*id, reachable(&parents, *id)))
        .collect::<BTreeMap<_, _>>();
    for (a, _) in &commits {
        for (b, _) in &commits {
            assert_eq!(
                is_ancestor(&graph, *a, *b).unwrap(),
                ancestors[b].contains(a),
                "{a} -> {b}"
            );
            let common = ancestors[a]
                .intersection(&ancestors[b])
                .copied()
                .collect::<BTreeSet<_>>();
            let expected = common
                .iter()
                .filter(|candidate| {
                    !common
                        .iter()
                        .any(|other| other != *candidate && ancestors[other].contains(candidate))
                })
                .copied()
                .collect::<Vec<_>>();
            assert_eq!(
                merge_bases(&graph, *a, *b).unwrap(),
                expected,
                "bases {a}, {b}"
            );
        }
        let mut expected = Some(*a);
        for distance in 0..=65 {
            assert_eq!(nth_first_parent(&graph, *a, distance).unwrap(), expected);
            expected = expected.and_then(|current| parents[&current].first().copied());
        }
    }
    assert_eq!(merge_bases(&graph, id(4), id(5)).unwrap(), {
        let mut expected = vec![id(2), id(3)];
        expected.sort();
        expected
    });
    assert!(!is_ancestor(&graph, id(1), id(5)).unwrap());
    assert!(is_ancestor(&graph, id(1), id(6)).unwrap());
}

#[test]
fn long_linear_history_and_disabled_caches_keep_all_jumps() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph");
    let commits = (0..1025)
        .map(|number| {
            (
                id(number),
                if number == 0 {
                    vec![]
                } else {
                    vec![id(number - 1)]
                },
            )
        })
        .collect::<Vec<_>>();
    GraphSnapshot::write_with_options(
        &path,
        "repo",
        source(9000),
        commits.len() as u64,
        commits.into_iter().map(Ok),
        GraphSnapshotOptions {
            cache_capacity_bytes: 0,
        },
    )
    .unwrap();
    let graph = open(&path);
    assert_eq!(graph.node(id(1024)).unwrap().jumps.len(), 11);
    for distance in [0, 1, 2, 31, 511, 1000, 1024, 1025, u64::MAX] {
        assert_eq!(
            nth_first_parent(&graph, id(1024), distance).unwrap(),
            (distance <= 1024).then(|| id(1024 - distance))
        );
    }
    assert!(is_ancestor(&graph, id(0), id(1024)).unwrap());
    assert_eq!(graph.cache.lock().unwrap().values.len(), 0);
}

#[test]
fn exactly_64_parents_and_the_zero_object_id_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph");
    // Empty slots use an absent record offset, never a reserved object ID.
    let zero = ObjectId([0; 32]);
    let mut roots = vec![zero];
    roots.extend((1..64).map(id));
    let mut commits = roots
        .iter()
        .map(|root| (*root, Vec::new()))
        .collect::<Vec<_>>();
    commits.push((id(1000), roots.clone()));
    commits.push((id(1001), vec![id(1000)]));
    write(&path, &commits);
    let graph = open(&path);
    assert!(graph.node(zero).unwrap().parents.is_empty());
    assert_eq!(graph.node(id(1000)).unwrap().parents, roots);
    for root in roots {
        assert!(is_ancestor(&graph, root, id(1001)).unwrap());
        assert_eq!(merge_bases(&graph, root, id(1001)).unwrap(), vec![root]);
    }
    assert_eq!(nth_first_parent(&graph, id(1001), 2).unwrap(), Some(zero));
    assert_eq!(nth_first_parent(&graph, id(1001), 3).unwrap(), None);
}

#[test]
fn empty_snapshot_identity_scope_and_unknown_endpoints_are_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("empty");
    let metadata = GraphSnapshot::write(
        &path,
        "repo",
        SnapshotSource {
            identity: id(9000),
            sequence: None,
        },
        0,
        std::iter::empty(),
    )
    .unwrap();
    assert_eq!(metadata.node_count, 0);
    assert_eq!(metadata.index_slots, 1);
    assert_eq!(metadata.file_bytes, (HEADER_BYTES + SLOT_BYTES) as u64);
    assert_eq!(metadata.source.sequence, None);
    let graph = GraphSnapshot::open(&path, "repo", id(9000)).unwrap();
    assert!(matches!(graph.node(id(1)), Err(Error::NotFound(_))));
    assert!(matches!(
        is_ancestor(&graph, id(1), id(1)),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        merge_bases(&graph, id(1), id(1)),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        nth_first_parent(&graph, id(1), 0),
        Err(Error::NotFound(_))
    ));
    assert!(GraphSnapshot::open(&path, "foreign", id(9000)).is_err());
    assert!(GraphSnapshot::open(&path, "repo", id(9001)).is_err());
    assert!(GraphSnapshot::open(&path, "repo/nested", id(9000)).is_err());
    assert!(
        GraphSnapshot::open_with_options(
            &path,
            "repo",
            id(9000),
            GraphSnapshotOptions {
                cache_capacity_bytes: usize::MAX
            }
        )
        .is_err()
    );
    let path = temp.path().join("nonempty");
    write(&path, &[(id(1), vec![])]);
    let graph = open(&path);
    assert!(matches!(
        is_ancestor(&graph, id(2), id(1)),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        is_ancestor(&graph, id(1), id(2)),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        merge_bases(&graph, id(1), id(2)),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn invalid_or_failed_input_never_replaces_an_existing_artifact() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph");
    write(&path, &[(id(1), vec![])]);
    let original = fs::read(&path).unwrap();
    let cases = [
        (0, vec![(id(1), vec![])]),
        (2, vec![(id(1), vec![])]),
        (1, vec![(id(2), vec![id(1)])]),
        (1, vec![(id(2), vec![id(2)])]),
        (2, vec![(id(1), vec![]), (id(1), vec![])]),
        (2, vec![(id(1), vec![]), (id(2), vec![id(1), id(1)])]),
        (1, vec![(id(2), (0..65).map(id).collect())]),
    ];
    for (count, commits) in cases {
        assert!(
            GraphSnapshot::write(
                &path,
                "repo",
                source(9001),
                count,
                commits.into_iter().map(Ok)
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }
    let failure = vec![
        Ok((id(1), vec![])),
        Err(Error::Storage("source failed".into())),
    ];
    assert!(GraphSnapshot::write(&path, "repo", source(9001), 2, failure).is_err());
    assert!(GraphSnapshot::write(&path, "bad/name", source(9001), 0, std::iter::empty()).is_err());
    assert!(
        GraphSnapshot::write(&path, "repo", source(9001), u64::MAX, std::iter::empty()).is_err()
    );
    assert!(
        GraphSnapshot::write_with_options(
            &path,
            "repo",
            source(9001),
            0,
            std::iter::empty(),
            GraphSnapshotOptions {
                cache_capacity_bytes: MAX_CACHE_BYTES + 1
            }
        )
        .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn atomic_replacement_preserves_old_open_reader_membership_on_unix_and_windows() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph");
    write(&path, &[(id(1), vec![]), (id(2), vec![id(1)])]);
    let old = open(&path);
    let updated = vec![(id(1), vec![]), (id(2), vec![id(1)]), (id(3), vec![id(2)])];
    GraphSnapshot::write(&path, "repo", source(9001), 3, updated.into_iter().map(Ok)).unwrap();
    assert!(GraphSnapshot::open(&path, "repo", source(9000).identity).is_err());
    let new = GraphSnapshot::open(&path, "repo", source(9001).identity).unwrap();
    assert!(is_ancestor(&old, id(1), id(2)).unwrap());
    assert!(matches!(old.node(id(3)), Err(Error::NotFound(_))));
    assert!(is_ancestor(&new, id(1), id(3)).unwrap());
    assert_eq!(old.metadata().node_count, 2);
    assert_eq!(new.metadata().node_count, 3);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                for _ in 0..32 {
                    assert_eq!(old.node(id(2)).unwrap().parents, vec![id(1)]);
                    assert_eq!(old.node(id(1)).unwrap().generation, 0);
                }
            });
        }
    });
}

fn rehash_header(bytes: &mut [u8]) {
    let hash = checksum(b"header", &[0; 32], 0, &bytes[..HEADER_HASH]);
    bytes[HEADER_HASH..HEADER_BYTES].copy_from_slice(&hash);
}
fn rehash_record(bytes: &mut [u8], layout: &Layout, location: Location) {
    let start = location.offset as usize;
    let hash_start = start + location.length as usize - 32;
    let hash = checksum(
        b"record",
        &layout.context,
        location.offset,
        &bytes[start..hash_start],
    );
    bytes[hash_start..hash_start + 32].copy_from_slice(&hash);
}

#[test]
fn every_truncation_header_damage_trailing_bytes_and_unchecked_sizes_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph");
    write(&path, &[(id(1), vec![]), (id(2), vec![id(1)])]);
    let original = fs::read(&path).unwrap();
    for end in 0..original.len() {
        fs::write(&path, &original[..end]).unwrap();
        assert!(GraphSnapshot::inspect(&path).is_err(), "truncated at {end}");
    }
    for position in 0..HEADER_BYTES {
        let mut damaged = original.clone();
        damaged[position] ^= 1;
        fs::write(&path, damaged).unwrap();
        assert!(
            GraphSnapshot::inspect(&path).is_err(),
            "header byte {position}"
        );
    }
    for (offset, value) in [
        (16, u64::MAX),
        (24, u64::MAX),
        (32, 0),
        (40, u64::MAX),
        (48, u64::MAX),
    ] {
        let mut damaged = original.clone();
        damaged[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        rehash_header(&mut damaged);
        fs::write(&path, damaged).unwrap();
        assert!(GraphSnapshot::inspect(&path).is_err());
    }
    let mut trailing = original;
    trailing.push(0);
    fs::write(&path, trailing).unwrap();
    assert!(GraphSnapshot::inspect(&path).is_err());
}

#[test]
fn occupied_and_empty_index_slots_records_and_source_binding_are_checksummed() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph");
    write(&path, &[(id(1), vec![]), (id(2), vec![id(1)])]);
    let original = fs::read(&path).unwrap();
    let graph = open(&path);
    let (slot, location) = find_slot(
        &graph.file,
        &graph.layout,
        id(2),
        graph.metadata().file_bytes,
    )
    .unwrap();
    let location = location.unwrap();
    drop(graph);
    for (start, length) in [
        (slot_offset(slot).unwrap() as usize, SLOT_BYTES),
        (HEADER_BYTES, 4 * SLOT_BYTES),
    ] {
        let mut damaged = original.clone();
        damaged[start..start + length].fill(0);
        fs::write(&path, damaged).unwrap();
        assert!(matches!(open(&path).node(id(2)), Err(Error::Corrupt(_))));
    }
    let mut damaged = original.clone();
    damaged[location.offset as usize + 32] ^= 1;
    fs::write(&path, damaged).unwrap();
    assert!(matches!(open(&path).node(id(2)), Err(Error::Corrupt(_))));
    let mut damaged = original;
    damaged[56..88].copy_from_slice(&source(9001).identity.0);
    rehash_header(&mut damaged);
    fs::write(&path, damaged).unwrap();
    let graph = GraphSnapshot::open(&path, "repo", source(9001).identity).unwrap();
    assert!(matches!(graph.node(id(2)), Err(Error::Corrupt(_))));

    let path = temp.path().join("empty");
    write(&path, &[]);
    let mut damaged = fs::read(&path).unwrap();
    damaged[HEADER_BYTES..].fill(0);
    fs::write(&path, damaged).unwrap();
    assert!(matches!(open(&path).node(id(100)), Err(Error::Corrupt(_))));
}

#[test]
fn rehashed_malformed_offsets_lengths_flags_and_graph_fields_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph");
    write(&path, &[(id(1), vec![]), (id(2), vec![id(1)])]);
    let original = fs::read(&path).unwrap();
    let graph = open(&path);
    let (slot, location) = find_slot(
        &graph.file,
        &graph.layout,
        id(2),
        graph.metadata().file_bytes,
    )
    .unwrap();
    let location = location.unwrap();
    for bad in [
        Location {
            offset: u64::MAX,
            ..location
        },
        Location {
            offset: 1,
            ..location
        },
        Location {
            length: u32::MAX,
            ..location
        },
        Location {
            length: MIN_RECORD as u32 - 1,
            ..location
        },
    ] {
        let mut damaged = original.clone();
        let start = slot_offset(slot).unwrap() as usize;
        damaged[start..start + SLOT_BYTES].copy_from_slice(&encode_slot(
            &graph.layout,
            slot,
            Some(bad),
        ));
        fs::write(&path, damaged).unwrap();
        assert!(matches!(open(&path).node(id(2)), Err(Error::Corrupt(_))));
    }
    for field in [48, 49, 50, 51, 52] {
        let mut damaged = original.clone();
        damaged[location.offset as usize + field] = u8::MAX;
        rehash_record(&mut damaged, &graph.layout, location);
        fs::write(&path, damaged).unwrap();
        assert!(matches!(open(&path).node(id(2)), Err(Error::Corrupt(_))));
    }
    for field in [32, 40] {
        let mut damaged = original.clone();
        let start = location.offset as usize + field;
        damaged[start..start + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        rehash_record(&mut damaged, &graph.layout, location);
        fs::write(&path, damaged).unwrap();
        assert!(matches!(open(&path).node(id(2)), Err(Error::Corrupt(_))));
    }
}

#[test]
fn damaged_full_hash_index_has_a_finite_probe_budget() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph");
    write(&path, &[(id(1), vec![])]);
    let graph = open(&path);
    let (_, location) = find_slot(
        &graph.file,
        &graph.layout,
        id(1),
        graph.metadata().file_bytes,
    )
    .unwrap();
    let mut damaged = fs::read(&path).unwrap();
    for slot in 0..graph.metadata().index_slots {
        let offset = slot_offset(slot).unwrap() as usize;
        damaged[offset..offset + SLOT_BYTES].copy_from_slice(&encode_slot(
            &graph.layout,
            slot,
            location,
        ));
    }
    fs::write(&path, damaged).unwrap();
    assert!(matches!(open(&path).node(id(999)), Err(Error::Limit(_))));
}

#[test]
fn truncation_after_open_fails_on_access_without_unbounded_allocation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph");
    write(&path, &[(id(1), vec![])]);
    let graph = open(&path);
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(graph.metadata().file_bytes - 1)
        .unwrap();
    assert!(matches!(graph.node(id(1)), Err(Error::Corrupt(_))));
}

#[test]
fn cache_memory_and_entry_count_are_bounded_and_clock_overflow_is_safe() {
    let mut cache = NodeCache::default();
    let node = GraphNode {
        parents: vec![],
        generation: 0,
        first_parent_depth: 0,
        jumps: vec![],
        linear_jumps: vec![],
    };
    for number in 0..5000 {
        cache.insert(id(number), node.clone(), MAX_CACHE_BYTES);
        assert!(cache.bytes <= MAX_CACHE_BYTES);
        assert!(cache.values.len() <= MAX_CACHE_ENTRIES);
    }
    assert_eq!(cache.values.len(), MAX_CACHE_ENTRIES);
    assert!(cache.get(id(0)).is_none());
    assert!(cache.get(id(4999)).is_some());
    cache.clock = u64::MAX;
    assert!(cache.get(id(4999)).is_none());
    assert_eq!(cache.bytes, 0);
    cache.insert(id(5000), node.clone(), 0);
    assert!(cache.values.is_empty());
    for number in 0..32 {
        cache.insert(id(number), node.clone(), 1024);
        assert!(cache.bytes <= 1024);
    }
    assert_eq!(cache.values.len(), 2);
}
