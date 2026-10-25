use super::*;

fn moderate(length: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            b"0123456789abcdef"[(state & 15) as usize]
        })
        .collect()
}

fn compress(bytes: &[u8]) -> io::Result<Vec<u8>> {
    zstd::bulk::compress(bytes, 1)
}

fn verify(bytes: &[u8], encoded: &[u8]) {
    assert_eq!(
        zstd::bulk::decompress(&encoded[HEADER..], bytes.len()).unwrap(),
        bytes
    );
    let legacy = zstd::stream::decode_all(&encoded[HEADER..]).unwrap();
    assert_eq!(legacy, bytes);
}

#[test]
fn admission_requires_eligible_size_ratio_and_byte_weighted_reuse_including_tails() {
    assert!(!MetadataCache::eligible(MIN_INPUT_BYTES - 1));
    assert!(MetadataCache::eligible(MIN_INPUT_BYTES));
    assert!(MetadataCache::eligible(MAX_INPUT_BYTES));
    assert!(!MetadataCache::eligible(MAX_INPUT_BYTES + 1));
    assert!(!MetadataCache::worthwhile(99, 800));
    assert!(MetadataCache::worthwhile(100, 800));
    assert!(MetadataCache::worthwhile(700, 800));
    assert!(!MetadataCache::worthwhile(701, 800));

    let original = moderate(MIN_INPUT_BYTES, 41);
    let mut cache = MetadataCache::new();
    cache.remember(&original);
    let mut changed = original.clone();
    changed[0] ^= 1;
    changed[CHUNK_BYTES] ^= 1;
    assert!(
        cache.similar(&changed),
        "exactly 75% of bytes can be reused"
    );
    changed[2 * CHUNK_BYTES] ^= 1;
    assert!(!cache.similar(&changed));

    let original = moderate(MIN_INPUT_BYTES + 1, 42);
    cache.remember(&original);
    let mut changed = original.clone();
    changed[0] ^= 1;
    changed[CHUNK_BYTES] ^= 1;
    assert!(cache.similar(&changed));
    *changed.last_mut().unwrap() ^= 1;
    assert!(
        !cache.similar(&changed),
        "a one-byte tail crosses the exact threshold"
    );
}

#[test]
fn cold_whole_frames_admit_related_changes_then_reuse_only_exact_chunks() {
    let mut cache = MetadataCache::new();
    let original = moderate(MIN_INPUT_BYTES + CHUNK_BYTES + 123, 51);
    let mut calls = Vec::new();
    let cold = cache
        .encode(&original, |bytes| {
            calls.push(bytes.len());
            compress(bytes)
        })
        .unwrap();
    assert_eq!(calls, [original.len()]);
    assert!(cache.frames.is_empty());
    assert_eq!(cache.previous, original);
    verify(&original, &cold);

    let mut next = original.clone();
    next[7] ^= 1;
    calls.clear();
    let admitted = cache
        .encode(&next, |bytes| {
            calls.push(bytes.len());
            compress(bytes)
        })
        .unwrap();
    assert_eq!(calls.len(), next.len().div_ceil(CHUNK_BYTES));
    assert_eq!(calls.last(), Some(&123));
    assert_eq!(cache.previous, next);
    verify(&next, &admitted);

    let mut last = next.clone();
    *last.last_mut().unwrap() ^= 1;
    calls.clear();
    let cached = cache
        .encode(&last, |bytes| {
            calls.push(bytes.len());
            compress(bytes)
        })
        .unwrap();
    assert_eq!(
        calls,
        [123],
        "only the changed short final frame is compressed"
    );
    verify(&last, &cached);
    assert_eq!(cache.previous, last);

    let unrelated = moderate(original.len(), 9_001);
    calls.clear();
    let whole = cache
        .encode(&unrelated, |bytes| {
            calls.push(bytes.len());
            compress(bytes)
        })
        .unwrap();
    assert_eq!(calls, [unrelated.len()]);
    assert!(cache.frames.is_empty());
    assert_eq!(cache.previous, unrelated);
    verify(&unrelated, &whole);
}

#[test]
fn cheap_whole_frames_do_not_retain_raw_data_and_ineligible_sizes_keep_the_base() {
    let mut cache = MetadataCache::new();
    let raw = moderate(MIN_INPUT_BYTES, 31);
    cache.encode(&raw, compress).unwrap();
    cache.encode(&raw, compress).unwrap();
    let previous = cache.previous.clone();
    let frames = cache.frames.len();
    for bytes in [vec![b'x'; 1], moderate(MAX_INPUT_BYTES + 1, 72)] {
        let encoded = cache.encode(&bytes, compress).unwrap();
        verify(&bytes, &encoded);
        assert_eq!(cache.previous, previous);
        assert_eq!(cache.frames.len(), frames);
    }
    let repeated = vec![0; MAX_INPUT_BYTES];
    let encoded = cache.encode(&repeated, compress).unwrap();
    verify(&repeated, &encoded);
    assert_eq!(cache.accounted_bytes(), 0);

    let mut random = Vec::with_capacity(MAX_INPUT_BYTES);
    let mut state = 0xd250_a44f_b8ee_1547u64;
    for _ in 0..MAX_INPUT_BYTES {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        random.push(state as u8);
    }
    for _ in 0..3 {
        let encoded = cache.encode(&random, compress).unwrap();
        verify(&random, &encoded);
        assert_eq!(cache.accounted_bytes(), 0);
    }
}

#[test]
fn collisions_require_complete_raw_equality_and_replace_with_correct_accounting() {
    let mut cache = MetadataCache::new();
    let key = [7; 32];
    cache.insert(key, b"first", b"compressed first".to_vec());
    let mut output = Vec::new();
    assert!(!cache.append(key, b"second", &mut output));
    assert!(output.is_empty());
    assert!(cache.append(key, b"first", &mut output));
    assert_eq!(output, b"compressed first");
    cache.insert(key, b"second", b"different frame".to_vec());
    assert_eq!(cache.frames.len(), 1);
    assert_eq!(cache.frame_bytes, cache.frames[0].buffer_bytes());
    output.clear();
    assert!(!cache.append(key, b"first", &mut output));
    assert!(cache.append(key, b"second", &mut output));
    assert_eq!(output, b"different frame");
}

fn key(number: u64) -> [u8; 32] {
    let mut key = [0; 32];
    key[..8].copy_from_slice(&number.to_le_bytes());
    key
}

#[test]
fn entry_and_allocation_bounds_lru_clear_and_clock_overflow_are_explicit() {
    let mut cache = MetadataCache::new();
    for number in 0..MAX_FRAMES as u64 {
        cache.insert(key(number), b"x", b"frame".to_vec());
    }
    assert_eq!(cache.frames.len(), MAX_FRAMES);
    assert_eq!(cache.frames.capacity(), MAX_FRAMES);
    assert!(cache.append(key(0), b"x", &mut Vec::new()));
    cache.insert(key(MAX_FRAMES as u64), b"x", b"frame".to_vec());
    assert!(cache.frames.iter().any(|frame| frame.key == key(0)));
    assert!(!cache.frames.iter().any(|frame| frame.key == key(1)));
    assert_eq!(
        cache.frame_bytes,
        cache.frames.iter().map(Frame::buffer_bytes).sum::<usize>()
    );

    cache.remember(&vec![b'p'; MAX_INPUT_BYTES]);
    for number in 100..300 {
        let mut encoded = Vec::with_capacity(CHUNK_BYTES * 2);
        encoded.push(1);
        cache.insert(key(number), &vec![b'a'; CHUNK_BYTES], encoded);
        assert!(cache.accounted_bytes() <= CACHE_BYTES);
        assert!(cache.frames.len() <= MAX_FRAMES);
        assert_eq!(
            cache.frame_bytes,
            cache.frames.iter().map(Frame::buffer_bytes).sum::<usize>()
        );
    }
    assert_eq!(cache.previous.capacity(), MAX_INPUT_BYTES);
    assert!(
        cache.frames.len() < MAX_FRAMES,
        "byte budget also evicts entries"
    );
    cache.remember(&vec![b'p'; MIN_INPUT_BYTES]);
    assert_eq!(cache.previous.capacity(), MAX_INPUT_BYTES);
    assert!(
        cache.accounted_bytes() <= CACHE_BYTES,
        "retained capacity counts after shortening"
    );

    cache.clear();
    assert_eq!(cache.accounted_bytes(), 0);
    assert_eq!(cache.frames.capacity(), 0);
    cache.insert(key(4), b"x", b"frame".to_vec());
    cache.clock = u64::MAX;
    assert!(!cache.append(key(4), b"x", &mut Vec::new()));
    assert_eq!(cache.accounted_bytes(), 0);
    cache.clock = u64::MAX;
    cache.insert(key(5), b"x", b"frame".to_vec());
    assert_eq!(cache.frames.len(), 1);
    assert_eq!(cache.frames[0].key, key(5));
    assert!(cache.accounted_bytes() <= CACHE_BYTES);
}

#[test]
fn encoding_errors_preserve_the_previous_base_and_only_complete_valid_frames() {
    let mut cache = MetadataCache::new();
    let original = moderate(MIN_INPUT_BYTES + 2 * CHUNK_BYTES, 51);
    assert!(
        cache
            .encode(&original, |_| Err(io::Error::other("whole-frame failure")))
            .is_err()
    );
    assert_eq!(cache.accounted_bytes(), 0);
    cache.encode(&original, compress).unwrap();
    cache.encode(&original, compress).unwrap();
    let mut changed = original.clone();
    changed[0] ^= 1;
    changed[CHUNK_BYTES] ^= 1;
    let mut calls = 0;
    assert!(
        cache
            .encode(&changed, |bytes| {
                calls += 1;
                if calls == 2 {
                    Err(io::Error::other("second-frame failure"))
                } else {
                    compress(bytes)
                }
            })
            .is_err()
    );
    assert_eq!(calls, 2);
    assert_eq!(cache.previous, original);
    for frame in &cache.frames {
        assert_eq!(
            zstd::bulk::decompress(&frame.encoded, frame.raw.len()).unwrap(),
            frame.raw
        );
    }
    let retry = cache.encode(&changed, compress).unwrap();
    verify(&changed, &retry);
    assert_eq!(cache.previous, changed);

    let unrelated = moderate(original.len(), 771);
    let old_count = cache.frames.len();
    assert!(
        cache
            .encode(&unrelated, |_| Err(io::Error::other("unrelated failure")))
            .is_err()
    );
    assert_eq!(cache.previous, changed);
    assert_eq!(cache.frames.len(), old_count);
}

#[test]
fn a_failed_ratio_fallback_does_not_publish_a_new_admission_base() {
    let mut cache = MetadataCache::new();
    let original = moderate(MIN_INPUT_BYTES, 91);
    cache.encode(&original, compress).unwrap();
    let mut changed = original.clone();
    changed[0] ^= 1;
    let mut fallback = false;
    assert!(
        cache
            .encode(&changed, |bytes| {
                if bytes.len() == changed.len() {
                    fallback = true;
                    Err(io::Error::other("whole fallback failed"))
                } else {
                    // Valid standalone frames with little/no match searching are large
                    // enough to force the ordinary whole-frame fallback here.
                    zstd::bulk::compress(bytes, -1000)
                }
            })
            .is_err()
    );
    assert!(fallback);
    assert_eq!(cache.previous, original);
    for frame in &cache.frames {
        assert_eq!(
            zstd::bulk::decompress(&frame.encoded, frame.raw.len()).unwrap(),
            frame.raw
        );
    }
    let retry = cache.encode(&changed, compress).unwrap();
    verify(&changed, &retry);
    assert_eq!(cache.previous, changed);
    assert!(
        cache.frames.is_empty(),
        "valid large frames cause a checked whole-frame fallback"
    );
}
