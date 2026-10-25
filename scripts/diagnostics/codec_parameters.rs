//! Isolated diagnostic: no production changes and no writes to the input store.
use scion_store::{PreparedObject, decode_object};
use scion_types::{Changeset, ObjectId, ObjectKind, Tree};
use serde::Serialize;
use std::{fs, hint::black_box, path::PathBuf, time::Instant};

const HEADER: usize = 49;
const TRIALS: usize = 31;
const ITERATIONS: usize = 32;
const WARMUPS: usize = 4;

#[derive(Clone, Copy, Serialize)]
struct Configuration {
    name: &'static str,
    level: i32,
    target_length: Option<u32>,
}

struct Variant {
    configuration: Configuration,
    encoder: zstd::bulk::Compressor<'static>,
}

#[derive(Serialize)]
struct CorpusEntry {
    name: &'static str,
    kind: ObjectKind,
    id: ObjectId,
    decoded_bytes: usize,
    tree_entries: Option<usize>,
}

struct Case {
    description: CorpusEntry,
    object: PreparedObject,
}

#[derive(Serialize)]
struct Observation {
    trial: usize,
    position: usize,
    configuration: &'static str,
    object: &'static str,
    iterations: usize,
    encoded_bytes: usize,
    encode_total_ns: u64,
    decode_verify_total_ns: u64,
    encode_ns_per_call: Vec<u64>,
    decode_verify_ns_per_call: Vec<u64>,
}

fn encode(
    encoder: &mut zstd::bulk::Compressor<'_>,
    object: &PreparedObject,
) -> std::io::Result<Vec<u8>> {
    let bytes = object.bytes();
    let mut encoded = Vec::with_capacity(HEADER + zstd::zstd_safe::compress_bound(bytes.len()));
    encoder.compress_to_buffer(bytes, &mut encoded)?;
    let length = encoded.len();
    encoded.resize(length + HEADER, 0);
    encoded.copy_within(..length, HEADER);
    encoded[..8].copy_from_slice(b"SCNOBJ01");
    encoded[8] = match object.kind() {
        ObjectKind::Blob => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Changeset => 3,
        ObjectKind::Derived => 4,
    };
    encoded[9..17].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
    encoded[17..49].copy_from_slice(&object.id().0);
    Ok(encoded)
}

fn load_corpus(directory: &str, manifest: &serde_json::Value) -> scion_types::Result<Vec<Case>> {
    [
        ("directory_100", ObjectKind::Tree, Some(100)),
        ("root_1000", ObjectKind::Tree, Some(1000)),
        ("changeset", ObjectKind::Changeset, None),
        ("blob_level3_control", ObjectKind::Blob, None),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (name, kind, tree_entries))| {
        let description = &manifest["cases"][index];
        assert_eq!(description["name"].as_str(), Some(name));
        assert_eq!(description["kind"].as_str(), Some(kind.as_str()));
        let id: ObjectId = description["id"].as_str().unwrap().parse()?;
        let bytes = fs::read(PathBuf::from(directory).join(description["file"].as_str().unwrap()))?;
        if let Some(entries) = tree_entries {
            let tree = Tree::from_json(&bytes)?;
            tree.validate()?;
            assert_eq!(tree.entries.len(), entries);
        }
        if kind == ObjectKind::Changeset {
            let changeset: Changeset = serde_json::from_slice(&bytes)?;
            changeset.validate()?;
        }
        let object = PreparedObject::new(kind, bytes)?;
        assert_eq!(object.id(), id);
        Ok(Case {
            description: CorpusEntry {
                name,
                kind,
                id,
                decoded_bytes: object.bytes().len(),
                tree_entries,
            },
            object,
        })
    })
    .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments.len() != 2 {
        return Err("usage: codec_probe COPIED_CORPUS_DIRECTORY OUTPUT.json".into());
    }
    let manifest: serde_json::Value = serde_json::from_slice(&fs::read(
        PathBuf::from(&arguments[0]).join("manifest.json"),
    )?)?;
    let destination = PathBuf::from(&arguments[1]);
    if destination.exists() {
        return Err("output already exists".into());
    }
    let corpus = load_corpus(&arguments[0], &manifest)?;
    let configurations = [
        Configuration {
            name: "level1",
            level: 1,
            target_length: None,
        },
        Configuration {
            name: "level1_target4",
            level: 1,
            target_length: Some(4),
        },
        Configuration {
            name: "level1_target8",
            level: 1,
            target_length: Some(8),
        },
        Configuration {
            name: "level1_target16",
            level: 1,
            target_length: Some(16),
        },
        Configuration {
            name: "level_minus1",
            level: -1,
            target_length: None,
        },
        Configuration {
            name: "level_minus2",
            level: -2,
            target_length: None,
        },
    ];
    let mut variants = configurations
        .iter()
        .map(|configuration| {
            let mut encoder = zstd::bulk::Compressor::new(configuration.level)?;
            if let Some(target) = configuration.target_length {
                encoder.set_parameter(zstd::zstd_safe::CParameter::TargetLength(target))?;
            }
            Ok::<_, std::io::Error>(Variant {
                configuration: *configuration,
                encoder,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Every candidate has a separate, identically configured blob context.
    // Blob output must stay byte-identical to production level 3.
    let mut blob_encoders = configurations
        .iter()
        .map(|_| zstd::bulk::Compressor::new(3))
        .collect::<Result<Vec<_>, _>>()?;
    let expected_blobs = corpus[3].object.encode()?;
    let mut records = Vec::new();
    let mut orders = Vec::new();
    for trial in 0..WARMUPS + TRIALS {
        // Rotate all positions, and reverse each complete six-trial cycle.
        let count = variants.len();
        let mut order: Vec<_> = (0..count)
            .map(|index| (index + trial % count) % count)
            .collect();
        if (trial / count) % 2 == 1 {
            order.reverse();
        }
        if trial >= WARMUPS {
            orders.push(
                order
                    .iter()
                    .map(|index| configurations[*index].name)
                    .collect::<Vec<_>>(),
            );
        }
        for (position, index) in order.into_iter().enumerate() {
            let variant = &mut variants[index];
            let mut observations: Vec<_> = corpus
                .iter()
                .map(|case| Observation {
                    trial: trial.saturating_sub(WARMUPS),
                    position,
                    configuration: variant.configuration.name,
                    object: case.description.name,
                    iterations: ITERATIONS,
                    encoded_bytes: 0,
                    encode_total_ns: 0,
                    decode_verify_total_ns: 0,
                    encode_ns_per_call: Vec::with_capacity(ITERATIONS),
                    decode_verify_ns_per_call: Vec::with_capacity(ITERATIONS),
                })
                .collect();
            for _ in 0..ITERATIONS {
                // Match production publication order: edited child, root,
                // changeset, then staged blob. Checks occur outside timers.
                for (case, observation) in corpus.iter().zip(&mut observations) {
                    let encoder = if case.object.kind() == ObjectKind::Blob {
                        &mut blob_encoders[index]
                    } else {
                        &mut variant.encoder
                    };
                    let start = Instant::now();
                    let encoded = encode(encoder, black_box(&case.object))?;
                    let encode_ns = start.elapsed().as_nanos() as u64;
                    let start = Instant::now();
                    let decoded =
                        decode_object(case.object.kind(), case.object.id(), black_box(&encoded))?;
                    let decode_ns = start.elapsed().as_nanos() as u64;
                    assert_eq!(decoded, case.object.bytes());
                    if case.object.kind() == ObjectKind::Blob {
                        assert_eq!(encoded, expected_blobs);
                    }
                    if observation.encoded_bytes != 0 {
                        assert_eq!(observation.encoded_bytes, encoded.len());
                    }
                    observation.encoded_bytes = encoded.len();
                    observation.encode_total_ns += encode_ns;
                    observation.decode_verify_total_ns += decode_ns;
                    observation.encode_ns_per_call.push(encode_ns);
                    observation.decode_verify_ns_per_call.push(decode_ns);
                    black_box(decoded);
                    black_box(encoded);
                }
            }
            if trial >= WARMUPS {
                records.extend(observations);
            }
        }
    }
    let report = serde_json::json!({
        "schema": 1,
        "scope": "codec-only fixed production corpus; not end-to-end prepared commit latency",
        "source_commit": "991fdc9c36190104019840110c69a689edc9e5a1",
        "input_corpus": arguments[0], "input_manifest": manifest,
        "zstd_version": zstd::zstd_safe::version_string(),
        "configurations": configurations,
        "corpus": corpus.iter().map(|case| &case.description).collect::<Vec<_>>(),
        "warmup_trials": WARMUPS, "trials": TRIALS, "iterations_per_trial": ITERATIONS,
        "order": "rotate positions every trial, reverse each six-trial cycle",
        "execution_orders": orders,
        "encode_timing": "same Vec reserve/compress/shift/header logic as bulk PreparedObject::encode; IDs computed once before measurement",
        "decode_timing": "production decode_object including envelope, allocation, decompression, full size and content-hash verification",
        "verification": "all 26880 representations decoded successfully and matched exact canonical bytes/IDs; blob output byte-identical at level 3",
        "results": records,
    });
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    serde_json::to_writer_pretty(file, &report)?;
    Ok(())
}
