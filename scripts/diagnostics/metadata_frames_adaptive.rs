//! Isolated diagnostic only: bounded compression-frame reuse, no production edits.
use scion_store::{PreparedObject, decode_object};
use scion_types::{Changeset, ObjectId, ObjectKind, Tree};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    hint::black_box,
    io::{self, Read},
    path::Path,
    time::Instant,
};

const HEADER: usize = 49;
const BULK_LIMIT: usize = 1024 * 1024;
const CHUNK_MAX_INPUT: usize = 256 * 1024;
const CACHE_BYTES: usize = 1024 * 1024;
const CACHE_ENTRIES: usize = 256;
const ENTRY_OVERHEAD: usize = 512;
const INDEX_RESERVE: usize = 4096;
const PREVIOUS_OVERHEAD: usize = 128;
const TRIALS: usize = 11;
const COLD_SAMPLES: usize = 9;
const DECODE_SAMPLES: usize = 9;

#[derive(Clone, Copy)]
struct Config {
    minimum: usize,
    chunk: usize,
}
impl Config {
    fn name(self) -> String {
        if self.chunk == 0 {
            "level1_baseline".into()
        } else {
            format!("chunk{}k_min{}k", self.chunk / 1024, self.minimum / 1024)
        }
    }
}

struct Frame {
    raw: Vec<u8>,
    encoded: Vec<u8>,
    stamp: u64,
    bytes: usize,
}
#[derive(Default)]
struct Cache {
    frames: BTreeMap<ObjectId, Frame>,
    lru: BTreeSet<(u64, ObjectId)>,
    bytes: usize,
    previous: Vec<u8>,
    peak: usize,
    clock: u64,
    hits: usize,
    misses: usize,
    evictions: usize,
}
impl Cache {
    fn clear(&mut self) {
        *self = Self::default();
    }
    fn clear_frames(&mut self) {
        self.frames.clear();
        self.lru.clear();
        self.bytes = 0;
    }
    fn clear_candidate(&mut self) {
        self.clear_frames();
        self.previous = Vec::new();
    }
    fn retained_bytes(&self) -> usize {
        self.bytes
            + self.previous.capacity()
            + if self.previous.capacity() == 0 {
                0
            } else {
                PREVIOUS_OVERHEAD
            }
            + self.index_bytes()
    }
    fn index_bytes(&self) -> usize {
        if self.frames.is_empty() {
            0
        } else {
            INDEX_RESERVE
        }
    }
    fn check_bound(&mut self) {
        self.peak = self.peak.max(self.retained_bytes());
        assert!(self.retained_bytes() <= CACHE_BYTES);
        assert!(self.frames.len() + usize::from(!self.previous.is_empty()) <= CACHE_ENTRIES);
        assert!(self.previous.capacity() <= CHUNK_MAX_INPUT);
    }
    fn evict_one(&mut self) {
        let (_, victim) = self.lru.pop_first().expect("nonempty bounded cache");
        self.bytes -= self
            .frames
            .remove(&victim)
            .expect("indexed cache entry")
            .bytes;
        self.evictions += 1;
    }
    fn remember(&mut self, raw: &[u8]) {
        let capacity = self.previous.capacity().max(raw.len());
        while self.bytes + capacity + PREVIOUS_OVERHEAD + self.index_bytes() > CACHE_BYTES
            || self.frames.len() + 1 > CACHE_ENTRIES
        {
            self.evict_one();
        }
        if self.previous.capacity() < raw.len() {
            self.previous = raw.to_vec();
        } else {
            self.previous.clear();
            self.previous.extend_from_slice(raw);
        }
        self.check_bound();
    }
    fn similar(&self, raw: &[u8], chunk: usize) -> bool {
        if self.previous.is_empty() {
            return false;
        }
        let reused: usize = raw
            .chunks(chunk)
            .zip(self.previous.chunks(chunk))
            .filter(|(new, old)| new == old)
            .map(|(part, _)| part.len())
            .sum();
        reused * 4 >= raw.len() * 3
    }
    fn stamp(&mut self) -> u64 {
        self.clock = match self.clock.checked_add(1) {
            Some(next) => next,
            None => {
                self.clear();
                1
            }
        };
        self.clock
    }
    fn append(&mut self, key: ObjectId, raw: &[u8], output: &mut Vec<u8>) -> bool {
        let stamp = self.stamp();
        if let Some(frame) = self.frames.get_mut(&key)
            && frame.raw.as_slice() == raw
        {
            self.lru.remove(&(frame.stamp, key));
            frame.stamp = stamp;
            self.lru.insert((stamp, key));
            output.extend_from_slice(&frame.encoded);
            self.hits += 1;
            true
        } else {
            self.misses += 1;
            false
        }
    }
    fn insert(&mut self, key: ObjectId, raw: &[u8], encoded: Vec<u8>) {
        let raw = raw.to_vec();
        let bytes = raw.capacity() + encoded.capacity() + ENTRY_OVERHEAD;
        if bytes + self.previous.capacity() + PREVIOUS_OVERHEAD + INDEX_RESERVE > CACHE_BYTES {
            return;
        }
        if let Some(old) = self.frames.remove(&key) {
            self.lru.remove(&(old.stamp, key));
            self.bytes -= old.bytes;
        }
        while self.retained_bytes() + bytes + INDEX_RESERVE - self.index_bytes() > CACHE_BYTES
            || self.frames.len() + usize::from(!self.previous.is_empty()) == CACHE_ENTRIES
        {
            self.evict_one();
        }
        let stamp = self.stamp();
        self.frames.insert(
            key,
            Frame {
                raw,
                encoded,
                stamp,
                bytes,
            },
        );
        self.lru.insert((stamp, key));
        self.bytes += bytes;
        self.check_bound();
    }
}

struct Encoder {
    config: Config,
    metadata: zstd::bulk::Compressor<'static>,
    blob: zstd::bulk::Compressor<'static>,
    cache: Cache,
}
impl Encoder {
    fn new(config: Config) -> io::Result<Self> {
        let mut metadata = zstd::bulk::Compressor::new(1)?;
        let mut blob = zstd::bulk::Compressor::new(3)?;
        // The diagnostic's cold condition means cold frame cache, with both
        // codec contexts primed equally, as after repository initialization.
        metadata.compress(b"prime the reusable metadata context")?;
        blob.compress(b"prime the reusable blob context")?;
        Ok(Self {
            config,
            metadata,
            blob,
            cache: Cache::default(),
        })
    }
    fn eligible(&self, case: &Case) -> bool {
        if self.config.chunk == 0
            || case.kind == ObjectKind::Blob
            || case.bytes.len() < self.config.minimum
            || case.bytes.len() > CHUNK_MAX_INPUT
        {
            return false;
        }
        let chunks = case.bytes.len().div_ceil(self.config.chunk);
        chunks + 1 <= CACHE_ENTRIES
            && case.bytes.len()
                + PREVIOUS_OVERHEAD
                + INDEX_RESERVE
                + case
                    .bytes
                    .chunks(self.config.chunk)
                    .map(|part| {
                        part.len() + zstd::zstd_safe::compress_bound(part.len()) + ENTRY_OVERHEAD
                    })
                    .sum::<usize>()
                <= CACHE_BYTES
    }
    fn worthwhile(encoded: usize, raw: usize) -> bool {
        encoded * 8 >= raw && encoded * 8 <= raw * 7
    }
    fn whole(&mut self, case: &Case) -> io::Result<Vec<u8>> {
        let encoder = if case.kind == ObjectKind::Blob {
            &mut self.blob
        } else {
            &mut self.metadata
        };
        let mut output =
            Vec::with_capacity(HEADER + zstd::zstd_safe::compress_bound(case.bytes.len()));
        encoder.compress_to_buffer(&case.bytes, &mut output)?;
        let length = output.len();
        output.resize(length + HEADER, 0);
        output.copy_within(..length, HEADER);
        Ok(output)
    }
    fn encode(&mut self, case: &Case) -> io::Result<Vec<u8>> {
        let eligible = self.eligible(case);
        let mut output = if case.bytes.len() > BULK_LIMIT {
            let mut output = Vec::with_capacity(HEADER + BULK_LIMIT);
            output.resize(HEADER, 0);
            zstd::stream::copy_encode(
                &case.bytes[..],
                &mut output,
                if case.kind == ObjectKind::Blob { 3 } else { 1 },
            )?;
            output
        } else if eligible && self.cache.similar(&case.bytes, self.config.chunk) {
            let capacity = HEADER
                + case
                    .bytes
                    .chunks(self.config.chunk)
                    .map(|part| zstd::zstd_safe::compress_bound(part.len()))
                    .sum::<usize>();
            let mut output = Vec::with_capacity(capacity);
            output.resize(HEADER, 0);
            for part in case.bytes.chunks(self.config.chunk) {
                // A non-authoritative cache key. Full bytes must also match.
                let key = ObjectId(*blake3::hash(part).as_bytes());
                if !self.cache.append(key, part, &mut output) {
                    let encoded = self.metadata.compress(part)?;
                    output.extend_from_slice(&encoded);
                    self.cache.insert(key, part, encoded);
                }
            }
            if Self::worthwhile(output.len() - HEADER, case.bytes.len()) {
                self.cache.remember(&case.bytes);
                output
            } else {
                let output = self.whole(case)?;
                self.cache.clear_candidate();
                if Self::worthwhile(output.len() - HEADER, case.bytes.len()) {
                    self.cache.remember(&case.bytes);
                }
                output
            }
        } else {
            let output = self.whole(case)?;
            if eligible {
                // Cheap whole-frame data never incurs a retained raw copy.
                self.cache.clear_candidate();
                if Self::worthwhile(output.len() - HEADER, case.bytes.len()) {
                    self.cache.remember(&case.bytes);
                }
            }
            output
        };
        output[..8].copy_from_slice(b"SCNOBJ01");
        output[8] = match case.kind {
            ObjectKind::Blob => 1,
            ObjectKind::Tree => 2,
            ObjectKind::Changeset => 3,
            ObjectKind::Derived => 4,
        };
        output[9..17].copy_from_slice(&(case.bytes.len() as u64).to_le_bytes());
        output[17..49].copy_from_slice(&case.id.0);
        Ok(output)
    }
}

struct Case {
    name: String,
    kind: ObjectKind,
    id: ObjectId,
    bytes: Vec<u8>,
}
#[derive(Deserialize)]
struct Manifest {
    batches: Vec<BatchSpec>,
}
#[derive(Deserialize)]
struct BatchSpec {
    cases: Vec<CaseSpec>,
}
#[derive(Deserialize)]
struct CaseSpec {
    name: String,
    kind: String,
    id: ObjectId,
    file: String,
}

fn load(directory: &Path) -> Result<Vec<Vec<Case>>, Box<dyn std::error::Error>> {
    let manifest: Manifest = serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
    let mut batches = Vec::new();
    for spec in manifest.batches {
        let mut batch = Vec::new();
        for item in spec.cases {
            let kind = match item.kind.as_str() {
                "tree" => ObjectKind::Tree,
                "changeset" => ObjectKind::Changeset,
                "blob" => ObjectKind::Blob,
                _ => return Err("unknown corpus kind".into()),
            };
            let bytes = fs::read(directory.join(item.file))?;
            assert_eq!(ObjectId::hash(kind, &bytes), item.id);
            match kind {
                ObjectKind::Tree => Tree::from_json(&bytes)?.validate()?,
                ObjectKind::Changeset => serde_json::from_slice::<Changeset>(&bytes)?.validate()?,
                _ => {}
            }
            batch.push(Case {
                name: item.name,
                kind,
                id: item.id,
                bytes,
            });
        }
        batches.push(batch);
    }
    assert_eq!(batches.len(), 34);
    Ok(batches)
}

fn verify(case: &Case, encoded: &[u8]) {
    assert_eq!(
        decode_object(case.kind, case.id, encoded).unwrap(),
        case.bytes
    );
    let mut legacy = Vec::new();
    zstd::stream::read::Decoder::new(&encoded[HEADER..])
        .unwrap()
        .take(case.bytes.len() as u64 + 1)
        .read_to_end(&mut legacy)
        .unwrap();
    assert_eq!(legacy, case.bytes);
}
fn encode_batch(encoder: &mut Encoder, cases: &[Case]) -> (u128, Vec<Vec<u8>>) {
    let started = Instant::now();
    let output: Vec<_> = cases
        .iter()
        .map(|case| encoder.encode(black_box(case)).unwrap())
        .collect();
    let elapsed = started.elapsed().as_nanos();
    black_box(&output);
    for (case, bytes) in cases.iter().zip(&output) {
        verify(case, bytes);
    }
    (elapsed, output)
}
fn row(
    phase: &str,
    trial: usize,
    sample: usize,
    encoder: &Encoder,
    elapsed: u128,
    output: &[Vec<u8>],
) -> Value {
    json!({"phase":phase,"trial":trial,"sample":sample,"config":encoder.config.name(),
        "elapsed_ns":elapsed,"encoded_bytes":output.iter().map(Vec::len).sum::<usize>(),
        "cache_bytes":encoder.cache.retained_bytes(),"cache_peak":encoder.cache.peak,"cache_entries":encoder.cache.frames.len(),
        "previous_capacity":encoder.cache.previous.capacity(),
        "cache_hits":encoder.cache.hits,"cache_misses":encoder.cache.misses,"cache_evictions":encoder.cache.evictions})
}

fn controls() -> Vec<Vec<Case>> {
    let mut controls = Vec::new();
    for (name, length, repeated) in [
        ("eligible_random_256k", CHUNK_MAX_INPUT, false),
        ("eligible_repeated_256k", CHUNK_MAX_INPUT, true),
        ("near_bulk_random", BULK_LIMIT - 1, false),
        ("near_bulk_repeated", BULK_LIMIT - 1, true),
        ("streaming_random", BULK_LIMIT + 1, false),
    ] {
        let mut state = 0x45c2_e81a_a52b_9831u64;
        let bytes: Vec<_> = (0..length)
            .map(|index| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                if repeated {
                    b"structured metadata repeated value\n"[index % 35]
                } else {
                    state as u8
                }
            })
            .collect();
        let id = ObjectId::hash(ObjectKind::Derived, &bytes);
        controls.push(vec![Case {
            name: name.into(),
            kind: ObjectKind::Derived,
            id,
            bytes,
        }]);
    }
    // Five unrelated128KiB objects exceed the cache's aggregate exact-byte
    // footprint. Exercise evictions and quantify cold mixed-workload costs.
    let mut pressure = Vec::new();
    for variant in 0..5u64 {
        let mut state = 0x128b_67e9_e36d_f251u64 ^ variant;
        let bytes = (0..128 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect::<Vec<_>>();
        pressure.push(Case {
            name: "mixed_random_pressure".into(),
            kind: ObjectKind::Derived,
            id: ObjectId::hash(ObjectKind::Derived, &bytes),
            bytes,
        });
    }
    controls.push(pressure);
    controls
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let corpus = args.next().ok_or("corpus directory required")?;
    let output = args.next().ok_or("output path required")?;
    let batches = load(Path::new(&corpus))?;
    let configs: Vec<_> = std::iter::once(Config {
        minimum: 0,
        chunk: 0,
    })
    .chain([32 * 1024, 64 * 1024].into_iter().flat_map(|minimum| {
        [4 * 1024, 8 * 1024, 16 * 1024]
            .into_iter()
            .map(move |chunk| Config { minimum, chunk })
    }))
    .collect();
    let mut verified = Vec::new();
    for config in &configs {
        let mut encoder = Encoder::new(*config)?;
        for case in &batches[0] {
            let cold = encoder.encode(case)?;
            let warm = encoder.encode(case)?;
            let repeated = encoder.encode(case)?;
            assert_eq!(
                warm, repeated,
                "admitted encoding changed on an exact repeat"
            );
            verify(case, &cold);
            verify(case, &warm);
            let production = PreparedObject::new(case.kind, case.bytes.clone())?.encode()?;
            if !encoder.eligible(case) {
                assert_eq!(cold, production);
            }
            verified.push(json!({"config":config.name(),"case":case.name,
                "raw_bytes":case.bytes.len(),"cold_encoded_bytes":cold.len(),"admitted_encoded_bytes":warm.len(),
                "production_bytes":production.len(),"id":case.id}));
        }
    }
    let mut rows = Vec::new();
    let controls = controls();
    for trial in 0..TRIALS {
        // Rotate the first configuration and reverse alternate rounds. All
        // trials/configurations are retained; no order or sample is discarded.
        let mut order: Vec<_> = (0..configs.len())
            .map(|step| (trial + step) % configs.len())
            .collect();
        if trial % 2 == 1 {
            order.reverse();
        }
        for index in order {
            let mut encoder = Encoder::new(configs[index])?;
            for sample in 0..COLD_SAMPLES {
                encoder.cache.clear();
                let (elapsed, output) = encode_batch(&mut encoder, &batches[0]);
                rows.push(row(
                    "cold_cache_encode",
                    trial,
                    sample,
                    &encoder,
                    elapsed,
                    &output,
                ));
            }
            encoder.cache.clear();
            for (sample, cases) in batches.iter().enumerate() {
                let (elapsed, output) = encode_batch(&mut encoder, cases);
                rows.push(row(
                    if sample < 3 {
                        "admission_warmup_encode"
                    } else {
                        "successive_edits_encode"
                    },
                    trial,
                    if sample < 3 { sample } else { sample - 3 },
                    &encoder,
                    elapsed,
                    &output,
                ));
            }
            let cases = batches.last().unwrap();
            let (_, encoded) = encode_batch(&mut encoder, cases);
            for sample in 0..DECODE_SAMPLES {
                let started = Instant::now();
                let decoded: Vec<_> = cases
                    .iter()
                    .zip(&encoded)
                    .map(|(case, bytes)| {
                        decode_object(case.kind, case.id, black_box(bytes)).unwrap()
                    })
                    .collect();
                let elapsed = started.elapsed().as_nanos();
                for (case, bytes) in cases.iter().zip(&decoded) {
                    assert_eq!(&case.bytes, bytes);
                }
                rows.push(row(
                    "full_checked_decode",
                    trial,
                    sample,
                    &encoder,
                    elapsed,
                    &encoded,
                ));
            }
            for cases in &controls {
                encoder.cache.clear();
                for sample in 0..4 {
                    let (elapsed, output) = encode_batch(&mut encoder, cases);
                    rows.push(row(
                        &format!(
                            "control_{}_{}",
                            cases[0].name,
                            if sample == 0 { "cold" } else { "repeat" }
                        ),
                        trial,
                        sample,
                        &encoder,
                        elapsed,
                        &output,
                    ));
                }
            }
        }
    }
    let result = json!({"methodology":{"trials":TRIALS,"cold_samples_per_trial":COLD_SAMPLES,
        "successive_edits_per_trial":31,"warmup_edits":3,"decode_samples_per_trial":DECODE_SAMPLES,
        "ordering":"rotate configurations, reverse odd trials", "timing":"codec only; full checks outside encode timings; full decode timing includes length and content hash",
        "cold_definition":"empty frame cache and previous candidate; equally primed reusable codec contexts; clearing outside timed region",
        "maximum_input_bytes":CHUNK_MAX_INPUT,"cache_capacity_bytes":CACHE_BYTES,"cache_entry_limit":CACHE_ENTRIES,
        "retention_accounting":"actual raw and encoded Vec capacities plus512bytes per frame,4096byte index reserve, previous raw capacity+128bytes; shared1MiB budget",
        "admission":"previous eligible metadata must match at least75%bytes at fixed chunk boundaries; previous whole-frame ratio must be in[1/8,7/8]; outside-ratio framed results fall back to whole encoding",
        "chunk_key":"one-shot blake3::hash; exact full-byte equality required on hits",
        "production_codec_modified":false,"diagnostic_dev_dependency":"existing workspace blake3"},"verified_cases":verified,"rows":rows});
    fs::write(output, serde_json::to_vec_pretty(&result)?)?;
    println!(
        "{}",
        json!({"rows":rows.len(),"trials":TRIALS,"verified":true})
    );
    Ok(())
}
