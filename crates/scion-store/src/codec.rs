use scion_types::{Error, ObjectId, ObjectKind, Result};
use std::io::Read;

const MAGIC: &[u8; 8] = b"SCNOBJ01";
const HEADER: usize = 8 + 1 + 8 + 32;
/// Per-object decompression ceiling, checked before allocating payload memory.
pub const MAX_OBJECT_SIZE: usize = 256 * 1024 * 1024;

fn tag(kind: ObjectKind) -> u8 {
    match kind { ObjectKind::Blob => 1, ObjectKind::Tree => 2, ObjectKind::Changeset => 3, ObjectKind::Derived => 4 }
}

/// Encode the same compressed representation used on disk and over the wire.
pub fn encode_object(kind: ObjectKind, bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() > MAX_OBJECT_SIZE { return Err(Error::Limit("object exceeds 256 MiB".into())); }
    let id = ObjectId::hash(kind, bytes);
    let mut encoded = Vec::with_capacity(HEADER + bytes.len().min(1_048_576));
    encoded.extend_from_slice(MAGIC);
    encoded.push(tag(kind));
    encoded.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    encoded.extend_from_slice(&id.0);
    zstd::stream::copy_encode(bytes, &mut encoded, 3)?;
    Ok(encoded)
}

/// Validate version, domain, expected ID, decompression bounds and content hash.
pub fn decode_object(kind: ObjectKind, id: ObjectId, encoded: &[u8]) -> Result<Vec<u8>> {
    if encoded.len() < HEADER || &encoded[..8] != MAGIC || encoded[8] != tag(kind) || encoded[17..49] != id.0 {
        return Err(Error::Corrupt("invalid immutable object envelope".into()));
    }
    let size = u64::from_le_bytes(encoded[9..17].try_into().map_err(|_| Error::Corrupt("short object header".into()))?);
    if size > MAX_OBJECT_SIZE as u64 || encoded.len() > MAX_OBJECT_SIZE + 1_048_576 {
        return Err(Error::Limit("encoded or decoded object exceeds size limit".into()));
    }
    let decoder = zstd::stream::read::Decoder::new(&encoded[HEADER..])?;
    let mut decoded = Vec::with_capacity((size as usize).min(1_048_576));
    decoder.take(size + 1).read_to_end(&mut decoded)?;
    if decoded.len() as u64 != size || ObjectId::hash(kind, &decoded) != id {
        return Err(Error::Corrupt(format!("object integrity mismatch: {id}")));
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn verifies_domain_id_length_and_compression() {
        let data = b"hello hello hello";
        let id = ObjectId::hash(ObjectKind::Blob, data);
        let encoded = encode_object(ObjectKind::Blob, data).unwrap();
        assert_eq!(decode_object(ObjectKind::Blob, id, &encoded).unwrap(), data);
        assert!(decode_object(ObjectKind::Tree, id, &encoded).is_err());
        assert!(decode_object(ObjectKind::Blob, ObjectId([0;32]), &encoded).is_err());
        let mut bad = encoded.clone(); bad[9..17].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode_object(ObjectKind::Blob, id, &bad).is_err());
        for end in 0..encoded.len() { assert!(decode_object(ObjectKind::Blob, id, &encoded[..end]).is_err()); }
    }
}
