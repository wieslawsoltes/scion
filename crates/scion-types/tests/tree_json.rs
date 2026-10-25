//! The stored-tree fast path must preserve the public derived Serde contract.
use scion_types::{ObjectId, ObjectKind, RepoPath, Tree, canonical};

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
