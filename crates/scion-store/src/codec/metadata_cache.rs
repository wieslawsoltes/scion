//! Bounded reuse of standalone Zstd frames for related metadata encodes.
//!
//! This cache only accepts caller-supplied raw bytes. It never serves object
//! reads, membership, or publication acknowledgments. Hash keys select possible
//! matches; complete raw equality is required before compressed bytes are reused.

use super::{HEADER, prepend_header};
use std::{io, mem::size_of};

pub(super) const CHUNK_BYTES: usize = 8 * 1024;
pub(super) const MIN_INPUT_BYTES: usize = 64 * 1024;
pub(super) const MAX_INPUT_BYTES: usize = 256 * 1024;
const CACHE_BYTES: usize = 1024 * 1024;
const MAX_FRAMES: usize = 64;
// Account separately for the actual index allocation, every raw/compressed Vec
// capacity, and conservative allocator/control overhead. This is not an RSS cap.
const FRAME_OVERHEAD: usize = 512;
const CONTROL_OVERHEAD: usize = 128;

struct Frame {
    key: [u8; 32],
    raw: Vec<u8>,
    encoded: Vec<u8>,
    stamp: u64,
}
impl Frame {
    fn buffer_bytes(&self) -> usize {
        self.raw.capacity() + self.encoded.capacity() + FRAME_OVERHEAD
    }
}

pub(super) struct MetadataCache {
    frames: Vec<Frame>,
    previous: Vec<u8>,
    frame_bytes: usize,
    clock: u64,
}
impl Default for MetadataCache {
    fn default() -> Self {
        Self::new()
    }
}
impl MetadataCache {
    pub(super) const fn new() -> Self {
        Self {
            frames: Vec::new(),
            previous: Vec::new(),
            frame_bytes: 0,
            clock: 0,
        }
    }

    pub(super) fn clear(&mut self) {
        *self = Self::new();
    }

    fn eligible(length: usize) -> bool {
        (MIN_INPUT_BYTES..=MAX_INPUT_BYTES).contains(&length)
    }

    fn worthwhile(encoded: usize, raw: usize) -> bool {
        encoded.saturating_mul(8) >= raw && encoded.saturating_mul(8) <= raw.saturating_mul(7)
    }

    fn similar(&self, bytes: &[u8]) -> bool {
        if self.previous.is_empty() {
            return false;
        }
        let reused: usize = bytes
            .chunks(CHUNK_BYTES)
            .zip(self.previous.chunks(CHUNK_BYTES))
            .filter(|(current, old)| current == old)
            .map(|(chunk, _)| chunk.len())
            .sum();
        reused * 4 >= bytes.len() * 3
    }

    fn accounted_bytes(&self) -> usize {
        let allocated = self.frame_bytes
            + self.previous.capacity()
            + self.frames.capacity() * size_of::<Frame>();
        if allocated == 0 {
            0
        } else {
            allocated + CONTROL_OVERHEAD
        }
    }

    #[cfg(test)]
    pub(super) fn retained_test_bytes(&self) -> usize {
        self.accounted_bytes()
    }

    fn evict_oldest(&mut self) {
        let index = self
            .frames
            .iter()
            .enumerate()
            .min_by_key(|(_, frame)| frame.stamp)
            .map(|(index, _)| index)
            .expect("cache has an eviction candidate");
        self.frame_bytes -= self.frames.swap_remove(index).buffer_bytes();
    }

    fn remember(&mut self, bytes: &[u8]) {
        let capacity = self.previous.capacity().max(bytes.len());
        while self.frame_bytes
            + capacity
            + self.frames.capacity() * size_of::<Frame>()
            + CONTROL_OVERHEAD
            > CACHE_BYTES
        {
            self.evict_oldest();
        }
        // Never let Vec growth double this buffer beyond the eligibility cap.
        if self.previous.capacity() < bytes.len() {
            self.previous = bytes.to_vec();
        } else {
            self.previous.clear();
            self.previous.extend_from_slice(bytes);
        }
        debug_assert!(self.previous.capacity() <= MAX_INPUT_BYTES);
        debug_assert!(self.accounted_bytes() <= CACHE_BYTES);
    }

    fn next_stamp(&mut self) -> u64 {
        if self.clock == u64::MAX {
            self.clear();
        }
        self.clock += 1;
        self.clock
    }

    fn append(&mut self, key: [u8; 32], raw: &[u8], output: &mut Vec<u8>) -> bool {
        let stamp = self.next_stamp();
        if let Some(frame) = self
            .frames
            .iter_mut()
            .find(|frame| frame.key == key && frame.raw.as_slice() == raw)
        {
            frame.stamp = stamp;
            output.extend_from_slice(&frame.encoded);
            true
        } else {
            false
        }
    }

    fn insert(&mut self, key: [u8; 32], raw: &[u8], encoded: Vec<u8>) {
        let stamp = self.next_stamp();
        let bytes = raw.len() + encoded.capacity() + FRAME_OVERHEAD;
        let index_bytes = self.frames.capacity().max(MAX_FRAMES) * size_of::<Frame>();
        if bytes + self.previous.capacity() + index_bytes + CONTROL_OVERHEAD > CACHE_BYTES {
            return;
        }
        if let Some(index) = self.frames.iter().position(|frame| frame.key == key) {
            // A colliding key is a miss, never permission to reuse other bytes.
            self.frame_bytes -= self.frames.swap_remove(index).buffer_bytes();
        }
        if self.frames.capacity() == 0 {
            self.frames = Vec::with_capacity(MAX_FRAMES);
        }
        while self.accounted_bytes() + bytes > CACHE_BYTES || self.frames.len() == MAX_FRAMES {
            self.evict_oldest();
        }
        let frame = Frame {
            key,
            raw: raw.to_vec(),
            encoded,
            stamp,
        };
        self.frame_bytes += frame.buffer_bytes();
        self.frames.push(frame);
        debug_assert!(self.frames.len() <= MAX_FRAMES);
        debug_assert!(self.accounted_bytes() <= CACHE_BYTES);
    }

    // Only completed successful compression can replace the admission base.
    fn observe_whole(&mut self, bytes: &[u8], encoded_size: usize) {
        if Self::worthwhile(encoded_size, bytes.len()) {
            self.frames.clear();
            self.frame_bytes = 0;
            self.remember(bytes);
        } else {
            self.clear();
        }
    }

    /// Return an encoded payload with space reserved for the ordinary envelope.
    /// The callback compresses one standalone frame and must not re-enter CODECS.
    pub(super) fn encode(
        &mut self,
        bytes: &[u8],
        mut compress: impl FnMut(&[u8]) -> io::Result<Vec<u8>>,
    ) -> io::Result<Vec<u8>> {
        if !Self::eligible(bytes.len()) {
            return Ok(prepend_header(compress(bytes)?));
        }
        if !self.similar(bytes) {
            let encoded = compress(bytes)?;
            self.observe_whole(bytes, encoded.len());
            return Ok(prepend_header(encoded));
        }
        // Independent frames need the sum of their individual bounds. A single
        // whole-input compression bound does not cover arbitrary concatenation.
        let capacity = HEADER
            + bytes
                .chunks(CHUNK_BYTES)
                .map(|part| zstd::zstd_safe::compress_bound(part.len()))
                .sum::<usize>();
        let mut encoded = Vec::with_capacity(capacity);
        encoded.resize(HEADER, 0);
        for part in bytes.chunks(CHUNK_BYTES) {
            let key = *blake3::hash(part).as_bytes();
            if !self.append(key, part, &mut encoded) {
                let frame = compress(part)?;
                encoded.extend_from_slice(&frame);
                self.insert(key, part, frame);
            }
        }
        if !Self::worthwhile(encoded.len() - HEADER, bytes.len()) {
            // Even an admitted change may alter compressibility. Keep cheap
            // whole-frame data on that path and update the admission estimate.
            let whole = compress(bytes)?;
            self.observe_whole(bytes, whole.len());
            return Ok(prepend_header(whole));
        }
        self.remember(bytes);
        Ok(encoded)
    }
}

#[cfg(test)]
#[path = "metadata_cache_tests.rs"]
mod tests;
