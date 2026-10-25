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
    if let Some(entry) = CanonicalLookup::lookup(bytes, name) {
        return Ok(entry);
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = de::DeserializeSeed::deserialize(Lookup(name), &mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

/// Recognize the fixed field layout emitted by `canonical(Tree)` directly. The
/// complete input, including every unused sibling, is checked before returning.
/// Any unsupported syntax takes the ordinary streaming Serde path below.
struct CanonicalLookup<'a> {
    remaining: &'a [u8],
}

impl<'a> CanonicalLookup<'a> {
    fn lookup(bytes: &'a [u8], wanted: &str) -> Option<Option<TreeEntry>> {
        let mut parser = Self { remaining: bytes };
        parser.expect(b"{\"entries\":{")?;
        let mut found = None;
        if parser.consume(b"}}") {
            return parser.remaining.is_empty().then_some(None);
        }
        loop {
            let name = parser.name()?;
            validate_path(name).ok()?;
            if name.is_empty() || name.contains('/') {
                return None;
            }
            let selected = name == wanted;
            parser.expect(b":")?;
            if parser.consume(b"{\"type\":\"directory\",\"id\":\"") {
                let id = parser.identifier(selected)?;
                parser.expect(b"\"}")?;
                if let Some(id) = id {
                    found = Some(TreeEntry::Directory { id });
                }
            } else if parser.consume(b"{\"type\":\"file\",\"entry\":{\"id\":\"") {
                let id = parser.identifier(selected)?;
                parser.expect(b"\",\"size\":")?;
                let size = parser.size(selected)?;
                parser.expect(b",\"kind\":\"")?;
                let kind = if parser.consume(b"regular\"}}") {
                    FileKind::Regular
                } else if parser.consume(b"executable\"}}") {
                    FileKind::Executable
                } else if parser.consume(b"symlink\"}}") {
                    FileKind::Symlink
                } else if parser.consume(b"submodule\"}}") {
                    FileKind::Submodule
                } else {
                    return None;
                };
                if let Some(id) = id {
                    found = Some(TreeEntry::File {
                        entry: FileEntry { id, size, kind },
                    });
                }
            } else {
                return None;
            }
            if parser.consume(b"}}") {
                return parser.remaining.is_empty().then_some(found);
            }
            parser.expect(b",")?;
        }
    }

    fn consume(&mut self, expected: &[u8]) -> bool {
        if let Some(remaining) = self.remaining.strip_prefix(expected) {
            self.remaining = remaining;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: &[u8]) -> Option<()> {
        self.consume(expected).then_some(())
    }

    fn name(&mut self) -> Option<&'a str> {
        self.expect(b"\"")?;
        let end = self.remaining.iter().position(|byte| *byte == b'"')?;
        let raw = &self.remaining[..end];
        // Escaped JSON names are handled by Serde. All other unescaped bytes
        // must form valid UTF-8 and exclude JSON control characters.
        if raw.iter().any(|byte| *byte == b'\\' || *byte < 0x20) {
            return None;
        }
        let name = std::str::from_utf8(raw).ok()?;
        self.remaining = &self.remaining[end + 1..];
        Some(name)
    }

    fn identifier(&mut self, selected: bool) -> Option<Option<ObjectId>> {
        let raw = self.remaining.get(..64)?;
        // A full reduction checks all digits without constructing an unused ID.
        // It also permits the optimizer to check independent bytes in parallel.
        let invalid = raw.iter().fold(0u8, |invalid, byte| {
            invalid | u8::from(!byte.is_ascii_hexdigit())
        });
        if invalid != 0 {
            return None;
        }
        self.remaining = &self.remaining[64..];
        if selected {
            Some(Some(std::str::from_utf8(raw).ok()?.parse().ok()?))
        } else {
            Some(None)
        }
    }

    fn size(&mut self, selected: bool) -> Option<u64> {
        let end = self
            .remaining
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        let raw = self.remaining.get(..end)?;
        if raw.is_empty()
            || (raw.len() > 1 && raw[0] == b'0')
            || raw.len() > 20
            || (raw.len() == 20 && raw > b"18446744073709551615")
        {
            return None;
        }
        self.remaining = &self.remaining[end..];
        if selected {
            Some(
                raw.iter()
                    .fold(0, |value, digit| value * 10 + u64::from(digit - b'0')),
            )
        } else {
            Some(0)
        }
    }
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
        for name in ["file", "folder", "missing"] {
            assert_eq!(
                CanonicalLookup::lookup(&canonical(&original).unwrap(), name),
                Some(original.entries.get(name).cloned()),
            );
        }
        assert_eq!(
            CanonicalLookup::lookup(b"{\"entries\":{}}", "missing"),
            Some(None)
        );
    }
}
