#![no_main]

use libfuzzer_sys::fuzz_target;
use scion_store::{decode_object, encode_object};
use scion_types::{ObjectId, ObjectKind};

fuzz_target!(|input: &[u8]| {
    if input.len() > 65_536 {
        return;
    }
    // Every input exercises a valid compressed object and a corrupt object.
    let id = ObjectId::hash(ObjectKind::Blob, input);
    let encoded = encode_object(ObjectKind::Blob, input).unwrap();
    assert_eq!(
        decode_object(ObjectKind::Blob, id, &encoded).unwrap(),
        input
    );
    assert!(decode_object(ObjectKind::Tree, id, &encoded).is_err());
    let mut incorrect_id = id;
    incorrect_id.0[0] ^= 1;
    assert!(decode_object(ObjectKind::Blob, incorrect_id, &encoded).is_err());
    // Also exercise arbitrary envelope headers and compressed streams directly.
    if input.len() >= 49 {
        let mut bytes = [0; 32];
        bytes.copy_from_slice(&input[17..49]);
        if let Ok(decoded) = decode_object(ObjectKind::Blob, ObjectId(bytes), input) {
            assert_eq!(ObjectId::hash(ObjectKind::Blob, &decoded), ObjectId(bytes));
        }
    } else {
        assert!(decode_object(ObjectKind::Blob, id, input).is_err());
    }
});
