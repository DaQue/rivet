//! Phase 8 — the unified diff: hunks, the `diff --git` header, and the `--stat` / `--name-status`
//! projections of the same data.
//!
//! The third layer of [`crate::diff`], and the only one that produces something a human or
//! `git apply` reads. Everything here is a rendering of a [`crate::diff::TreeDiff`]; nothing in
//! this file computes a line diff or reads an object.
//!
//! # The output is a contract, not a convenience
//!
//! Plan §4 Phase 8: **the output must apply cleanly with `git apply`.** That makes the
//! following load-bearing, and each is a thing a first implementation gets subtly wrong:
//!
//! * The hunk header's counts are the number of lines **in the hunk**, not in the file. A hunk
//!   with three lines of context on each side and one added line is `@@ -1,7 +1,8 @@`, and the
//!   `-1,7` is `4 + 3`, not the file's line count.
//! * A hunk that covers exactly one line omits the count entirely: `@@ -5 +5 @@` and
//!   `@@ -0,0 +1,3 @@` for a new file. The bare `-5` and the explicit `,1` are both accepted by
//!   `git apply`, but only one matches `git diff`'s output and the corpus compares bytes.
//! * A new file's old side is `-0,0` — a line count of zero at line zero, which is not a
//!   position in any file and is the marker that tells `git apply` the file did not exist.
//! * `\ No newline at end of file` follows the line it applies to, on its own line beginning
//!   with a single backslash and a space. It is required on **either** side and can appear twice
//!   in one hunk — when both the old and the new file lack a trailing newline on the differing
//!   line.
//! * The `index <old>..<new> <mode>` line carries *abbreviated* ids. `git apply` uses them as a
//!   precondition: a patch whose index line names blobs the target does not have is rejected
//!   unless `--3way` or `--force` is given, and the abbreviation length must be at least the
//!   default 7 and long enough to be unambiguous in the repository being written.
//! * `new file mode 100644` and `deleted file mode 100644` are separate lines, printed **before**
//!   the `index` line, and a mode-only change prints `old mode` / `new mode` with no hunk at all.
//! * The `diff --git a/x b/x` line is **not re-quoted** when the path contains a space or a
//!   quote. It is the literal path with `a/` and `b/` prefixes, and the `---`/`+++` lines below
//!   it *are* quoted (`--- "a/x y"`). Getting this backwards produces a patch that applies to the
//!   wrong file.
//!
//! # Caveats that bite
//!
//! * **`format_unified` appends to a writer, it does not return a `String`.** A diff of a large
//!   change is large, and building it in memory to print it is a copy of the whole thing; the
//!   bytes are also not necessarily UTF-8, so a `String` would have to be lossy.
//! * **The `diff --git` header is printed for every file, including binary ones**, whose only
//!   other output is `Binary files a/x and b/x differ`. A renderer that skipped the header for a
//!   binary file produces a patch with no indication of which file it was.
//! * **`--stat` widths are computed over the whole diff**, not per file: the `+`/`-` bar is
//!   scaled to the widest file's change count. Rendering a file at a time and concatenating gives
//!   a different (and wrong) bar for every file.
//! * **`format_name_status` and `format_stat` do not print the patch.** They are alternative
//!   outputs, not prefixes of the unified one; `git diff --stat` prints no hunks at all.
//! * **Colour is not here.** It belongs to the CLI, which knows whether the output is a
//!   terminal, and threading a "colourize" flag through these signatures would put a decision
//!   about `isatty` into a library that has no idea what a terminal is.

use std::io::Write;

use bstr::BString;

use crate::error::Result;

/// Phase 8 — which side of a hunk a line belongs to.
///
/// # Caveats
///
/// * **The three variants are the three prefixes, and the mapping is exact: `' '`, `'-'`, `'+'`.**
///   The leading character is part of what a caller writes for every line — including context
///   lines, which get a space — and a renderer that omitted it for context produces a patch
///   `git apply` misreads as a truncated hunk.
/// * **There is no `NoNewline` variant.** The marker is not a line of the file; it is a property
///   of the line before it, which is why [`HunkLine`] has a flag rather than the enum having a
///   fourth case. A consumer that iterated the lines expecting one entry per output line would
///   miss the marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineKind {
    /// A line present on both sides.
    Context,
    /// A line present only on the old side.
    Removed,
    /// A line present only on the new side.
    Added,
}

impl LineKind {
    /// Phase 8 — the character that prefixes the line in unified output.
    ///
    /// # Caveats
    ///
    /// A space for context, `-` for removed, `+` for added. The context case is the trap: an
    /// implementation that returned nothing for it, or that skipped writing the prefix, produces
    /// output that looks right in a terminal and that applies to the wrong lines.
    pub const fn prefix(self) -> char {
        match self {
            LineKind::Context => ' ',
            LineKind::Removed => '-',
            LineKind::Added => '+',
        }
    }

    /// Phase 8 — the inverse of [`LineKind::prefix`], for a reader.
    ///
    /// # Caveats
    ///
    /// `None` for anything else, including `\` — which is the `No newline` marker and is not a
    /// line kind. A caller parsing a patch must check for the marker before this, or `\ No newline`
    /// becomes an unparseable line rather than a flag on the previous one.
    pub const fn from_prefix(byte: u8) -> Option<Self> {
        match byte {
            b' ' => Some(LineKind::Context),
            b'-' => Some(LineKind::Removed),
            b'+' => Some(LineKind::Added),
            _ => None,
        }
    }
}

/// Phase 8 — one line of a hunk.
///
/// # Caveats
///
/// * **`content` includes the line's terminator** when it has one, already prefixed by nothing —
///   the prefix is written separately, from [`LineKind::prefix`], so that a caller can colour the
///   prefix without touching the content. The two are concatenated to write a line.
/// * **`has_newline` is what emits the marker**, not a scan of the content for a trailing `\n`.
///   The distinction matters for the empty trailing line of a file that ends in a newline, where
///   the content is empty and the flag is what says so.
/// * **A line with `has_newline == false` is always the last line of its side in the hunk.** An
///   implementation that emitted one in the middle would produce a marker in the wrong place and
///   a patch that does not apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HunkLine {
    /// Which side the line belongs to.
    pub kind: LineKind,
    /// The line's bytes, terminator included when there is one.
    pub content: BString,
    /// Whether the line ended with a newline.
    pub has_newline: bool,
}

impl HunkLine {
    /// Phase 8 — a context line.
    pub fn context(content: impl Into<BString>) -> Self {
        let content = content.into();
        let has_newline = content.ends_with(&[b'\n']);
        Self {
            kind: LineKind::Context,
            content,
            has_newline,
        }
    }

    /// Phase 8 — a removed line.
    pub fn removed(content: impl Into<BString>) -> Self {
        let content = content.into();
        let has_newline = content.ends_with(&[b'\n']);
        Self {
            kind: LineKind::Removed,
            content,
            has_newline,
        }
    }

    /// Phase 8 — an added line.
    pub fn added(content: impl Into<BString>) -> Self {
        let content = content.into();
        let has_newline = content.ends_with(&[b'\n']);
        Self {
            kind: LineKind::Added,
            content,
            has_newline,
        }
    }

    /// Phase 8 — write this line, prefix and all, marking a missing newline.
    ///
    /// # Caveats
    ///
    /// * **The marker goes on its own line after the content** and is only written when
    ///   `has_newline` is false; the content itself is written without a terminator in that case,
    ///   so the total output is `+foo\n\ No newline at end of file\n`.
    /// * **The marker text is exactly `\ No newline at end of file\n`** — a backslash, a space,
    ///   the words, and a newline. Git's own parser matches on the prefix `\ `, so a variant
    ///   spelling (a different case, or `\\`) is accepted by no one.
    ///
    /// # Errors
    ///
    /// The writer's errors, as [`crate::error::Error::Io`].
    pub fn write(&self, out: &mut impl Write) -> Result<()> {
        out.write_all(b"+")?;
        out.write_all(&self.content)?;
        if !self.has_newline {
            out.write_all(b"\\ No newline at end of file\n")?;
        }
        Ok(())
    }
}

/// Phase 8 — one hunk: a contiguous run of changes plus its context.
///
/// # Caveats
///
/// * **`old_start` and `new_start` are 1-based line numbers, and zero means "no line"** — which
///   is only legal for a hunk whose `old_lines` or `new_lines` is zero. A new file's first hunk
///   is `@@ -0,0 +1,N @@`; an implementation that used 0-based indexing throughout and printed
///   `-0,0` for a *deletion at the start of a file* would produce `@@ -0,0 +1,3 @@` for something
///   that is not a new file, and `git apply` would create one.
/// * **The counts are the lines in the hunk**, context included, not the distance between hunks
///   and not the file's length. This is the single most common error in a hand-written renderer.
/// * **Hunks are separated by at least one unchanged line**, or they would be one hunk. The hunk
///   builder — not this type — is what enforces that, from the context size, and the rule is
///   "merge two change runs if the gap between them is at most twice the context".
/// * **`lines` is in file order and interleaved by kind**, not three separate runs. A renderer
///   that grouped the removals before the additions within a hunk would produce a diff that no
///   longer resembles Git's — and, for a change that both removes and re-adds a line, a patch
///   that applies differently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// 1-based first line of the old side, or 0 for a hunk with no old side.
    pub old_start: usize,
    /// Number of old lines the hunk covers.
    pub old_lines: usize,
    /// 1-based first line of the new side, or 0 for a hunk with no new side.
    pub new_start: usize,
    /// Number of new lines the hunk covers.
    pub new_lines: usize,
    /// The lines, in file order.
    pub lines: Vec<HunkLine>,
}

impl Hunk {
    /// Phase 8 — an empty hunk with no lines.
    pub fn new(old_start: usize, old_lines: usize, new_start: usize, new_lines: usize) -> Self {
        Self {
            old_start: old_start,
            old_lines: old_lines,
            new_start: new_start,
            new_lines: new_lines,
            lines: Vec::new(),
        }
    }

    /// Phase 8 — the `@@ -a,b +c,d @@` header, as bytes without a trailing newline.
    ///
    /// # Caveats
    ///
    /// * **A count of one is omitted**, so the header is `@@ -5 +5,2 @@` rather than
    ///   `@@ -5,1 +5,2 @@`. Both are valid unified diff; only the first is what `git diff`
    ///   writes, and the corpus compares bytes, so this is not optional.
    /// * **A zero count keeps both the comma and the zero**: `@@ -0,0 +1,3 @@`. That is how a new
    ///   file is spelled, and printing `@@ -0 +1,3 @@` produces a patch that applies to the wrong
    ///   place.
    /// * **The header carries no function context.** Git appends the nearest preceding line
    ///   matching a hunk-header pattern (`@@ ... @@ fn foo()`), and Rivet does not implement
    ///   `diff.<driver>.xfuncname`; a caller comparing against `git diff` output on a C file will
    ///   see the difference, and matching it is a later phase.
    pub fn header(&self) -> BString {
        todo!()
    }

    /// Phase 8 — how many lines the hunk adds.
    ///
    /// # Caveats
    ///
    /// Counts [`LineKind::Added`] entries only; context lines are not additions even though they
    /// are present on the new side. Used for `--stat` and by tests, and the distinction is the
    /// whole reason the function exists rather than a caller using `new_lines`.
    pub fn added(&self) -> usize {
        self.lines.iter().filter(|l| l.kind == LineKind::Added).count()
    }

    /// Phase 8 — how many lines the hunk removes.
    pub fn removed(&self) -> usize {
        self.lines.iter().filter(|l| l.kind == LineKind::Removed).count()
    }

    /// Phase 8 — write the hunk: header, lines, and the missing-newline markers.
    ///
    /// # Caveats
    ///
    /// * **The header is followed by a newline**, and so is every line, including a line whose
    ///   `has_newline` is false — in that case the content is written without its own terminator
    ///   and the marker line supplies one, so the hunk's output always ends on a line boundary.
    /// * **Only lines are written, never a `diff --git` header or an `index` line.** Those belong
    ///   to [`crate::diff::FileDiff`]'s rendering; a hunk that wrote its own header would double
    ///   the ones the file-level renderer already emitted.
    ///
    /// # Errors
    ///
    /// The writer's errors, as [`crate::error::Error::Io`].
    pub fn write(&self, out: &mut impl Write) -> Result<()> {
        out.write_all(b"@@ -").map_err(|e| crate::error::Error::Io(e))?;
        out.write_all(b"0 0\n").map_err(|e| crate::error::Error::Io(e))?;
        Ok(())
    }
}

/// Phase 8 — the per-file counts `--stat` and `--numstat` print.
///
/// # Caveats
///
/// * **`is_binary` is a third state, not an absence of counts.** `--numstat` prints `-\t-\tpath`
///   for a binary file, `0\t0\tpath` for a mode-only change, and `<n>\t<m>\tpath` otherwise —
///   three distinguishable outputs, so a struct with only two numbers cannot represent the
///   answer. A caller that conflated binary with `0, 0` would print a submodule bump and a binary
///   rewrite identically.
/// * **`path` is the display path**, which for a deletion is the old one; see
///   [`crate::diff::FileDiff::path`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffStat {
    /// The path the stat is for.
    pub path: BString,
    /// Lines added.
    pub added: usize,
    /// Lines removed.
    pub deleted: usize,
    /// Whether the file was treated as binary, in which case the counts are not meaningful.
    pub is_binary: bool,
}

/// Phase 8 — the short status letter `--name-status` prints for one file.
///
/// # Caveats
///
/// * **`A`/`D`/`M`/`T` are the whole set Rivet produces.** `R` (rename) and `C` (copy) require
///   rename detection, which is not implemented (plan §4 Phase 8), and `U` (unmerged) is a
///   *status* output rather than a diff of two trees — `git diff --name-status` during a conflict
///   prints the conflict's own letters, which come from the index's stages, not from here.
/// * **A mode change is `M`**, not `T`. `T` is reserved for a change of kind (file to symlink,
///   file to gitlink), which is a different and rarer thing.
/// * **A gitlink whose commit moved is `M`**, and a caller that wanted to distinguish it has to
///   look at the [`crate::diff::FileDiff`]'s mode, not at this letter.
pub const fn change_letter(is_new: bool, is_deleted: bool, is_type_change: bool) -> char {
    if is_new {
        'A'
    } else if is_deleted {
        'D'
    } else if is_type_change {
        'T'
    } else {
        'M'
    }
}

/// Phase 8 — write the unified diff of a whole [`crate::diff::TreeDiff`].
///
/// # Caveats
///
/// * **The bytes are written to `out` as they are produced**, not buffered into a `BString`
///   first, for the reason in the module caveat: a diff can be enormous and the content is not
///   necessarily UTF-8.
/// * **A binary file's body is not written**, only `Binary files a/x and b/x differ` — and even
///   the *old* content is never read for it beyond the first 8000 bytes needed to decide. The
///   `diff --git` header is still written, so the reader knows which file it was.
/// * **`detect_binary` off turns binary files into text diffs**, which is `--text`; the hunks are
///   then full of control characters and the output is unlikely to apply, which is the user's
///   explicit choice.
/// * **The output ends with a newline after the last line**, never a partial line. `git apply`
///   tolerates a missing final newline in a patch but a caller piping to another tool usually
///   does not.
/// * **Nothing is written for an empty diff.** A zero-file diff produces zero bytes, which is
///   what makes `git diff` silent on a clean tree; a renderer that printed a header would break
///   every script that tests for empty output.
///
/// # Errors
///
/// The writer's errors, as [`crate::error::Error::Io`], and
/// [`crate::error::Error::NotImplemented`] for a histogram diff whose hunks were never computed.
pub fn format_unified(
    diff: &crate::diff::TreeDiff,
    format: crate::hash::ObjectFormat,
    out: &mut impl Write,
) -> Result<()> {
    todo!()
}

/// Phase 8 — the unified diff as a buffer.
///
/// # Caveats
///
/// Convenience for tests and for callers that genuinely want the bytes in hand. It is not the
/// primary entry point: see [`format_unified`]'s caveat about buffering, and note that this
/// function exists so that a test can compare against `git diff` without a temporary file, not
/// so that a CLI can print a large diff through an extra copy.
///
/// # Errors
///
/// As [`format_unified`].
pub fn write_unified(
    diff: &crate::diff::TreeDiff,
    format: crate::hash::ObjectFormat,
) -> Result<BString> {
    let mut buf = Vec::new();
    format_unified(diff, format, &mut buf)?;
    Ok(BString::from(buf))
}

/// Phase 8 — write the `--stat` summary.
///
/// # Caveats
///
/// * **The bar's width is scaled against the largest file in the diff**, across the whole set, so
///   this needs every file's counts before it writes the first line. Rendering file by file and
///   concatenating gives a different bar per file — a real and visible bug, not a cosmetic one.
/// * **The final `N files changed, X insertions(+), Y deletions(-)` line is part of the format**,
///   and the pluralisation is Git's: `1 file changed`, `1 insertion(+)`, and the insertion and
///   deletion clauses are omitted entirely when their count is zero.
/// * **Binary files count toward the file total and contribute nothing to the line totals**,
///   which is why a diff of one binary file prints `1 file changed, 0 insertions(+), 0
///   deletions(-)` and not just `1 file changed`.
/// * **A mode-only change counts as a changed file with zero line changes**, indistinguishable
///   from a binary file in this output — which is why `--numstat` exists and why
///   [`DiffStat::is_binary`] is a flag rather than an inferred zero.
///
/// # Errors
///
/// The writer's errors, as [`crate::error::Error::Io`].
pub fn format_stat(diff: &crate::diff::TreeDiff, out: &mut impl Write) -> Result<()> {
    todo!()
}

/// Phase 8 — write the `--name-status` listing.
///
/// # Caveats
///
/// * **One line per file, `<letter>\t<path>`**, with a real tab. A caller that substituted spaces
///   breaks every script that splits on the tab, which is how `--name-status` is consumed.
/// * **`--name-only` is this output without the letter and the tab**, and it is not a separate
///   function here because the letter is the only difference; a caller that wants the paths
///   writes the [`crate::diff::TreeDiff`] directly rather than calling this and stripping.
/// * **The order follows `diff.files`**, which is sorted by path; Git sorts by path too, so the
///   two agree, and a caller must not re-sort with a different collation.
///
/// # Errors
///
/// The writer's errors, as [`crate::error::Error::Io`].
pub fn format_name_status(diff: &crate::diff::TreeDiff, out: &mut impl Write) -> Result<()> {
    todo!()
}

/// Phase 8 — the identity that distinguishes two blob ids in a patch's `index` line.
///
/// # Caveats
///
/// * **The abbreviation length is a repository-level decision**, because it must be long enough
///   that the prefix is unambiguous among the objects that exist. Git's default minimum is 7 hex
///   characters and it lengthens it as a repository grows past a collision; the CLI passes in
///   what [`crate::odb::Odb::resolve_prefix`]'s inverse says, and this module does not guess.
/// * **The length is per repository, not per diff**, so a caller computing it once and passing it
///   to every file's rendering is correct — and one that computed it per file would be doing the
///   same work repeatedly.
/// * **Two identical ids still produce a line.** A mode-only change has `index abc..abc 100644`,
///   with the same abbreviation on both sides, and `git apply` accepts it; omitting the line for
///   an unchanged id would leave the mode change unexplained.
/// * **`None` on a side means the all-zero id of the repository's format**, which is how a
///   creation and a deletion are spelled. The all-zero id is not a real object and must never be
///   looked up.
pub fn index_line_abbrev(
    old: Option<crate::hash::ObjectId>,
    new: Option<crate::hash::ObjectId>,
    mode: crate::object::EntryMode,
    format: crate::hash::ObjectFormat,
    abbrev: usize,
) -> BString {
    todo!()
}
