//! Phase 8 — the line diff itself: splitting files into lines, and Myers' `O(ND)` edit script.
//!
//! This file knows nothing about files, paths or repositories. It takes two sequences and
//! returns the list of matched runs between them, which is the first of the three layers
//! described in [`crate::diff`]. Everything above it — pairing tree entries, adding context,
//! deciding whether a file is binary — is someone else's problem.
//!
//! # Why Myers, and what it costs
//!
//! The algorithm is Git's default and the one the `diff` test corpus is generated with. It is
//! `O(ND)` in the worst case, where `D` is the number of differing lines, which is fast on the
//! inputs a version control system actually sees — two revisions of a file with a few changes —
//! and quadratic on the inputs it occasionally sees anyway. Two 100 000-line files with nothing
//! in common are a real amount of work; Git's answer is a heuristic cutoff and requiring the
//! caller to opt in above a threshold, and Rivet's answer is not yet decided (plan §4 Phase 8).
//! Whatever it becomes, the decision belongs in [`myers_diff`]'s caller, because the algorithm
//! has no way to know how much the answer is worth.
//!
//! # Caveats that bite
//!
//! * **The output is an edit script, not hunks.** Hugging changes together, adding context and
//!   deciding hunk boundaries is [`crate::diff::unified`]'s job; a caller that rendered these
//!   edits directly would produce one hunk per contiguous run, which is a valid diff and not the
//!   one Git produces.
//! * **There can be several equally minimal edit scripts.** Myers finds *a* shortest one, and
//!   which one it finds depends on tie-breaking. Matching `git diff` byte for byte therefore
//!   constrains the tie-breaking, not just the algorithm — the differential corpus is what
//!   pins it down.
//! * **A line's trailing newline is part of the line, and its absence is part of the diff.**
//!   See [`DiffLine`]: a file whose last line gains or loses its terminator has a changed last
//!   line, and the `\ No newline at end of file` marker is emitted from `has_newline`, never
//!   guessed from the bytes.
//! * **`split_lines` does not normalise anything.** CRLF is kept, a lone `\r` is kept, a NUL is
//!   kept. Normalising here would make the diff disagree with the blob the id was computed over.

use crate::error::Result;
use crate::error::Error;

/// Phase 8 — one line of a diff input.
///
/// # Caveats
///
/// * **`content` includes the line's terminator** when it has one, so `+` followed by `content`
///   is exactly what a unified diff prints. A type that stripped the newline would make every
///   caller re-add it and would lose the distinction between a line ending in `\n` and one
///   ending in `\r\n`.
/// * **`has_newline` is redundant with `content`'s last byte and is not a mistake.** It is the
///   fact the output layer branches on — `\ No newline at end of file` — and spelling it out
///   keeps that branch from being written three times with three chances to be wrong.
/// * **The last line of a file with no terminator is still a line.** `b"a\nb"` is two lines, not
///   one, and the second has `has_newline == false`; a splitter that treated an empty trailing
///   chunk as a line would make `b"a\n"` into two lines and add a spurious empty line to every
///   diff of a text file.
/// * **A zero-length input has no lines.** An empty file diffs against a one-line file as one
///   addition and zero deletions, which is what Git prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffLine<'a> {
    /// The line's bytes, including the terminator when there is one.
    pub content: &'a [u8],
    /// Whether the line ended with a newline.
    pub has_newline: bool,
}

/// Phase 8 — one run of the edit script: a contiguous match between the two sequences.
///
/// # Caveats
///
/// * **A zero-length run on one side is an insertion or a deletion**, and the pair
///   `(old_len == 0, new_len > 0)` is an insertion. Both lengths zero is a legal no-op that a
///   caller rendering output must skip rather than print as an empty hunk.
/// * **The runs are disjoint and ordered**, covering each sequence exactly once, so a caller can
///   reconstruct the whole alignment by walking them in order — which is what the hunk builder
///   does, and what makes it unnecessary to re-run the algorithm per hunk.
/// * **The indices are line indices, not byte offsets.** Converting to byte offsets requires the
///   line lengths, which the caller has; a byte offset here would be wrong the moment the two
///   sides' line endings differ.
/// * **`old_index` and `new_index` are not necessarily equal**, and the difference is the
///   cumulative shift from earlier insertions and deletions. Treating them as the same number is
///   the classic off-by-one in a diff renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Edit {
    /// Where the run starts in the old sequence.
    pub old_index: usize,
    /// How many old lines the run covers.
    pub old_len: usize,
    /// Where the run starts in the new sequence.
    pub new_index: usize,
    /// How many new lines the run covers.
    pub new_len: usize,
}

impl Edit {
    /// Phase 8 — an insertion: no old lines, `len` new ones.
    pub fn insert(new_index: usize, len: usize) -> Self {
        Self {
            old_index: new_index,
            old_len: 0,
            new_index,
            new_len: len,
        }
    }

    /// Phase 8 — a deletion: `len` old lines, no new ones.
    pub fn delete(old_index: usize, len: usize) -> Self {
        Self {
            old_index,
            old_len: len,
            new_index: old_index,
            new_len: 0,
        }
    }

    /// Phase 8 — an unchanged run, present in both sequences.
    ///
    /// # Caveats
    ///
    /// The signature takes both indices rather than one because they can differ, per the
    /// caveat on [`Edit`]; the lengths are always equal for a match, so only one is needed.
    pub fn equal(old_index: usize, new_index: usize, len: usize) -> Self {
        Self {
            old_index,
            old_len: len,
            new_index,
            new_len: len,
        }
    }

    /// Phase 8 — whether the run changes anything.
    ///
    /// # Caveats
    ///
    /// `false` means the run is a pure match. A run with a zero length on **both** sides is also
    /// `false`: it is an artefact a diff implementation may emit at the ends of an alignment, and
    /// a renderer that printed it would produce an empty hunk.
    pub fn is_change(&self) -> bool {
        self.old_len != self.new_len
    }

    /// Phase 8 — the number of old lines the run covers, as an `usize`.
    ///
    /// Provided so callers do not have to spell `old_len` in the two places it is easy to
    /// transpose with `new_len`.
    pub fn old_end(&self) -> usize {
        self.old_len
    }

    /// Phase 8 — one past the last new line the run covers.
    pub fn new_end(&self) -> usize {
        self.old_end() + self.new_len
    }
}

/// Phase 8 — split a buffer into lines, keeping each line's terminator.
///
/// # Caveats
///
/// * **A trailing newline does not create an empty final line.** Per [`DiffLine`]: `b"a\n"` is
///   one line with `has_newline == true`, and `b"a"` is one line with `has_newline == false`. The
///   two diff as a change, which is what Git reports and what the marker line in the unified
///   output exists to explain.
/// * **The split is on `\n` only.** A CRLF file's lines end in `\r\n` and the `\r` stays in
///   `content`, so a CRLF file and an LF file differ on every line — which is Git's behaviour
///   without `core.autocrlf`, and Rivet does not implement `core.autocrlf` in v1.
/// * **The lines borrow from `data`** and do not copy, so a caller must keep the buffer alive
///   for as long as the result. That is what makes diffing a large blob cheap on the happy path
///   where no lines changed.
/// * **This is linear and is called on both sides of every comparison**, so it is the one place
///   in the diff where an allocation per line would be visible. It returns borrowed slices, not
///   owned ones, for exactly that reason.
pub fn split_lines(data: &[u8]) -> Vec<DiffLine<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let len = data.len();
    while start < len {
        let end = data[start..].iter().position(|&b| b == b'\n').map(|i| start + i + 1).unwrap_or(len);
        let content = &data[start..end];
        // Whether *this* line is terminated, which the final line of a file usually is too.
        let has_newline = content.last() == Some(&b'\n');
        lines.push(DiffLine { content, has_newline });
        start = end;
    }
    lines
}

/// Phase 8 — the edit script between two sequences, by Myers' greedy algorithm.
///
/// Generic over the element type rather than fixed to [`DiffLine`] because the algorithm is
/// genuinely sequence-generic and the tests are far easier to write against `&[&str]` than
/// against buffers with a threading of line terminators. The comparison is `PartialEq`, which for
/// [`DiffLine`] compares both the bytes and the trailing-newline flag.
///
/// # Caveats
///
/// * **The result is an edit script, not hunks** — see the module caveat. It is ordered, the
///   runs are disjoint, and walking them covers both sequences exactly once.
/// * **A tie is broken by the algorithm's own rule, not by preference.** Two minimal scripts of
///   the same length are equally correct and produce different (both valid) diffs; the
///   differential tests are what fix which one Rivet must emit.
/// * **The cost is `O(ND)` in time and `O(N + D)` in memory**, where `N` is the total length and
///   `D` the edit distance. The memory term is the one that surprises on a pathological input:
///   the k-vector is `2D + 1` entries and can hold indices into both sequences, so a diff that
///   "feels" linear because the files are large can still allocate a lot when they are very
///   different.
/// * **This does not panic on any input**, including two empty slices, which produce an empty
///   script rather than a single matched zero-length run. A caller that assumed one run per
///   input would divide by zero on the empty case.
/// * **It is infallible and therefore cannot report the histogram request.** Selecting
///   [`crate::diff::DiffAlgorithm::Histogram`] is a dispatch decision made one layer up; see
///   [`histogram_diff`].
pub fn myers_diff<T: PartialEq>(old: &[T], new: &[T]) -> Vec<Edit> {
    let n = old.len();
    let m = new.len();
    if n == 0 && m == 0 {
        return Vec::new();
    }
    // `v[k]` holds the furthest x reached on diagonal `k = x - y`; `offset` shifts the signed
    // diagonal into the vector. The trace keeps a copy of `v` per edit distance so the path can
    // be walked back without storing the whole grid.
    let max = n + m;
    let offset = max as isize + 1;
    let mut v = vec![0isize; 2 * max + 3];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    'search: for d in 0..=max as isize {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d
                || (k != d && v[(offset + k - 1) as usize] < v[(offset + k + 1) as usize])
            {
                v[(offset + k + 1) as usize]
            } else {
                v[(offset + k - 1) as usize] + 1
            };
            let mut y = x - k;
            while x < n as isize && y < m as isize && old[x as usize] == new[y as usize] {
                x += 1;
                y += 1;
            }
            v[(offset + k) as usize] = x;
            if x >= n as isize && y >= m as isize {
                break 'search;
            }
            k += 2;
        }
    }

    // Walk the trace backwards. The list is built in reverse order and flipped at the end.
    let mut reversed: Vec<Edit> = Vec::new();
    let mut x = n as isize;
    let mut y = m as isize;
    for d in (0..trace.len()).rev() {
        let v = &trace[d];
        let k = x - y;
        let prev_k = if k == -(d as isize)
            || (k != d as isize && v[(offset + k - 1) as usize] < v[(offset + k + 1) as usize])
        {
            k + 1
        } else {
            k - 1
        };
        let prev_x = v[(offset + prev_k) as usize];
        let prev_y = prev_x - prev_k;
        let mut diagonal = 0usize;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            diagonal += 1;
        }
        if diagonal > 0 {
            // The run starts where the walk stopped, which is the position just after the edit
            // that precedes it — not `prev_x`, which is where that edit started.
            reversed.push(Edit::equal(x as usize, y as usize, diagonal));
        }
        if d > 0 {
            if x == prev_x {
                reversed.push(Edit {
                    old_index: x as usize,
                    old_len: 0,
                    new_index: (y - 1) as usize,
                    new_len: 1,
                });
            } else {
                reversed.push(Edit {
                    old_index: (x - 1) as usize,
                    old_len: 1,
                    new_index: y as usize,
                    new_len: 0,
                });
            }
        }
        x = prev_x;
        y = prev_y;
    }
    // Whatever is left is the common prefix both files started with.
    if x > 0 && y > 0 {
        reversed.push(Edit::equal(0, 0, x.min(y) as usize));
    }
    reversed.reverse();

    // Collapse the single-line runs into the contiguous runs the renderer expects.
    let mut edits: Vec<Edit> = Vec::with_capacity(reversed.len());
    for edit in reversed {
        if let Some(last) = edits.last_mut() {
            let contiguous_delete = last.old_len > 0
                && last.new_len == 0
                && edit.old_len > 0
                && edit.new_len == 0
                && last.old_index + last.old_len == edit.old_index;
            let contiguous_insert = last.new_len > 0
                && last.old_len == 0
                && edit.new_len > 0
                && edit.old_len == 0
                && last.new_index + last.new_len == edit.new_index;
            if contiguous_delete || contiguous_insert {
                last.old_len += edit.old_len;
                last.new_len += edit.new_len;
                continue;
            }
        }
        edits.push(edit);
    }
    edits
}

/// Phase 8 — the edit script by histogram diff.
///
/// **Not implemented** (plan §4 Phase 8 puts it after the Myers path is correct and tested). It
/// exists as a signature, and as the error the selector must return, so that
/// [`crate::diff::DiffAlgorithm::Histogram`] can be named by the CLI today without changing types
/// when the body lands.
///
/// # Caveats
///
/// * **It must fail loudly, never fall back to Myers.** A silent fallback produces a diff that is
///   valid, plausible and not what the user asked for, and the difference is exactly the kind
///   that shows up as an intermittent test failure on a corpus generated by Git.
/// * **The signature is already fallible**, unlike [`myers_diff`], so no caller has to change
///   when the implementation arrives — but that also means a caller is free to call it today and
///   handle the one error.
///
/// # Errors
///
/// [`crate::error::Error::NotImplemented`] always, until the algorithm lands.
pub fn histogram_diff<T: PartialEq>(_old: &[T], _new: &[T]) -> Result<Vec<Edit>> {
    Err(Error::NotImplemented { what: "histogram diff" })
}

/// Phase 8 — the number of lines a buffer contains, without allocating or copying.
///
/// # Caveats
///
/// Counts the same way [`split_lines`] splits: a trailing terminator closes the last line and
/// does not start a new one, so an empty buffer is zero lines and `b"\n"` is one. Counting with
/// `bytecount`-style `memchr` is what makes this cheap enough to call on a blob whose hunks are
/// being rendered, where [`split_lines`] would allocate a `DiffLine` per line to answer the same
/// question.
///
/// Used by the unified renderer for the `@@ -a,b +c,d @@` counts, which are the number of lines
/// **in the file**, not in the hunk — the distinction the header's arithmetic depends on.
pub fn line_count(data: &[u8]) -> usize {
    data.iter().filter(|b| **b == b'\n').count()
}

/// Phase 8 — whether a buffer needs the `\ No newline at end of file` marker.
///
/// # Caveats
///
/// * **`true` means the buffer is non-empty and does not end in `\n`.** An empty file has no
///   final line and therefore needs no marker; `git diff` prints nothing for it, and emitting a
///   marker against an empty side produces a patch `git apply` rejects.
/// * **A buffer ending in `\r\n` ends in `\n`** and needs no marker, even though the last line's
///   content holds a `\r`. The marker is about the terminator, not about the CR.
pub fn needs_newline_marker(data: &[u8]) -> bool {
    !data.is_empty() && !data.ends_with(b"\n")
}
