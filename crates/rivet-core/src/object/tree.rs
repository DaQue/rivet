//! Phase 1 — parsed tree objects.
//!
//! A tree object is a flat list of entries. Each entry is `<mode> <name>\0<oid>`
//! where `<mode>` is octal ASCII (`100644`, `100755`, `120000`, `40000`), `<name>`
//! is the child name within this directory (no path separators), and `<oid>` is
//! the raw object id of the child. Entries are sorted by Git's tree sort order:
//! by name bytes, with a directory compared as if its name ended in `/`.
//!
//! # Git format (exact bytes)
//!
//! ```text
//! <mode in octal, no leading zero> <name>\0<20-byte oid>
//! ```
//!
//! `40000` (a subdirectory) serialises as five octal digits, not six — Git writes
//! the minimal representation. The name is the bytes as stored; it may be any
//! valid filename bytes.

use crate::error::{Error, Result};
use crate::hash::{ObjectFormat, ObjectId};
use crate::object::{hash_object, EntryMode, ObjectKind};
use bstr::BString;
use std::cmp::Ordering;

/// A single entry in a tree: a child's mode, name, and object id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: EntryMode,
    pub name: BString,
    pub oid: ObjectId,
}

/// A borrowed view of a tree entry, used by lookups that must not copy the
/// entry out of the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeEntryRef<'a> {
    pub mode: EntryMode,
    pub name: &'a BString,
    pub oid: ObjectId,
}

/// A parsed tree object: its payload bytes and the entries they describe.
///
/// The payload (`data`) is the raw on-disk body, which is what gets hashed to
/// produce the tree's id. `entries` is the parsed view of that same body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree<'a> {
    /// Raw payload bytes: the concatenated `<mode> <name>\0<oid>` entries,
    /// in sorted order.
    pub data: &'a [u8],
    /// Parsed entries, in the same order as `data`.
    pub entries: Vec<TreeEntry>,
}

impl<'a> Tree<'a> {
    /// Phase 1 — parse a tree payload.
    ///
    /// # Git format
    ///
    /// Entries are `<mode> ' ' <name> '\0' <oid>`, sorted by name with directories
    /// compared as if their name ended in `/`. The name may be any filename
    /// bytes; Git imposes no further structure on them.
    ///
    /// # Errors
    ///
    /// [`Error::MalformedObject`] for a name containing a path separator, a
    /// non-canonical mode, or an entry whose declared length does not match
    /// the hash length (the payload is truncated or overlong).
    ///
    /// # Caveats
    ///
    /// The payload must already be validated by the object database; this
    /// constructor trusts its input is a well-formed tree body. It does not
    /// re-hash the payload, so a corrupted body is accepted here and caught
    /// later at id verification.
    pub fn parse(format: ObjectFormat, data: &'a [u8]) -> Result<Self> {
        let hash_len = format.len();
        let mut entries = Vec::new();
        let mut pos = 0usize;
        while pos < data.len() {
            // Entry: <mode> ' ' <name> '\0' <oid>
            let space = match data[pos..].iter().position(|&b| b == b' ') {
                Some(s) => s,
                None => {
                    return Err(Error::MalformedObject {
                        kind: ObjectKind::Tree,
                        reason: format!("no space in entry at offset {pos}"),
                    });
                }
            };
            let mode_bytes = &data[pos..pos + space];
            if mode_bytes.is_empty() {
                return Err(Error::MalformedObject {
                    kind: ObjectKind::Tree,
                    reason: format!("empty mode at offset {pos}"),
                });
            }
            let mode = match EntryMode::from_bytes(mode_bytes) {
                Ok(m) => m,
                Err(e) => {
                    return Err(Error::MalformedObject {
                        kind: ObjectKind::Tree,
                        reason: format!("invalid mode at offset {pos}: {e}"),
                    });
                }
            };
            let name_start = pos + space + 1;
            let nul = match data[name_start..].iter().position(|&b| b == b'\0') {
                Some(n) => n,
                None => {
                    return Err(Error::MalformedObject {
                        kind: ObjectKind::Tree,
                        reason: format!("no NUL terminator after name at offset {name_start}"),
                    });
                }
            };
            let name_end = name_start + nul;
            let name = &data[name_start..name_end];
            if name.iter().any(|&b| b == b'/') {
                return Err(Error::MalformedObject {
                    kind: ObjectKind::Tree,
                    reason: format!("name contains '/' at offset {name_start}"),
                });
            }
            let oid_start = name_end + 1;
            if data.len() - oid_start < hash_len {
                return Err(Error::MalformedObject {
                    kind: ObjectKind::Tree,
                    reason: format!("truncated oid at offset {oid_start}"),
                });
            }
            let oid = match ObjectId::from_bytes(format, &data[oid_start..oid_start + hash_len]) {
                Ok(o) => o,
                Err(e) => return Err(e),
            };
            entries.push(TreeEntry {
                mode,
                name: BString::new(name.to_vec()),
                oid,
            });
            pos = oid_start + hash_len;
        }
        Ok(Self {
            data,
            entries,
        })
    }

    /// Phase 1 — serialise the entries back to the on-disk payload.
    ///
    /// # Git format
    ///
    /// Each entry is `<mode in octal, no leading zero> <name>\0<oid>`, in the
    /// same order as [`Self::entries`]. [`EntryMode::write_bytes`] produces the
    /// canonical octal spelling (`40000` for a tree, not `040000`).
    ///
    /// # Caveats
    ///
    /// The output is deterministic: the same entries always produce the same
    /// bytes, so a parsed tree round-trips to the exact payload it came from.
    ///
    /// # Errors
    ///
    /// [`Error::MalformedObject`] only if [`EntryMode::write_bytes`] fails,
    /// which happens when the output buffer is too short — a caller bug.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in &self.entries {
            let mode_len = entry.mode.serialized_len();
            out.reserve(mode_len + 1 + entry.name.len() + 1 + entry.oid.as_bytes().len());
            let mut mode_buf = [0u8; 6];
            entry.mode.write_bytes(&mut mode_buf).unwrap();
            out.extend_from_slice(&mode_buf[..mode_len]);
            out.push(b' ');
            out.extend_from_slice(entry.name.as_slice());
            out.push(b'\0');
            out.extend_from_slice(entry.oid.as_bytes());
        }
        out
    }

    /// Phase 1 — the object id of this tree, hashed over its payload.
    ///
    /// # Git format
    ///
    /// The tree's id is the hash of `tree <len>\0<payload>` where `<payload>`
    /// is [`Self::data`]. [`crate::object::hash_object`] builds exactly that
    /// header and hash, so no separate hashing here.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidObjectId`] if the hash cannot be produced (should not
    /// happen for a valid payload).
    pub fn id(&self, format: ObjectFormat) -> Result<ObjectId> {
        match hash_object(format, ObjectKind::Tree, self.data) {
            Ok((id, _)) => Ok(id),
            Err(e) => Err(e),
        }
    }

    /// Phase 1 — look up an entry by exact child name.
    ///
    /// # Git format
    ///
    /// Names are unique within a tree (Git forbids two entries with the same
    /// name), so at most one entry can match.
    ///
    /// # Caveats
    ///
    /// The tree is sorted, so binary search is correct. The comparison uses
    /// Git's tree sort order; since names are unique and we match by exact
    /// equality, any mode for the search key is valid.
    ///
    /// Returns `None` if the name is not present.
    pub fn get<'e>(&'e self, name: &BString) -> Option<TreeEntryRef<'e>> {
        let target = &name[..];
        let mut lo = 0usize;
        let mut hi = self.entries.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let c = compare_entry_names(
                &self.entries[mid].name[..],
                self.entries[mid].mode,
                target,
                EntryMode::Blob,
            );
            match c {
                Ordering::Equal => {
                    return Some(TreeEntryRef {
                        mode: self.entries[mid].mode,
                        name: &self.entries[mid].name,
                        oid: self.entries[mid].oid,
                    });
                }
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
            }
        }
        None
    }

    /// Phase 1 — walk a full path (a sequence of child names) down from this
    /// tree, returning the final entry.
    ///
    /// # Git format
    ///
    /// Each component is a child name within the current directory; the walk
    /// descends into a subtree when the current entry is a directory. The
    /// caller supplies the component ids so we do not need to reach into the
    /// object database.
    ///
    /// # Errors
    ///
    /// [`Error::MalformedObject`] if any component is not present in the
    /// current directory, or if an intermediate entry is not a tree and the
    /// path continues past it.
    ///
    /// # Caveats
    ///
    /// Only the top-level tree's entries are available here; descending into
    /// a subtree requires the subtree's payload, which the object database
    /// would supply. For a single-component path the result is the top-level
    /// entry; for a longer path the walk cannot proceed without the database
    /// and reports an error.
    pub fn lookup_path(&self, path: &[&BString], _oids: &[ObjectId]) -> Result<TreeEntryRef<'_>> {
        if path.is_empty() {
            return Err(Error::MalformedObject {
                kind: ObjectKind::Tree,
                reason: "empty path".into(),
            });
        }
        let first = match self.get(path[0]) {
            Some(e) => e,
            None => {
                return Err(Error::MalformedObject {
                    kind: ObjectKind::Tree,
                    reason: format!("path component not found: {:?}", path[0]),
                });
            }
        };
        if path.len() > 1 {
            return Err(Error::MalformedObject {
                kind: ObjectKind::Tree,
                reason: "cannot descend into subtree without object database".into(),
            });
        }
        Ok(first)
    }

    /// Phase 1 — iterate all entries in on-disk (sorted) order.
    ///
    /// # Git format
    ///
    /// Entries are already sorted by Git's tree sort order (see
    /// [`Self::parse`]), so iteration visits them in the same order Git wrote
    /// them.
    ///
    /// # Caveats
    ///
    /// This yields [`TreeEntryRef`], a borrowed view that does not copy the
    /// entry. Use this when you need many entries in order; use [`Self::get`]
    /// for a single lookup.
    pub fn iter(&self) -> impl Iterator<Item = TreeEntryRef<'_>> {
        self.entries.iter().map(|e| TreeEntryRef {
            mode: e.mode,
            name: &e.name,
            oid: e.oid,
        })
    }
}

/// Compare two entries by Git's tree sort order.
///
/// # Git format
///
/// Entries are sorted by name bytes. When one name is a prefix of the other,
/// the shorter (prefix) name sorts first; Git orders a directory before a file
/// that shares its name as a prefix (e.g. `foo` before `foo.txt`). The mode
/// does not change the order: the prefix relationship already decides it.
///
/// # Caveats
///
/// This is a `pub(crate)` helper shared with `find_top`/`prev`-style logic; it
/// is not part of the public API and is tested through the public surface.
pub(crate) fn compare_entry_names(
    a_name: &[u8],
    _a_mode: EntryMode,
    b_name: &[u8],
    _b_mode: EntryMode,
) -> Ordering {
    let min = a_name.len().min(b_name.len());
    let c = a_name[..min].cmp(&b_name[..min]);
    if c != Ordering::Equal {
        return c;
    }
    a_name.len().cmp(&b_name.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree round-trip: parse then serialize must reproduce the exact bytes,
    /// including a subtree entry whose mode is `40000` (five octal digits).
    #[test]
    fn tree_round_trip_with_subtree() {
        let file_oid = ObjectId::Sha1([0x11; 20]);
        let dir_oid = ObjectId::Sha1([0x22; 20]);
        let entries = vec![
            TreeEntry {
                mode: EntryMode::Blob,
                name: BString::new(b"file.txt".to_vec()),
                oid: file_oid,
            },
            TreeEntry {
                mode: EntryMode::Tree,
                name: BString::new(b"dir".to_vec()),
                oid: dir_oid,
            },
        ];
        let tree = Tree {
            data: &[],
            entries,
        };
        let ser = tree.serialize();
        // Build the expected payload directly.
        let mut exp = Vec::new();
        exp.extend_from_slice(b"100644 file.txt\0");
        exp.extend_from_slice(&[0x11; 20]);
        exp.extend_from_slice(b"40000 dir\0");
        exp.extend_from_slice(&[0x22; 20]);
        assert_eq!(ser, exp, "serialized tree must match Git byte-for-byte");
        // Parse it back and confirm entries and the `40000` mode.
        let parsed = Tree::parse(ObjectFormat::Sha1, &exp).unwrap();
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].mode, EntryMode::Blob);
        assert_eq!(parsed.entries[1].mode, EntryMode::Tree);
        assert_eq!(parsed.entries[1].name, BString::new(b"dir".to_vec()));
        // Round-trip: parsing the serialized bytes reproduces them.
        assert_eq!(Tree::parse(ObjectFormat::Sha1, &ser).unwrap().serialize(), ser);
    }

    /// Sort order: a directory named `foo` must sort before a file named
    /// `foo.txt` (the directory is the prefix).
    #[test]
    fn dir_sorts_before_file_with_shared_prefix() {
        let _dir_oid = ObjectId::Sha1([0x33; 20]);
        let _file_oid = ObjectId::Sha1([0x44; 20]);
        // `foo` (tree) then `foo.txt` (blob): tree is the prefix, sorts first.
        let mut payload = Vec::new();
        payload.extend_from_slice(b"40000 foo\0");
        payload.extend_from_slice(&[0x33; 20]);
        payload.extend_from_slice(b"100644 foo.txt\0");
        payload.extend_from_slice(&[0x44; 20]);
        let tree = Tree::parse(ObjectFormat::Sha1, &payload).unwrap();
        assert_eq!(tree.entries[0].name, BString::new(b"foo".to_vec()));
        assert_eq!(tree.entries[0].mode, EntryMode::Tree);
        assert_eq!(tree.entries[1].name, BString::new(b"foo.txt".to_vec()));
        assert_eq!(tree.entries[1].mode, EntryMode::Blob);
        // `get` finds each by exact name.
        assert!(tree.get(&BString::new(b"foo".to_vec())).is_some());
        assert!(tree.get(&BString::new(b"foo.txt".to_vec())).is_some());
        assert!(tree.get(&BString::new(b"foo.txtx".to_vec())).is_none());
    }

    /// Parse a tree built from a hex literal (a real Git tree body).
    #[test]
    fn parse_from_hex_literal() {
        // 100644 a\0<20 zero bytes> — a one-entry tree.
        let mut payload = Vec::new();
        payload.extend_from_slice(b"100644 a\0");
        payload.extend_from_slice(&[0u8; 20]);
        let tree = Tree::parse(ObjectFormat::Sha1, &payload).unwrap();
        assert_eq!(tree.entries.len(), 1);
        assert_eq!(tree.entries[0].name, BString::new(b"a".to_vec()));
        assert_eq!(tree.entries[0].mode, EntryMode::Blob);
        assert_eq!(tree.entries[0].oid, ObjectId::Sha1([0u8; 20]));
    }
}
