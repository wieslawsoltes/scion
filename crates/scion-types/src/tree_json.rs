//! Allocation-light decoder for the shape emitted by `canonical(Tree)`.
//!
//! An internally tagged enum normally buffers all fields before deserializing
//! the selected variant. Here the canonical fields stream directly into their
//! final types. Unknown or incompatible shapes are rejected so `Tree::from_json`
//! can use the existing derived decoder, preserving its compatibility contract.

use crate::{FileEntry, FileKind, ObjectId, TreeEntry};
use serde::{Deserialize, Deserializer, de};
use std::{collections::BTreeMap, fmt};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CanonicalTree {
    #[serde(deserialize_with = "entries")]
    pub entries: BTreeMap<String, TreeEntry>,
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum EntryKind {
    File,
    Directory,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalEntry {
    #[serde(rename = "type")]
    kind: EntryKind,
    entry: Option<CanonicalFile>,
    id: Option<ObjectId>,
}

// Unknown fields must trigger fallback here too: the legacy enum's buffered
// content validates every JSON value, including fields FileEntry then ignores.
// Deserializing a normal FileEntry directly could skip invalid ignored numbers.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalFile {
    id: ObjectId,
    size: u64,
    kind: FileKind,
}

struct DecodedEntry(TreeEntry);

impl<'de> Deserialize<'de> for DecodedEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // The internally tagged enum accepts objects only, even though a
        // derived struct also accepts positional JSON arrays.
        struct EntryVisitor;
        impl<'de> de::Visitor<'de> for EntryVisitor {
            type Value = CanonicalEntry;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a tree entry object")
            }
            fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                CanonicalEntry::deserialize(de::value::MapAccessDeserializer::new(map))
            }
        }
        let entry = deserializer.deserialize_map(EntryVisitor)?;
        match (entry.kind, entry.entry, entry.id) {
            (EntryKind::File, Some(file), None) => Ok(Self(TreeEntry::File {
                entry: FileEntry {
                    id: file.id,
                    size: file.size,
                    kind: file.kind,
                },
            })),
            (EntryKind::Directory, None, Some(id)) => Ok(Self(TreeEntry::Directory { id })),
            _ => Err(de::Error::custom("noncanonical tree entry shape")),
        }
    }
}

fn entries<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, TreeEntry>, D::Error> {
    struct EntriesVisitor;
    impl<'de> de::Visitor<'de> for EntriesVisitor {
        type Value = BTreeMap<String, TreeEntry>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map of tree entries")
        }

        fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut result = BTreeMap::new();
            while let Some((name, entry)) = map.next_entry::<String, DecodedEntry>()? {
                result.insert(name, entry.0);
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(EntriesVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ObjectKind, Tree, canonical};

    #[test]
    fn canonical_file_and_directory_take_the_streaming_path() {
        let id = ObjectId::hash(ObjectKind::Blob, b"contents");
        let original = Tree {
            entries: BTreeMap::from([
                ("folder".into(), TreeEntry::Directory { id }),
                (
                    "file".into(),
                    TreeEntry::File {
                        entry: FileEntry {
                            id,
                            size: 8,
                            kind: FileKind::Executable,
                        },
                    },
                ),
            ]),
        };
        let decoded: CanonicalTree =
            serde_json::from_slice(&canonical(&original).unwrap()).unwrap();
        assert_eq!(decoded.entries, original.entries);
    }
}
