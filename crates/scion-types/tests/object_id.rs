//! Independent radix parsing is an oracle for the allocation-free ID decoder.
use scion_types::{ObjectId, ObjectKind};

fn reference(text: &str) -> Option<ObjectId> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut bytes = [0; 32];
    for (index, pair) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        if !pair.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        bytes[index] = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).ok()?;
    }
    Some(ObjectId(bytes))
}

fn compare(text: &str) {
    assert_eq!(text.parse::<ObjectId>().ok(), reference(text), "{text:?}");
    let json = serde_json::to_vec(text).unwrap();
    assert_eq!(
        serde_json::from_slice::<ObjectId>(&json).ok(),
        reference(text),
        "{text:?}"
    );
}

#[test]
fn every_ascii_byte_and_position_matches_independent_hex_parsing() {
    let original =
        ObjectId::hash(ObjectKind::Blob, b"hexadecimal differential fixture").to_string();
    compare(&original);
    compare(&original.to_ascii_uppercase());
    for position in 0..64 {
        let mut text = original.as_bytes().to_vec();
        for byte in 0..=127 {
            text[position] = byte;
            compare(std::str::from_utf8(&text).unwrap());
        }
    }
    // Every pair of accepted hexadecimal characters covers byte reconstruction
    // and mixed case independently of the surrounding valid identifier.
    for high in b"0123456789abcdefABCDEF" {
        for low in b"0123456789abcdefABCDEF" {
            let mut text = original.as_bytes().to_vec();
            text[30] = *high;
            text[31] = *low;
            compare(std::str::from_utf8(&text).unwrap());
        }
    }
}

#[test]
fn unicode_and_length_boundaries_never_alias_ascii_hexadecimal() {
    for length in 0..=128 {
        compare(&"a".repeat(length));
    }
    for scalar in ["é", "λ", "ÿ", "Ā", "☃", "１", "Ａ", "𝟙", "🦀", "\u{10ffff}"] {
        for offset in 0..=64 - scalar.len() {
            // Exactly 64 UTF-8 bytes exercises the table's non-ASCII rejection,
            // independently of the outer identifier length check.
            compare(&format!(
                "{}{}{}",
                "a".repeat(offset),
                scalar,
                "b".repeat(64 - scalar.len() - offset)
            ));
        }
    }
    for invalid in ["+1", "-1", " 0", "0 ", "0x", "0X", "_0"] {
        compare(&format!("{invalid}{}", "0".repeat(62)));
    }
}

#[test]
fn every_raw_json_byte_preserves_string_decoder_acceptance() {
    let original = ObjectId::hash(ObjectKind::Blob, b"raw JSON byte fixture").to_string();
    let encoded = serde_json::to_vec(&original).unwrap();
    for position in 1..=64 {
        let mut input = encoded.clone();
        for byte in 0..=255 {
            input[position] = byte;
            let expected = serde_json::from_slice::<String>(&input)
                .ok()
                .and_then(|text| reference(&text));
            assert_eq!(
                serde_json::from_slice::<ObjectId>(&input).ok(),
                expected,
                "byte {byte:#04x} at position {position}"
            );
        }
    }
}
