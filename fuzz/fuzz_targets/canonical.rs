#![no_main]

use libfuzzer_sys::fuzz_target;
use scion_types::{canonical, Changeset, ObjectId, ObjectKind, Tree};

fuzz_target!(|input: &[u8]| {
    if let Ok(changeset) = serde_json::from_slice::<Changeset>(input) {
        let bytes = canonical(&changeset).unwrap();
        let roundtrip: Changeset = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(changeset, roundtrip);
        assert_eq!(bytes, canonical(&roundtrip).unwrap());
        assert_eq!(changeset.id().unwrap(), roundtrip.id().unwrap());
        assert_eq!(changeset.validate().is_ok(), roundtrip.validate().is_ok());
    }
    if let Ok(tree) = serde_json::from_slice::<Tree>(input) {
        let bytes = canonical(&tree).unwrap();
        let roundtrip: Tree = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(tree, roundtrip);
        assert_eq!(tree.validate().is_ok(), roundtrip.validate().is_ok());
        assert_ne!(ObjectId::hash(ObjectKind::Tree, &bytes), ObjectId::hash(ObjectKind::Blob, &bytes));
    }
});
