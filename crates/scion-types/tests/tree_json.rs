//! The stored-tree fast path must preserve the public derived Serde contract.
use scion_types::{
    FileEntry, FileKind, ObjectId, ObjectKind, RepoPath, Tree, TreeEntry, canonical,
};
use std::collections::BTreeMap;

fn compare_encoding(tree: &Tree) {
    let expected = canonical(tree).unwrap();
    let actual = tree.canonical_bytes().unwrap();
    assert_eq!(actual, expected);
    assert_eq!(Tree::from_json(&actual).unwrap(), *tree);
    assert_eq!(serde_json::from_slice::<Tree>(&actual).unwrap(), *tree);
    assert_eq!(
        ObjectId::hash(ObjectKind::Tree, &actual),
        ObjectId::hash(ObjectKind::Tree, &expected)
    );
}

#[test]
fn canonical_tree_writer_preserves_names_modes_identifiers_and_integer_boundaries() {
    assert_eq!(
        Tree::default().canonical_bytes().unwrap(),
        br#"{"entries":{}}"#
    );
    let mut names: Vec<String> = (0u8..=127)
        .map(|byte| format!("before{}after", char::from(byte)))
        .collect();
    names.extend([
        "".into(),
        "quote\"slash\\newline\n\r\ttab".into(),
        "日本語/é/☃/🦀".into(),
        "e\u{301}".into(),
        "\u{80}\u{7ff}\u{800}\u{d7ff}\u{e000}\u{ffff}\u{10000}\u{10ffff}".into(),
        "long".repeat(4096),
    ]);
    let modes = [
        FileKind::Regular,
        FileKind::Executable,
        FileKind::Symlink,
        FileKind::Submodule,
    ];
    let mut sizes = vec![0, u64::MAX, i64::MAX as u64, i64::MAX as u64 + 1];
    for power in 0..=19 {
        let boundary = 10u64.pow(power);
        sizes.extend([boundary - 1, boundary, boundary + 1]);
    }
    for byte in 0u8..=255 {
        let id = ObjectId([byte; 32]);
        for (mode_index, kind) in modes.into_iter().enumerate() {
            let entries = names
                .iter()
                .enumerate()
                .rev()
                .map(|(index, name)| {
                    let entry = if index % 7 == 0 {
                        TreeEntry::Directory { id }
                    } else {
                        TreeEntry::File {
                            entry: FileEntry {
                                id,
                                size: sizes[(index + byte as usize + mode_index) % sizes.len()],
                                kind,
                            },
                        }
                    };
                    (name.clone(), entry)
                })
                .collect();
            // Canonical serialization also accepts unvalidated Tree values;
            // escaping must match even for names that paths would reject.
            compare_encoding(&Tree { entries });
        }
    }
}

#[test]
fn canonical_tree_writer_matches_serde_for_generated_and_wide_trees() {
    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }
    let mut state = 0x6373_696f_6e74_7265;
    for case in 0..256 {
        let mut entries = BTreeMap::new();
        let count = if case == 255 {
            10_000
        } else {
            (next(&mut state) % 96) as usize
        };
        for index in 0..count {
            let mut name = format!("{index:08}-");
            for _ in 0..(next(&mut state) % 12) {
                let value = (next(&mut state) % 0x11_0000) as u32;
                name.push(char::from_u32(value).unwrap_or('\0'));
            }
            let id = ObjectId(std::array::from_fn(|_| next(&mut state) as u8));
            let entry = match next(&mut state) % 5 {
                0 => TreeEntry::Directory { id },
                mode => TreeEntry::File {
                    entry: FileEntry {
                        id,
                        size: next(&mut state),
                        kind: [
                            FileKind::Regular,
                            FileKind::Executable,
                            FileKind::Symlink,
                            FileKind::Submodule,
                        ][mode as usize - 1],
                    },
                },
            };
            entries.insert(name, entry);
        }
        compare_encoding(&Tree { entries });
    }
}

fn compare(input: &str) {
    let reference = serde_json::from_str::<Tree>(input);
    let optimized = Tree::from_json(input.as_bytes());
    for name in ["item", "missing"] {
        let lookup = Tree::entry_from_json(input.as_bytes(), name);
        match reference
            .as_ref()
            .ok()
            .filter(|tree| tree.validate().is_ok())
        {
            Some(tree) => assert_eq!(lookup.unwrap(), tree.entries.get(name).cloned(), "{input}"),
            None => assert!(lookup.is_err(), "lookup accepted an invalid tree: {input}"),
        }
    }
    match (reference, optimized) {
        (Ok(reference), Ok(optimized)) => {
            assert_eq!(
                canonical(&reference).unwrap(),
                canonical(&optimized).unwrap(),
                "{input}"
            );
            assert_eq!(
                reference.validate().is_ok(),
                optimized.validate().is_ok(),
                "{input}"
            );
        }
        (Err(_), Err(_)) => {}
        (reference, optimized) => panic!(
            "decoder disagreement for {input}: reference {reference:?}, optimized {optimized:?}"
        ),
    }
}

fn tree(entry: &str) -> String {
    format!("{{\"entries\":{{\"item\":{entry}}}}}")
}

#[test]
fn streaming_tree_decoder_matches_derived_for_field_orders_and_noncanonical_shapes() {
    let id = format!("\"{}\"", ObjectId::hash(ObjectKind::Blob, b"fixture"));
    let file = format!("{{\"id\":{id},\"size\":17,\"kind\":\"regular\"}}");
    let fields = [
        "\"type\":\"file\"".to_owned(),
        "\"type\":\"directory\"".to_owned(),
        "\"type\":null".to_owned(),
        "\"type\":0".to_owned(),
        "\"type\":{\"file\":null}".to_owned(),
        format!("\"entry\":{file}"),
        "\"entry\":null".to_owned(),
        "\"entry\":{}".to_owned(),
        format!("\"entry\":[{id},17,\"regular\"]"),
        format!("\"id\":{id}"),
        "\"id\":null".to_owned(),
        "\"id\":\"invalid\"".to_owned(),
        "\"id\":1e999".to_owned(),
        "\"unknown\":[1,{\"nested\":null}]".to_owned(),
        "\"unknown\":1e999".to_owned(),
    ];
    // Includes duplicate relevant and irrelevant keys, missing fields, and all
    // tag/payload orders. Compare normalized values, not merely acceptance.
    for first in &fields {
        compare(&tree(&format!("{{{first}}}")));
        for second in &fields {
            compare(&tree(&format!("{{{first},{second}}}")));
            for third in &fields {
                compare(&tree(&format!("{{{first},{second},{third}}}")));
            }
        }
    }
    for entry in [
        "null".to_owned(),
        "[]".to_owned(),
        format!("[\"file\",{file}]"),
        format!("[\"directory\",{id}]"),
        format!("[\"file\",{file},null]"),
        format!("[\"directory\",null,{id}]"),
        format!("{{\"type\":\"file\",\"entry\":{file}}}"),
        format!("{{\"type\":\"directory\",\"id\":{id}}}"),
    ] {
        compare(&tree(&entry));
        compare(&format!("[{{\"item\":{entry}}}]"));
        compare(&format!(
            "{{\"entries\":{{\"item\":{entry},\"item\":{{\"type\":\"directory\",\"id\":{id}}}}}}}"
        ));
    }
    for input in [
        "null",
        "{}",
        "[]",
        "[{}]",
        "{\"entries\":{},\"entries\":{}}",
        "{\"entries\":{},\"unknown\":1e999}",
    ] {
        compare(input);
    }
}

#[test]
fn streaming_tree_decoder_preserves_nested_file_payload_validation() {
    let id = format!("\"{}\"", ObjectId::hash(ObjectKind::Blob, b"fixture"));
    let deep = format!("{}0{}", "[".repeat(160), "]".repeat(160));
    let values = [
        "null",
        "0",
        "17",
        "-1",
        "1.0",
        "1e999",
        "18446744073709551615",
        "18446744073709551616",
        "true",
        "[]",
        "{}",
        "\"regular\"",
        "{\"regular\":null}",
        "{\"regular\":[]}",
        "{\"regular\":null,\"x\":null}",
        "\"directory\"",
        "\"executable\"",
        "\"symlink\"",
        "\"submodule\"",
        &id,
        &deep,
    ];
    for value in values {
        for file in [
            format!("{{\"id\":{value},\"size\":17,\"kind\":\"regular\"}}"),
            format!("{{\"id\":{id},\"size\":{value},\"kind\":\"regular\"}}"),
            format!("{{\"id\":{id},\"size\":17,\"kind\":{value}}}"),
            format!("{{\"id\":{id},\"size\":17,\"kind\":\"regular\",\"unknown\":{value}}}"),
            format!("{{\"id\":{id},\"size\":17,\"kind\":\"regular\",\"size\":{value}}}"),
        ] {
            compare(&tree(&format!("{{\"type\":\"file\",\"entry\":{file}}}")));
            compare(&tree(&format!("{{\"entry\":{file},\"type\":\"file\"}}")));
            compare(&tree(&format!(
                "{{\"type\":\"directory\",\"id\":{id},\"entry\":{file}}}"
            )));
        }
        compare(&tree(&format!(
            "{{\"type\":\"directory\",\"id\":{id},\"extra\":{value}}}"
        )));
    }
}

#[test]
fn borrowed_tree_name_validation_matches_repository_paths() {
    let id = ObjectId::hash(ObjectKind::Tree, b"fixture");
    let mut names = vec![
        "".to_owned(),
        "a".into(),
        "/a".into(),
        "a/".into(),
        "a/b".into(),
        "a//b".into(),
        "../a".into(),
        ".".into(),
        "..".into(),
        "a\\b".into(),
        "C:x".into(),
        ".git".into(),
        ".ScIoN".into(),
        "CON".into(),
        "con.txt".into(),
        "COM1".into(),
        "com0".into(),
        "lPt9.txt".into(),
        "ConIn$".into(),
        "CONOUT$".into(),
        "你好".into(),
        "éé".into(),
        "☃x".into(),
        "a.".into(),
        "a ".into(),
        "nul\0x".into(),
        "a".repeat(255),
        "a".repeat(256),
    ];
    for stem in ["con", "prn", "aux", "nul", "com", "lpt", "a", "é"] {
        for number in 0..=10 {
            names.push(format!("{stem}{number}.txt"));
        }
    }
    for name in names {
        let expected = RepoPath::new(name.clone())
            .is_ok_and(|path| !path.is_root() && path.components().count() == 1);
        let tree = Tree {
            entries: [(name.clone(), scion_types::TreeEntry::Directory { id })].into(),
        };
        assert_eq!(expected, tree.validate().is_ok(), "{name:?}");
        let encoded = canonical(&tree).unwrap();
        assert_eq!(
            expected,
            Tree::entry_from_json(&encoded, &name).is_ok(),
            "{name:?}"
        );
        assert_eq!(
            expected,
            Tree::entry_from_json(&encoded, "absent").is_ok(),
            "{name:?}"
        );
    }
}

#[test]
fn point_lookup_checks_siblings_after_match_and_escaped_names() {
    let id = ObjectId::hash(ObjectKind::Tree, b"fixture");
    let entry = format!(r#"{{"type":"directory","id":"{id}"}}"#);
    for name in ["item", "é", "quote\"name", "☃"] {
        let key = serde_json::to_string(name).unwrap();
        let input = format!(r#"{{"entries":{{{key}:{entry},"tail":{entry}}}}}"#);
        compare(&input);
        assert_eq!(
            Tree::entry_from_json(input.as_bytes(), name).unwrap(),
            Some(scion_types::TreeEntry::Directory { id })
        );
        let invalid = format!(
            r#"{{"entries":{{{key}:{entry},"tail":{{"type":"directory","id":"invalid"}}}}}}"#
        );
        assert!(Tree::entry_from_json(invalid.as_bytes(), name).is_err());
    }
}

#[test]
fn canonical_point_parser_matches_complete_validation_under_byte_mutations() {
    use scion_types::{FileEntry, FileKind, TreeEntry};
    let id = ObjectId::hash(ObjectKind::Blob, b"point-lookup oracle");
    let tree = Tree {
        entries: [
            ("directory".into(), TreeEntry::Directory { id }),
            (
                "item".into(),
                TreeEntry::File {
                    entry: FileEntry {
                        id,
                        size: u64::MAX,
                        kind: FileKind::Regular,
                    },
                },
            ),
            (
                "last".into(),
                TreeEntry::File {
                    entry: FileEntry {
                        id,
                        size: 0,
                        kind: FileKind::Submodule,
                    },
                },
            ),
        ]
        .into(),
    };
    let original = canonical(&tree).unwrap();
    let check = |input: &[u8]| {
        let reference = serde_json::from_slice::<Tree>(input)
            .ok()
            .filter(|tree| tree.validate().is_ok());
        for name in ["directory", "item", "last", "missing"] {
            let actual = Tree::entry_from_json(input, name);
            match &reference {
                Some(tree) => assert_eq!(
                    actual.unwrap(),
                    tree.entries.get(name).cloned(),
                    "{input:?}"
                ),
                None => assert!(actual.is_err(), "accepted malformed tree: {input:?}"),
            }
        }
    };
    check(&original);
    for position in 0..original.len() {
        check(&original[..position]);
        let mut input = original.clone();
        for byte in [
            b' ', b'0', b'9', b'a', b'g', b'{', b'}', b'[', b']', b'"', b'\\', b',', b':', 0, 0xff,
        ] {
            input[position] = byte;
            check(&input);
        }
    }
    for size in [
        "0",
        "1",
        "01",
        "-1",
        "18446744073709551615",
        "18446744073709551616",
        "1.0",
        "1e2",
        "1e999",
    ] {
        let input = String::from_utf8(original.clone())
            .unwrap()
            .replace("18446744073709551615", size);
        check(input.as_bytes());
    }
}
