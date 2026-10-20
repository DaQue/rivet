//! Rename detection: pairing a deleted path with an added one that holds (nearly) the same
//! content, as `git diff -M` does by default.
//!
//! Exact renames (identical blob ids) are found first. The rest are scored by how many bytes of
//! the new file already existed in the old one, counted over line-sized spans exactly as Git's
//! `diffcore-delta` does, and a pair needs at least 50% to count. Pairs are then taken best
//! first, so one file is never the source of two renames.

use std::collections::HashMap;

use bstr::{BStr, BString, ByteSlice};

use crate::diff::{DiffOptions, FileDiff, TreeDiff, blob_diff};
use crate::error::Result;
use crate::hash::ObjectId;
use crate::object::EntryMode;
use crate::odb::Odb;

/// A score of 100% in Git's fixed-point scale.
pub const MAX_SCORE: u64 = 60_000;
/// The default `-M` threshold: 50%.
pub const MINIMUM_SCORE: u64 = 30_000;
/// Git's `diff.renameLimit` default.
pub const RENAME_LIMIT: usize = 1000;

/// One side of a potential rename.
#[derive(Clone, Debug)]
pub struct RenameSide {
    /// The path.
    pub path: BString,
    /// The blob id.
    pub oid: ObjectId,
    /// The entry mode.
    pub mode: EntryMode,
}

/// A detected rename: indices into the source and destination lists, and the 0..=60000 score.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenamePair {
    /// Index into the deleted side.
    pub source: usize,
    /// Index into the added side.
    pub destination: usize,
    /// Similarity on Git's scale; 60000 is identical.
    pub score: u64,
}

impl RenamePair {
    /// The percentage Git prints (`similarity index 87%`), truncated.
    pub fn percent(&self) -> u32 {
        (self.score * 100 / MAX_SCORE) as u32
    }
}

fn is_regular_kind(mode: EntryMode) -> bool {
    matches!(mode, EntryMode::Blob | EntryMode::BlobExecutable | EntryMode::Link)
}

fn same_kind(a: EntryMode, b: EntryMode) -> bool {
    (a == EntryMode::Link) == (b == EntryMode::Link)
}

/// Span hashes of a blob: Git splits at newlines and at 64 bytes, and counts bytes per span.
fn span_counts(data: &[u8]) -> HashMap<u64, u64> {
    let mut counts: HashMap<u64, u64> = HashMap::new();
    let binary = data.iter().take(8000).any(|b| *b == 0);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut length: u64 = 0;
    for &byte in data {
        hash = (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3);
        length += 1;
        if (!binary && byte == b'\n') || length >= 64 {
            *counts.entry(hash).or_insert(0) += length;
            hash = 0xcbf2_9ce4_8422_2325;
            length = 0;
        }
    }
    if length > 0 {
        *counts.entry(hash).or_insert(0) += length;
    }
    counts
}

fn basename(path: &[u8]) -> &[u8] {
    path.rsplit(|b| *b == b'/').next().unwrap_or(path)
}

/// Pair deleted sources with added destinations. Entries that are not regular files or symlinks
/// never pair. Returns nothing when the problem is larger than Git's rename limit.
pub fn pair_renames(
    sources: &[RenameSide],
    destinations: &[RenameSide],
    odb: &Odb,
) -> Result<Vec<RenamePair>> {
    let mut pairs: Vec<RenamePair> = Vec::new();
    if sources.is_empty() || destinations.is_empty() {
        return Ok(pairs);
    }
    if sources.len().saturating_mul(destinations.len()) > RENAME_LIMIT * RENAME_LIMIT {
        return Ok(pairs);
    }
    let mut source_used = vec![false; sources.len()];
    let mut destination_used = vec![false; destinations.len()];

    // Exact renames: same blob, preferring the source with the same file name.
    let mut by_oid: HashMap<ObjectId, Vec<usize>> = HashMap::new();
    for (index, source) in sources.iter().enumerate() {
        if is_regular_kind(source.mode) {
            by_oid.entry(source.oid).or_default().push(index);
        }
    }
    for (d, destination) in destinations.iter().enumerate() {
        if !is_regular_kind(destination.mode) {
            continue;
        }
        let Some(candidates) = by_oid.get(&destination.oid) else { continue };
        let free: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|s| !source_used[*s] && same_kind(sources[*s].mode, destination.mode))
            .collect();
        let best = free
            .iter()
            .copied()
            .find(|s| basename(&sources[*s].path) == basename(&destination.path))
            .or_else(|| free.first().copied());
        if let Some(s) = best {
            source_used[s] = true;
            destination_used[d] = true;
            pairs.push(RenamePair { source: s, destination: d, score: MAX_SCORE });
        }
    }

    // Inexact renames among what is left.
    let mut candidates: Vec<(u64, u8, usize, usize)> = Vec::new();
    let mut cache: HashMap<ObjectId, (u64, HashMap<u64, u64>)> = HashMap::new();
    let mut load = |oid: &ObjectId, cache: &mut HashMap<ObjectId, (u64, HashMap<u64, u64>)>| -> Result<()> {
        if !cache.contains_key(oid) {
            let data = odb.read_typed(oid, crate::object::ObjectKind::Blob)?.into_data();
            cache.insert(*oid, (data.len() as u64, span_counts(&data)));
        }
        Ok(())
    };
    for (d, destination) in destinations.iter().enumerate() {
        if destination_used[d] || !is_regular_kind(destination.mode) {
            continue;
        }
        for (s, source) in sources.iter().enumerate() {
            if source_used[s] || !is_regular_kind(source.mode) || !same_kind(source.mode, destination.mode) {
                continue;
            }
            load(&source.oid, &mut cache)?;
            load(&destination.oid, &mut cache)?;
            let (source_size, source_spans) = &cache[&source.oid];
            let (destination_size, destination_spans) = &cache[&destination.oid];
            let max_size = (*source_size).max(*destination_size);
            let min_size = (*source_size).min(*destination_size);
            if max_size == 0 {
                continue;
            }
            let delta = max_size - min_size;
            if max_size * (MAX_SCORE - MINIMUM_SCORE) < delta * MAX_SCORE {
                continue;
            }
            let copied: u64 = source_spans
                .iter()
                .filter_map(|(hash, count)| destination_spans.get(hash).map(|d| (*count).min(*d)))
                .sum();
            let score = copied * MAX_SCORE / max_size;
            if score >= MINIMUM_SCORE {
                let name_score = (basename(&source.path) == basename(&destination.path)) as u8;
                candidates.push((score, name_score, d, s));
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
    for (score, _, d, s) in candidates {
        if source_used[s] || destination_used[d] {
            continue;
        }
        source_used[s] = true;
        destination_used[d] = true;
        pairs.push(RenamePair { source: s, destination: d, score });
    }
    pairs.sort_by_key(|p| p.destination);
    Ok(pairs)
}

/// Replace matching deleted/added file pairs in `diff` with rename entries.
pub fn detect_renames(diff: &mut TreeDiff, odb: &Odb, options: &DiffOptions) -> Result<()> {
    let mut sources = Vec::new();
    let mut source_files = Vec::new();
    let mut destinations = Vec::new();
    let mut destination_files = Vec::new();
    for (index, file) in diff.files.iter().enumerate() {
        if file.is_deleted {
            if let (Some(oid), Some(mode)) = (file.old_oid, file.old_mode) {
                sources.push(RenameSide { path: file.path().to_owned(), oid, mode });
                source_files.push(index);
            }
        } else if file.is_new {
            if let (Some(oid), Some(mode)) = (file.new_oid, file.new_mode) {
                destinations.push(RenameSide { path: file.path().to_owned(), oid, mode });
                destination_files.push(index);
            }
        }
    }
    let pairs = pair_renames(&sources, &destinations, odb)?;
    if pairs.is_empty() {
        return Ok(());
    }
    let format = odb.format();
    let mut remove: Vec<usize> = Vec::new();
    for pair in pairs {
        let source = &sources[pair.source];
        let destination = &destinations[pair.destination];
        let mut renamed = blob_diff(
            format,
            BStr::new(&destination.path[..]),
            Some((source.oid, source.mode)),
            Some((destination.oid, destination.mode)),
            odb,
            options,
        )?;
        renamed.old_path = Some(source.path.clone());
        renamed.new_path = Some(destination.path.clone());
        renamed.is_new = false;
        renamed.is_deleted = false;
        renamed.similarity = Some(pair.percent());
        diff.files[destination_files[pair.destination]] = renamed;
        remove.push(source_files[pair.source]);
    }
    remove.sort_unstable();
    for index in remove.into_iter().rev() {
        diff.files.remove(index);
    }
    diff.stats = diff.files.iter().map(FileDiff::stat).collect();
    Ok(())
}

/// Git's `pprint_rename`: `a/b/{old => new}/c`, with the common prefix and suffix factored out.
pub fn pprint_rename(old: &[u8], new: &[u8]) -> BString {
    let (len_a, len_b) = (old.len(), new.len());
    let mut prefix = 0usize;
    let mut i = 0usize;
    while i < len_a && i < len_b && old[i] == new[i] {
        if old[i] == b'/' {
            prefix = i + 1;
        }
        i += 1;
    }
    let adjust = if prefix > 0 { 1usize } else { 0 };
    let mut suffix = 0usize;
    let (mut ao, mut bo) = (len_a as isize, len_b as isize);
    while (prefix as isize - adjust as isize) <= ao
        && (prefix as isize - adjust as isize) <= bo
        && ao >= 0
        && bo >= 0
        && old.get(ao as usize) == new.get(bo as usize)
    {
        if old.get(ao as usize) == Some(&b'/') {
            suffix = len_a - ao as usize;
        }
        ao -= 1;
        bo -= 1;
    }
    let a_mid = (len_a as isize - prefix as isize - suffix as isize).max(0) as usize;
    let b_mid = (len_b as isize - prefix as isize - suffix as isize).max(0) as usize;
    let mut out = BString::default();
    let factored = prefix + suffix > 0;
    if factored {
        out.extend_from_slice(&old[..prefix]);
        out.push(b'{');
    }
    out.extend_from_slice(&old[prefix..prefix + a_mid]);
    out.extend_from_slice(b" => ");
    out.extend_from_slice(&new[prefix..prefix + b_mid]);
    if factored {
        out.push(b'}');
        out.extend_from_slice(&old[len_a - suffix..]);
    }
    out
}
