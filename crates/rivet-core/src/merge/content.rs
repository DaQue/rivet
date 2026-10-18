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
    if algorithm == crate::diff::DiffAlgorithm::Histogram {
        return Err(crate::error::Error::NotImplemented { what: "histogram merge" });
    }
    let base_bytes: &[u8] = base.unwrap_or(&[]);
    let mut opts = crate::diff::DiffOptions::new();
    opts.context(crate::diff::DEFAULT_CONTEXT);
    opts.algorithm(algorithm);
    let mut hunks: Vec<ConflictHunk> = Vec::new();
    let mut out: Vec<u8> = Vec::new();
    let ours_lines: Vec<&[u8]> = ours.split(|b| *b == b'\n').collect();
    let theirs_lines: Vec<&[u8]> = theirs.split(|b| *b == b'\n').collect();
    let base_lines: Vec<&[u8]> = base_bytes.split(|b| *b == b'\n').collect();
    let mut oi = 0usize;
    let mut ti = 0usize;
    let mut bi = 0usize;
    while oi < ours_lines.len() && ti < theirs_lines.len() {
        if ours_lines[oi] == theirs_lines[ti] {
            out.extend_from_slice(ours_lines[oi]);
            if oi < ours_lines.len() - 1 { out.push(b'\n'); }
            oi += 1; ti += 1; bi += 1;
        } else {
            let mut oe = oi;
            let mut te = ti;
            let mut be = bi;
            while oe < ours_lines.len() && te < theirs_lines.len() && ours_lines[oe] != theirs_lines[te] {
                oe += 1; te += 1; be += 1;
            }
            out.extend_from_slice(b"<<<<<<< ");
            out.extend_from_slice(labels.0.as_bytes());
            out.extend_from_slice(b"\n");
            for l in &ours_lines[oi..oe] { out.extend_from_slice(l); out.push(b'\n'); }
            out.extend_from_slice(b"||||||| base\n");
            for l in &base_lines[bi..be] { out.extend_from_slice(l); out.push(b'\n'); }
            out.extend_from_slice(b">>>>>>> ");
            out.extend_from_slice(labels.1.as_bytes());
            out.extend_from_slice(b"\n");
            for l in &theirs_lines[ti..te] { out.extend_from_slice(l); out.push(b'\n'); }
            out.extend_from_slice(b"=======\n");
            for l in &theirs_lines[ti..te] { out.extend_from_slice(l); out.push(b'\n'); }
            out.extend_from_slice(b">>>>>>> ");
            out.extend_from_slice(labels.1.as_bytes());
            out.extend_from_slice(b"\n");
            hunks.push(ConflictHunk {
                ours_start: oi,
                ours_len: oe - oi,
                theirs_start: ti,
                theirs_len: te - ti,
                base_start: bi,
                base_len: be - bi,
            });
            oi = oe; ti = te; bi = be;
        }
    }
    while oi < ours_lines.len() {
        out.extend_from_slice(ours_lines[oi]);
        if oi < ours_lines.len() - 1 { out.push(b'\n'); }
        oi += 1;
    }
    while ti < theirs_lines.len() {
        out.extend_from_slice(theirs_lines[ti]);
        if ti < theirs_lines.len() - 1 { out.push(b'\n'); }
        ti += 1;
    }
    Ok(ContentMerge {
        data: BString::from(out),
        conflict: !hunks.is_empty(),
        hunks,
    })
}
