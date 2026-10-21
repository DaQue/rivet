//! Phase 9 — three-way content merge: combining two changed versions of one file.
//!
//! Given a base, ours and theirs, produce the file that has both sides' changes — with conflict
//! markers only where the two changes genuinely overlap.
//!
//! # The algorithm is a diff, not a patch application
//!
//! The shape every implementation converges on: diff base→ours and base→theirs into two edit
//! scripts, walk them together in base coordinates, and for each region decide:
//!
//! * **only ours changed it** → take ours;
//! * **only theirs changed it** → take theirs;
//! * **both changed it identically** → take it once (this is *not* a conflict, and treating it as
//!   one is the most common way a merge implementation annoys its users — two branches adding the
//!   same import line must merge silently);
//! * **both changed it differently** → a conflict, marked up in the output;
//! * **neither changed it** → the base's version.
//!
//! # Caveats that bite
//!
//! * **A conflict is a range, not a line.** Two changes that touch adjacent lines conflict if and
//!   only if their regions overlap once the "changed region" is taken to include the surrounding
//!   context the diff matched on. Getting this wrong makes the merge either conflict-happy (mark
//!   changes that do not overlap) or, far worse, silently drop one side's line because the two
//!   regions were computed to be disjoint.
//! * **A deletion against a modification of a different line is not a conflict**, and a deletion
//!   against a modification of the *same* line is. A delete/modify disagreement at the file level
//!   is [`crate::merge::ConflictKind::ModifyDelete`] and never reaches this function; a
//!   delete/modify at the *line* level is a content conflict and does.
//! * **The result is bytes, and it is written to the work tree as-is.** Line endings are not
//!   normalised — merging an LF file and a CRLF file produces a file with both, which is what Git
//!   does without `core.autocrlf` and what a caller must not "fix" here.
//! * **A missing trailing newline is preserved from whichever side won**, and a merge where one
//!   side's last line has no newline and the other's does is a conflict on that line, because the
//!   two differ. That is Git's behaviour and it is why the `\ No newline` marker appears in
//!   conflict output.
//! * **Nothing here reads or writes the object database.** The inputs are three byte slices and
//!   the output is a byte slice; storing the result is the caller's job, and it must happen only
//!   after the whole merge has been decided, so that a failure partway through does not leave a
//!   half-merged tree referenced from the index.

use bstr::BString;

use crate::error::Result;
use crate::merge::{ConflictHunk, ConflictStyle};

/// Phase 9 — the result of merging one file's content.
///
/// # Caveats
///
/// * **`data` is always present, conflict or not.** A conflicted merge still produces a best-effort
///   file — with markers in it — because that is what is written to the work tree and what the
///   user edits. A caller that expected `None` on conflict and wrote nothing would leave the work
///   tree without the file, and the user with nothing to resolve.
/// * **`conflict` is `true` when markers were inserted**, which is not the same as "the two sides
///   differed": a three-way merge of two different changes that do not overlap produces a
///   conflict-free result with both changes in it, and the most valuable thing this function does
///   is get that case right.
/// * **`hunks` is empty when `conflict` is false**, and its ranges are in base coordinates. It is
///   populated even for [`ConflictStyle::Merge`], where the base text is not printed, so that a
///   resolver can reconstruct the alignment without re-running the diff.
/// * **A `data` equal to one of the inputs is a common and correct outcome** — when only one side
///   changed, the result *is* that side, byte for byte. A caller that compared the result against
///   the inputs and skipped writing on a match would be doing a legitimate optimisation and would
///   also, in the conflicted case, skip writing the markers, so the comparison must be against the
///   `conflict` flag and not against equality with an input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentMerge {
    /// The merged bytes, markers included when there is a conflict.
    pub data: BString,
    /// Whether conflict markers were inserted.
    pub conflict: bool,
    /// The conflicted regions, in base coordinates.
    pub hunks: Vec<ConflictHunk>,
}

/// Phase 9 — merge three versions of one file's content.
///
/// # Caveats
///
/// * **`base: None` means the file did not exist in the merge base**, which is the add/add case.
///   It is not the same as an empty base: two sides that both created a file with the same
///   content merge cleanly, and two sides that created it differently conflict even though there
///   is no base to diff against. Treating a missing base as an empty file makes every add/add
///   conflict look like "both sides added these lines to an empty file", and the conflict markers
///   then show the whole file on both sides — which is what Git does, and what the caller asked
///   for by passing `None` rather than an empty blob.
/// * **`labels` are the marker text** — `(ours, theirs)` — and the base section in the `diff3`
///   styles carries the string `base` rather than a label, because Git's format does. The labels
///   come from the caller (a branch name, `HEAD`, a commit description), never from anything in
///   the objects, so this function cannot produce them itself.
/// * **`style` changes the output's bytes, not its shape**; see
///   [`crate::merge::ConflictStyle`]. `zdiff3` in particular narrows the conflicted region by
///   trimming lines the two sides share, so the same inputs produce a smaller conflict — and a
///   caller that hashed the result to detect "did the merge change this file" gets a different
///   answer under a different style.
/// * **Line endings and encodings are not normalised.** See the module caveat. Two files that
///   differ only in line endings conflict on every line, which is the honest answer without a
///   `core.autocrlf` implementation.
/// * **The comparison is byte equality per line, not whitespace-insensitive.** Git's
///   `merge.conflictStyle` has no `-w` equivalent and neither does this; a caller that wanted
///   whitespace-insensitive merging would be asking for a different result that no tool could
///   reproduce.
///
/// # Errors
///
/// [`crate::error::Error::NotImplemented`] when the algorithm selected is
/// [`crate::diff::DiffAlgorithm::Histogram`] — reported before any output is produced, so a
/// failed merge writes nothing.
pub fn merge_content(
    base: Option<&[u8]>,
    ours: &[u8],
    theirs: &[u8],
    labels: (&str, &str),
    style: ConflictStyle,
    algorithm: crate::diff::DiffAlgorithm,
) -> Result<ContentMerge> {
    let base_bytes: &[u8] = base.unwrap_or(&[]);
    let base_lines = lines_keeping_newline(base_bytes);
    let our_lines = lines_keeping_newline(ours);
    let their_lines = lines_keeping_newline(theirs);

    // Change regions in base coordinates, one list per side. A region is a run of base lines the
    // side rewrote; an insertion is a zero-length region at a boundary.
    let our_changes = change_regions(&base_lines, &our_lines, algorithm);
    let their_changes = change_regions(&base_lines, &their_lines, algorithm);
    let our_boundary = boundary_map(&base_lines, &our_lines, algorithm);
    let their_boundary = boundary_map(&base_lines, &their_lines, algorithm);

    let mut all: Vec<(Side, Region)> = Vec::with_capacity(our_changes.len() + their_changes.len());
    all.extend(our_changes.iter().map(|r| (Side::Ours, *r)));
    all.extend(their_changes.iter().map(|r| (Side::Theirs, *r)));
    all.sort_by_key(|(_, r)| (r.base_start, r.base_end));

    let clusters = cluster_regions(&all);

    let mut out: Vec<u8> = Vec::new();
    let mut hunks: Vec<ConflictHunk> = Vec::new();
    let mut cursor = 0usize;
    for cluster in &clusters {
        for line in &base_lines[cursor..cluster.base_start] {
            out.extend_from_slice(line);
        }
        // A cluster's sides span the whole cluster, not just the rewritten part: when a change
        // touches an insertion at its edge, Git's conflict block shows the neighbouring unchanged
        // lines too, and a section that stopped at the change would leave them out.
        let our_start = our_boundary[cluster.base_start]
            .min(cluster.ours.map(|r| r.side_start).unwrap_or(usize::MAX));
        let our_end =
            our_boundary[cluster.base_end].max(cluster.ours.map(|r| r.side_end).unwrap_or(0));
        let their_start = their_boundary[cluster.base_start]
            .min(cluster.theirs.map(|r| r.side_start).unwrap_or(usize::MAX));
        let their_end =
            their_boundary[cluster.base_end].max(cluster.theirs.map(|r| r.side_end).unwrap_or(0));

        let our_slice = &our_lines[our_start..our_end];
        let their_slice = &their_lines[their_start..their_end];
        let base_slice = &base_lines[cluster.base_start..cluster.base_end];

        if lines_equal(our_slice, their_slice) {
            // Both sides produced the same text — the `B X X` row of the merge table.
            for line in our_slice {
                out.extend_from_slice(line);
            }
        } else if cluster.ours.is_none() {
            for line in their_slice {
                out.extend_from_slice(line);
            }
        } else if cluster.theirs.is_none() {
            for line in our_slice {
                out.extend_from_slice(line);
            }
        } else {
            emit_conflict(&mut out, base_slice, our_slice, their_slice, labels, style);
            hunks.push(ConflictHunk {
                ours_start: our_start,
                ours_len: our_end - our_start,
                theirs_start: their_start,
                theirs_len: their_end - their_start,
                base_start: cluster.base_start,
                base_len: cluster.base_end - cluster.base_start,
            });
        }
        cursor = cluster.base_end;
    }
    for line in &base_lines[cursor..] {
        out.extend_from_slice(line);
    }

    Ok(ContentMerge {
        data: BString::from(out),
        conflict: !hunks.is_empty(),
        hunks,
    })
}

/// Which side a change region came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Ours,
    Theirs,
}

/// One side's rewrite of a base region, in base and side coordinates.
#[derive(Clone, Copy)]
struct Region {
    base_start: usize,
    base_end: usize,
    side_start: usize,
    side_end: usize,
}

/// A group of overlapping regions from either side: one decision point.
struct Cluster {
    base_start: usize,
    base_end: usize,
    ours: Option<Region>,
    theirs: Option<Region>,
}

/// Split a buffer into lines, each retaining its newline.
///
/// `b"a\nb"` is `["a\n", "b"]` and `b"a\n"` is `["a\n"]` — an empty input has no lines, and a
/// trailing newline does not invent an empty one. Concatenating the result reproduces the input
/// exactly, which is what lets the merge emit whole lines without tracking terminators separately.
fn lines_keeping_newline(data: &[u8]) -> Vec<&[u8]> {
    if data.is_empty() {
        return Vec::new();
    }
    data.split_inclusive(|b| *b == b'\n').collect()
}

fn lines_equal(a: &[&[u8]], b: &[&[u8]]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

/// The runs of base lines `side` rewrote, as half-open ranges in both files.
///
/// A replacement arrives from the line diff as a deletion followed by an insertion at the same
/// point, and the two must become one region: split apart, the deletion would look like it ends
/// before the insertion begins and the pair would be judged as two independent clean edits
/// instead of the one disagreement it is.
fn change_regions(
    base: &[&[u8]],
    side: &[&[u8]],
    algorithm: crate::diff::DiffAlgorithm,
) -> Vec<Region> {
    let mut regions = Vec::new();
    let mut pending_delete: Option<crate::diff::Edit> = None;
    let mut pending_insert: Option<crate::diff::Edit> = None;
    let flush = |del: &mut Option<crate::diff::Edit>,
                 ins: &mut Option<crate::diff::Edit>,
                 out: &mut Vec<Region>| {
        if del.is_none() && ins.is_none() {
            return;
        }
        let base_start = del
            .map(|d| d.old_index)
            .or_else(|| ins.map(|i| i.old_index))
            .unwrap_or(0);
        let base_end = del.map(|d| d.old_index + d.old_len).unwrap_or(base_start);
        let side_start = ins
            .map(|i| i.new_index)
            .or_else(|| del.map(|d| d.new_index))
            .unwrap_or(0);
        let side_end = ins.map(|i| i.new_index + i.new_len).unwrap_or(side_start);
        out.push(Region {
            base_start,
            base_end,
            side_start,
            side_end,
        });
        *del = None;
        *ins = None;
    };
    for edit in side_edits(base, side, algorithm) {
        if !edit.is_change() {
            flush(&mut pending_delete, &mut pending_insert, &mut regions);
            continue;
        }
        if edit.old_len > 0 {
            flush(&mut pending_delete, &mut pending_insert, &mut regions);
            pending_delete = Some(edit);
        } else {
            pending_insert = Some(edit);
        }
    }
    flush(&mut pending_delete, &mut pending_insert, &mut regions);
    regions
}

/// Edit script between `base` and `side` with the selected algorithm, as git computes it.
fn side_edits(
    base: &[&[u8]],
    side: &[&[u8]],
    algorithm: crate::diff::DiffAlgorithm,
) -> Vec<crate::diff::Edit> {
    match algorithm {
        crate::diff::DiffAlgorithm::Histogram => {
            crate::diff::myers::histogram_diff(base, side).unwrap_or_default()
        }
        crate::diff::DiffAlgorithm::Myers => crate::diff::myers::git_diff(base, side),
    }
}

/// Map every base boundary to the side boundary it sits at.
///
/// `result[i]` is the side line index that base line `i` starts at, and `result[base.len()]` is
/// the side's line count. Inside an unchanged run the mapping is one-to-one; at the edges of a
/// rewritten region it is the region's own side coordinates.
fn boundary_map(
    base: &[&[u8]],
    side: &[&[u8]],
    algorithm: crate::diff::DiffAlgorithm,
) -> Vec<usize> {
    let mut map: Vec<Option<usize>> = vec![None; base.len() + 1];
    for edit in side_edits(base, side, algorithm) {
        if edit.old_len > 0 && edit.new_len > 0 {
            for k in 0..=edit.old_len {
                map[edit.old_index + k] = Some(edit.new_index + k);
            }
        }
    }
    for region in change_regions(base, side, algorithm) {
        if map[region.base_start].is_none() {
            map[region.base_start] = Some(region.side_start);
        }
        if map[region.base_end].is_none() {
            map[region.base_end] = Some(region.side_end);
        }
    }
    map[base.len()] = Some(side.len());
    let mut last = 0usize;
    for slot in &mut map {
        match slot {
            Some(value) => last = *value,
            None => *slot = Some(last),
        }
    }
    map.into_iter().map(|v| v.unwrap()).collect()
}

/// Whether two regions describe the same base lines badly enough to be decided together.
///
/// Rewrites overlap when their base ranges intersect. An insertion (a zero-length region) sits
/// *between* lines and therefore only conflicts with a rewrite that covers it strictly, or with
/// another insertion at the same boundary — which is what lets one side edit a line while the
/// other adds a line above it and still merge the two cleanly.
fn regions_overlap(lo: usize, hi: usize, region: &Region) -> bool {
    // Closed intervals: a change that ends where the next one starts is one disagreement, not two
    // independent edits. Git needs an unchanged line between two changes to merge them
    // independently, and an insertion exactly at the edge of a rewritten line touches it.
    let other_lo = region.base_start;
    let other_hi = region.base_end;
    lo.max(other_lo) <= hi.min(other_hi)
}

/// Group regions into the clusters a single merge decision covers.
fn cluster_regions(regions: &[(Side, Region)]) -> Vec<Cluster> {
    let mut clusters: Vec<Cluster> = Vec::new();
    for (side, region) in regions {
        let mut merged = false;
        if let Some(last) = clusters.last_mut()
            && regions_overlap(last.base_start, last.base_end, region)
        {
            last.base_start = last.base_start.min(region.base_start);
            last.base_end = last.base_end.max(region.base_end);
            match side {
                Side::Ours => {
                    let r = last.ours.get_or_insert(*region);
                    r.base_start = r.base_start.min(region.base_start);
                    r.base_end = r.base_end.max(region.base_end);
                    r.side_start = r.side_start.min(region.side_start);
                    r.side_end = r.side_end.max(region.side_end);
                }
                Side::Theirs => {
                    let r = last.theirs.get_or_insert(*region);
                    r.base_start = r.base_start.min(region.base_start);
                    r.base_end = r.base_end.max(region.base_end);
                    r.side_start = r.side_start.min(region.side_start);
                    r.side_end = r.side_end.max(region.side_end);
                }
            }
            merged = true;
        }
        if !merged {
            let mut cluster = Cluster {
                base_start: region.base_start,
                base_end: region.base_end,
                ours: None,
                theirs: None,
            };
            match side {
                Side::Ours => cluster.ours = Some(*region),
                Side::Theirs => cluster.theirs = Some(*region),
            }
            clusters.push(cluster);
        }
    }
    clusters
}

/// Write one conflict, markers and all.
fn emit_conflict(
    out: &mut Vec<u8>,
    base: &[&[u8]],
    ours: &[&[u8]],
    theirs: &[&[u8]],
    labels: (&str, &str),
    style: ConflictStyle,
) {
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    let mut our_start = 0usize;
    let mut our_end = ours.len();
    let mut their_start = 0usize;
    let mut their_end = theirs.len();
    let mut base_start = 0usize;
    let mut base_end = base.len();

    // Git pulls lines the two sides agree on out of the conflict block in `merge` and `zdiff3`
    // style (and not in `diff3`, which shows the whole region so the base section lines up).
    // Without this, an add/add of files that share a header conflicts on the header too.
    if style != ConflictStyle::Diff3 {
        let mut prefix = 0usize;
        while prefix < our_end - our_start
            && prefix < their_end - their_start
            && ours[our_start + prefix] == theirs[their_start + prefix]
        {
            prefix += 1;
        }
        if prefix > 0 {
            for line in &ours[our_start..our_start + prefix] {
                out.extend_from_slice(line);
            }
            our_start += prefix;
            their_start += prefix;
            // The base section follows as far as it has the same lines; a base that is shorter
            // than the shared prefix (the add/add case, where it is empty) simply contributes
            // nothing, which is what Git prints.
            let mut base_prefix = 0usize;
            while base_prefix < prefix
                && base_start + base_prefix < base_end
                && base[base_start + base_prefix] == ours[our_start - prefix + base_prefix]
            {
                base_prefix += 1;
            }
            base_start += base_prefix;
        }

        let mut suffix = 0usize;
        while suffix < our_end - our_start
            && suffix < their_end - their_start
            && ours[our_end - 1 - suffix] == theirs[their_end - 1 - suffix]
        {
            suffix += 1;
        }
        if suffix > 0 {
            let mut base_suffix = 0usize;
            while base_suffix < suffix
                && base_end - base_suffix > base_start
                && base[base_end - 1 - base_suffix] == ours[our_end - 1 - base_suffix]
            {
                base_suffix += 1;
            }
            our_end -= suffix;
            their_end -= suffix;
            base_end -= base_suffix;
            write_marker_block(
                out,
                &base[base_start..base_end],
                &ours[our_start..our_end],
                &theirs[their_start..their_end],
                labels,
                style,
            );
            for line in &ours[our_end..our_end + suffix] {
                out.extend_from_slice(line);
            }
            if !out.ends_with(b"\n") {
                out.push(b'\n');
            }
            return;
        }
    }

    write_marker_block(
        out,
        &base[base_start..base_end],
        &ours[our_start..our_end],
        &theirs[their_start..their_end],
        labels,
        style,
    );
}

fn write_marker_block(
    out: &mut Vec<u8>,
    base: &[&[u8]],
    ours: &[&[u8]],
    theirs: &[&[u8]],
    labels: (&str, &str),
    style: ConflictStyle,
) {
    out.extend_from_slice(b"<<<<<<< ");
    out.extend_from_slice(labels.0.as_bytes());
    out.push(b'\n');
    write_section(out, ours);
    if style != ConflictStyle::Merge {
        out.extend_from_slice(b"||||||| base\n");
        write_section(out, base);
    }
    out.extend_from_slice(b"=======\n");
    write_section(out, theirs);
    out.extend_from_slice(b">>>>>>> ");
    out.extend_from_slice(labels.1.as_bytes());
    out.push(b'\n');
}

/// Write a section of a conflict, guaranteeing the next marker starts on its own line.
fn write_section(out: &mut Vec<u8>, lines: &[&[u8]]) {
    for line in lines {
        out.extend_from_slice(line);
    }
    if !out.ends_with(b"\n") {
        out.extend_from_slice(b"\n");
    }
}
