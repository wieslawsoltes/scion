use scion_types::{Error, ObjectId, ObjectKind, Result};
use std::{cell::RefCell, io::Read};

const MAGIC: &[u8; 8] = b"SCNOBJ01";
pub(crate) const HEADER: usize = 8 + 1 + 8 + 32;
/// Per-object decompression ceiling, checked before allocating payload memory.
pub const MAX_OBJECT_SIZE: usize = 256 * 1024 * 1024;

// Keep the common small-object path independent of the streaming codec's input
// and output buffers. Each thread reuses its own contexts: calls never serialize
// behind a global codec lock. Large inputs retain the bounded streaming path and
// cannot enlarge a context that will then be held for the thread's lifetime.
const BULK_LIMIT: usize = 1024 * 1024;
#[derive(Default)]
struct Codecs {
    encoder: Option<zstd::bulk::Compressor<'static>>,
    decoder: Option<zstd::bulk::Decompressor<'static>>,
}
thread_local! {
    static CODECS: RefCell<Codecs> = const { RefCell::new(Codecs { encoder: None, decoder: None }) };
}

fn tag(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Blob => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Changeset => 3,
        ObjectKind::Derived => 4,
    }
}

/// Encode the same compressed representation used on disk and over the wire.
pub fn encode_object(kind: ObjectKind, bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() > MAX_OBJECT_SIZE {
        return Err(Error::Limit("object exceeds 256 MiB".into()));
    }
    let id = ObjectId::hash(kind, bytes);
    let mut encoded = if bytes.len() <= BULK_LIMIT {
        CODECS.with(|codecs| -> Result<Vec<u8>> {
            let mut codecs = codecs.borrow_mut();
            if codecs.encoder.is_none() {
                codecs.encoder = Some(zstd::bulk::Compressor::new(3)?);
            }
            let mut encoded =
                Vec::with_capacity(HEADER + zstd::zstd_safe::compress_bound(bytes.len()));
            codecs
                .encoder
                .as_mut()
                .expect("initialized encoder")
                .compress_to_buffer(bytes, &mut encoded)?;
            // Reserve the header in the same allocation and move only compressed
            // bytes; zero-initializing a worst-case output buffer is unnecessary.
            let length = encoded.len();
            encoded.resize(length + HEADER, 0);
            encoded.copy_within(..length, HEADER);
            Ok(encoded)
        })?
    } else {
        let mut encoded = Vec::with_capacity(HEADER + BULK_LIMIT);
        encoded.resize(HEADER, 0);
        zstd::stream::copy_encode(bytes, &mut encoded, 3)?;
        encoded
    };
    encoded[..8].copy_from_slice(MAGIC);
    encoded[8] = tag(kind);
    encoded[9..17].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
    encoded[17..49].copy_from_slice(&id.0);
    Ok(encoded)
}

/// Validate an envelope prefix without allocating or decoding its payload.
/// `encoded_length` is the complete representation length, not prefix length.
pub(crate) fn object_size(
    kind: ObjectKind,
    id: ObjectId,
    prefix: &[u8],
    encoded_length: u64,
) -> Result<u64> {
    if prefix.len() < HEADER
        || encoded_length <= HEADER as u64
        || &prefix[..8] != MAGIC
        || prefix[8] != tag(kind)
        || prefix[17..49] != id.0
    {
        return Err(Error::Corrupt("invalid immutable object envelope".into()));
    }
    let size = u64::from_le_bytes(
        prefix[9..17]
            .try_into()
            .map_err(|_| Error::Corrupt("short object header".into()))?,
    );
    if size > MAX_OBJECT_SIZE as u64 || encoded_length > (MAX_OBJECT_SIZE + 1_048_576) as u64 {
        return Err(Error::Limit(
            "encoded or decoded object exceeds size limit".into(),
        ));
    }
    Ok(size)
}

// Bound both input and output for speculative small-object reads. Valid unusual
// representations (for example huge skippable frames) fall back to normal reads.
pub(crate) fn small_object_encoded_limit(max_decoded_bytes: usize) -> usize {
    (HEADER + zstd::zstd_safe::compress_bound(max_decoded_bytes.min(MAX_OBJECT_SIZE)))
        .min(MAX_OBJECT_SIZE + 1_048_576)
}

pub(crate) fn decode_small_object(
    kind: ObjectKind,
    id: ObjectId,
    encoded: &[u8],
    max_decoded_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    let size = object_size(kind, id, encoded, encoded.len() as u64)?;
    if size > max_decoded_bytes as u64
        || encoded.len() > small_object_encoded_limit(max_decoded_bytes)
    {
        return Ok(None);
    }
    Ok(Some(decode_object(kind, id, encoded)?))
}

/// Validate version, domain, expected ID, decompression bounds and content hash.
pub fn decode_object(kind: ObjectKind, id: ObjectId, encoded: &[u8]) -> Result<Vec<u8>> {
    let size = object_size(kind, id, encoded, encoded.len() as u64)?;
    let decoded = if size <= BULK_LIMIT as u64 {
        CODECS.with(|codecs| -> Result<Vec<u8>> {
            let mut codecs = codecs.borrow_mut();
            if codecs.decoder.is_none() {
                codecs.decoder = Some(zstd::bulk::Decompressor::new()?);
            }
            Ok(codecs
                .decoder
                .as_mut()
                .expect("initialized decoder")
                .decompress(&encoded[HEADER..], size as usize)?)
        })?
    } else {
        // Do not trust an advertised large size enough to allocate it eagerly.
        // Streaming retains the original incremental-allocation and size bound.
        let decoder = zstd::stream::read::Decoder::new(&encoded[HEADER..])?;
        let mut decoded = Vec::with_capacity(BULK_LIMIT);
        decoder.take(size + 1).read_to_end(&mut decoded)?;
        decoded
    };
    if decoded.len() as u64 != size || ObjectId::hash(kind, &decoded) != id {
        return Err(Error::Corrupt(format!("object integrity mismatch: {id}")));
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verifies_domain_id_length_and_compression() {
        let data = b"hello hello hello";
        let id = ObjectId::hash(ObjectKind::Blob, data);
        let encoded = encode_object(ObjectKind::Blob, data).unwrap();
        assert_eq!(decode_object(ObjectKind::Blob, id, &encoded).unwrap(), data);
        assert!(decode_object(ObjectKind::Tree, id, &encoded).is_err());
        assert!(decode_object(ObjectKind::Blob, ObjectId([0; 32]), &encoded).is_err());
        let mut bad = encoded.clone();
        bad[9..17].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode_object(ObjectKind::Blob, id, &bad).is_err());
        for end in 0..encoded.len() {
            assert!(decode_object(ObjectKind::Blob, id, &encoded[..end]).is_err());
        }
    }
    fn stream_encoded(kind: ObjectKind, bytes: &[u8]) -> Vec<u8> {
        let mut encoded = encode_object(kind, bytes).unwrap()[..HEADER].to_vec();
        zstd::stream::copy_encode(bytes, &mut encoded, 3).unwrap();
        encoded
    }

    fn stream_decoded(encoded: &[u8], limit: usize) -> std::io::Result<Vec<u8>> {
        let mut decoded = Vec::new();
        zstd::stream::read::Decoder::new(&encoded[HEADER..])?
            .take(limit as u64 + 1)
            .read_to_end(&mut decoded)?;
        Ok(decoded)
    }

    #[test]
    fn bulk_and_streaming_frames_interoperate_across_size_boundary() {
        for length in [0, 1, 64, 1024, 65_537, BULK_LIMIT, BULK_LIMIT + 1] {
            let data: Vec<_> = (0..length)
                .map(|i| (i.wrapping_mul(17) ^ (i >> 9)) as u8)
                .collect();
            let id = ObjectId::hash(ObjectKind::Blob, &data);
            let old = stream_encoded(ObjectKind::Blob, &data);
            let new = encode_object(ObjectKind::Blob, &data).unwrap();
            assert_eq!(decode_object(ObjectKind::Blob, id, &old).unwrap(), data);
            assert_eq!(decode_object(ObjectKind::Blob, id, &new).unwrap(), data);
            assert_eq!(stream_decoded(&new, data.len()).unwrap(), data);
        }
    }

    #[test]
    fn concatenated_frames_and_skippable_frames_remain_supported() {
        let data = b"first frame and second frame";
        let id = ObjectId::hash(ObjectKind::Blob, data);
        let mut encoded = encode_object(ObjectKind::Blob, data).unwrap()[..HEADER].to_vec();
        zstd::stream::copy_encode(&data[..10], &mut encoded, 3).unwrap();
        // A standard zero-length Zstd skippable frame between data frames.
        encoded.extend_from_slice(&0x184d2a50_u32.to_le_bytes());
        encoded.extend_from_slice(&0u32.to_le_bytes());
        zstd::stream::copy_encode(&data[10..], &mut encoded, 3).unwrap();
        assert_eq!(stream_decoded(&encoded, data.len()).unwrap(), data);
        assert_eq!(decode_object(ObjectKind::Blob, id, &encoded).unwrap(), data);
        encoded.push(0xff);
        assert!(stream_decoded(&encoded, data.len()).is_err());
        assert!(decode_object(ObjectKind::Blob, id, &encoded).is_err());
    }

    #[test]
    fn reused_codec_recovers_after_truncation_corruption_and_wrong_lengths() {
        let data = b"verified codec contents".repeat(50);
        let id = ObjectId::hash(ObjectKind::Tree, &data);
        let encoded = encode_object(ObjectKind::Tree, &data).unwrap();
        for index in HEADER..encoded.len() {
            let mut corrupt = encoded.clone();
            corrupt[index] ^= 0xa5;
            let old = stream_decoded(&corrupt, data.len());
            let expected = old.as_ref().is_ok_and(|decoded| decoded == &data);
            assert_eq!(
                decode_object(ObjectKind::Tree, id, &corrupt).is_ok(),
                expected
            );
            assert_eq!(decode_object(ObjectKind::Tree, id, &encoded).unwrap(), data);
        }
        for length in [0, data.len() - 1, data.len() + 1, BULK_LIMIT + 1] {
            let mut corrupt = encoded.clone();
            corrupt[9..17].copy_from_slice(&(length as u64).to_le_bytes());
            assert!(decode_object(ObjectKind::Tree, id, &corrupt).is_err());
            assert_eq!(decode_object(ObjectKind::Tree, id, &encoded).unwrap(), data);
        }
    }
    #[test]
    fn empty_objects_require_complete_valid_compression_frames() {
        let id = ObjectId::hash(ObjectKind::Blob, b"");
        let encoded = encode_object(ObjectKind::Blob, b"").unwrap();
        for end in HEADER..encoded.len() {
            let old = stream_decoded(&encoded[..end], 0).is_ok();
            assert_eq!(
                decode_object(ObjectKind::Blob, id, &encoded[..end]).is_ok(),
                old,
                "prefix {end}"
            );
        }
    }
}
