//! Phase 8 — diff: what changed, between two blobs, two trees, or an index and a work tree.
//!
//! Everything in this module is one of three layers, and keeping them apart is most of the
//! design:
//!
//! 1. **The line diff** ([`myers_diff`]) — pure algorithm over two sequences of lines. It knows
//!    nothing about files, paths or repositories, and it is the only place a quadratic
//!    algorithm can be.
//! 2. **The tree diff** ([`tree_diff`], [`index_diff`], [`work_tree_diff`]) — a parallel walk of
//!    two sorted trees that pairs entries by name, classifies each pair as added, deleted,
//!    modified or type-changed, and produces one [`FileDiff`] per changed path.
//! 3. **The unified output** ([`format_unified`], [`unified`]) — the `diff --git` text,
//!    hunks and all, that must match `git diff` closely enough that `git apply` accepts it.
//!
//! # The bytes that matter
//!
//! Plan §4 Phase 8 sets the bar: **the output must apply cleanly with `git apply`, and match
//! `git diff` on a test corpus.** That makes several details load-bearing rather than cosmetic,
//! and each is called out where it lives: the hunk header's line counts (which count the lines
//! *in the hunk*, not in the file), the `index <old>..<new> <mode>` line (whose abbreviations
//! are what `git apply` uses to check the patch applies to the right blobs), the `new file
//! mode` / `deleted file mode` lines, the `\ No newline at end of file` marker, and the exact
//! spelling of the `diff --git a/x b/x` header, which is *not* re-quoted when a path contains a
//! space or a quote.
//!
//! # Caveats that bite
//!
//! * **Binary detection is a heuristic and it is Git's.** A NUL byte in the first 8000 bytes
//!   marks a file binary, and the output is `Binary files a/x and b/x differ` — no hunks, no
//!   content. The window is 8000, not 8192, and matching it exactly is what the differential
//!   tests check. See [`crate::object::Blob::is_binary`].
//! * **A mode change with identical content is still a diff.** The blob ids are equal and there
//!   is nothing to output but the header: `old mode 100644` / `new mode 100755`, with no hunk
//!   at all. A caller that skipped the file because the ids matched would lose the change
//!   entirely.
//! * **`diff` between two trees is a tree walk, not a work tree walk.** `--staged` compares
//!   `HEAD`'s tree with the index; a bare `diff` compares the index with the work tree; and the
//!   only one that reads the filesystem is the last, which is why it takes a
//!   [`crate::worktree::Worktree`].
//! * **Renames are not detected.** Plan §4 Phase 8 puts rename detection later; a rename is
//!   reported as a deletion and an addition, exactly as `git diff --no-renames` prints it.
//! * **A gitlink is a diff with no content.** A `160000` entry's "old" and "new" are commit ids
//!   in another repository, so the diff is a one-line `Subproject commit <id>` change and the
//!   ids are never looked up locally — they are not in this object database.
//! * **`--stat` and `--name-status` are computed from the same [`TreeDiff`]**, not by parsing
//!   the unified output back in. A caller that rendered text and re-parsed it would be parsing
//!   its own output, which is the long way round and the way that drifts.

use bstr::{BStr, BString};

pub mod combined;
pub mod compact;
pub mod myers;
pub mod rename;
pub mod tree;
pub mod unified;
pub mod xdiff;

pub use myers::{
    DiffLine, Edit, histogram_diff, line_count, myers_diff, needs_newline_marker, split_lines,
};
pub use rename::{
    MAX_SCORE, RenamePair, RenameSide, detect_renames, pair_copies, pair_renames, pair_renames_at,
    pprint_rename,
};
pub use tree::{FileDiff, TreeDiff, blob_diff, entry_diff, index_diff, tree_diff, work_tree_diff};
pub use unified::{
    DiffStat, Hunk, HunkLine, LineKind, change_letter, format_name_status, format_stat,
    format_stat_sized, format_unified, index_line_abbrev, write_unified,
};

/// Phase 8 — which algorithm computes the line diff.
///
/// # Caveats
///
/// * **The variant is part of the output's identity, not a performance knob.** Myers' algorithm
///   and a histogram diff produce *different but equally valid* hunks for the same input, and
///   the differential tests have to know which one they are comparing against. A caller that
///   treated this as an implementation detail would see a test corpus that passes on one input
///   and fails on the next for no visible reason.
/// * **[`DiffAlgorithm::Myers`] is quadratic in the worst case** — `O(ND)` where `D` is the edit
///   distance — so two files that share nothing, of 100 000 lines each, are a serious amount of
///   work. Git's own default has the same property and mitigates it with a heuristic cutoff;
///   Rivet computes the full script and does not shorten it.
/// * **[`DiffAlgorithm::Histogram`] is implemented and selected per call.** It is not a fallback
///   for Myers: the two produce different hunks for the same input, and where lines repeat the
///   histogram's tie-breaking can pick a different but equally valid alignment than Git's. The
///   differential tests in `tests/` measure that rather than assume it away.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum DiffAlgorithm {
    /// Myers' `O(ND)` greedy algorithm, Git's default.
    #[default]
    Myers,
    /// Histogram diff, which produces smaller hunks on repetitive input. Not implemented.
    Histogram,
}

/// Phase 8 — how a diff should be computed.
///
/// # Caveats
///
/// * **`context` is the `-U<n>` count**, in lines, and it is `3` by default because that is
///   Git's default and the value the test corpus is generated with. Zero is legal and produces
///   hunks with no context at all, which apply only with `git apply --unidiff-zero`.
/// * **`detect_binary` off makes the diff read and compare binary content as text**, which is
///   what `--text` means. The resulting hunks are bytes rather than lines, and the output is
///   unlikely to apply — that is the user's choice and Rivet must honour it rather than
///   second-guess.
/// * **`pathspec` filters [`TreeDiff::files`] after the walk.** Filtering the walk instead
///   would be faster and would change which directories are descended into; the filter is on
///   the resulting paths, and it is a prefix match on path components, not on bytes.
/// * **There is no `ignore_whitespace`.** Git has three separate flags for it (each with its
///   own semantics), they change what "modified" means, and folding a half-implemented version
///   in here would make `status` and `diff` disagree about which files changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffOptions {
    /// Which line-diff algorithm to use.
    pub algorithm: DiffAlgorithm,
    /// Lines of context around each change, Git's `-U`.
    pub context: usize,
    /// Whether to detect binary content and refuse to diff it as text.
    pub detect_binary: bool,
    /// A path prefix to limit the diff to, empty for everything.
    pub pathspec: BString,
}

impl DiffOptions {
    /// Phase 8 — the options a plain `rivet diff` uses: Myers, three lines of context, binary
    /// detection on, no pathspec.
    pub fn new() -> Self {
        Self {
            algorithm: DiffAlgorithm::Myers,
            context: 3,
            detect_binary: true,
            pathspec: BString::from(""),
        }
    }

    /// Phase 8 — set the context line count.
    pub fn context(&mut self, lines: usize) -> &mut Self {
        self.context = lines;
        self
    }

    /// Phase 8 — select the algorithm.
    pub fn algorithm(&mut self, algorithm: DiffAlgorithm) -> &mut Self {
        self.algorithm = algorithm;
        self
    }

    /// Phase 8 — limit the diff to a path prefix.
    pub fn pathspec(&mut self, prefix: &BStr) -> &mut Self {
        self.pathspec = prefix.to_owned();
        self
    }
}

impl Default for DiffOptions {
    /// Phase 8 — the defaults of [`DiffOptions::new`].
    fn default() -> Self {
        Self {
            algorithm: DiffAlgorithm::Myers,
            pathspec: BString::default(),
            context: 3,
            detect_binary: true,
        }
    }
}

/// Phase 8 — whether a byte slice should be treated as binary.
///
/// Git's rule, in the place both the object model and the diff need it: a NUL byte anywhere in
/// the first 8000 bytes. Exposed here as well as on [`crate::object::Blob`] because a diff may
/// be looking at work tree bytes that were never wrapped in a `Blob`.
///
/// # Caveats
///
/// * **The window is 8000 bytes, not 8192**, and a file whose only NUL is at offset 8001 is
///   "text". That is Git's behaviour and matching it is what the differential tests require.
/// * **It is a heuristic and wrong in both directions.** A UTF-16 file is text and reports as
///   binary; a binary file whose first 8 KB are NUL-free reports as text and produces a diff
///   full of control characters.
/// * **Git caches the answer in `gitattributes`** and honours a `-diff` attribute; Rivet does
///   not implement attributes, so this is the whole rule here — worth knowing before a user's
///   `.gitattributes` appears to be ignored.
pub fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|&b| b == 0)
}

/// Phase 8 — the `-U<n>` context size Git uses when nothing is specified.
///
/// # Caveats
///
/// Three. It is a constant rather than a literal scattered through the module because the
/// differential tests assert hunk boundaries, and a hunk built with a different context is a
/// different-but-valid hunk that fails the comparison for the wrong reason.
pub const DEFAULT_CONTEXT: usize = 3;
