//! Deterministic, dependency-free scaling benchmark. Run `cargo bench -p
//! scion-engine --bench scaling -- --quick` for the small profile. Measurements
//! describe this process/backend, not distributed production throughput.
use scion_engine::Repository;
use scion_store::MemoryStore;
use scion_types::*;
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

fn commit(
    repo: &Repository,
    parents: Vec<ObjectId>,
    changes: BTreeMap<RepoPath, FileChange>,
    message: String,
) -> ObjectId {
    repo.create_commit(Changeset {
        parents,
        author: "benchmark".into(),
        committer: None,
        timestamp_ms: 0,
        message,
        changes,
        metadata: BTreeMap::new(),
    })
    .unwrap()
    .id()
    .unwrap()
}
fn report(name: &str, scale: usize, times: &mut [Duration]) {
    times.sort();
    let p50 = times[times.len() / 2].as_secs_f64() * 1e6;
    let p95 = times[(times.len() * 95 / 100).min(times.len() - 1)].as_secs_f64() * 1e6;
    println!("{name},{scale},{},{p50:.3},{p95:.3}", times.len());
}
fn main() {
    let quick = std::env::args().any(|a| a == "--quick");
    println!("# Scion engine benchmark; MemoryStore; release optimizations; microseconds");
    println!("operation,scale,samples,p50_us,p95_us");
    for files in if quick {
        vec![100, 1000]
    } else {
        vec![1000, 10000, 100000]
    } {
        let repo = Repository::new("bench", Arc::new(MemoryStore::new())).unwrap();
        let blob = repo.put_blob(b"one\ntwo\nthree\n").unwrap();
        let initial = (0..files)
            .map(|i| {
                (
                    RepoPath::new(format!("dir{}/f{i}", i / 100)).unwrap(),
                    FileChange::put(FileEntry {
                        id: blob,
                        size: 14,
                        kind: FileKind::Regular,
                    }),
                )
            })
            .collect();
        let start = Instant::now();
        let root = commit(&repo, vec![], initial, "initial".into());
        report("initial_commit", files, &mut [start.elapsed()]);
        let mut tip = root;
        let mut times = Vec::new();
        let mut diffs = Vec::new();
        for n in 0..if quick { 10 } else { 50 } {
            let bytes = format!("edited {n}\ntwo\nthree\n");
            let blob = repo.put_blob(bytes.as_bytes()).unwrap();
            let changes = BTreeMap::from([(
                RepoPath::new("dir0/f0").unwrap(),
                FileChange::put(FileEntry {
                    id: blob,
                    size: bytes.len() as u64,
                    kind: FileKind::Regular,
                }),
            )]);
            let start = Instant::now();
            let next = commit(&repo, vec![tip], changes, format!("edit{n}"));
            times.push(start.elapsed());
            let start = Instant::now();
            assert_eq!(repo.diff(tip, next).unwrap().len(), 1);
            diffs.push(start.elapsed());
            tip = next;
        }
        report("single_file_commit", files, &mut times);
        report("single_file_diff", files, &mut diffs);
    }
    let repo = Repository::new("graph", Arc::new(MemoryStore::new())).unwrap();
    let root = commit(&repo, vec![], BTreeMap::new(), "root".into());
    let side = commit(&repo, vec![root], BTreeMap::new(), "side".into());
    let depth = if quick { 1000 } else { 10000 };
    let mut tip = root;
    for n in 0..depth {
        tip = commit(&repo, vec![tip], BTreeMap::new(), format!("linear{n}"));
    }
    for (name, ancestor, expected) in [
        ("ancestry_positive", root, true),
        ("ancestry_negative", side, false),
    ] {
        let mut times = Vec::new();
        for _ in 0..100 {
            let start = Instant::now();
            assert_eq!(repo.is_ancestor(ancestor, tip).unwrap(), expected);
            times.push(start.elapsed());
        }
        report(name, depth, &mut times);
    }
}
