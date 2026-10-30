//! The resolve-undo (`REUC`) extension: what an unmerged path looked like before it was resolved.
//!
//! Each record is the path, three NUL-terminated octal modes (stages 1, 2 and 3; `0` for a stage
//! that was absent), then the raw ids of the stages whose mode is not zero. Records are sorted by
//! path. `git ls-files --resolve-undo`, `git update-index --unresolve`, `git checkout -m <path>`
//! and `git rerere forget` read them back.

use bstr::{BStr, BString};

use super::{Index, IndexEntry, IndexExtension, Stage};
use crate::error::{Error, Result};
use crate::hash::{ObjectFormat, ObjectId};
use crate::object::EntryMode;

/// One resolved path: the mode and id of each of its three conflict stages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolveUndoEntry {
    /// The path, relative to the work tree root.
    pub path: BString,
    /// The mode of stages 1, 2 and 3; `0` when the stage was absent.
    pub modes: [u32; 3],
    /// The id of stages 1, 2 and 3; `None` exactly when the mode is `0`.
    pub oids: [Option<ObjectId>; 3],
}

fn corrupt(reason: &'static str) -> Error {
    Error::CorruptIndex { offset: 0, reason }
}

fn parse(format: ObjectFormat, data: &[u8]) -> Result<Vec<ResolveUndoEntry>> {
    let mut at = 0usize;
    let mut out = Vec::new();
    let take_cstr = |at: &mut usize| -> Result<&[u8]> {
        let rest = &data[*at..];
        let n = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| corrupt("truncated resolve-undo extension"))?;
        *at += n + 1;
        Ok(&rest[..n])
    };
    while at < data.len() {
        let path = BString::from(take_cstr(&mut at)?);
        let mut modes = [0u32; 3];
        for mode in &mut modes {
            let text = take_cstr(&mut at)?;
            if text.is_empty() || !text.iter().all(|b| (b'0'..=b'7').contains(b)) {
                return Err(corrupt("bad mode in resolve-undo extension"));
            }
            *mode = text
                .iter()
                .try_fold(0u32, |acc, &b| {
                    acc.checked_mul(8)?.checked_add((b - b'0') as u32)
                })
                .ok_or_else(|| corrupt("bad mode in resolve-undo extension"))?;
        }
        let mut oids: [Option<ObjectId>; 3] = [None, None, None];
        for (slot, &mode) in oids.iter_mut().zip(&modes) {
            if mode == 0 {
                continue;
            }
            let raw = data
                .get(at..at + format.len())
                .ok_or_else(|| corrupt("truncated resolve-undo extension"))?;
            *slot = Some(ObjectId::from_bytes(format, raw)?);
            at += format.len();
        }
        out.push(ResolveUndoEntry { path, modes, oids });
    }
    Ok(out)
}

fn serialize(entries: &[ResolveUndoEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        out.extend_from_slice(&entry.path);
        out.push(0);
        for mode in entry.modes {
            out.extend_from_slice(format!("{mode:o}").as_bytes());
            out.push(0);
        }
        for oid in entry.oids.iter().flatten() {
            out.extend_from_slice(oid.as_bytes());
        }
    }
    out
}

fn mode_value(entry: &IndexEntry) -> u32 {
    entry.mode.to_u32()
}

impl Index {
    /// The resolve-undo records, sorted by path; empty when the extension is absent or unreadable.
    pub fn resolve_undo(&self, format: ObjectFormat) -> Vec<ResolveUndoEntry> {
        self.extensions
            .iter()
            .find(|e| e.is_reuc())
            .and_then(|e| parse(format, &e.data).ok())
            .unwrap_or_default()
    }

    /// Replace the resolve-undo records; an empty list drops the extension.
    pub fn set_resolve_undo(&mut self, entries: &[ResolveUndoEntry]) {
        self.extensions.retain(|e| !e.is_reuc());
        if entries.is_empty() {
            return;
        }
        let ext = IndexExtension::new(*b"REUC", serialize(entries));
        // Git writes it after `link` and `TREE`; a reader does not care, but the bytes then match.
        let at = self
            .extensions
            .iter()
            .position(|e| !(e.is_split_index_link() || e.is_tree()))
            .unwrap_or(self.extensions.len());
        self.extensions.insert(at, ext);
    }

    /// Remember the conflict stages in `removed` the way git's `record_resolve_undo` does: one
    /// record per path, each stage filled in the first time it is lost and never overwritten.
    pub(super) fn record_resolve_undo(&mut self, removed: &[IndexEntry]) {
        let Some(first) = removed.iter().find(|e| e.stage() != Stage::Normal) else {
            return;
        };
        let format = first.oid.format();
        let mut records = self.resolve_undo(format);
        let at = match records.iter().position(|r| r.path == first.path) {
            Some(at) => at,
            None => {
                records.push(ResolveUndoEntry {
                    path: first.path.clone(),
                    modes: [0; 3],
                    oids: [None, None, None],
                });
                records.len() - 1
            }
        };
        for entry in removed {
            let stage = entry.stage().as_bits() as usize;
            if (1..=3).contains(&stage) && records[at].modes[stage - 1] == 0 {
                records[at].modes[stage - 1] = mode_value(entry);
                records[at].oids[stage - 1] = Some(entry.oid);
            }
        }
        records.sort_by(|a, b| a.path.cmp(&b.path));
        self.set_resolve_undo(&records);
    }

    /// Drop every resolve-undo record.
    pub fn clear_resolve_undo(&mut self) {
        self.extensions.retain(|e| !e.is_reuc());
    }

    /// Put `path` back into its conflicted state from the resolve-undo record, as git's
    /// `unresolve_one` does: a stage-0 entry, if there is one, is replaced by the recorded stages.
    /// Returns whether the path was unmerged; a path with no record is left alone.
    pub fn unmerge_path(&mut self, path: &BStr, format: ObjectFormat) -> bool {
        let mut records = self.resolve_undo(format);
        let Some(i) = records.iter().position(|r| r.path == path) else {
            return false;
        };
        let record = records.remove(i);
        self.remove(path, Stage::Normal);
        for stage in 0..3 {
            let (mode, Some(oid)) = (record.modes[stage], record.oids[stage]) else {
                continue;
            };
            let Ok(mode) = EntryMode::from_bytes(format!("{mode:o}").as_bytes()) else {
                continue;
            };
            let bits = (stage as u16 + 1) << 12;
            self.entries.push(IndexEntry {
                ctime_seconds: 0,
                ctime_nanos: 0,
                mtime_seconds: 0,
                mtime_nanos: 0,
                dev: 0,
                ino: 0,
                mode,
                uid: 0,
                gid: 0,
                size: 0,
                oid,
                flags: super::IndexEntryFlags::from_bits(bits | path.len().min(0xfff) as u16),
                extended_flags: 0,
                path: path.to_owned(),
            });
        }
        self.sort();
        self.invalidate_cache_tree();
        self.set_resolve_undo(&records);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexExt;

    fn entry(path: &str, stage: u16, byte: u8) -> IndexEntry {
        let mut entry = crate::ops::make_index_entry(
            BStr::new(path),
            EntryMode::Blob,
            ObjectId::from_bytes(ObjectFormat::Sha1, &[byte; 20]).unwrap(),
            None,
        );
        entry.flags = super::super::IndexEntryFlags::from_bits((stage << 12) | path.len() as u16);
        entry
    }

    fn reuc(index: &Index) -> Vec<u8> {
        index
            .extensions()
            .iter()
            .find(|e| e.is_reuc())
            .map(|e| e.data.clone())
            .unwrap_or_default()
    }

    #[test]
    fn removing_a_conflicted_path_records_its_stages_in_gits_layout() {
        let mut index = Index::new();
        for e in [
            entry("a", 1, 1),
            entry("a", 2, 2),
            entry("a", 3, 3),
            entry("b", 0, 9),
        ] {
            index.insert(e);
        }
        index.sort();
        index.remove_path(BStr::new("b"));
        assert!(reuc(&index).is_empty(), "a stage-0 entry records nothing");
        index.remove_path(BStr::new("a"));
        let mut expected = b"a\x00100644\x00100644\x00100644\x00".to_vec();
        for byte in 1..=3u8 {
            expected.extend_from_slice(&[byte; 20]);
        }
        assert_eq!(reuc(&index), expected);
    }

    #[test]
    fn a_missing_stage_has_mode_zero_and_no_id_and_the_first_record_wins() {
        let mut index = Index::new();
        index.insert(entry("a", 2, 2));
        index.insert(entry("a", 3, 3));
        index.sort();
        index.remove(BStr::new("a"), Stage::Ours);
        index.remove(BStr::new("a"), Stage::Theirs);
        let mut expected = b"a\x000\x00100644\x00100644\x00".to_vec();
        expected.extend_from_slice(&[2; 20]);
        expected.extend_from_slice(&[3; 20]);
        assert_eq!(reuc(&index), expected);
        // Losing the same stage again does not overwrite it.
        index.insert(entry("a", 2, 7));
        index.remove(BStr::new("a"), Stage::Ours);
        assert_eq!(reuc(&index), expected);
    }

    #[test]
    fn unmerge_puts_the_stages_back_and_drops_the_record() {
        let mut index = Index::new();
        for e in [entry("a", 1, 1), entry("a", 3, 3)] {
            index.insert(e);
        }
        index.sort();
        index.remove_path(BStr::new("a"));
        index.insert(entry("a", 0, 5));
        assert!(index.unmerge_path(BStr::new("a"), ObjectFormat::Sha1));
        let stages: Vec<_> = index
            .entries()
            .iter()
            .map(|e| e.stage().as_bits())
            .collect();
        assert_eq!(stages, [1, 3]);
        assert!(reuc(&index).is_empty());
        assert!(!index.unmerge_path(BStr::new("a"), ObjectFormat::Sha1));
    }

    #[test]
    fn a_damaged_record_reads_as_empty() {
        let mut index = Index::new();
        index.extensions_mut().push(IndexExtension::new(
            *b"REUC",
            b"a\x00100644\x00100644\x00".to_vec(),
        ));
        assert!(index.resolve_undo(ObjectFormat::Sha1).is_empty());
        let mut data = b"a\x00100644\x00zz\x00100644\x00".to_vec();
        data.extend_from_slice(&[1; 40]);
        index.extensions_mut().clear();
        index
            .extensions_mut()
            .push(IndexExtension::new(*b"REUC", data));
        assert!(index.resolve_undo(ObjectFormat::Sha1).is_empty());
    }
}
