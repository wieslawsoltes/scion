//! Allocation-light decoder for the shape emitted by `canonical(Tree)`.
//!
//! An internally tagged enum normally buffers all fields before deserializing
//! the selected variant. Here the canonical fields stream directly into their
//! final types. Unknown or incompatible shapes are rejected so `Tree::from_json`
//! can use the existing derived decoder, preserving its compatibility contract.

use crate::{FileEntry, FileKind, ObjectId, TreeEntry, validate_path};
use serde::{Deserialize, Deserializer, de};
use std::{borrow::Cow, collections::BTreeMap, fmt};

/// Stream every entry and validate every name, retaining only the requested
/// value. Unlike `IgnoredAny`, this still checks ignored sibling payloads.
pub(crate) fn lookup(bytes: &[u8], name: &str) -> serde_json::Result<Option<TreeEntry>> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = de::DeserializeSeed::deserialize(Lookup(name), &mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

struct Name<'a>(Cow<'a, str>);

impl<'de> Deserialize<'de> for Name<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NameVisitor;
        impl<'de> de::Visitor<'de> for NameVisitor {
            type Value = Name<'de>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a tree entry name")
            }

            fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
                Ok(Name(Cow::Borrowed(value)))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(Name(Cow::Owned(value.to_owned())))
            }
        }
        deserializer.deserialize_str(NameVisitor)
    }
}

struct Lookup<'a>(&'a str);

impl<'de> de::DeserializeSeed<'de> for Lookup<'_> {
    type Value = Option<TreeEntry>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> de::Visitor<'de> for Lookup<'_> {
    type Value = Option<TreeEntry>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a canonical tree object")
    }

    fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        let Some(Name(key)) = map.next_key()? else {
            return Err(de::Error::missing_field("entries"));
        };
        if key != "entries" {
            return Err(de::Error::custom("noncanonical tree field"));
        }
        let result = map.next_value_seed(EntriesLookup(self.0))?;
        if map.next_key::<Name<'de>>()?.is_some() {
            return Err(de::Error::custom("noncanonical tree field"));
        }
        Ok(result)
    }
}

struct EntriesLookup<'a>(&'a str);

impl<'de> de::DeserializeSeed<'de> for EntriesLookup<'_> {
    type Value = Option<TreeEntry>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> de::Visitor<'de> for EntriesLookup<'_> {
    type Value = Option<TreeEntry>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a map of tree entries")
    }

    fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        let mut result = None;
        while let Some((Name(name), entry)) = map.next_entry::<Name<'de>, DecodedEntry>()? {
            validate_path(&name).map_err(de::Error::custom)?;
            if name.is_empty() || name.contains('/') {
                return Err(de::Error::custom(
                    "tree entries must be single path components",
                ));
            }
            if name == self.0 {
                // BTreeMap deserialization keeps the last duplicate.
                result = Some(entry.0);
            }
        }
        Ok(result)
    }
}

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
        // The legacy internally tagged enum also supports positional arrays.
        // Their shape differs from this helper struct, so use the original
        // decoder for them and keep this fast path restricted to objects.
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
