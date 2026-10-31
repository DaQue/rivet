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
use crate::hash::ObjectId;

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
        let has_newline = content.ends_with(b"\n");
        Self {
            kind: LineKind::Context,
            content,
            has_newline,
        }
    }

    /// Phase 8 — a removed line.
    pub fn removed(content: impl Into<BString>) -> Self {
        let content = content.into();
        let has_newline = content.ends_with(b"\n");
        Self {
            kind: LineKind::Removed,
            content,
            has_newline,
        }
    }

    /// Phase 8 — an added line.
    pub fn added(content: impl Into<BString>) -> Self {
        let content = content.into();
        let has_newline = content.ends_with(b"\n");
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
        out.write_all(&[self.kind.prefix() as u8])?;
        out.write_all(&self.content)?;
        if !self.has_newline {
            // The content has no terminator of its own, so the marker needs its line started.
            out.write_all(b"\n\\ No newline at end of file\n")?;
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
    /// Git's "function context": the nearest line before the hunk that starts a letter, `_` or
    /// `$`, cut to 80 bytes. Empty when there is none; shown after the closing `@@`.
    pub section: BString,
}

impl Hunk {
    /// Phase 8 — an empty hunk with no lines.
    pub fn new(old_start: usize, old_lines: usize, new_start: usize, new_lines: usize) -> Self {
        Self {
            old_start,
            old_lines,
            new_start,
            new_lines,
            lines: Vec::new(),
            section: BString::default(),
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
    /// * **The header carries Git's default function context** (see [`Hunk::section`]): the
    ///   nearest preceding line that starts with a letter, `_` or `$`. Rivet does not implement
    ///   `diff.<driver>.xfuncname`, so a file with a custom driver will differ.
    pub fn header(&self) -> BString {
        let mut buf = BString::from("@@ -");
        if self.old_lines == 1 {
            buf.extend_from_slice(self.old_start.to_string().as_bytes());
        } else {
            buf.extend_from_slice(self.old_start.to_string().as_bytes());
            buf.push(b',');
            buf.extend_from_slice(self.old_lines.to_string().as_bytes());
        }
        buf.push(b' ');
        buf.push(b'+');
        if self.new_lines == 1 {
            buf.extend_from_slice(self.new_start.to_string().as_bytes());
        } else {
            buf.extend_from_slice(self.new_start.to_string().as_bytes());
            buf.push(b',');
            buf.extend_from_slice(self.new_lines.to_string().as_bytes());
        }
        buf.extend_from_slice(b" @@");
        if !self.section.is_empty() {
            buf.push(b' ');
            buf.extend_from_slice(&self.section);
        }
        buf
    }

    /// Phase 8 — how many lines the hunk adds.
    ///
    /// # Caveats
    ///
    /// Counts [`LineKind::Added`] entries only; context lines are not additions even though they
    /// are present on the new side. Used for `--stat` and by tests, and the distinction is the
    /// whole reason the function exists rather than a caller using `new_lines`.
    pub fn added(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| l.kind == LineKind::Added)
            .count()
    }

    /// Phase 8 — how many lines the hunk removes.
    pub fn removed(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| l.kind == LineKind::Removed)
            .count()
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
        out.write_all(&self.header())?;
        out.write_all(b"\n")?;
        for line in &self.lines {
            line.write(out)?;
        }
        Ok(())
    }
}

/// Phase 8 — build the hunks for one file from its Myers edit script.
///
/// # Caveats
///
/// * **The edit script covers both sequences**, including the matched runs, so the hunks are
///   sliced out of it rather than re-diffed per hunk.
/// * **Two change runs closer than `2 * context` unchanged lines are one hunk.** That is Git's
///   merge rule: the context of the earlier run and the leading context of the later one would
///   overlap, and two hunks with overlapping context do not apply.
/// * **A hunk with no old (or no new) lines starts at the insertion point and reports a zero
///   count**, which is why the start is derived from the first line it contains rather than from
///   arithmetic on the index.
pub fn build_hunks(
    old: &[u8],
    new: &[u8],
    edits: &[crate::diff::myers::Edit],
    context: usize,
) -> Vec<Hunk> {
    use crate::diff::myers::split_lines;

    let old_lines = split_lines(old);
    let new_lines = split_lines(new);

    #[derive(Clone, Copy)]
    enum Side {
        Ctx(usize, usize),
        Del(usize),
        Add(usize),
    }

    let mut seq: Vec<Side> = Vec::new();
    for edit in edits {
        if edit.old_len > 0 && edit.new_len > 0 {
            for k in 0..edit.old_len {
                seq.push(Side::Ctx(edit.old_index + k, edit.new_index + k));
            }
        } else if edit.old_len > 0 {
            for k in 0..edit.old_len {
                seq.push(Side::Del(edit.old_index + k));
            }
        } else if edit.new_len > 0 {
            for k in 0..edit.new_len {
                seq.push(Side::Add(edit.new_index + k));
            }
        }
    }

    // Group the changed positions into hunk ranges, merging runs whose gap is at most
    // `2 * context` unchanged lines.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (index, side) in seq.iter().enumerate() {
        if matches!(side, Side::Ctx(_, _)) {
            continue;
        }
        match ranges.last_mut() {
            Some(last) if index <= last.1 + 2 * context + 1 => last.1 = index,
            _ => ranges.push((index, index)),
        }
    }
    if ranges.is_empty() {
        return Vec::new();
    }

    let mut hunks = Vec::with_capacity(ranges.len());
    for (first, last) in ranges {
        let start = first.saturating_sub(context);
        let end = (last + context + 1).min(seq.len());

        let mut lines = Vec::with_capacity(end - start);
        let mut old_start = None;
        let mut new_start = None;
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        for side in &seq[start..end] {
            match *side {
                Side::Ctx(o, n) => {
                    old_start.get_or_insert(o + 1);
                    new_start.get_or_insert(n + 1);
                    old_count += 1;
                    new_count += 1;
                    let line = old_lines[o];
                    lines.push(HunkLine {
                        kind: LineKind::Context,
                        content: BString::from(line.content),
                        has_newline: line.has_newline,
                    });
                }
                Side::Del(o) => {
                    old_start.get_or_insert(o + 1);
                    old_count += 1;
                    let line = old_lines[o];
                    lines.push(HunkLine {
                        kind: LineKind::Removed,
                        content: BString::from(line.content),
                        has_newline: line.has_newline,
                    });
                }
                Side::Add(n) => {
                    new_start.get_or_insert(n + 1);
                    new_count += 1;
                    let line = new_lines[n];
                    lines.push(HunkLine {
                        kind: LineKind::Added,
                        content: BString::from(line.content),
                        has_newline: line.has_newline,
                    });
                }
            }
        }

        let old_before = seq[..start]
            .iter()
            .filter(|s| matches!(s, Side::Ctx(_, _) | Side::Del(_)))
            .count();
        let new_before = seq[..start]
            .iter()
            .filter(|s| matches!(s, Side::Ctx(_, _) | Side::Add(_)))
            .count();

        hunks.push(Hunk {
            old_start: if old_count == 0 {
                old_before
            } else {
                old_start.unwrap_or(old_before + 1)
            },
            old_lines: old_count,
            new_start: if new_count == 0 {
                new_before
            } else {
                new_start.unwrap_or(new_before + 1)
            },
            new_lines: new_count,
            lines,
            section: {
                // The line before the hunk's first old line, searching upwards.
                let first = if old_count == 0 {
                    old_before
                } else {
                    old_start.unwrap_or(old_before + 1) - 1
                };
                let mut found = BString::default();
                for index in (0..first.min(old_lines.len())).rev() {
                    let content = old_lines[index].content;
                    if content
                        .first()
                        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_' || *b == b'$')
                    {
                        let mut text: Vec<u8> = content.to_vec();
                        while text.last().is_some_and(|b| b.is_ascii_whitespace()) {
                            text.pop();
                        }
                        text.truncate(80);
                        found = BString::from(text);
                        break;
                    }
                }
                found
            },
        });
    }
    hunks
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
/// The writer's errors, as [`crate::error::Error::Io`]. The diff is formatted as given; both diff
/// algorithms are already resolved into hunks by the time it gets here.
pub fn format_unified(
    diff: &crate::diff::TreeDiff,
    _format: crate::hash::ObjectFormat,
    out: &mut impl Write,
) -> Result<()> {
    let hex = |oid: &crate::hash::ObjectId| -> String {
        // `to_hex` is already sized to the repository's own format, so the short form is a
        // seven-character prefix of *that* width rather than of a hard-coded forty.
        let full = oid.to_hex();
        String::from_utf8_lossy(&full[..7.min(full.len())]).into_owned()
    };
    let zeros = "0000000";

    // A symlink swapped with a regular file prints as the deletion followed by the addition.
    let files = diff.files.iter().flat_map(|file| match &file.split {
        Some(split) => vec![&split.0, &split.1],
        None => vec![file],
    });
    for file in files {
        let path = file.path();
        let mode = file.new_mode.or(file.old_mode);

        let old_name = file.old_path.as_ref().map(|p| &p[..]).unwrap_or(path);
        let new_name = file.new_path.as_ref().map(|p| &p[..]).unwrap_or(path);
        let side = |prefix: &str, name: &[u8]| -> Vec<u8> {
            let mut full = prefix.as_bytes().to_vec();
            full.extend_from_slice(name);
            crate::ops::quote_path(&full, false).to_vec()
        };
        out.write_all(b"diff --git ")?;
        out.write_all(&side("a/", old_name))?;
        out.write_all(b" ")?;
        out.write_all(&side("b/", new_name))?;
        out.write_all(b"\n")?;

        if file.is_rename() {
            if file.old_mode != file.new_mode {
                out.write_all(
                    format!(
                        "old mode {:06o}\n",
                        file.old_mode.map(|m| m.to_u32()).unwrap_or(0o100644)
                    )
                    .as_bytes(),
                )?;
                out.write_all(
                    format!(
                        "new mode {:06o}\n",
                        file.new_mode.map(|m| m.to_u32()).unwrap_or(0o100644)
                    )
                    .as_bytes(),
                )?;
            }
            out.write_all(
                format!("similarity index {}%\n", file.similarity.unwrap_or(100)).as_bytes(),
            )?;
            out.write_all(b"rename from ")?;
            out.write_all(&crate::ops::quote_path(old_name, false))?;
            out.write_all(b"\nrename to ")?;
            out.write_all(&crate::ops::quote_path(new_name, false))?;
            out.write_all(b"\n")?;
            if file.old_oid != file.new_oid {
                out.write_all(b"index ")?;
                out.write_all(
                    file.old_oid
                        .as_ref()
                        .map(hex)
                        .unwrap_or_else(|| zeros.to_string())
                        .as_bytes(),
                )?;
                out.write_all(b"..")?;
                out.write_all(
                    file.new_oid
                        .as_ref()
                        .map(hex)
                        .unwrap_or_else(|| zeros.to_string())
                        .as_bytes(),
                )?;
                if file.old_mode == file.new_mode {
                    out.write_all(
                        format!(" {:06o}", mode.map(|m| m.to_u32()).unwrap_or(0o100644)).as_bytes(),
                    )?;
                }
                out.write_all(b"\n")?;
            }
        } else if file.is_new {
            out.write_all(b"new file mode ")?;
            out.write_all(
                format!("{:06o}\n", mode.map(|m| m.to_u32()).unwrap_or(0o100644)).as_bytes(),
            )?;
            out.write_all(b"index ")?;
            out.write_all(zeros.as_bytes())?;
            out.write_all(b"..")?;
            out.write_all(
                file.new_oid
                    .as_ref()
                    .map(hex)
                    .unwrap_or_else(|| zeros.to_string())
                    .as_bytes(),
            )?;
            out.write_all(b"\n")?;
        } else if file.is_deleted {
            out.write_all(b"deleted file mode ")?;
            out.write_all(
                format!("{:06o}\n", mode.map(|m| m.to_u32()).unwrap_or(0o100644)).as_bytes(),
            )?;
            out.write_all(b"index ")?;
            out.write_all(
                file.old_oid
                    .as_ref()
                    .map(hex)
                    .unwrap_or_else(|| zeros.to_string())
                    .as_bytes(),
            )?;
            out.write_all(b"..")?;
            out.write_all(zeros.as_bytes())?;
            out.write_all(b"\n")?;
        } else if file.old_mode != file.new_mode {
            out.write_all(
                format!(
                    "old mode {:06o}\n",
                    file.old_mode.map(|m| m.to_u32()).unwrap_or(0o100644)
                )
                .as_bytes(),
            )?;
            out.write_all(
                format!(
                    "new mode {:06o}\n",
                    file.new_mode.map(|m| m.to_u32()).unwrap_or(0o100644)
                )
                .as_bytes(),
            )?;
            // A pure mode change keeps the blob, and Git prints no `index` line for it.
            if file.old_oid != file.new_oid {
                out.write_all(b"index ")?;
                out.write_all(
                    file.old_oid
                        .as_ref()
                        .map(hex)
                        .unwrap_or_else(|| zeros.to_string())
                        .as_bytes(),
                )?;
                out.write_all(b"..")?;
                out.write_all(
                    file.new_oid
                        .as_ref()
                        .map(hex)
                        .unwrap_or_else(|| zeros.to_string())
                        .as_bytes(),
                )?;
                out.write_all(b"\n")?;
            }
        } else {
            out.write_all(b"index ")?;
            out.write_all(
                file.old_oid
                    .as_ref()
                    .map(hex)
                    .unwrap_or_else(|| zeros.to_string())
                    .as_bytes(),
            )?;
            out.write_all(b"..")?;
            out.write_all(
                file.new_oid
                    .as_ref()
                    .map(hex)
                    .unwrap_or_else(|| zeros.to_string())
                    .as_bytes(),
            )?;
            out.write_all(b" ")?;
            out.write_all(
                format!("{:06o}\n", mode.map(|m| m.to_u32()).unwrap_or(0o100644)).as_bytes(),
            )?;
        }

        if file.is_binary {
            out.write_all(b"Binary files ")?;
            if file.is_new {
                out.write_all(b"/dev/null")?;
            } else {
                out.write_all(&side("a/", old_name))?;
            }
            out.write_all(b" and ")?;
            if file.is_deleted {
                out.write_all(b"/dev/null")?;
            } else {
                out.write_all(&side("b/", new_name))?;
            }
            out.write_all(b" differ\n")?;
            continue;
        }

        if file.hunks.is_empty() {
            continue;
        }

        // Git ends a name containing a space with a tab so a patch reader can find its end.
        let tab = |name: &[u8]| -> &'static [u8] { if name.contains(&b' ') { b"\t" } else { b"" } };
        out.write_all(b"--- ")?;
        if file.is_new {
            out.write_all(b"/dev/null\n")?;
        } else {
            out.write_all(&side("a/", old_name))?;
            out.write_all(tab(old_name))?;
            out.write_all(b"\n")?;
        }
        out.write_all(b"+++ ")?;
        if file.is_deleted {
            out.write_all(b"/dev/null\n")?;
        } else {
            out.write_all(&side("b/", new_name))?;
            out.write_all(tab(new_name))?;
            out.write_all(b"\n")?;
        }

        for hunk in &file.hunks {
            hunk.write(out)?;
        }
    }
    Ok(())
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
    format_stat_sized(diff, None, out)
}

/// [`format_stat`], with binary files shown as `Bin <old> -> <new> bytes` the way Git prints
/// them, which needs the object database to size the blobs.
pub fn format_stat_sized(
    diff: &crate::diff::TreeDiff,
    odb: Option<&crate::odb::Odb>,
    out: &mut impl Write,
) -> Result<()> {
    // Git's `show_stats` with the non-terminal width of 80 columns.
    format_stat_width(diff, odb, 80, out)
}

/// [`format_stat_sized`] for a total width of `width` columns; `format-patch` uses 72.
pub fn format_stat_width(
    diff: &crate::diff::TreeDiff,
    odb: Option<&crate::odb::Odb>,
    width: usize,
    out: &mut impl Write,
) -> Result<()> {
    // Git prints nothing at all for an empty diff, not a `0 files changed` line.
    if diff.stats.is_empty() {
        return Ok(());
    }
    let decimal_width = |n: usize| n.to_string().len();
    let size_of = |oid: Option<crate::hash::ObjectId>| -> usize {
        match (oid, odb) {
            (Some(oid), Some(odb)) => odb
                .read_header(&oid)
                .map(|(_, size)| size as usize)
                .unwrap_or(0),
            _ => 0,
        }
    };

    let mut max_change = 0usize;
    let mut name_width = 0usize;
    let mut number_width = 0usize;
    for (stat, file) in diff.stats.iter().zip(diff.files.iter()) {
        name_width = name_width.max(
            String::from_utf8_lossy(&stat_name(stat, file))
                .chars()
                .count(),
        );
        if stat.is_binary {
            // "Bin" occupies the number column.
            number_width = number_width.max(3);
            let _ = file;
        } else {
            max_change = max_change.max(stat.added + stat.deleted);
        }
    }
    number_width = number_width.max(decimal_width(max_change));
    let mut graph_width = max_change;
    if name_width + number_width + 6 + graph_width > width {
        let limit = (width * 3 / 8).saturating_sub(number_width + 6);
        if graph_width > limit {
            graph_width = limit.max(6);
        }
        if name_width > width.saturating_sub(number_width + 6 + graph_width) {
            name_width = width.saturating_sub(number_width + 6 + graph_width);
        } else {
            graph_width = width - number_width - 6 - name_width;
        }
    }
    // Git's `scale_linear`: a non-zero count always gets at least one mark.
    let scale = |n: usize| -> usize {
        if n == 0 {
            0
        } else {
            1 + n * (graph_width.saturating_sub(1)) / max_change
        }
    };

    let mut total_added = 0usize;
    let mut total_deleted = 0usize;
    for (stat, file) in diff.stats.iter().zip(diff.files.iter()) {
        let mut name = String::from_utf8_lossy(&stat_name(stat, file)).into_owned();
        if name.len() > name_width {
            // Git keeps the tail of a long name behind "...".
            let keep = name_width.saturating_sub(3);
            let cut = name.len() - keep;
            let mut start = cut;
            while !name.is_char_boundary(start) {
                start += 1;
            }
            name = format!("...{}", &name[start..]);
        }
        let mut line = format!(" {name:<name_width$} |");
        if stat.is_binary {
            let old = size_of(file.old_oid);
            let new = size_of(file.new_oid);
            line.push_str(&format!(" {:>number_width$} {old} -> {new} bytes", "Bin"));
        } else {
            let total = stat.added + stat.deleted;
            total_added += stat.added;
            total_deleted += stat.deleted;
            let (mut plus, mut minus) = (stat.added, stat.deleted);
            if graph_width <= max_change {
                let mut scaled = scale(total);
                if scaled < 2 && plus > 0 && minus > 0 {
                    scaled = 2;
                }
                if plus < minus {
                    plus = scale(plus);
                    minus = scaled - plus;
                } else {
                    minus = scale(minus);
                    plus = scaled - minus;
                }
            }
            line.push_str(&format!(" {total:>number_width$}"));
            if total > 0 {
                line.push(' ');
            }
            line.push_str(&"+".repeat(plus));
            line.push_str(&"-".repeat(minus));
        }
        line.push('\n');
        out.write_all(line.as_bytes())?;
    }
    let files = diff.stats.len();
    let mut summary = format!(
        " {} file{} changed",
        files,
        if files == 1 { "" } else { "s" }
    );
    if total_added > 0 || total_deleted == 0 {
        summary.push_str(&format!(
            ", {} insertion{}(+)",
            total_added,
            if total_added == 1 { "" } else { "s" }
        ));
    }
    if total_deleted > 0 || total_added == 0 {
        summary.push_str(&format!(
            ", {} deletion{}(-)",
            total_deleted,
            if total_deleted == 1 { "" } else { "s" }
        ));
    }
    summary.push('\n');
    out.write_all(summary.as_bytes())?;
    Ok(())
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
    for file in &diff.files {
        if file.is_rename() {
            out.write_all(format!("R{:03}\t", file.similarity.unwrap_or(100)).as_bytes())?;
            out.write_all(&crate::ops::quote_path(
                file.old_path.as_ref().map(|p| &p[..]).unwrap_or(&b""[..]),
                false,
            ))?;
            out.write_all(b"\t")?;
            out.write_all(&crate::ops::quote_path(file.path(), false))?;
            out.write_all(b"\n")?;
            continue;
        }
        let letter = change_letter(file.is_new, file.is_deleted, file.is_type_change());
        out.write_all(letter.to_string().as_bytes())?;
        out.write_all(b"\t")?;
        out.write_all(&crate::ops::quote_path(file.path(), false))?;
        out.write_all(b"\n")?;
    }
    Ok(())
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
    let old = old.unwrap_or_else(|| ObjectId::zero(format));
    let new = new.unwrap_or_else(|| ObjectId::zero(format));
    let old_hex = old.to_hex();
    let new_hex = new.to_hex();
    let old_abbr = BString::from(&old_hex[..abbrev.min(old_hex.len())]);
    let new_abbr = BString::from(&new_hex[..abbrev.min(new_hex.len())]);
    BString::from(format!(
        "index {}..{} {}\n",
        old_abbr,
        new_abbr,
        mode.to_string()
    ))
}

/// The name column of a `--stat` line: the quoted path, or `old => new` for a rename.
fn stat_name(stat: &DiffStat, file: &crate::diff::FileDiff) -> Vec<u8> {
    if file.is_rename() {
        let old = file.old_path.as_ref().map(|p| &p[..]).unwrap_or(&b""[..]);
        let new = file.path();
        let (qo, qn) = (
            crate::ops::quote_path(old, false),
            crate::ops::quote_path(new, false),
        );
        if qo.as_slice() != old || qn.as_slice() != new {
            let mut out = qo.to_vec();
            out.extend_from_slice(b" => ");
            out.extend_from_slice(&qn);
            return out;
        }
        return crate::diff::rename::pprint_rename(old, new).to_vec();
    }
    crate::ops::quote_path(&stat.path[..], false).to_vec()
}
