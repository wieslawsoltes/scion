//! Durable comparison worker used by scripts/compare_git.py. It is an example,
//! not a Cargo benchmark, so `cargo test --all-targets` cannot start a long run.
use scion_engine::{CommitRefUpdate, Repository};
use scion_store::{LocalStore, PreparedObject};
use scion_types::{
    Changeset, FileChange, FileEntry, FileKind, ObjectId, ObjectKind, RepoPath, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
    sync::Arc,
    time::Instant,
};

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Noop,
    Initialize {
        files: usize,
        fanout: usize,
        variants: usize,
        blob_bytes: usize,
    },
    Commit {
        parent: ObjectId,
        path: RepoPath,
        content: String,
        sequence: u64,
    },
    Chain {
        parent: ObjectId,
        count: usize,
        label: String,
    },
    Ancestry {
        ancestor: ObjectId,
        descendant: ObjectId,
    },
    MergeBases {
        left: ObjectId,
        right: ObjectId,
    },
    Diff {
        base: ObjectId,
        target: ObjectId,
    },
    Read {
        revision: ObjectId,
        path: RepoPath,
    },
}

#[derive(Serialize)]
#[serde(untagged)]
enum Response {
    Success {
        ok: bool,
        result: Value,
        operation_ns: u128,
    },
    Failure {
        ok: bool,
        error: String,
        operation_ns: u128,
    },
}

fn fixture_body(variant: usize, size: usize) -> Vec<u8> {
    let mut bytes = format!("fixture {variant:03}\n").into_bytes();
    let line = b"one two three four five six seven eight nine ten\n";
    while bytes.len() < size {
        bytes.extend_from_slice(line);
    }
    bytes.truncate(size);
    if let Some(last) = bytes.last_mut() {
        *last = b'\n';
    }
    bytes
}

fn create(
    repository: &Repository,
    parent: Option<ObjectId>,
    changes: BTreeMap<RepoPath, FileChange>,
    message: String,
    sequence: u64,
) -> Result<ObjectId> {
    repository
        .create_commit(Changeset {
            parents: parent.into_iter().collect(),
            author: "Benchmark <benchmark@example.invalid>".into(),
            committer: None,
            timestamp_ms: 1_700_000_000_000 + sequence as i64 * 1000,
            message,
            changes,
            metadata: BTreeMap::new(),
        })?
        .id()
}

fn put(repository: &Repository, bytes: &[u8]) -> Result<FileEntry> {
    Ok(FileEntry {
        id: repository.put_blob(bytes)?,
        size: bytes.len() as u64,
        kind: FileKind::Regular,
    })
}

fn execute(repository: &Repository, request: Request) -> Result<Value> {
    match request {
        Request::Noop => Ok(json!(true)),
        Request::Initialize {
            files,
            fanout,
            variants,
            blob_bytes,
        } => {
            if files == 0 || fanout == 0 || variants == 0 || blob_bytes < 32 {
                return Err(scion_types::Error::Invalid(
                    "invalid fixture dimensions".into(),
                ));
            }
            let entries = (0..variants)
                .map(|variant| put(repository, &fixture_body(variant, blob_bytes)))
                .collect::<Result<Vec<_>>>()?;
            let changes = (0..files)
                .map(|i| {
                    Ok((
                        RepoPath::new(format!("dir{:04}/file{i:06}.txt", i / fanout))?,
                        FileChange::put(entries[i % variants].clone()),
                    ))
                })
                .collect::<Result<_>>()?;
            let root = create(repository, None, changes, "initial".into(), 0)?;
            if !repository.update_ref("bench", None, Some(root))? {
                return Err(scion_types::Error::Conflict(
                    "benchmark fixture already exists".into(),
                ));
            }
            Ok(json!(root))
        }
        Request::Commit {
            parent,
            path,
            content,
            sequence,
        } => {
            let blob = PreparedObject::new(ObjectKind::Blob, content.into_bytes())?;
            let entry = FileEntry {
                id: blob.id(),
                size: blob.bytes().len() as u64,
                kind: FileKind::Regular,
            };
            let publication = repository.create_commit_with_blobs(
                Changeset {
                    parents: vec![parent],
                    author: "Benchmark <benchmark@example.invalid>".into(),
                    committer: None,
                    timestamp_ms: 1_700_000_000_000 + sequence as i64 * 1000,
                    message: format!("edit {sequence}"),
                    changes: BTreeMap::from([(path, FileChange::put(entry))]),
                    metadata: BTreeMap::new(),
                },
                vec![blob],
                Some(CommitRefUpdate {
                    name: "bench".into(),
                    expected: Some(parent),
                }),
            )?;
            if publication.reference_updated != Some(true) {
                return Err(scion_types::Error::Conflict("benchmark ref changed".into()));
            }
            Ok(json!(publication.changeset.id()?))
        }
        Request::Chain {
            parent,
            count,
            label,
        } => {
            let mut tip = parent;
            for sequence in 0..count {
                tip = create(
                    repository,
                    Some(tip),
                    BTreeMap::new(),
                    format!("{label} {sequence}"),
                    sequence as u64 + 1,
                )?;
            }
            if !repository.update_ref(&label, None, Some(tip))? {
                return Err(scion_types::Error::Conflict(
                    "benchmark branch exists".into(),
                ));
            }
            Ok(json!(tip))
        }
        Request::Ancestry {
            ancestor,
            descendant,
        } => Ok(json!(repository.is_ancestor(ancestor, descendant)?)),
        Request::MergeBases { left, right } => Ok(json!(repository.merge_bases(left, right)?)),
        Request::Diff { base, target } => Ok(json!(repository.diff(base, target)?)),
        Request::Read { revision, path } => Ok(json!(
            String::from_utf8(repository.read_file(revision, &path)?)
                .map_err(|error| scion_types::Error::Corrupt(error.to_string()))?
        )),
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments.as_slice() == ["--version"] {
        println!("scion-comparison-driver {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let directory = arguments
        .first()
        .ok_or("usage: comparison_driver STORE [--once] [--read-only]")?;
    if arguments
        .iter()
        .skip(1)
        .any(|argument| argument != "--once" && argument != "--read-only")
    {
        return Err("unknown comparison driver option".into());
    }
    let once = arguments.iter().any(|argument| argument == "--once");
    let read_only = arguments.iter().any(|argument| argument == "--read-only");
    let store = if read_only && once {
        // One logical query keeps a consistent WAL view and reuses its read
        // transaction. Resident workers retain live visibility between requests.
        LocalStore::open_read_snapshot(directory)?
    } else if read_only {
        LocalStore::open_read_only(directory)?
    } else {
        LocalStore::open(directory)?
    };
    let repository = Repository::new("benchmark", Arc::new(store))?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if !once {
        writeln!(output, "{{\"ready\":true}}")?;
        output.flush()?;
    }
    let mut input = io::stdin().lock();
    let mut line = String::new();
    let mut response_bytes = Vec::with_capacity(4096);
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        let request: Request = serde_json::from_str(&line)?;
        let start = Instant::now();
        let result = execute(&repository, request);
        let elapsed = start.elapsed().as_nanos();
        let response = match result {
            Ok(result) => Response::Success {
                ok: true,
                result,
                operation_ns: elapsed,
            },
            Err(error) => Response::Failure {
                ok: false,
                error: error.to_string(),
                operation_ns: elapsed,
            },
        };
        // Deliver one complete response per write. Streaming JSON fragments
        // through line-buffered stdout can wake the caller before the terminating
        // newline, adding avoidable pipe reads and scheduler round trips. The
        // request/ack timer still includes serialization, every byte and flushing.
        response_bytes.clear();
        serde_json::to_writer(&mut response_bytes, &response)?;
        response_bytes.push(b'\n');
        output.write_all(&response_bytes)?;
        output.flush()?;
        if once {
            break;
        }
    }
    Ok(())
}
