//! Phase 5 — ignore rules: `.gitignore`, `info/exclude` and `core.excludesFile`.
//!
//! Ignoring files is how a repository says "not everything here is mine". Rivet needs it for
//! `status` and `add` — an ignored file must not appear as untracked, and must not be staged by a
//! bare `rivet add` — and `check-ignore -v` needs it to explain *which* rule decided, because a
//! user debugging their ignore rules is the whole reason that command exists.
//!
//! # Pattern semantics, which are not glob and not fnmatch
//!
//! A `.gitignore` line is a pattern in Git's own dialect, and every rule below is a place a
//! reasonable implementation goes wrong:
//!
//! * **A trailing `/` means directories only.** `build/` ignores the directory `build` and
//!   everything under it, but never a *file* named `build`. And because a directory cannot be
//!   re-included once excluded, `!build/keep` has no effect at all — an ignored directory's
//!   contents are ignored wholesale, and Git does not even look inside.
//! * **A `/` anywhere but the end anchors the pattern to the `.gitignore`'s own directory.**
//!   `doc/frotz` matches `<dir>/doc/frotz` and not `<dir>/a/doc/frotz`, while a pattern with no
//!   slash at all matches at any depth: `frotz` matches `a/b/frotz` too.
//! * **A leading `/` is the same anchor**, spelled explicitly. `/frotz` is identical to `frotz` in
//!   a root `.gitignore` and different in a nested one — which is the point.
//! * **`*` does not match `/`.** `src/*.rs` matches `src/main.rs` and not `src/nested/main.rs`,
//!   which is the difference from globstar-matching `*`. To cross directories you need `**`.
//! * **`**` has three meanings, distinguished by its neighbours.** `**/foo` matches `foo` at any
//!   depth; `foo/**` matches everything under `foo`; and `a/**/b` matches `a/b`, `a/x/b` and
//!   `a/x/y/b`. Anywhere else — `a**b` — two consecutive asterisks are just two asterisks, which is
//!   the same as one. A matcher that expands `**` to "any characters including `/" unconditionally
//!   gets two of the three cases wrong.
//! * **`?` matches any single character except `/`**, and `[...]` is a character class that may be
//!   negated with `!` or `^` — but the class is the one place a `!` does *not* mean a negation
//!   pattern.
//! * **A backslash escapes the next character**, anywhere. `\#foo` is a file literally named
//!   `#foo`, `\!foo` is one named `!foo`, and a trailing `\` is... undefined, which is why Git
//!   treats a pattern that is only backslashes as matching nothing rather than as an error.
//! * **`#` at the start of a line is a comment**, and so is an empty line; `#` anywhere else is a
//!   literal hash. A line of only whitespace is a comment too — but *trailing* whitespace on a
//!   pattern is significant unless escaped, which is a rule almost nobody knows and which is why a
//!   pattern copied out of a web page with a trailing space silently stops matching.
//!
//! # Priority order, which is the other half of the semantics
//!
//! Sources are consulted from highest priority to lowest, and within a source the **last** matching
//! rule wins — so a `!` on a later line overrides an earlier exclusion. The order is:
//!
//! 1. patterns given on the command line (`git add -f` overrides everything; a pathspec does not
//!    participate at all — it selects, it does not ignore)
//! 2. `.gitignore` files, from the directory containing the path **upward** to the work tree root —
//!    a nested `.gitignore` beats the one above it, and both beat
//! 3. `.git/info/exclude`, which is per-repository but not committed
//! 4. `core.excludesFile`, usually `~/.config/git/ignore`, which is per-user
//!
//! # The rule that catches people
//!
//! **It is not possible to re-include a file if a parent directory is excluded.** Git's own
//! documentation says so, and it is not an implementation shortcut: when a directory is excluded,
//! Git never descends into it, so the negation inside is never read. `build/` plus `!build/keep.txt`
//! therefore ignores `keep.txt` too. This is why [`IgnoreStack::is_ignored`] takes a **path** and not
//! a set of path components — the decision genuinely depends on walking the ancestors, and a matcher
//! that tests the trailing element alone gets this backwards.
//!
//! # Caveats that bite
//!
//! * **Tracked files are never ignored.** An ignore rule stops a file *becoming* tracked; once it is
//!   in the index, `status` reports its changes and `add` stages them, whatever the rules say. That
//!   is a property of the caller — this module has no index — but every caller has to apply it, and
//!   a `status` that filters by ignore rules without that check reports a modified tracked file as
//!   missing.
//! * **Paths are bytes.** A `.gitignore` may be UTF-8 and a filename need not be; matching is
//!   byte-exact and never transcodes. A pattern that was written in one encoding will not match a
//!   path in another, and Git behaves the same way.
//! * **A `.gitignore` applies to the directory it is in and below**, not to the whole repository.
//!   The anchoring rules above are all relative to that directory, and a matcher that keeps only the
//!   pattern and drops the base directory produces anchors that point at the repository root.
//! * **The files are read lazily.** A work tree walk touches `.gitignore` once per directory it
//!   enters, and reading every `.gitignore` in a monorepo up front is expensive for a command that
//!   only looks at one path. [`IgnoreStack`] is therefore built for a directory and pushed as the
//!   walk descends.
//! * **A missing `.gitignore` is not an error**, and neither is an unreadable one — Git warns on the
//!   latter and continues ignoring nothing from it.
//! * **`git check-ignore` and this module must agree exactly**, which is why
//!   [`IgnoreStack::check`] returns the [`Pattern`] that decided rather than a `bool`: the
//!   differential corpus for Phase 5 (plan §4) ports Git's `t0008` and compares the *matching rule's
//!   source and line*, not just the verdict.

use std::path::{Path, PathBuf};

use bstr::{BStr, BString};

use crate::error::Result;

/// Phase 5 — where a set of patterns came from.
///
/// # Caveats
///
/// * **The order of these variants is not the priority order.** Priority is a property of the
///   *stack*, not of the source kind: a `.gitignore` in a subdirectory outranks one above it, and
///   command-line patterns outrank every file. A caller that sorted by this enum would put all
///   `.gitignore`s together and lose the nesting.
/// * **The variant is carried through to the report**, which is why a negation still needs to know
///   where it came from: `check-ignore -v` prints the source path and the line number, and a user
///   resolving an unexpected ignore needs both.
/// * [`IgnoreSource::CommandLine`] has no file and no line; a report renders it as such rather than
///   inventing one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum IgnoreSource {
    /// A pattern given directly on the command line, which outranks every file.
    CommandLine,
    /// A `.gitignore` in a directory of the work tree. Carries its path relative to the work tree
    /// root, so a report can name it.
    Gitignore(BString),
    /// `.git/info/exclude`, which is not committed and not shared.
    InfoExclude,
    /// The per-user file named by `core.excludesFile`.
    CoreExcludesFile,
}

impl IgnoreSource {
    /// Phase 5 — the source's path, for `check-ignore -v` output.
    ///
    /// # Caveats
    ///
    /// `None` for [`IgnoreSource::CommandLine`], which has no file — `check-ignore -v` prints
    /// `::` for the two empty fields rather than omitting them, so a renderer must not treat this as
    /// "print nothing".
    pub fn path(&self) -> Option<&BStr> {
        match self {
            IgnoreSource::Gitignore(p) => Some(p.as_ref()),
            _ => None
        }
    }

    /// Phase 5 — how a report names this source.
    ///
    /// # Caveats
    ///
    /// `.gitignore` files report their work-tree-relative path; the other two report their own
    /// spelling (`.git/info/exclude` and the excludes file's path). The string is for a user reading
    /// `check-ignore -v`, not for a parser.
    pub fn describe(&self) -> BString {
        match self {
            IgnoreSource::CommandLine => BString::from("command line"),
            IgnoreSource::Gitignore(p) => p.clone(),
            IgnoreSource::InfoExclude => BString::from(".git/info/exclude"),
            IgnoreSource::CoreExcludesFile => BString::from(".git/info/exclude"),
        }
    }
}

/// Phase 5 — the kind of file a pattern's result applies to.
///
/// # Caveats
///
/// * **The distinction only exists for the decisions this module makes.** A pattern ending in `/`
///   ignores a directory and everything under it; deciding that requires knowing whether the path in
///   hand *is* a directory, which means a caller that has only a path string cannot get the answer
///   right. A walk knows — it stat'd the entry — and must pass what it found.
/// * **`Other` covers a gitlink and any file type the filesystem reports that is not a regular file
///   or a directory.** A submodule is a directory on disk and an ignore rule matching a directory
///   does apply to it, so the caller that has one should report `Directory`; `Other` is for the
///   cases a caller genuinely cannot classify.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PathKind {
    /// A regular file, including a symlink (which Git ignores as a unit, never following it).
    File,
    /// A directory.
    Directory,
    /// Neither, or unknown — a socket, a fifo, a gitlink the caller did not classify.
    Other,
}

/// Phase 5 — one parsed `.gitignore` line.
///
/// # Caveats
///
/// * **The pattern is stored in its resolved form**: the leading `!` and the trailing `/` are
///   *removed* and recorded as the [`Pattern::negated`] and [`Pattern::directory_only`] flags, and
///   the anchoring is recorded as [`Pattern::anchored`]. Keeping the raw line and re-interpreting it
///   at match time is how a matcher ends up applying the `!` twice.
/// * **`anchored` is a property of the pattern text**, decided once at parse: true when the pattern
///   contained a `/` anywhere but the very end, or began with one. A pattern with no slash is
///   unanchored and matches at any depth.
/// * **The `matches` method takes a path relative to the pattern's base directory**, not one relative
///   to the work tree root. Getting this wrong makes every anchored pattern in a nested `.gitignore`
///   point at the root, which is the single most common bug in an ignore implementation and the
///   reason [`Pattern::base`] exists.
/// * **A pattern Git would treat as a comment is not represented here.** Blank lines and `#`-led
///   lines are reported as `Ok(None)` by [`Pattern::parse`] rather than becoming a pattern that never
///   matches — the two are indistinguishable at match time, but the *source line* matters to
///   `check-ignore -v`, and dropping the line entirely is what keeps a comment from being reported as
///   the rule that ignored something.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    /// The pattern with `!`, a trailing `/` and a leading `/` removed, and anchors resolved.
    pub text: BString,
    /// Whether the pattern was written with a leading `!`, i.e. it re-includes.
    pub negated: bool,
    /// Whether the pattern ended with `/`, i.e. it matches directories only.
    pub directory_only: bool,
    /// Whether the pattern is anchored to its base directory rather than matching at any depth.
    pub anchored: bool,
    /// Where the pattern came from and on which line, for reporting.
    pub source: IgnoreSource,
    /// The 1-based line number within the source, or 0 for a pattern with no source line — a
    /// command-line pattern.
    pub line: usize,
    /// The directory the pattern's anchors are relative to, as a work-tree-relative path. Empty for
    /// a pattern from a root `.gitignore` or from `core.excludesFile`.
    pub base: BString,
}

impl Pattern {
    /// Phase 5 — parse one `.gitignore` line.
    ///
    /// # Caveats
    ///
    /// * **A comment or blank line is `Ok(None)`**, not an error and not an empty pattern. See the
    ///   type caveat on why it must not become a pattern.
    /// * **Trailing whitespace is part of the pattern unless escaped.** `foo ` matches a file named
    ///   `foo ` — with a space — not `foo`. This is Git's documented rule and the reason a pattern
    ///   pasted from documentation with a trailing space stops working. Leading whitespace is *not*
    ///   significant, which is the asymmetry.
    /// * **A trailing `/` is stripped before anything else except the `!`**, so `!/foo/` is a
    ///   negated directory-anchored pattern for `foo`. Handling the markers in the other order leaves
    ///   a `/` in the text that then looks like an anchor and changes the match.
    /// * **`\` escapes the next character**, and a line consisting only of backslashes is a pattern
    ///   that matches nothing — Git's response to a genuinely undefined case, reproduced rather than
    ///   rejected because refusing would make a repository Git reads unlistable.
    /// * **`base` is supplied by the caller**, because the line does not know which directory it was
    ///   read from and a pattern read from a nested `.gitignore` is meaningless without it.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidIgnorePattern`] for a pattern that would match nothing at all —
    /// one that is empty after the markers are stripped, or that consists only of `/` characters.
    /// Everything else Git accepts is accepted here, including patterns whose meaning is surprising.
    pub fn parse(
        line: &BStr,
        source: IgnoreSource,
        line_number: usize,
        base: &BStr,
    ) -> Result<Option<Self>> {
        todo!()
    }

    /// Phase 5 — parse a whole file's contents into patterns.
    ///
    /// # Caveats
    ///
    /// * **Line numbers start at 1** and count physical lines, so a report names the line a user's
    ///   editor shows.
    /// * **A `\r\n` line ending is handled**, since a `.gitignore` checked out on Windows and copied
    ///   to Linux keeps its CRs — without this, every pattern in the file would end in `\r` and match
    ///   nothing, which looks like the file being ignored entirely.
    /// * **A file that is not valid UTF-8 parses fine.** The patterns are bytes; there is no
    ///   decoding step to fail.
    /// * Comments and blanks are dropped, so the returned vector's indices do **not** correspond to
    ///   line numbers — the [`Pattern::line`] field does.
    ///
    /// # Errors
    ///
    /// Any [`crate::error::Error::InvalidIgnorePattern`] from [`Pattern::parse`] for a line that is
    /// not a comment.
    pub fn parse_file(data: &[u8], source: IgnoreSource, base: &BStr) -> Result<Vec<Self>> {
        let mut out = Vec::new();
        let mut line_number = 1usize;
        for line in data.split(|b| *b == b'\n') {
            let trimmed = line.strip_suffix(b"\r").unwrap_or(line);
            let trimmed = trimmed.trim_ascii();
            if !trimmed.is_empty() && !trimmed.starts_with(b"#") {
                let pattern = Pattern::parse(BStr::new(trimmed), source.clone(), line_number, base)?;
                if let Some(p) = pattern {
                    out.push(p);
                }
            }
            line_number += 1;
        }
        Ok(out)
    }

    /// Phase 5 — whether this pattern matches a path.
    ///
    /// # Caveats
    ///
    /// * **`path` is relative to [`Pattern::base`]**, not to the work tree root. A caller that
    ///   passes a root-relative path to a pattern from a nested `.gitignore` gets a match on the
    ///   wrong thing, or on nothing.
    /// * **`kind` decides whether a directory-only pattern applies**, so it must be the filesystem's
    ///   answer for this path, not a guess from the path's spelling — a path with no extension is not
    ///   a directory, and a path ending in `/` is not how Git spells one.
    /// * **`is_dir_ancestor` is what makes the "cannot re-include" rule work.** A `build/` pattern
    ///   must be reported as matching the *directory* `build` — and, for the ancestor check the
    ///   caller performs, as matching when the path is `build/x/y.txt`. The matching itself is
    ///   prefix-based: a directory pattern matches a path when the pattern matches one of the path's
    ///   leading components.
    /// * **This answers "does this rule match", not "is this path ignored".** The negation and
    ///   priority rules are [`IgnoreStack`]'s; a caller testing patterns one at a time will get a
    ///   negated pattern backwards.
    pub fn matches(&self, path: &BStr, kind: PathKind) -> bool {
        self.is_inapplicable(kind) || self.matches_ancestor(path, kind)
    }

    /// Phase 5 — whether this pattern matches a path or any of its ancestors.
    ///
    /// The check behind the "an excluded directory cannot be re-included" rule: a `!build/keep.txt`
    /// has no effect when `build/` matched an ancestor of the path.
    ///
    /// # Caveats
    ///
    /// **A directory-only pattern can match an ancestor even when `kind` is [`PathKind::File`]**,
    /// because the ancestor of a file is a directory whatever the file is. A caller that passed the
    /// path's own kind through to every prefix would miss exactly the case this exists for.
    pub fn matches_ancestor(&self, path: &BStr, kind: PathKind) -> bool {
        self.matches(path, kind)
        || {
            let mut p = path.to_vec();
            while p.pop().is_some() {
                if self.matches(BStr::new(&p), kind) {
                    return true;
                }
            }
            false
        }
    }

    /// Phase 5 — whether this pattern has no effect on a path of the given kind.
    ///
    /// # Caveats
    ///
    /// True for a directory-only pattern and a file path, and for nothing else. This is not the
    /// negation of [`Pattern::matches`] — it is the narrower question "is this rule even applicable
    /// here", which `check-ignore -v` needs to explain why a rule a user expected to fire did not.
    pub fn is_inapplicable(&self, kind: PathKind) -> bool {
        self.directory_only && kind == PathKind::File
    }

    /// Phase 5 — the pattern's origin as `path:line`, for a report.
    ///
    /// # Caveats
    ///
    /// A command-line pattern has no path and no meaningful line, so it renders as `::` — the two
    /// empty fields Git's own `check-ignore -v` prints. That is what makes the output
    /// distinguishable from a real file at line 0, which cannot exist.
    pub fn origin(&self) -> BString {
        match self.source {
            IgnoreSource::CommandLine => BString::from("::"),
            _ => {
                let path = self.source.path().map(|p| p.to_string()).unwrap_or_default();
                BString::from(format!("{}:{}", path, self.line))
            }
        }
    }
}

/// Phase 5 — the patterns in effect for one directory, from every contributing source.
///
/// # Caveats
///
/// * **Built incrementally as a walk descends**, because the priority order is per-directory: the
///   `.gitignore` of the directory being scanned outranks every `.gitignore` above it, which in turn
///   outrank `info/exclude` and `core.excludesFile`. [`IgnoreStack::push_dir`] is what maintains that
///   invariant, and a stack built all at once from the root has the nesting order backwards.
/// * **The command-line patterns are checked first, always**, whatever the stack is doing — that is
///   the highest priority in Git's order.
/// * **A `.gitignore` may itself be ignored**, and Git does not care: the file is read for patterns
///   whether or not the rules say it should be tracked. Special-casing it would make a repository's
///   behaviour depend on itself.
/// * **The stack is not a set.** The same pattern text can appear from two sources with different
///   anchors and both are kept; deduplicating would change which one wins.
/// * **A stack is cheap to clone and holds parsed patterns, not file contents.** A walk that descends
///   and ascends should push and truncate rather than rebuild, which is what [`IgnoreStack::truncate`]
///   is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IgnoreStack {
    /// The work tree root the stack is anchored to.
    root: PathBuf,
    /// The directory the stack currently describes, work-tree-relative and empty at the root.
    current_dir: BString,
    /// Frames in priority order, lowest first, so a reverse scan finds the last matching rule.
    frames: Vec<IgnoreFrame>,
}

/// Phase 5 — one group of patterns from one source, at one directory depth.
///
/// # Caveats
///
/// A frame is a unit because priority is a property of the *source and its depth*, not of an
/// individual pattern: all the patterns in one `.gitignore` share a base directory and a relative
/// priority, and splitting them per pattern would lose the base. The last pattern *within* a frame
/// still beats the earlier ones, so matching is a reverse scan of each frame in turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IgnoreFrame {
    /// The source these patterns came from.
    pub source: IgnoreSource,
    /// The directory the patterns' anchors are relative to, work-tree-relative. Empty at the root.
    pub base: BString,
    /// How far below the work tree root the frame's directory is, for priority ordering.
    ///
    /// # Caveats
    ///
    /// The depth is what makes two `.gitignore` frames comparable: a frame at depth 3 beats one at
    /// depth 1 regardless of where in the file the patterns sit. It is stored rather than derived
    /// from [`IgnoreFrame::base`] because a base path with an unusual component count would otherwise
    /// silently misorder — and because deriving it means counting separators on every comparison.
    pub depth: usize,
    /// The patterns, in file order.
    pub patterns: Vec<Pattern>,
}

impl IgnoreFrame {
    /// Phase 5 — parse a frame from a `.gitignore`'s contents.
    ///
    /// # Errors
    ///
    /// Any [`crate::error::Error::InvalidIgnorePattern`] from [`Pattern::parse_file`].
    pub fn parse(data: &[u8], source: IgnoreSource, base: &BStr, depth: usize) -> Result<Self> {
        todo!()
    }

    /// Phase 5 — the frame's patterns, in file order.
    pub fn patterns(&self) -> &[Pattern] {
        &self.patterns
    }

    /// Phase 5 — the frame's source.
    pub fn source(&self) -> &IgnoreSource {
        &self.source
    }
}

/// Phase 5 — the decision on one path: ignored, re-included, or no rule at all.
///
/// # Caveats
///
/// * **Three outcomes, not two.** "Ignored" and "not ignored because a negation matched" are
///   different from "no rule mentioned this path", and `check-ignore` distinguishes them: the first
///   prints the matching rule, the second prints nothing and exits 1, the third prints nothing and
///   exits 1 as well — but a caller *composing* stacks needs the difference, because a negation in a
///   higher-priority source means a lower-priority source must not be consulted at all.
/// * **A negated match does not mean "not ignored"** in the presence of an excluded ancestor. The
///   stack reports the negation, and [`IgnoreStack::is_ignored`] is what folds that together with
///   the ancestor rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IgnoreDecision {
    /// A pattern matched and it was not negated.
    Ignored,
    /// A negated pattern matched, re-including the path unless an ancestor is excluded.
    ReIncluded,
    /// No pattern in any source matched.
    Unmatched,
}

impl IgnoreDecision {
    /// Phase 5 — the pattern that produced this decision, if any.
    ///
    /// # Caveats
    ///
    /// `None` for [`IgnoreDecision::Unmatched`]. For a negation it returns the negating pattern,
    /// which is what `check-ignore -v` prints for a path that is *not* ignored — the report names the
    /// rule that saved it, not the one that condemned it.
    pub fn pattern(&self) -> Option<&Pattern> {
        None
    }

    /// Phase 5 — whether this decision re-includes.
    pub const fn is_reincluded(&self) -> bool {
        matches!(self, IgnoreDecision::ReIncluded)
    }

    /// Phase 5 — whether this decision ignores.
    ///
    /// # Caveats
    ///
    /// True only for [`IgnoreDecision::Ignored`]. A caller asking "should I treat this path as
    /// ignored" wants [`IgnoreStack::is_ignored`], which also applies the ancestor rule; this is the
    /// raw answer of one match.
    pub const fn is_ignored(&self) -> bool {
        matches!(self, IgnoreDecision::Ignored)
    }
}

impl IgnoreStack {
    /// Phase 5 — an empty stack anchored at a work tree root.
    ///
    /// # Caveats
    ///
    /// The root is the **work tree**, not the git directory: `.gitignore` files live beside the files
    /// they describe, and every path handed to [`IgnoreStack::is_ignored`] is relative to this root.
    /// A bare repository has no work tree and therefore no ignore rules, which is why the constructor
    /// takes a path rather than a repository.
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            current_dir: BString::from(""),
            frames: Vec::new(),
        }
    }

    /// Phase 5 — the work tree root the stack is anchored to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Phase 5 — the directory the stack currently describes.
    ///
    /// # Caveats
    ///
    /// Work-tree-relative and empty at the root, which is the same spelling [`Pattern::base`] uses —
    /// so a caller can hand it straight to [`IgnoreStack::is_ignored`] as a prefix without
    /// re-deriving it and risking a leading slash that would break every anchor comparison.
    pub fn current_dir(&self) -> &BStr {
        use std::convert::AsRef;
        self.current_dir.as_ref()
    }

    /// Phase 5 — add the patterns given on the command line.
    ///
    /// # Caveats
    ///
    /// These outrank every file and are matched first, so they live in their own frame at a depth
    /// that cannot be beaten. `git add -f` is the opposite — it bypasses the whole stack — and is a
    /// caller-side decision rather than something an added pattern can express.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidIgnorePattern`] for an unparseable pattern. A command line is
    /// the one place an error is worth raising rather than skipping, because a user is watching.
    pub fn add_command_line(&mut self, patterns: &[BString]) -> Result<()> {
        let mut frame = IgnoreFrame {
            source: IgnoreSource::CommandLine,
            base: BString::from(""),
            depth: 0,
            patterns: Vec::new(),
        };
        for p in patterns {
            let line = BStr::new(&p[..]);
            let parsed = Pattern::parse(line, IgnoreSource::CommandLine, frame.patterns.len() + 1, BStr::new(&frame.base[..]))?;
            if let Some(pat) = parsed {
                frame.patterns.push(pat);
            }
        }
        self.frames.push(frame);
        Ok(())
    }

    /// Phase 5 — push the `.gitignore` files for a directory being entered.
    ///
    /// The operation a walk performs on descending. It reads the directory's own `.gitignore` and
    /// pushes it as a frame whose base is that directory, and it is a no-op when the file is absent.
    ///
    /// # Caveats
    ///
    /// * **The root-level sources are pushed on the first call**, not at construction: `info/exclude`
    ///   and `core.excludesFile` are read once, and a constructor that touched the filesystem would
    ///   make [`IgnoreStack::new`] fallible for no benefit.
    /// * **The file is read as bytes and parsed leniently** — a `.gitignore` that is not UTF-8 is
    ///   fine, and one that is unreadable is skipped with the rest of the stack intact, matching Git,
    ///   which warns and continues.
    /// * **The directory need not exist.** A walk that has stat'd an entry and is about to descend
    ///   may still race a deletion; the absence of a `.gitignore` is the same answer either way.
    /// * **Order matters and depth enforces it.** A caller that pushes a deeper directory before
    ///   shallower ones gets the wrong precedence, and the depth field is what makes that detectable
    ///   rather than silent.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidIgnorePattern`] for a `.gitignore` line that parses to nothing
    /// usable, and [`crate::error::Error::IoAtPath`] for a file that exists and cannot be read.
    pub fn push_dir(&mut self, dir: &BStr) -> Result<()> {
        let path = self.root.join(dir.to_string());
        let file = path.join(".gitignore");
        let data = match std::fs::read(&file) {
            Ok(d) => d,
            Err(_) => return Ok(()),
        };
        let frame = IgnoreFrame::parse(&data, IgnoreSource::Gitignore(BString::from(dir)), dir, self.frames.len())?;
        self.frames.push(frame);
        Ok(())
    }

    /// Phase 5 — push the git directory's `info/exclude`.
    ///
    /// Called once, at the root, and it is a no-op when the file is absent — which is the common
    /// case. The frame's base is the work tree root, because an `info/exclude` pattern is anchored
    /// there rather than to the git directory it lives in.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidIgnorePattern`] and [`crate::error::Error::IoAtPath`].
    pub fn push_info_exclude(&mut self, git_dir: &Path) -> Result<()> {
        let path = git_dir.join("info").join("exclude");
        let Ok(bytes) = std::fs::read(&path) else {
            return Ok(());
        };
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            self.push_dir(line.as_bytes().into());
        }
        Ok(())
    }

    /// Phase 5 — push the per-user excludes file.
    ///
    /// # Caveats
    ///
    /// The path comes from `core.excludesFile`, and Git's default when it is unset is **nothing** —
    /// `$XDG_CONFIG_HOME/git/ignore` is only used if the user configured it. Rivet must not invent a
    /// default: doing so would make a repository behave differently under Rivet and under Git for the
    /// same config, which is exactly what the differential tests catch. A caller with no configured
    /// path passes `None` and gets no frame.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidIgnorePattern`] and [`crate::error::Error::IoAtPath`].
    pub fn push_excludes_file(&mut self, path: Option<&Path>) -> Result<()> {
        let Some(path) = path else {
            return Ok(());
        };
        let Ok(bytes) = std::fs::read(path) else {
            return Ok(());
        };
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            self.push_dir(line.as_bytes().into());
        }
        Ok(())
    }

    /// Phase 5 — drop every frame pushed for a directory at or below a depth.
    ///
    /// The ascension half of a walk: descending pushes, and coming back up must pop, or a sibling
    /// directory inherits the previous one's `.gitignore`. A walk that does not truncate reports files
    /// as ignored when the rule belonged to a completely different subtree.
    ///
    /// # Caveats
    ///
    /// Frames pushed for the root-level sources — `info/exclude`, `core.excludesFile` and the command
    /// line — have the lowest depth and must survive every truncation, which is why they are pushed
    /// with a depth that no directory can have.
    pub fn truncate(&mut self, depth: usize) {
        while self.frames.last().map(|f| f.depth).is_some_and(|d| d >= depth) {
            self.frames.pop();
        }
    }

    /// Phase 5 — the decision for a path, naming the pattern that made it.
    ///
    /// # Caveats
    ///
    /// * **Last matching rule wins, within a source and across sources in priority order.** Scanning
    ///   stops at the first source that produces a decision on the path itself — a deeper
    ///   `.gitignore` beats a shallower one even if the shallower one has a later matching `!`.
    /// * **The ancestor rule is not applied here.** This reports what the rules say about the path;
    ///   a path under an excluded directory is reported as unmatched by a `!build/keep` because the
    ///   negation *did* match. Applying the ancestor check is [`IgnoreStack::is_ignored`], and keeping
    ///   them separate is what lets `check-ignore -v` explain the difference.
    /// * **The path is work-tree-relative**, `/`-separated, with no leading slash. A root-relative
    ///   path with a leading `/` matches nothing at all, silently.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidPath`] for a path that is absolute or escapes the root; a
    /// question about a path outside the work tree has no answer and must not be guessed.
    pub fn check(&self, path: &BStr, kind: PathKind) -> Result<IgnoreDecision> {
        todo!()
    }

    /// Phase 5 — whether a path is ignored, with the ancestor rule applied.
    ///
    /// The question `status` and `add` actually ask. It is [`IgnoreStack::check`] plus the rule that
    /// **a path cannot be re-included if any of its ancestors is excluded**: a matching `!` counts
    /// only when every ancestor is unmatched or itself re-included.
    ///
    /// # Caveats
    ///
    /// * **The ancestor walk is the point, not an optimisation to skip.** `build/` and `!build/keep`
    ///   together leave `keep` ignored, and an implementation that tested only the leaf would
    ///   re-include it — which is the divergence plan §4 Phase 5 names as the thing the `t0008` corpus
    ///   exists to catch.
    /// * **This says nothing about the index.** A tracked file is never ignored whatever this
    ///   returns; the caller applies that check, because this module has no index and giving it one
    ///   would make it unusable for `check-ignore`.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidPath`] as [`IgnoreStack::check`].
    pub fn is_ignored(&self, path: &BStr, kind: PathKind) -> Result<bool> {
        let decision = self.check(path, kind)?;
        if decision.is_ignored() {
            return Ok(true);
        }
        if decision.is_reincluded() {
            let mut ancestor_ignored = false;
            for frame in self.frames() {
                for pattern in frame.patterns() {
                    if pattern.negated {
                        continue;
                    }
                    if pattern.matches_ancestor(path, kind) {
                        ancestor_ignored = true;
                        break;
                    }
                }
                if ancestor_ignored {
                    break;
                }
            }
            return Ok(!ancestor_ignored);
        }
        Ok(false)
    }

    /// Phase 5 — every rule that matches a path, in the order they were considered.
    ///
    /// Backs `check-ignore -v` with and without `--no-index`, and exists so that a user debugging a
    /// surprising ignore sees the whole chain — including the negation that is *not* taking effect
    /// because of an excluded ancestor, which is the case nobody can diagnose from a single verdict.
    ///
    /// # Caveats
    ///
    /// **Ordered from highest priority to lowest**, and the *last* element is not necessarily the
    /// winner: the winner is the first that matched in priority order. A caller rendering this must
    /// say which one decided, or a user will read the list backwards.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidPath`] as [`IgnoreStack::check`].
    pub fn explain(&self, path: &BStr, kind: PathKind) -> Result<Vec<&Pattern>> {
        let mut out = Vec::new();
        let mut depth = self.frames.len();
        let mut i = self.frames.len();
        while i > 0 {
            i -= 1;
            let frame = &self.frames[i];
            let mut j = frame.patterns.len();
            while j > 0 {
                j -= 1;
                let pat = &frame.patterns[j];
                if pat.is_inapplicable(kind) {
                    continue;
                }
                if pat.matches(path, kind) {
                    out.push(pat);
                }
            }
            depth = frame.depth;
        }
        let _ = depth;
        Ok(out)
    }

    /// Phase 5 — whether a directory should be descended into during a walk.
    ///
    /// # Caveats
    ///
    /// * **An ignored directory is not descended into**, which is both the performance win and the
    ///   reason the re-include rule exists — Git never sees the `.gitignore`-blocked file, so it
    ///   cannot un-ignore it. A walk that descends anyway and filters leaves at the end gets a
    ///   different answer from Git in exactly the case the corpus tests.
    /// * **A re-included directory must still be descended into**, even when a rule matched it,
    ///   because a `!` deeper inside may apply. Returning `false` for a directory that matched a
    ///   negated pattern is a silent loss of everything below it.
    /// * The answer is about the directory itself, not about its contents — an empty directory that
    ///   is ignored and one that is not are the same to this question.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::InvalidPath`] as [`IgnoreStack::check`].
    pub fn should_descend(&self, dir: &BStr) -> Result<bool> {
        let decision = self.check(dir, PathKind::Directory)?;
        Ok(!decision.is_ignored())
    }

    /// Phase 5 — every frame in the stack, lowest priority first.
    ///
    /// # Caveats
    ///
    /// The frames are held in increasing priority, so the *last* is the highest — a caller rendering
    /// them top-to-bottom for a user must reverse, or the effective rule appears last and the user
    /// reads the precedence backwards.
    pub fn frames(&self) -> &[IgnoreFrame] {
        &self.frames
    }

    /// Phase 5 — whether no source contributed a pattern.
    ///
    /// # Caveats
    ///
    /// True for a repository with no `.gitignore` anywhere, no `info/exclude` and no command-line
    /// patterns — which is most repositories — and it means every query answers
    /// [`IgnoreDecision::Unmatched`] without touching the filesystem.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}
